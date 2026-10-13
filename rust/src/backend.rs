//! Direct Wayland backend: one wlr-layer-shell background surface per
//! output, SHM buffers scaled to the output mode (port of
//! `WaylandBackend.hpp/cpp`).
//!
//! Protocol rules mirrored from C++:
//! - Never attach a buffer before the first acked configure (fatal error).
//! - SHM buffers are ARGB8888 (`wl_shm` format).
//! - Output names/modes come from xdg-output (`wl_output` mode as fallback).
//! - Switches animate through the active [`TransitionEffect`](crate::transition)
//!   (slide for workspace switches, fade for set/reload/init), paced at 120fps.
//!
//! Without a compositor every constructor works but [`WaylandBackend::init`]
//! returns `false` and all monitor operations degrade to safe no-ops, so
//! headless tests and `--help`-style paths never panic.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wayland_client::{
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::image::{scale_cover_argb, DecodedImage};
use crate::transition::{effect_name_for, make_transition, ScaledCache, SlideDir};

/// Log sink, mirroring C++ `WaylandBackend::LogFn`.
pub type LogFn = Arc<dyn Fn(String) + Send + Sync + 'static>;

/// Fallback output size when neither configure nor mode arrived yet.
const FALLBACK_W: u32 = 1920;
const FALLBACK_H: u32 = 1080;
/// Animation frame rate (even cadence on 60Hz and 144Hz panels).
const ANIM_FPS: u64 = 120;

/// One SHM file + pool slot. Pixel uploads go through `write_all_at`
/// (no mmap needed); the file stays alive in the record so commits never
/// outlive their storage.
struct ShmSlot {
    file: File,
    size: usize,
    pool: Option<wl_shm_pool::WlShmPool>,
}

struct OutputRecord {
    output: wl_output::WlOutput,
    /// `wl_registry` global name (hotplug removal key + Dispatch udata).
    global: u32,
    /// Bound `wl_output` version (`release` exists since v3).
    wl_version: u32,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    /// Connector name from xdg-output.
    name: String,
    mode_w: u32,
    mode_h: u32,
    scale: i32,
    cfg_w: u32,
    cfg_h: u32,
    configured: bool,
    surface: Option<wl_surface::WlSurface>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    slots: [Option<ShmSlot>; 2],
    slot_idx: usize,
    /// False until a buffer is attached (nil attach unmaps).
    mapped: bool,
    /// Image waiting for (or surviving) configure.
    pending: Option<Arc<DecodedImage>>,
    /// Last presented frame, for transitions.
    last_frame: Vec<u8>,
    last_w: u32,
    last_h: u32,
    scaled: ScaledCache,
    /// Bumps on every switch; animation loops abort when stale.
    generation: u64,
}

impl OutputRecord {
    fn size(&self) -> (u32, u32) {
        let w = if self.cfg_w > 0 {
            self.cfg_w
        } else {
            self.mode_w
        };
        let h = if self.cfg_h > 0 {
            self.cfg_h
        } else {
            self.mode_h
        };
        if w == 0 || h == 0 {
            (FALLBACK_W, FALLBACK_H)
        } else {
            (w, h)
        }
    }

    /// Tear down surface/buffer/pools but keep `pending` so a later
    /// configure or `set_wallpaper` re-attaches.
    fn teardown_surface(&mut self) {
        self.configured = false;
        self.mapped = false;
        self.generation = self.generation.wrapping_add(1);
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        for slot in self.slots.iter_mut() {
            if let Some(s) = slot.take() {
                if let Some(pool) = s.pool {
                    pool.destroy();
                }
                // `File` closes on drop.
            }
        }
    }
}

struct Shared {
    log: Option<LogFn>,
    conn: Option<Connection>,
    qh: Option<QueueHandle<WlState>>,
    display: Option<wayland_client::protocol::wl_display::WlDisplay>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_mgr: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    /// Keyed by `wl_registry` global name.
    outputs: HashMap<u32, OutputRecord>,
    /// Images stashed by monitor name across hotplug removal.
    pending_by_monitor: HashMap<String, Arc<DecodedImage>>,
    transition_name: String,
}

impl Shared {
    fn vlog(&self, msg: &str) {
        if let Some(log) = &self.log {
            log(msg.to_owned());
        }
    }

    fn find_key(&self, monitor: &str) -> Option<u32> {
        self.outputs
            .iter()
            .find(|(_, rec)| rec.name == monitor)
            .map(|(k, _)| *k)
    }

    fn flush(&self) {
        if let Some(conn) = &self.conn {
            let _ = conn.flush();
        }
    }
}

/// Dispatch state: callbacks lock [`Shared`] briefly and never round-trip.
struct WlState {
    shared: Arc<Mutex<Shared>>,
}

fn create_shm_file(size: usize) -> io::Result<File> {
    let dir: PathBuf = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for _ in 0..32 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("walleclipse-shm-{pid}-{nanos}-{n}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(f) => {
                f.set_len(size as u64)?;
                // Unlink immediately: fd-backed, like mkstemp+unlink.
                let _ = std::fs::remove_file(&path);
                return Ok(f);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create unique shm file",
    ))
}

/// (Re)create the layer surface + initial empty commit. True when a layer
/// surface exists afterwards (buffer still waits for configure).
/// Call with `shared` locked.
fn ensure_surface_locked(shared: &mut Shared, key: u32) -> bool {
    let (compositor, layer_shell) = match (&shared.compositor, &shared.layer_shell) {
        (Some(c), Some(l)) => (c.clone(), l.clone()),
        _ => return false,
    };
    let qh = match &shared.qh {
        Some(qh) => qh.clone(),
        None => return false,
    };
    let rec = match shared.outputs.get_mut(&key) {
        Some(rec) => rec,
        None => return false,
    };
    if rec.layer.is_some() {
        return true;
    }
    let surface: wl_surface::WlSurface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        Some(&rec.output),
        zwlr_layer_shell_v1::Layer::Background,
        "walleclipse".to_owned(),
        &qh,
        rec.global,
    );
    layer.set_size(0, 0);
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_exclusive_zone(-1);
    rec.surface = Some(surface);
    rec.layer = Some(layer);
    rec.configured = false;
    // Initial commit WITHOUT a buffer: the compositor replies with
    // configure; only then may a buffer be attached.
    if let Some(surface) = &rec.surface {
        surface.commit();
    }
    shared.flush();
    true
}

/// Attach `argb` (`w*h*4` ARGB8888) to the output's surface.
/// Call with `shared` locked.
fn attach_pixels_locked(shared: &mut Shared, key: u32, argb: &[u8], w: u32, h: u32) {
    if w == 0 || h == 0 || argb.len() != w as usize * h as usize * 4 {
        return;
    }
    // Snapshot the handles we need so the borrow checker is happy.
    let (surface, shm, qh) = {
        let rec = match shared.outputs.get(&key) {
            Some(rec) => rec,
            None => return,
        };
        if rec.surface.is_none() || rec.layer.is_none() || !rec.configured {
            return;
        }
        match (&rec.surface, &shared.shm, &shared.qh) {
            (Some(s), Some(shm), Some(qh)) => (s.clone(), shm.clone(), qh.clone()),
            _ => return,
        }
    };
    let size = w as usize * h as usize * 4;
    let stride = w as i32 * 4;
    // (Re)allocate the slot pair on size change; otherwise reuse the files.
    let pool = {
        let rec = match shared.outputs.get_mut(&key) {
            Some(rec) => rec,
            None => return,
        };
        let need_new = rec.slots[0]
            .as_ref()
            .map(|s| s.size != size)
            .unwrap_or(true);
        if need_new {
            for slot in rec.slots.iter_mut() {
                if let Some(s) = slot.take() {
                    if let Some(pool) = s.pool {
                        pool.destroy();
                    }
                }
            }
            for slot in rec.slots.iter_mut() {
                let file = match create_shm_file(size) {
                    Ok(f) => f,
                    Err(_) => {
                        for s in rec.slots.iter_mut() {
                            *s = None;
                        }
                        return;
                    }
                };
                let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
                *slot = Some(ShmSlot {
                    file,
                    size,
                    pool: Some(pool),
                });
            }
            rec.slot_idx = 0;
        }
        rec.slot_idx %= 2;
        let slot = match rec.slots[rec.slot_idx].as_mut() {
            Some(slot) => slot,
            None => return,
        };
        rec.slot_idx += 1;
        if slot.file.write_all_at(argb, 0).is_err() {
            return;
        }
        match &slot.pool {
            Some(pool) => pool.clone(),
            None => return,
        }
    };
    let buffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    shared.flush();
    if let Some(rec) = shared.outputs.get_mut(&key) {
        if let Some(old) = rec.buffer.replace(buffer) {
            old.destroy();
        }
        rec.mapped = true;
        rec.last_frame = argb.to_vec();
        rec.last_w = w;
        rec.last_h = h;
    } else {
        buffer.destroy();
    }
}

/// Scale `img` cover-fit into `w*h` (cache-aware) and attach.
/// Locks `shared` internally in two phases (lookup, then store) so the
/// ~90ms scale never blocks Wayland event dispatch.
fn attach_locked(shared: &Arc<Mutex<Shared>>, key: u32) {
    let (img, w, h) = {
        let guard = shared.lock().unwrap();
        let rec = match guard.outputs.get(&key) {
            Some(rec) => rec,
            None => return,
        };
        if rec.surface.is_none() || rec.layer.is_none() || !rec.configured {
            return;
        }
        let pending = match rec.pending.clone() {
            Some(img) => img,
            None => return,
        };
        let (w, h) = rec.size();
        (pending, w, h)
    };
    let argb = cover_cached(shared, key, &img, w, h);
    let mut guard = shared.lock().unwrap();
    // Skip when superseded between scale and attach.
    let current = guard
        .outputs
        .get(&key)
        .filter(|rec| rec.configured)
        .and_then(|rec| rec.pending.clone())
        .is_some_and(|pending| Arc::ptr_eq(&pending, &img));
    if current {
        attach_pixels_locked(&mut guard, key, &argb, w, h);
    }
}

/// Cover-fit `img` into `w*h`, reusing the per-output cache.
/// Locking mirrors C++ `coverCached`: lookup under lock, scale outside it.
fn cover_cached(
    shared: &Arc<Mutex<Shared>>,
    key: u32,
    img: &Arc<DecodedImage>,
    w: u32,
    h: u32,
) -> Vec<u8> {
    {
        let guard = shared.lock().unwrap();
        if let Some(rec) = guard.outputs.get(&key) {
            if let Some(hit) = rec.scaled.find(img, w, h) {
                return hit.clone();
            }
        }
    }
    let argb = scale_cover_argb(img, w, h);
    {
        let mut guard = shared.lock().unwrap();
        if let Some(rec) = guard.outputs.get_mut(&key) {
            if rec.scaled.find(img, w, h).is_none() {
                rec.scaled.store(Arc::clone(img), w, h, argb.clone());
            }
        }
    }
    argb
}

fn valid_img(img: &DecodedImage) -> bool {
    img.width > 0 && img.height > 0 && !img.rgba.is_empty()
}

// --- Dispatch ---------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
    fn event(
        state: &mut Self,
        proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                let mut guard = state.shared.lock().unwrap();
                match interface.as_str() {
                    "wl_compositor" => {
                        guard.compositor = Some(proxy.bind(name, version.min(4), qh, ()));
                    }
                    "wl_shm" => {
                        guard.shm = Some(proxy.bind(name, version.min(1), qh, ()));
                    }
                    "wl_output" => {
                        let wl_version = version.min(3);
                        let output: wl_output::WlOutput = proxy.bind(name, wl_version, qh, name);
                        let xdg = guard
                            .xdg_mgr
                            .as_ref()
                            .map(|mgr| mgr.get_xdg_output(&output, qh, name));
                        guard.outputs.insert(
                            name,
                            OutputRecord {
                                output,
                                global: name,
                                wl_version,
                                xdg,
                                name: String::new(),
                                mode_w: 0,
                                mode_h: 0,
                                scale: 1,
                                cfg_w: 0,
                                cfg_h: 0,
                                configured: false,
                                surface: None,
                                layer: None,
                                buffer: None,
                                slots: [None, None],
                                slot_idx: 0,
                                mapped: false,
                                pending: None,
                                last_frame: Vec::new(),
                                last_w: 0,
                                last_h: 0,
                                scaled: ScaledCache::new(),
                                generation: 0,
                            },
                        );
                        guard.vlog(&format!("output global {name} added"));
                    }
                    "zwlr_layer_shell_v1" => {
                        guard.layer_shell = Some(proxy.bind(name, version.min(4), qh, ()));
                    }
                    "zxdg_output_manager_v1" => {
                        guard.xdg_mgr = Some(proxy.bind(name, version.min(3), qh, ()));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                let mut guard = state.shared.lock().unwrap();
                guard.vlog(&format!("output global {name} removed (hotplug/sleep?)"));
                if let Some(mut rec) = guard.outputs.remove(&name) {
                    if !rec.name.is_empty() {
                        if let Some(pending) = rec.pending.clone() {
                            guard.pending_by_monitor.insert(rec.name.clone(), pending);
                        }
                    }
                    rec.teardown_surface();
                    if let Some(xdg) = rec.xdg.take() {
                        xdg.destroy();
                    }
                    // `release` exists since v3; older binds just drop.
                    if rec.wl_version >= 3 {
                        rec.output.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut guard = state.shared.lock().unwrap();
        let Some(rec) = guard.outputs.get_mut(global) else {
            return;
        };
        match event {
            wl_output::Event::Mode { width, height, .. } => {
                // Preferred mode wins in practice; the last announced mode
                // is close enough (configure size is authoritative).
                if width > 0 && height > 0 {
                    rec.mode_w = width as u32;
                    rec.mode_h = height as u32;
                }
            }
            wl_output::Event::Scale { factor } => {
                rec.scale = if factor > 0 { factor } else { 1 };
            }
            wl_output::Event::Geometry { .. }
            | wl_output::Event::Done
            | wl_output::Event::Name { .. }
            | wl_output::Event::Description { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for WlState {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Name handling needs the qh-free helpers only; lock, update, and
        // (for reconnect restore) ensure the surface while locked.
        let zxdg_output_v1::Event::Name { name } = event else {
            return;
        };
        let key = *global;
        // Update the connector name first (short borrow).
        {
            let mut guard = state.shared.lock().unwrap();
            let Some(rec) = guard.outputs.get_mut(&key) else {
                return;
            };
            rec.name = name;
        }
        // Monitor (re)appeared after sleep/hotplug: restore the stashed
        // image and eagerly recreate the layer surface. Never attach here
        // (fatal before first acked configure); the configure handler does.
        let mut guard = state.shared.lock().unwrap();
        let stashed = guard
            .outputs
            .get(&key)
            .and_then(|rec| guard.pending_by_monitor.get(&rec.name).cloned());
        if let Some(stashed) = stashed {
            let already = guard
                .outputs
                .get(&key)
                .is_some_and(|rec| rec.pending.is_some());
            if !already {
                if let Some(rec) = guard.outputs.get_mut(&key) {
                    rec.pending = Some(stashed);
                }
            }
        }
        let needs_surface = guard.outputs.get(&key).is_some_and(|rec| {
            rec.pending.is_some()
                && rec.layer.is_none()
                && guard.compositor.is_some()
                && guard.layer_shell.is_some()
        });
        if needs_surface && ensure_surface_locked(&mut guard, key) {
            let name = guard
                .outputs
                .get(&key)
                .map(|rec| rec.name.clone())
                .unwrap_or_default();
            guard.vlog(&format!("output '{name}' reconnected, restoring wallpaper"));
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for WlState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let key = *global;
        {
            let mut guard = state.shared.lock().unwrap();
            let Some(rec) = guard.outputs.get_mut(&key) else {
                return;
            };
            match event {
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                } => {
                    proxy.ack_configure(serial);
                    rec.configured = true;
                    rec.cfg_w = width;
                    rec.cfg_h = height;
                }
                zwlr_layer_surface_v1::Event::Closed => {
                    // Surface is gone (reload/reconfigure/sleep): tear down
                    // so the next set_wallpaper recreates it. Pending
                    // survives.
                    rec.teardown_surface();
                    guard.vlog("layer surface closed, will repaint on next setWallpaper/configure");
                    return;
                }
                _ => return,
            }
            if guard
                .outputs
                .get(&key)
                .and_then(|rec| rec.pending.clone())
                .is_none()
            {
                return;
            }
        }
        // Scale outside the lock; attach_locked re-validates before attach.
        attach_locked(&state.shared, key);
    }
}

wayland_client::delegate_noop!(WlState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(WlState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(WlState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(WlState: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(WlState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(WlState: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
wayland_client::delegate_noop!(WlState: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(WlState: ignore wl_callback::WlCallback);

// --- Backend ----------------------------------------------------------------

/// Direct Wayland wallpaper backend (port of C++ `WaylandBackend`).
pub struct WaylandBackend {
    shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
    dispatch: Mutex<Option<JoinHandle<()>>>,
}

impl WaylandBackend {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                log: None,
                conn: None,
                qh: None,
                display: None,
                compositor: None,
                shm: None,
                layer_shell: None,
                xdg_mgr: None,
                outputs: HashMap::new(),
                pending_by_monitor: HashMap::new(),
                transition_name: "slide".to_owned(),
            })),
            running: Arc::new(AtomicBool::new(false)),
            dispatch: Mutex::new(None),
        }
    }

    /// Connect to the compositor and create one background layer surface per
    /// output. Returns `false` (cleanly, no panic) when no compositor is
    /// reachable or required globals are missing.
    pub fn init(&self, log: Option<LogFn>) -> bool {
        if self.running.load(Ordering::SeqCst) {
            return true;
        }
        {
            let mut guard = self.shared.lock().unwrap();
            guard.log = log;
        }
        let conn = match Connection::connect_to_env() {
            Ok(conn) => conn,
            Err(_) => {
                self.shared
                    .lock()
                    .unwrap()
                    .vlog("wl_display_connect failed (no Wayland display?)");
                return false;
            }
        };
        let mut queue: EventQueue<WlState> = conn.new_event_queue();
        let qh = queue.handle();
        let state = WlState {
            shared: Arc::clone(&self.shared),
        };
        let mut state = state;
        let display = conn.display();
        display.get_registry(&qh, ());
        // Two round-trips so xdg-output names arrive before surfaces exist.
        if queue.roundtrip(&mut state).is_err() || queue.roundtrip(&mut state).is_err() {
            self.shared
                .lock()
                .unwrap()
                .vlog("initial wayland roundtrip failed");
            return false;
        }
        {
            let mut guard = self.shared.lock().unwrap();
            if guard.compositor.is_none() || guard.shm.is_none() || guard.layer_shell.is_none() {
                guard.vlog("compositor missing wl_compositor/wl_shm/layer-shell");
                return false;
            }
            guard.conn = Some(conn);
            guard.qh = Some(qh);
            guard.display = Some(display);
            let keys: Vec<u32> = guard.outputs.keys().copied().collect();
            for key in keys {
                ensure_surface_locked(&mut guard, key);
            }
        }
        // Settle configures for freshly created surfaces (best-effort).
        let _ = queue.roundtrip(&mut state);
        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let handle = std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                match queue.blocking_dispatch(&mut state) {
                    Ok(_) => {}
                    Err(_) => {
                        if running.load(Ordering::SeqCst) {
                            shared
                                .lock()
                                .unwrap()
                                .vlog("wl_display_dispatch error; event loop stopped");
                        }
                        break;
                    }
                }
            }
        });
        *self.dispatch.lock().unwrap() = Some(handle);
        true
    }

    /// Stop the dispatch thread and release live Wayland objects.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        // Wake the dispatch thread: a no-op sync round-trips through the
        // server and ends `blocking_dispatch`.
        {
            let guard = self.shared.lock().unwrap();
            if let (Some(display), Some(qh)) = (&guard.display, &guard.qh) {
                display.sync(qh, ());
                guard.flush();
            }
        }
        if let Some(handle) = self.dispatch.lock().unwrap().take() {
            let _ = handle.join();
        }
        // Release live objects so a later `init` starts fresh.
        {
            let mut guard = self.shared.lock().unwrap();
            for (_, mut rec) in guard.outputs.drain() {
                rec.teardown_surface();
                if let Some(xdg) = rec.xdg.take() {
                    xdg.destroy();
                }
                if rec.wl_version >= 3 {
                    rec.output.release();
                }
            }
            guard.compositor = None;
            guard.shm = None;
            guard.layer_shell = None;
            guard.xdg_mgr = None;
            guard.display = None;
            guard.qh = None;
            guard.conn = None;
        }
    }

    /// Currently known output (monitor) names via xdg-output.
    pub fn outputs(&self) -> Vec<String> {
        let guard = self.shared.lock().unwrap();
        guard
            .outputs
            .values()
            .map(|rec| {
                if rec.name.is_empty() {
                    "(unnamed)".to_owned()
                } else {
                    rec.name.clone()
                }
            })
            .collect()
    }

    /// Show a decoded image fullscreen on a monitor (cover-fit).
    /// Returns `false` when the monitor is unknown or not yet configured.
    /// `SlideDir::None` cross-fades (black on first paint); directional
    /// switches use the active transition effect.
    pub fn set_wallpaper(&self, monitor: &str, img: Arc<DecodedImage>) -> bool {
        self.set_wallpaper_dir(monitor, img, SlideDir::None)
    }

    /// [`WaylandBackend::set_wallpaper`] with an explicit slide direction
    /// (from workspace order).
    pub fn set_wallpaper_dir(&self, monitor: &str, img: Arc<DecodedImage>, dir: SlideDir) -> bool {
        if !valid_img(&img) {
            return false;
        }
        struct Snapshot {
            key: u32,
            w: u32,
            h: u32,
            was_configured: bool,
            was_mapped: bool,
            old_frame: Vec<u8>,
            old_w: u32,
            old_h: u32,
            trans_name: String,
            generation: u64,
        }
        let snap = {
            let mut guard = self.shared.lock().unwrap();
            let key = match guard.find_key(monitor) {
                Some(key) => key,
                None => return false,
            };
            guard
                .pending_by_monitor
                .insert(monitor.to_owned(), Arc::clone(&img));
            let trans_name = guard.transition_name.clone();
            if !ensure_surface_locked(&mut guard, key) {
                return false;
            }
            let rec = match guard.outputs.get_mut(&key) {
                Some(rec) => rec,
                None => return false,
            };
            rec.pending = Some(Arc::clone(&img));
            rec.generation = rec.generation.wrapping_add(1);
            let (w, h) = rec.size();
            Snapshot {
                key,
                w,
                h,
                was_configured: rec.configured,
                was_mapped: rec.mapped,
                old_frame: rec.last_frame.clone(),
                old_w: rec.last_w,
                old_h: rec.last_h,
                trans_name,
                generation: rec.generation,
            }
        };
        let new_argb = cover_cached(&self.shared, snap.key, &img, snap.w, snap.h);
        let is_current = || {
            self.shared
                .lock()
                .unwrap()
                .outputs
                .get(&snap.key)
                .is_some_and(|rec| rec.generation == snap.generation)
        };
        let want = effect_name_for(dir, &snap.trans_name);
        let effect = make_transition(&want);
        let have_old = snap.was_mapped
            && snap.old_w == snap.w
            && snap.old_h == snap.h
            && !snap.old_frame.is_empty()
            && snap.old_frame.len() == new_argb.len();
        // First paint fades in from black (opaque).
        let mut from = snap.old_frame;
        let fade_from_black = dir == SlideDir::None
            && from.is_empty()
            && !snap.was_mapped
            && effect.as_ref().is_some_and(|e| e.duration_ms() > 0);
        if fade_from_black {
            from = vec![0u8; new_argb.len()];
            for px in from.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
        let animated = effect.as_ref().is_some_and(|e| e.duration_ms() > 0)
            && snap.was_configured
            && (have_old || fade_from_black);
        if !animated {
            let mut guard = self.shared.lock().unwrap();
            let rec = match guard.outputs.get_mut(&snap.key) {
                Some(rec) if rec.generation == snap.generation => rec,
                // Superseded by a newer switch, or output vanished.
                _ => return true,
            };
            if !rec.configured {
                return true; // pends until configure (handler attaches)
            }
            if rec.mapped {
                let (w, h) = (snap.w, snap.h);
                attach_pixels_locked(&mut guard, snap.key, &new_argb, w, h);
            } else if rec.surface.is_some() {
                // Unmapped (hidden for video): empty commit asks the
                // compositor to remap; the configure handler attaches.
                if let Some(surface) = rec.surface.clone() {
                    surface.commit();
                }
                guard.flush();
            }
            return true;
        }
        let effect = effect.expect("checked animated");
        let frames = (i64::from(effect.duration_ms()) * ANIM_FPS as i64 / 1000).max(1);
        let frame_ns = 1_000_000_000u64 / ANIM_FPS;
        let t0 = Instant::now();
        for i in 1..=frames {
            if !is_current() {
                return true; // superseded (or hidden) mid-animation
            }
            let progress = i as f64 / frames as f64;
            let frame = effect.render(&from, &new_argb, snap.w, snap.h, progress, dir);
            {
                let mut guard = self.shared.lock().unwrap();
                match guard.outputs.get(&snap.key) {
                    Some(rec)
                        if rec.generation == snap.generation
                            && rec.configured
                            && rec.surface.is_some() => {}
                    _ => return true,
                }
                attach_pixels_locked(&mut guard, snap.key, &frame, snap.w, snap.h);
            }
            if i < frames {
                let deadline = t0 + Duration::from_nanos(frame_ns * i as u64);
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }
            }
        }
        true
    }

    /// Swap the transition effect ("slide", "fade", "none"). Unknown names
    /// are ignored.
    pub fn set_transition(&self, name: &str) {
        if make_transition(name).is_none() {
            return;
        }
        self.shared.lock().unwrap().transition_name = name.to_owned();
    }

    pub fn transition(&self) -> String {
        self.shared.lock().unwrap().transition_name.clone()
    }

    /// Whether the monitor's layer surface currently has a buffer attached.
    pub fn is_mapped(&self, monitor: &str) -> bool {
        let guard = self.shared.lock().unwrap();
        guard
            .find_key(monitor)
            .and_then(|key| guard.outputs.get(&key))
            .is_some_and(|rec| rec.mapped)
    }

    /// Re-show the surface with an image after it was hidden, waiting (up to
    /// `timeout_ms`) for the compositor configure round-trip. No-op `true`
    /// when already mapped; `false` on unknown monitor or timeout.
    pub fn remap_with_image(&self, monitor: &str, img: Arc<DecodedImage>, timeout_ms: u64) -> bool {
        if !valid_img(&img) {
            return false;
        }
        {
            let mut guard = self.shared.lock().unwrap();
            let key = match guard.find_key(monitor) {
                Some(key) => key,
                None => return false,
            };
            let mapped = guard.outputs.get(&key).is_some_and(|rec| rec.mapped);
            if mapped {
                return true;
            }
            if !ensure_surface_locked(&mut guard, key) {
                return false;
            }
            guard
                .pending_by_monitor
                .insert(monitor.to_owned(), Arc::clone(&img));
            let surface = {
                let rec = match guard.outputs.get_mut(&key) {
                    Some(rec) => rec,
                    None => return false,
                };
                rec.pending = Some(img);
                rec.generation = rec.generation.wrapping_add(1);
                rec.surface.clone()
            };
            match surface {
                // Empty commit asks the compositor to remap; the configure
                // handler attaches pending.
                Some(surface) => surface.commit(),
                None => return false,
            }
            guard.flush();
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if self.is_mapped(monitor) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.is_mapped(monitor)
    }

    /// Scale + cache a wallpaper for a monitor without showing it.
    pub fn precache(&self, monitor: &str, img: Arc<DecodedImage>) {
        if !valid_img(&img) {
            return;
        }
        let (key, w, h) = {
            let guard = self.shared.lock().unwrap();
            let key = match guard.find_key(monitor) {
                Some(key) => key,
                None => return,
            };
            let rec = match guard.outputs.get(&key) {
                Some(rec) => rec,
                None => return,
            };
            let (w, h) = rec.size();
            if rec.scaled.find(&img, w, h).is_some() {
                return;
            }
            (key, w, h)
        };
        let _ = cover_cached(&self.shared, key, &img, w, h);
    }

    /// Unmap the layer surface (used when video takes over).
    pub fn hide_monitor(&self, monitor: &str) {
        let mut guard = self.shared.lock().unwrap();
        let key = match guard.find_key(monitor) {
            Some(key) => key,
            None => return,
        };
        guard.pending_by_monitor.remove(monitor);
        let surface = {
            let rec = match guard.outputs.get_mut(&key) {
                Some(rec) => rec,
                None => return,
            };
            rec.pending = None; // stay hidden across future configures
            rec.generation = rec.generation.wrapping_add(1);
            if rec.surface.is_none() || !rec.mapped {
                return; // already unmapped: no commit needed
            }
            rec.surface.clone()
        };
        if let Some(surface) = surface {
            surface.attach(None, 0, 0);
            surface.commit();
        }
        guard.flush();
        if let Some(rec) = guard.outputs.get_mut(&key) {
            rec.mapped = false;
        }
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

impl Default for WaylandBackend {
    fn default() -> Self {
        Self::new()
    }
}
