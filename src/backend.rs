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
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::image::{scale_cover_argb, DecodedImage};
use crate::transition::{effect_name_for, make_transition, ScaledCache, SlideDir};

// glibc: hand freed heap back to the OS. A 1080p ARGB frame is 8MB, and a
// preview fade allocates a few of them per call; without this the freed
// blocks stay in the process's arena and the daemon's RSS parks hundreds
// of MB above its baseline after a handful of hovers (measured: 381MB
// idle -> 800MB after 20 preview/clear cycles). No-ops off glibc.
#[cfg(target_env = "gnu")]
extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

/// Return freed heap to the OS (glibc). Cheap enough to call once at the
/// end of a preview show/clear; a no-op on non-glibc targets.
fn trim_heap() {
    #[cfg(target_env = "gnu")]
    unsafe {
        malloc_trim(0);
    }
}

/// Log sink, mirroring C++ `WaylandBackend::LogFn`.
pub type LogFn = Arc<dyn Fn(String) + Send + Sync + 'static>;

/// Fallback output size when neither configure nor mode arrived yet.
const FALLBACK_W: u32 = 1920;
const FALLBACK_H: u32 = 1080;
/// Animation frame rate (even cadence on 60Hz and 144Hz panels).
const ANIM_FPS: u64 = 120;

/// Tag attached to each `wl_buffer` so release events can be routed back
/// to the exact (output, slot) that owns it. `seq` guards against stale
/// releases from a buffer retired by resize/overwrite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlotTag {
    output: u32,
    slot: usize,
    seq: u64,
}

/// One SHM file + pool slot. Pixel uploads go through `write_all_at`
/// (no mmap needed); the file stays alive in the record so commits never
/// outlive their storage. `busy` is set on attach and cleared by the
/// compositor's `wl_buffer.release`; a slot is never overwritten while
/// busy unless every slot is busy past the wait budget (see
/// `wait_for_free_slot`).
struct ShmSlot {
    file: File,
    size: usize,
    pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    busy: bool,
    seq: u64,
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
    slots: [Option<ShmSlot>; 2],
    slot_idx: usize,
    /// Monotonic attach counter; feeds `SlotTag::seq`.
    buf_seq: u64,
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

    /// Tear down surface/buffers/pools but keep `pending` so a later
    /// configure or `set_wallpaper` re-attaches. Only call outside
    /// Dispatch callbacks (e.g. `shutdown`, after the dispatch thread is
    /// joined); callbacks must use the graveyard (`on_surface_closed`,
    /// `on_global_remove`) instead.
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
        for slot in self.slots.iter_mut() {
            if let Some(s) = slot.take() {
                if let Some(buffer) = s.buffer {
                    buffer.destroy();
                }
                if let Some(pool) = s.pool {
                    pool.destroy();
                }
                // `File` closes on drop.
            }
        }
    }

    /// Move every live Wayland handle out of the record into a graveyard
    /// entry. The record keeps `pending`/cache; destruction happens later
    /// in `drain_graveyard_locked`, never inside a Dispatch callback.
    fn into_dead(mut self) -> DeadHandles {
        let mut pools = Vec::new();
        let mut buffers = Vec::new();
        for slot in self.slots.iter_mut() {
            if let Some(s) = slot.take() {
                pools.extend(s.pool);
                buffers.extend(s.buffer);
            }
        }
        DeadHandles {
            surface: self.surface.take(),
            layer: self.layer.take(),
            buffers,
            pools,
            xdg: self.xdg.take(),
            output: Some((self.output, self.wl_version)),
        }
    }
}

/// Wayland objects retired from a Dispatch callback. Destroying the
/// dispatching proxy inside its own callback risks use-after-free in the
/// dispatch machinery, so callbacks only *move* handles here. The single
/// destroy site is `drain_graveyard_locked`, invoked outside callbacks
/// (dispatch-loop turn, public API entry, `shutdown`).
#[derive(Default)]
struct DeadHandles {
    surface: Option<wl_surface::WlSurface>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    buffers: Vec<wl_buffer::WlBuffer>,
    pools: Vec<wl_shm_pool::WlShmPool>,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    output: Option<(wl_output::WlOutput, u32)>,
}

/// Overlay preview surface for one output: a transient `Layer::Overlay`
/// layer surface above the background, used for hover previews. Lives in
/// `Shared::preview_surfaces` (keyed by output global), parallel to
/// `OutputRecord` — never in `OutputRecord.surface`, so show/clear never
/// disturbs the background underneath.
struct PreviewRecord {
    surface: Option<wl_surface::WlSurface>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    slots: [Option<ShmSlot>; 2],
    slot_idx: usize,
    /// Monotonic attach counter; feeds `PreviewSlotTag::seq`.
    buf_seq: u64,
    /// Configure size of the preview layer surface (0 until first ack).
    cfg_w: u32,
    cfg_h: u32,
    configured: bool,
    /// False until a buffer is attached (nil attach unmaps).
    mapped: bool,
    /// Image waiting for (or surviving) configure.
    pending: Option<Arc<DecodedImage>>,
    /// Last presented frame, for fade-out.
    last_frame: Vec<u8>,
    last_w: u32,
    last_h: u32,
    scaled: ScaledCache,
    /// Current opacity (1.0 shown, 0.0 cleared); tracks the fade progress
    /// while animating.
    alpha: f32,
    /// Bumps on every show/clear; animation loops abort when stale.
    generation: u64,
}

impl PreviewRecord {
    /// Tear down surface/buffers/pools but keep `pending` so a later
    /// configure or `show_preview` re-attaches. Only call outside
    /// Dispatch callbacks (e.g. `shutdown`, after the dispatch thread is
    /// joined); callbacks must use the graveyard instead.
    fn teardown_surface(&mut self) {
        self.configured = false;
        self.mapped = false;
        self.alpha = 0.0;
        self.generation = self.generation.wrapping_add(1);
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        for slot in self.slots.iter_mut() {
            if let Some(s) = slot.take() {
                if let Some(buffer) = s.buffer {
                    buffer.destroy();
                }
                if let Some(pool) = s.pool {
                    pool.destroy();
                }
                // `File` closes on drop.
            }
        }
    }

    /// Move every live Wayland handle out of the record into a graveyard
    /// entry. Destruction happens later in `drain_graveyard_locked`, never
    /// inside a Dispatch callback.
    fn into_dead(mut self) -> DeadHandles {
        let mut pools = Vec::new();
        let mut buffers = Vec::new();
        for slot in self.slots.iter_mut() {
            if let Some(s) = slot.take() {
                pools.extend(s.pool);
                buffers.extend(s.buffer);
            }
        }
        DeadHandles {
            surface: self.surface.take(),
            layer: self.layer.take(),
            buffers,
            pools,
            xdg: None,
            output: None,
        }
    }
}

/// Dispatch udata for preview layer surfaces: the output global they
/// overlay. Distinct from the background `u32` udata so preview
/// configure/closed events route to their own `Dispatch` impl instead of
/// the background one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewLayerTag(u32);

/// Tag attached to each preview `wl_buffer` so release events can be
/// routed back to the exact preview (output, slot) that owns it. `seq`
/// guards against stale releases, mirroring `SlotTag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewSlotTag {
    output: u32,
    slot: usize,
    seq: u64,
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
    /// Transient Overlay preview surfaces, keyed by output global.
    /// Parallel to `outputs`: preview show/clear never touches the
    /// background record underneath.
    preview_surfaces: HashMap<u32, PreviewRecord>,
    /// Handles moved out of records inside Dispatch callbacks; destroyed
    /// by `drain_graveyard_locked` outside callbacks. See `DeadHandles`.
    graveyard: Vec<DeadHandles>,
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

    /// Queue a buffer for destruction outside the current callback.
    fn retire_buffer(&mut self, buffer: wl_buffer::WlBuffer) {
        self.graveyard.push(DeadHandles {
            buffers: vec![buffer],
            ..DeadHandles::default()
        });
    }

    /// Queue a pool for destruction outside the current callback.
    fn retire_pool(&mut self, pool: wl_shm_pool::WlShmPool) {
        self.graveyard.push(DeadHandles {
            pools: vec![pool],
            ..DeadHandles::default()
        });
    }
}

/// Destroy every handle retired to the graveyard. Call only outside
/// Dispatch callbacks (the dispatch loop drains after each turn; public
/// API entries drain first). This is the single destroy site for
/// callback-retired objects.
fn drain_graveyard_locked(shared: &mut Shared) {
    for dead in shared.graveyard.drain(..) {
        if let Some(layer) = dead.layer {
            layer.destroy();
        }
        if let Some(surface) = dead.surface {
            surface.destroy();
        }
        for buffer in dead.buffers {
            buffer.destroy();
        }
        for pool in dead.pools {
            pool.destroy();
        }
        if let Some(xdg) = dead.xdg {
            xdg.destroy();
        }
        if let Some((output, wl_version)) = dead.output {
            // `release` exists since v3; older binds just drop.
            if wl_version >= 3 {
                output.release();
            }
        }
    }
}

/// Lock-and-drain convenience for public API entries and the dispatch loop.
fn drain_graveyard(shared: &Arc<Mutex<Shared>>) {
    let mut guard = shared.lock().unwrap();
    if !guard.graveyard.is_empty() {
        drain_graveyard_locked(&mut guard);
    }
}

/// Handle a layer-surface `closed` event (reload/reconfigure/sleep).
/// Only flags + handle *moves* happen here; destruction is deferred to
/// the graveyard so the dispatching layer proxy is never destroyed inside
/// its own callback.
fn on_surface_closed(shared: &Arc<Mutex<Shared>>, key: u32) {
    let mut guard = shared.lock().unwrap();
    let dead = {
        let Some(rec) = guard.outputs.get_mut(&key) else {
            return;
        };
        rec.configured = false;
        rec.mapped = false;
        rec.generation = rec.generation.wrapping_add(1);
        let mut pools = Vec::new();
        let mut buffers = Vec::new();
        for slot in rec.slots.iter_mut() {
            if let Some(s) = slot.as_mut() {
                pools.extend(s.pool.take());
                buffers.extend(s.buffer.take());
                s.busy = false;
            }
        }
        DeadHandles {
            surface: rec.surface.take(),
            layer: rec.layer.take(),
            buffers,
            pools,
            xdg: None,
            output: None,
        }
    };
    guard.graveyard.push(dead);
    // The overlay belongs to the same output: retire it on the same path
    // so a later show_preview rebuilds from clean state.
    retire_preview_locked(&mut guard, key);
    guard.vlog("layer surface closed, will repaint on next setWallpaper/configure");
}

/// Handle a `wl_output` global removal (hotplug/sleep). The record is
/// detached from the map immediately (stashing `pending` by monitor name)
/// and its handles move to the graveyard; destruction happens outside
/// the callback via `drain_graveyard_locked`.
fn on_global_remove(shared: &Arc<Mutex<Shared>>, name: u32) {
    let mut guard = shared.lock().unwrap();
    guard.vlog(&format!("output global {name} removed (hotplug/sleep?)"));
    // The overlay dies with its output: same-path retirement.
    if let Some(prec) = guard.preview_surfaces.remove(&name) {
        guard.graveyard.push(prec.into_dead());
    }
    let Some(rec) = guard.outputs.remove(&name) else {
        return; // not an output global (compositor/shm/...)
    };
    if !rec.name.is_empty() {
        if let Some(pending) = rec.pending.clone() {
            guard.pending_by_monitor.insert(rec.name.clone(), pending);
        }
    }
    guard.graveyard.push(rec.into_dead());
}

/// Drop all live connection state (outputs, globals, queue handles) while
/// keeping sticky configuration (`pending_by_monitor`, transition, log).
/// Used on `init` failure paths so a retry never re-binds over stale
/// entries. No protocol `destroy` is sent: the connection itself is being
/// abandoned and its fd close releases server-side objects.
fn reset_live_locked(shared: &mut Shared) {
    shared.outputs.clear();
    shared.preview_surfaces.clear();
    shared.graveyard.clear();
    shared.compositor = None;
    shared.shm = None;
    shared.layer_shell = None;
    shared.xdg_mgr = None;
    shared.display = None;
    shared.qh = None;
    shared.conn = None;
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

/// Maximum time an attach waits for a compositor-released SHM slot
/// before reusing the oldest one anyway (one frame at the 120fps cadence).
const SLOT_WAIT_BUDGET: Duration = Duration::from_millis(8);
/// Poll step while waiting for a release.
const SLOT_WAIT_STEP: Duration = Duration::from_millis(1);

/// Wait (without holding the lock, so release events keep flowing) for a
/// non-busy slot with a matching pool, then return its index. Falls back
/// to round-robin reuse after `budget`; returns `None` when the output
/// vanished or its pools no longer match `size`.
fn wait_for_free_slot(
    shared: &Arc<Mutex<Shared>>,
    key: u32,
    size: usize,
    budget: Duration,
) -> Option<usize> {
    let start = Instant::now();
    loop {
        {
            let guard = shared.lock().unwrap();
            let rec = match guard.outputs.get(&key) {
                Some(rec) => rec,
                None => return None,
            };
            for k in 0..2 {
                let i = (rec.slot_idx + k) % 2;
                match rec.slots[i].as_ref() {
                    Some(slot) if slot.size == size && slot.pool.is_some() && !slot.busy => {
                        return Some(i);
                    }
                    _ => {}
                }
            }
            let fallback_usable = rec.slots[rec.slot_idx % 2]
                .as_ref()
                .is_some_and(|slot| slot.size == size && slot.pool.is_some());
            if !fallback_usable {
                return None;
            }
            if start.elapsed() >= budget {
                return Some(rec.slot_idx % 2);
            }
        }
        std::thread::sleep(SLOT_WAIT_STEP);
    }
}

/// Handle a `wl_buffer.release`: the compositor is done sampling the
/// frame, so the slot is reusable. Runs inside the buffer's own dispatch
/// callback — only flags + graveyard moves here, never destruction.
/// Stale tags (seq mismatch, e.g. after resize/overwrite) are ignored.
fn on_buffer_release(shared: &Arc<Mutex<Shared>>, tag: &SlotTag) {
    let mut guard = shared.lock().unwrap();
    let released = {
        let rec = match guard.outputs.get_mut(&tag.output) {
            Some(rec) => rec,
            None => return,
        };
        let slot = match rec.slots.get_mut(tag.slot) {
            Some(slot) => match slot.as_mut() {
                Some(slot) => slot,
                None => return,
            },
            None => return,
        };
        if slot.seq != tag.seq || !slot.busy {
            return;
        }
        slot.busy = false;
        slot.buffer.take()
    };
    if let Some(buffer) = released {
        guard.retire_buffer(buffer);
    }
}

/// Attach `argb` (`w*h*4` ARGB8888) to the output's surface using a
/// release-gated SHM slot. Locks internally in short phases; the slot
/// wait never holds the lock so `release` events keep flowing.
fn attach_pixels_locked(shared: &Arc<Mutex<Shared>>, key: u32, argb: &[u8], w: u32, h: u32) {
    if w == 0 || h == 0 || argb.len() != w as usize * h as usize * 4 {
        return;
    }
    let size = w as usize * h as usize * 4;
    let stride = w as i32 * 4;
    // Phase 1: validate + snapshot handles + (re)allocate pools.
    let (surface, qh) = {
        let mut guard = shared.lock().unwrap();
        {
            let rec = match guard.outputs.get(&key) {
                Some(rec) => rec,
                None => return,
            };
            if rec.surface.is_none() || rec.layer.is_none() || !rec.configured {
                return;
            }
        }
        let (surface, shm, qh) = match (
            guard.outputs.get(&key).and_then(|rec| rec.surface.clone()),
            guard.shm.clone(),
            guard.qh.clone(),
        ) {
            (Some(surface), Some(shm), Some(qh)) => (surface, shm, qh),
            _ => return,
        };
        let need_new = guard
            .outputs
            .get(&key)
            .map(|rec| {
                rec.slots[0]
                    .as_ref()
                    .map(|slot| slot.size != size || slot.pool.is_none())
                    .unwrap_or(true)
            })
            .unwrap_or(true);
        if need_new {
            let mut retired_pools = Vec::new();
            let mut retired_buffers = Vec::new();
            if let Some(rec) = guard.outputs.get_mut(&key) {
                for slot in rec.slots.iter_mut() {
                    if let Some(s) = slot.take() {
                        retired_pools.extend(s.pool);
                        retired_buffers.extend(s.buffer);
                    }
                }
            }
            for pool in retired_pools {
                guard.retire_pool(pool);
            }
            for buffer in retired_buffers {
                guard.retire_buffer(buffer);
            }
            let mut files = Vec::new();
            for _ in 0..2 {
                match create_shm_file(size) {
                    Ok(file) => files.push(file),
                    Err(_) => return,
                }
            }
            let mut files = files.into_iter();
            if let Some(rec) = guard.outputs.get_mut(&key) {
                for slot in rec.slots.iter_mut() {
                    let file = match files.next() {
                        Some(file) => file,
                        None => return,
                    };
                    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
                    *slot = Some(ShmSlot {
                        file,
                        size,
                        pool: Some(pool),
                        buffer: None,
                        busy: false,
                        seq: 0,
                    });
                }
                rec.slot_idx = 0;
            }
        }
        (surface, qh)
    };
    // Phase 2: wait for a free slot without holding the lock.
    let idx = match wait_for_free_slot(shared, key, size, SLOT_WAIT_BUDGET) {
        Some(idx) => idx,
        None => return,
    };
    // Phase 3: write pixels, create the tagged buffer, attach — atomically
    // under one lock hold so a release for our tag cannot slip between
    // create and store (the tag is unknowable to the server before our
    // commit, which happens before unlock).
    {
        let mut guard = shared.lock().unwrap();
        {
            let rec = match guard.outputs.get(&key) {
                Some(rec) => rec,
                None => return,
            };
            if !rec.configured || rec.surface.is_none() || rec.layer.is_none() {
                return;
            }
            if rec.slots[idx]
                .as_ref()
                .map(|slot| slot.size != size || slot.pool.is_none())
                .unwrap_or(true)
            {
                return;
            }
        }
        let (pool, tag, displaced) = {
            let rec = match guard.outputs.get_mut(&key) {
                Some(rec) => rec,
                None => return,
            };
            let slot = match rec.slots[idx].as_mut() {
                Some(slot) => slot,
                None => return,
            };
            if slot.file.write_all_at(argb, 0).is_err() {
                return;
            }
            rec.buf_seq = rec.buf_seq.wrapping_add(1);
            slot.seq = rec.buf_seq;
            slot.busy = true;
            let pool = match slot.pool.clone() {
                Some(pool) => pool,
                None => return,
            };
            // Still-busy slot (wait budget exhausted): the in-flight buffer
            // is retired; its late release is discarded by seq mismatch.
            let displaced = slot.buffer.take();
            (
                pool,
                SlotTag {
                    output: key,
                    slot: idx,
                    seq: slot.seq,
                },
                displaced,
            )
        };
        if let Some(old) = displaced {
            guard.retire_buffer(old);
        }
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            stride,
            wl_shm::Format::Argb8888,
            &qh,
            tag,
        );
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        guard.flush();
        {
            let mut buffer = Some(buffer);
            let stale = {
                let rec = match guard.outputs.get_mut(&key) {
                    Some(rec) => rec,
                    None => {
                        if let Some(buffer) = buffer.take() {
                            guard.retire_buffer(buffer);
                        }
                        return;
                    }
                };
                match rec.slots[idx].as_mut() {
                    Some(slot) if slot.seq == tag.seq => {
                        let stale = slot.buffer.take();
                        slot.buffer = buffer.take();
                        rec.slot_idx = (idx + 1) % 2;
                        rec.mapped = true;
                        rec.last_frame = argb.to_vec();
                        rec.last_w = w;
                        rec.last_h = h;
                        stale
                    }
                    _ => None,
                }
            };
            if let Some(stale) = stale {
                guard.retire_buffer(stale);
            }
            // `buffer` is `Some` only when not stored (superseded/gone).
            if let Some(buffer) = buffer.take() {
                guard.retire_buffer(buffer);
            }
        }
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
    let guard = shared.lock().unwrap();
    // Skip when superseded between scale and attach.
    let current = guard
        .outputs
        .get(&key)
        .filter(|rec| rec.configured)
        .and_then(|rec| rec.pending.clone())
        .is_some_and(|pending| Arc::ptr_eq(&pending, &img));
    drop(guard);
    if current {
        attach_pixels_locked(shared, key, &argb, w, h);
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

/// (Re)create the Overlay preview surface + initial empty commit. True
/// when a preview layer surface exists afterwards (buffer still waits for
/// configure). Mirrors `ensure_surface_locked` with `Layer::Overlay`,
/// `set_exclusive_zone(-1)` and an empty input region (a region with no
/// rects), so the preview floats above the background and never takes
/// input. Call with `shared` locked.
fn ensure_preview_surface_locked(shared: &mut Shared, key: u32) -> bool {
    let (compositor, layer_shell) = match (&shared.compositor, &shared.layer_shell) {
        (Some(c), Some(l)) => (c.clone(), l.clone()),
        _ => return false,
    };
    let qh = match &shared.qh {
        Some(qh) => qh.clone(),
        None => return false,
    };
    let output = match shared.outputs.get(&key) {
        Some(rec) => rec.output.clone(),
        None => return false,
    };
    if shared
        .preview_surfaces
        .get(&key)
        .is_some_and(|prec| prec.layer.is_some())
    {
        return true;
    }
    let surface: wl_surface::WlSurface = compositor.create_surface(&qh, ());
    // Empty input region: the overlay is purely visual.
    let region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();
    let layer = layer_shell.get_layer_surface(
        &surface,
        Some(&output),
        zwlr_layer_shell_v1::Layer::Overlay,
        "walleclipse-preview".to_owned(),
        &qh,
        PreviewLayerTag(key),
    );
    layer.set_size(0, 0);
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_exclusive_zone(-1);
    let prec = shared
        .preview_surfaces
        .entry(key)
        .or_insert_with(|| PreviewRecord {
            surface: None,
            layer: None,
            slots: [None, None],
            slot_idx: 0,
            buf_seq: 0,
            cfg_w: 0,
            cfg_h: 0,
            configured: false,
            mapped: false,
            pending: None,
            last_frame: Vec::new(),
            last_w: 0,
            last_h: 0,
            scaled: ScaledCache::new(),
            alpha: 0.0,
            generation: 0,
        });
    prec.surface = Some(surface);
    prec.layer = Some(layer);
    prec.configured = false;
    // Initial commit WITHOUT a buffer: the compositor replies with
    // configure; only then may a buffer be attached.
    if let Some(surface) = &prec.surface {
        surface.commit();
    }
    shared.flush();
    true
}

/// Preview geometry: its own configure size once acked, else the output
/// size underneath (the overlay always covers the full output).
/// Call with `shared` locked.
fn preview_size_locked(shared: &Shared, key: u32) -> (u32, u32) {
    if let Some(prec) = shared.preview_surfaces.get(&key) {
        if prec.cfg_w > 0 && prec.cfg_h > 0 {
            return (prec.cfg_w, prec.cfg_h);
        }
    }
    shared
        .outputs
        .get(&key)
        .map(|rec| rec.size())
        .unwrap_or((FALLBACK_W, FALLBACK_H))
}

/// Cover-fit `img` into `w*h`, reusing the per-preview cache.
/// Locking mirrors `cover_cached`: lookup under lock, scale outside it.
fn cover_cached_preview(
    shared: &Arc<Mutex<Shared>>,
    key: u32,
    img: &Arc<DecodedImage>,
    w: u32,
    h: u32,
) -> Vec<u8> {
    {
        let guard = shared.lock().unwrap();
        if let Some(prec) = guard.preview_surfaces.get(&key) {
            if let Some(hit) = prec.scaled.find(img, w, h) {
                return hit.clone();
            }
        }
    }
    let argb = scale_cover_argb(img, w, h);
    {
        let mut guard = shared.lock().unwrap();
        if let Some(prec) = guard.preview_surfaces.get_mut(&key) {
            if prec.scaled.find(img, w, h).is_none() {
                prec.scaled.store(Arc::clone(img), w, h, argb.clone());
            }
        }
    }
    argb
}

/// Preview twin of `wait_for_free_slot`: the same release-gated search
/// over the preview record's slots. Never holds the lock while waiting.
fn wait_for_free_preview_slot(
    shared: &Arc<Mutex<Shared>>,
    key: u32,
    size: usize,
    budget: Duration,
) -> Option<usize> {
    let start = Instant::now();
    loop {
        {
            let guard = shared.lock().unwrap();
            let prec = match guard.preview_surfaces.get(&key) {
                Some(prec) => prec,
                None => return None,
            };
            for k in 0..2 {
                let i = (prec.slot_idx + k) % 2;
                match prec.slots[i].as_ref() {
                    Some(slot) if slot.size == size && slot.pool.is_some() && !slot.busy => {
                        return Some(i);
                    }
                    _ => {}
                }
            }
            let fallback_usable = prec.slots[prec.slot_idx % 2]
                .as_ref()
                .is_some_and(|slot| slot.size == size && slot.pool.is_some());
            if !fallback_usable {
                return None;
            }
            if start.elapsed() >= budget {
                return Some(prec.slot_idx % 2);
            }
        }
        std::thread::sleep(SLOT_WAIT_STEP);
    }
}

/// Preview twin of `on_buffer_release`: clears the owning preview slot's
/// busy flag (seq-guarded) and retires the buffer. Callback-safe: flags +
/// graveyard moves only, never destruction.
fn on_preview_release(shared: &Arc<Mutex<Shared>>, tag: &PreviewSlotTag) {
    let mut guard = shared.lock().unwrap();
    let released = {
        let prec = match guard.preview_surfaces.get_mut(&tag.output) {
            Some(prec) => prec,
            None => return,
        };
        let slot = match prec.slots.get_mut(tag.slot) {
            Some(slot) => match slot.as_mut() {
                Some(slot) => slot,
                None => return,
            },
            None => return,
        };
        if slot.seq != tag.seq || !slot.busy {
            return;
        }
        slot.busy = false;
        slot.buffer.take()
    };
    if let Some(buffer) = released {
        guard.retire_buffer(buffer);
    }
}

/// Attach `argb` (`w*h*4` ARGB8888) to the preview's overlay surface using
/// a release-gated SHM slot. Preview twin of `attach_pixels_locked`:
/// same three phases (snapshot + allocate, lock-free slot wait, atomic
/// write/create/attach/store), plus preview bookkeeping (`mapped`,
/// `last_frame` for fade-out). `generation` must be the caller's snapshot
/// generation: a stale attach (superseded show/clear, or a surface
/// recreated under the same record) is refused entirely — it neither
/// commits a buffer nor pins `mapped`, so an aborted show can never
/// re-present pixels over a dismissal. Returns true only when pixels
/// attached and stored; any no-op returns false so callers pin
/// shown-state on success alone.
fn attach_preview_pixels_locked(
    shared: &Arc<Mutex<Shared>>,
    key: u32,
    argb: &[u8],
    w: u32,
    h: u32,
    generation: u64,
    store_frame: bool,
) -> bool {
    // `store_frame` copies this frame into `last_frame` (the fade-out
    // source). It is false for intermediate fade frames: a ~40-frame fade
    // would otherwise copy 8MB per frame, per show and per clear — churn
    // that never returns to the OS and parks the daemon's RSS hundreds of
    // MB above its baseline.
    if w == 0 || h == 0 || argb.len() != w as usize * h as usize * 4 {
        return false;
    }
    let size = w as usize * h as usize * 4;
    let stride = w as i32 * 4;
    // Phase 1: validate + snapshot handles + (re)allocate pools.
    let (surface, qh) = {
        let mut guard = shared.lock().unwrap();
        {
            let prec = match guard.preview_surfaces.get(&key) {
                Some(prec) => prec,
                None => return false,
            };
            if prec.surface.is_none()
                || prec.layer.is_none()
                || !prec.configured
                || prec.generation != generation
            {
                return false;
            }
        }
        let (surface, shm, qh) = match (
            guard
                .preview_surfaces
                .get(&key)
                .and_then(|prec| prec.surface.clone()),
            guard.shm.clone(),
            guard.qh.clone(),
        ) {
            (Some(surface), Some(shm), Some(qh)) => (surface, shm, qh),
            _ => return false,
        };
        let need_new = guard
            .preview_surfaces
            .get(&key)
            .map(|prec| {
                prec.slots[0]
                    .as_ref()
                    .map(|slot| slot.size != size || slot.pool.is_none())
                    .unwrap_or(true)
            })
            .unwrap_or(true);
        if need_new {
            let mut retired_pools = Vec::new();
            let mut retired_buffers = Vec::new();
            if let Some(prec) = guard.preview_surfaces.get_mut(&key) {
                for slot in prec.slots.iter_mut() {
                    if let Some(s) = slot.take() {
                        retired_pools.extend(s.pool);
                        retired_buffers.extend(s.buffer);
                    }
                }
            }
            for pool in retired_pools {
                guard.retire_pool(pool);
            }
            for buffer in retired_buffers {
                guard.retire_buffer(buffer);
            }
            let mut files = Vec::new();
            for _ in 0..2 {
                match create_shm_file(size) {
                    Ok(file) => files.push(file),
                    Err(_) => return false,
                }
            }
            let mut files = files.into_iter();
            if let Some(prec) = guard.preview_surfaces.get_mut(&key) {
                for slot in prec.slots.iter_mut() {
                    let file = match files.next() {
                        Some(file) => file,
                        None => return false,
                    };
                    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
                    *slot = Some(ShmSlot {
                        file,
                        size,
                        pool: Some(pool),
                        buffer: None,
                        busy: false,
                        seq: 0,
                    });
                }
                prec.slot_idx = 0;
            }
        }
        (surface, qh)
    };
    // Phase 2: wait for a free slot without holding the lock.
    let idx = match wait_for_free_preview_slot(shared, key, size, SLOT_WAIT_BUDGET) {
        Some(idx) => idx,
        None => return false,
    };
    // Phase 3: write pixels, create the tagged buffer, attach — atomically
    // under one lock hold so a release for our tag cannot slip between
    // create and store.
    {
        let mut guard = shared.lock().unwrap();
        {
            let prec = match guard.preview_surfaces.get(&key) {
                Some(prec) => prec,
                None => return false,
            };
            if !prec.configured
                || prec.surface.is_none()
                || prec.layer.is_none()
                || prec.generation != generation
            {
                return false;
            }
            if prec.slots[idx]
                .as_ref()
                .map(|slot| slot.size != size || slot.pool.is_none())
                .unwrap_or(true)
            {
                return false;
            }
        }
        let (pool, tag, displaced) = {
            let prec = match guard.preview_surfaces.get_mut(&key) {
                Some(prec) => prec,
                None => return false,
            };
            let slot = match prec.slots[idx].as_mut() {
                Some(slot) => slot,
                None => return false,
            };
            if slot.file.write_all_at(argb, 0).is_err() {
                return false;
            }
            prec.buf_seq = prec.buf_seq.wrapping_add(1);
            slot.seq = prec.buf_seq;
            slot.busy = true;
            let pool = match slot.pool.clone() {
                Some(pool) => pool,
                None => return false,
            };
            // Still-busy slot (wait budget exhausted): the in-flight buffer
            // is retired; its late release is discarded by seq mismatch.
            let displaced = slot.buffer.take();
            (
                pool,
                PreviewSlotTag {
                    output: key,
                    slot: idx,
                    seq: slot.seq,
                },
                displaced,
            )
        };
        if let Some(old) = displaced {
            guard.retire_buffer(old);
        }
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            stride,
            wl_shm::Format::Argb8888,
            &qh,
            tag,
        );
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        guard.flush();
        let stored = {
            let mut buffer = Some(buffer);
            let (stale, stored) = {
                let prec = match guard.preview_surfaces.get_mut(&key) {
                    Some(prec) => prec,
                    None => {
                        if let Some(buffer) = buffer.take() {
                            guard.retire_buffer(buffer);
                        }
                        return false;
                    }
                };
                match prec.slots[idx].as_mut() {
                    Some(slot) if slot.seq == tag.seq => {
                        let stale = slot.buffer.take();
                        slot.buffer = buffer.take();
                        prec.slot_idx = (idx + 1) % 2;
                        prec.mapped = true;
                        if store_frame {
                            prec.last_frame = argb.to_vec();
                            prec.last_w = w;
                            prec.last_h = h;
                        }
                        (stale, true)
                    }
                    _ => (None, false),
                }
            };
            if let Some(stale) = stale {
                guard.retire_buffer(stale);
            }
            // `buffer` is `Some` only when not stored (superseded/gone).
            if let Some(buffer) = buffer.take() {
                guard.retire_buffer(buffer);
            }
            stored
        };
        stored
    }
}

/// Scale the preview's pending image cover-fit and attach.
/// Preview twin of `attach_locked`: two phases so the scale never blocks
/// Wayland event dispatch.
fn attach_preview_locked(shared: &Arc<Mutex<Shared>>, key: u32) {
    let (img, w, h, generation) = {
        let guard = shared.lock().unwrap();
        let prec = match guard.preview_surfaces.get(&key) {
            Some(prec) => prec,
            None => return,
        };
        if prec.surface.is_none() || prec.layer.is_none() || !prec.configured {
            return;
        }
        let pending = match prec.pending.clone() {
            Some(img) => img,
            None => return,
        };
        let (w, h) = preview_size_locked(&guard, key);
        (pending, w, h, prec.generation)
    };
    let argb = cover_cached_preview(shared, key, &img, w, h);
    let guard = shared.lock().unwrap();
    // Skip when superseded between scale and attach.
    let current = guard
        .preview_surfaces
        .get(&key)
        .filter(|prec| prec.configured && prec.generation == generation)
        .and_then(|prec| prec.pending.clone())
        .is_some_and(|pending| Arc::ptr_eq(&pending, &img));
    drop(guard);
    if current && attach_preview_pixels_locked(shared, key, &argb, w, h, generation, true) {
        // The pending image is now fully presented. This path never runs
        // the fade loop, so pin opacity here — otherwise `mapped` would be
        // true with `alpha == 0.0` and `has_preview` would read false while
        // pixels are on screen. Still ptr-guarded: a clear landing between
        // the attach and this pin owns `alpha` from here on.
        let mut guard = shared.lock().unwrap();
        if let Some(prec) = guard.preview_surfaces.get_mut(&key) {
            if prec.generation == generation
                && prec.pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, &img))
            {
                prec.alpha = 1.0;
            }
        }
    }
}

/// Nil-attach the preview surface (unmap) and flag it down, but only when
/// `generation` still matches (superseded clears must not unmap a newer
/// show). The overlay vanishes; the background underneath shows through
/// untouched.
///
/// The nil-attach commit and the flag clear share one lock hold: an
/// attach running concurrently can only land entirely before this (its
/// pixels are then overwritten by the nil commit) or entirely after (its
/// own `mapped = true` stands), never between the commit and the flag
/// clear — which would leave pixels on screen with `mapped == false` and
/// strand the overlay against every later dismiss.
fn unmap_preview_locked(shared: &Arc<Mutex<Shared>>, key: u32, generation: u64) {
    let mut guard = shared.lock().unwrap();
    let surface = match guard.preview_surfaces.get(&key) {
        Some(prec) if prec.generation == generation => prec.surface.clone(),
        _ => return,
    };
    if let Some(surface) = surface {
        surface.attach(None, 0, 0);
        surface.commit();
    }
    if let Some(prec) = guard.preview_surfaces.get_mut(&key) {
        if prec.generation == generation {
            prec.mapped = false;
            prec.alpha = 0.0;
            prec.last_frame.clear();
            prec.last_w = 0;
            prec.last_h = 0;
            // The NULL commit above unmaps this layer surface, which makes
            // it unconfigured again: attaching a buffer before the next
            // configure is a protocol error that tears down the whole
            // wl_display connection. `show_preview` re-requests a configure
            // (empty commit) and pends until the handler attaches.
            prec.configured = false;
            // Drop the scaled-frame cache with the overlay: up to a dozen
            // 1080p frames (~100MB) would otherwise sit resident while no
            // preview is on screen. A re-hover re-scales (~90ms), which the
            // panel's hover dwell hides.
            prec.scaled.clear();
        }
    }
    guard.flush();
    drop(guard);
    // Every dismiss funnels through here, so the frames the fade/attach
    // allocated are released to the OS from one place.
    trim_heap();
}

/// Move one preview record's live handles to the graveyard (generation
/// bump, unmap, pending/cache kept for the next show). Call with `shared`
/// locked; moves only, never destruction.
fn retire_preview_locked(shared: &mut Shared, key: u32) {
    let dead = {
        let Some(prec) = shared.preview_surfaces.get_mut(&key) else {
            return;
        };
        prec.configured = false;
        prec.mapped = false;
        prec.alpha = 0.0;
        // Pixels die with the surface: drop the frame cache too, so a later
        // dismiss never fades from (or mistakes) a stale frame.
        prec.last_frame.clear();
        prec.last_w = 0;
        prec.last_h = 0;
        prec.generation = prec.generation.wrapping_add(1);
        let mut pools = Vec::new();
        let mut buffers = Vec::new();
        for slot in prec.slots.iter_mut() {
            if let Some(s) = slot.as_mut() {
                pools.extend(s.pool.take());
                buffers.extend(s.buffer.take());
                s.busy = false;
            }
        }
        DeadHandles {
            surface: prec.surface.take(),
            layer: prec.layer.take(),
            buffers,
            pools,
            xdg: None,
            output: None,
        }
    };
    shared.graveyard.push(dead);
    shared.vlog("preview surface retired, will rebuild on next show_preview");
}

/// Handle a preview layer-surface `closed` event: flags + handle moves
/// only, destruction deferred to the graveyard. See `on_surface_closed`.
fn on_preview_closed(shared: &Arc<Mutex<Shared>>, key: u32) {
    let mut guard = shared.lock().unwrap();
    retire_preview_locked(&mut guard, key);
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
                                slots: [None, None],
                                slot_idx: 0,
                                buf_seq: 0,
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
                        // The wlr XML advertises v5, but this backend uses
                        // no v5 requests; cap at v4 for C++ parity.
                        guard.layer_shell = Some(proxy.bind(name, version.min(4), qh, ()));
                    }
                    "zxdg_output_manager_v1" => {
                        guard.xdg_mgr = Some(proxy.bind(name, version.min(3), qh, ()));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                // Never destroy proxies here: the removed output's dispatch
                // may still be on the stack. Defer to the graveyard.
                on_global_remove(&state.shared, name);
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
        let need_attach = {
            let mut guard = state.shared.lock().unwrap();
            match event {
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                } => {
                    proxy.ack_configure(serial);
                    let Some(rec) = guard.outputs.get_mut(&key) else {
                        return;
                    };
                    rec.configured = true;
                    rec.cfg_w = width;
                    rec.cfg_h = height;
                    guard
                        .outputs
                        .get(&key)
                        .and_then(|rec| rec.pending.clone())
                        .is_some()
                }
                zwlr_layer_surface_v1::Event::Closed => {
                    // Drop the lock first: the retire path locks internally,
                    // and destruction must never happen inside this callback.
                    drop(guard);
                    on_surface_closed(&state.shared, key);
                    return;
                }
                _ => return,
            }
        };
        if need_attach {
            // Scale outside the lock; attach_locked re-validates.
            attach_locked(&state.shared, key);
        }
    }
}

/// Preview overlay layer surfaces carry a [`PreviewLayerTag`] (not the
/// background `u32`), so their configure/closed events land here instead
/// of the background handler: configure arms the preview record (the
/// pending image attaches, never before the first acked configure — fatal
/// otherwise), closed retires it to the graveyard.
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, PreviewLayerTag> for WlState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        tag: &PreviewLayerTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let key = tag.0;
        let need_attach = {
            let mut guard = state.shared.lock().unwrap();
            match event {
                zwlr_layer_surface_v1::Event::Configure {
                    serial,
                    width,
                    height,
                } => {
                    proxy.ack_configure(serial);
                    let Some(prec) = guard.preview_surfaces.get_mut(&key) else {
                        return;
                    };
                    prec.configured = true;
                    prec.cfg_w = width;
                    prec.cfg_h = height;
                    prec.pending.clone().is_some()
                }
                zwlr_layer_surface_v1::Event::Closed => {
                    // Drop the lock first: the retire path locks internally,
                    // and destruction must never happen inside this callback.
                    drop(guard);
                    on_preview_closed(&state.shared, key);
                    return;
                }
                _ => return,
            }
        };
        if need_attach {
            // Scale outside the lock; attach_preview_locked re-validates.
            attach_preview_locked(&state.shared, key);
        }
    }
}

/// Routes compositor `wl_buffer.release` events back to the owning slot.
/// Runs inside the buffer's own dispatch callback: flags + graveyard
/// moves only, never destruction. See `on_buffer_release`.
impl Dispatch<wl_buffer::WlBuffer, SlotTag> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        tag: &SlotTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_buffer::Event::Release => on_buffer_release(&state.shared, tag),
            _ => {}
        }
    }
}

/// Routes compositor `wl_buffer.release` events for preview buffers back
/// to the owning preview slot. See `on_preview_release`.
impl Dispatch<wl_buffer::WlBuffer, PreviewSlotTag> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        tag: &PreviewSlotTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_buffer::Event::Release => on_preview_release(&state.shared, tag),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(WlState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(WlState: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(WlState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(WlState: ignore wl_surface::WlSurface);
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
                preview_surfaces: HashMap::new(),
                graveyard: Vec::new(),
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
            // Start from a clean slate: a previous failed init must never
            // leak stale outputs/globals into the retry.
            reset_live_locked(&mut guard);
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
            let mut guard = self.shared.lock().unwrap();
            guard.vlog("initial wayland roundtrip failed");
            reset_live_locked(&mut guard);
            return false;
        }
        {
            let mut guard = self.shared.lock().unwrap();
            if guard.compositor.is_none() || guard.shm.is_none() || guard.layer_shell.is_none() {
                guard.vlog("compositor missing wl_compositor/wl_shm/layer-shell");
                reset_live_locked(&mut guard);
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
                    // Retired handles are destroyed here, outside any
                    // Dispatch callback (see `DeadHandles`).
                    Ok(_) => drain_graveyard(&shared),
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
            drain_graveyard_locked(&mut guard);
            for (_, mut rec) in guard.outputs.drain() {
                rec.teardown_surface();
                if let Some(xdg) = rec.xdg.take() {
                    xdg.destroy();
                }
                if rec.wl_version >= 3 {
                    rec.output.release();
                }
            }
            for (_, mut prec) in guard.preview_surfaces.drain() {
                prec.teardown_surface();
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
        drain_graveyard(&self.shared);
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
        drain_graveyard(&self.shared);
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
            // Snapshot the decision under lock; the attach itself locks
            // internally (slot wait must not hold the lock).
            let attach = {
                let guard = self.shared.lock().unwrap();
                let rec = match guard.outputs.get(&snap.key) {
                    Some(rec) if rec.generation == snap.generation => rec,
                    // Superseded by a newer switch, or output vanished.
                    _ => return true,
                };
                if !rec.configured {
                    return true; // pends until configure (handler attaches)
                }
                if rec.mapped {
                    true
                } else if let Some(surface) = rec.surface.clone() {
                    // Unmapped (hidden for video): empty commit asks the
                    // compositor to remap; the configure handler attaches.
                    surface.commit();
                    guard.flush();
                    false
                } else {
                    false
                }
            };
            if attach {
                let (w, h) = (snap.w, snap.h);
                attach_pixels_locked(&self.shared, snap.key, &new_argb, w, h);
            }
            return true;
        }
        let effect = effect.expect("checked animated");
        let frames = (i64::from(effect.duration_ms()) * ANIM_FPS as i64 / 1000).max(1);
        let frame_ns = 1_000_000_000u64 / ANIM_FPS;
        let t0 = Instant::now();
        // Reused frame buffers: one animation costs 2 allocations no
        // matter the frame count (was: one 8MB Vec per frame).
        let mut buf_a = vec![0u8; new_argb.len()];
        let mut buf_b = vec![0u8; new_argb.len()];
        for i in 1..=frames {
            if !is_current() {
                return true; // superseded (or hidden) mid-animation
            }
            let progress = i as f64 / frames as f64;
            let frame: &[u8] = if i % 2 == 1 {
                effect.render_into(&from, &new_argb, &mut buf_a, snap.w, snap.h, progress, dir);
                &buf_a
            } else {
                effect.render_into(&from, &new_argb, &mut buf_b, snap.w, snap.h, progress, dir);
                &buf_b
            };
            {
                let guard = self.shared.lock().unwrap();
                match guard.outputs.get(&snap.key) {
                    Some(rec)
                        if rec.generation == snap.generation
                            && rec.configured
                            && rec.surface.is_some() => {}
                    _ => return true,
                }
            }
            attach_pixels_locked(&self.shared, snap.key, frame, snap.w, snap.h);
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
        drain_graveyard(&self.shared);
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
        drain_graveyard(&self.shared);
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
        drain_graveyard(&self.shared);
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
        drain_graveyard(&self.shared);
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

    /// Show a transient fullscreen preview overlay on a monitor
    /// (cover-fit). The overlay is a `Layer::Overlay` surface floating
    /// above the untouched background: it takes no input and fades in from
    /// transparent via the fade effect. Re-previewing replaces the active
    /// preview in place. Returns `false` when the monitor is unknown or no
    /// preview surface can be created (headless / no compositor).
    pub fn show_preview(&self, monitor: &str, img: Arc<DecodedImage>) -> bool {
        if !valid_img(&img) {
            return false;
        }
        drain_graveyard(&self.shared);
        struct Snapshot {
            key: u32,
            w: u32,
            h: u32,
            generation: u64,
        }
        let snap = {
            let mut guard = self.shared.lock().unwrap();
            let key = match guard.find_key(monitor) {
                Some(key) => key,
                None => return false,
            };
            if !ensure_preview_surface_locked(&mut guard, key) {
                return false;
            }
            let (w, h) = preview_size_locked(&guard, key);
            let prec = match guard.preview_surfaces.get_mut(&key) {
                Some(prec) => prec,
                None => return false,
            };
            prec.pending = Some(Arc::clone(&img));
            prec.generation = prec.generation.wrapping_add(1);
            // An unmapped (or brand-new) layer surface must be configured
            // before any buffer may be attached. Ask for that configure with
            // an empty commit and let the configure handler attach `pending`
            // — exactly like the background's remap path. Without this the
            // surface never reconfigures, so the preview could not come back
            // after a dismiss.
            let needs_configure = !prec.configured;
            let surface = prec.surface.clone();
            let snap = Snapshot {
                key,
                w,
                h,
                generation: prec.generation,
            };
            if needs_configure {
                if let Some(surface) = surface {
                    surface.commit();
                }
                guard.flush();
            }
            snap
        };
        let new_argb = cover_cached_preview(&self.shared, snap.key, &img, snap.w, snap.h);
        let is_current = || {
            self.shared
                .lock()
                .unwrap()
                .preview_surfaces
                .get(&snap.key)
                .is_some_and(|prec| prec.generation == snap.generation)
        };
        // Fade duration source of truth (same constant the background
        // switches animate with); `transition == "none"` cuts straight to
        // the opaque frame.
        let no_anim = self.shared.lock().unwrap().transition_name == "none"
            || make_transition("fade")
                .map(|e| e.duration_ms() <= 0)
                .unwrap_or(true);
        if no_anim {
            let attach = {
                let guard = self.shared.lock().unwrap();
                let prec = match guard.preview_surfaces.get(&snap.key) {
                    Some(prec) if prec.generation == snap.generation => prec,
                    // Superseded by a newer show/clear, or output vanished.
                    _ => return true,
                };
                if !prec.configured {
                    return true; // pends until configure (handler attaches)
                }
                prec.surface.is_some() && prec.layer.is_some()
            };
            let attached = if attach {
                attach_preview_pixels_locked(
                    &self.shared,
                    snap.key,
                    &new_argb,
                    snap.w,
                    snap.h,
                    snap.generation,
                    true,
                )
            } else {
                false
            };
            // Pin shown-state only when pixels actually attached: a no-op
            // (surface gone mid-call, SHM failure) leaves the record
            // pending exactly like the animated path, so `has_preview`
            // stays false until the configure handler presents.
            if attached {
                let mut guard = self.shared.lock().unwrap();
                if let Some(prec) = guard.preview_surfaces.get_mut(&snap.key) {
                    if prec.generation == snap.generation {
                        prec.mapped = true;
                        prec.alpha = 1.0;
                        prec.last_frame = new_argb.clone();
                        prec.last_w = snap.w;
                        prec.last_h = snap.h;
                    }
                }
            }
            return true;
        }
        let effect = make_transition("fade").expect("checked duration above");
        // Transparent-black start; the fade blends it into the image,
        // scaling alpha from 0 to 1 (`SlideDir::None` = pure cross-blend).
        let from = vec![0u8; new_argb.len()];
        let frames = (i64::from(effect.duration_ms()) * ANIM_FPS as i64 / 1000).max(1);
        let frame_ns = 1_000_000_000u64 / ANIM_FPS;
        let t0 = Instant::now();
        // Reused frame buffers: one animation costs 2 allocations no
        // matter the frame count.
        let mut buf_a = vec![0u8; new_argb.len()];
        let mut buf_b = vec![0u8; new_argb.len()];
        for i in 1..=frames {
            if !is_current() {
                return true; // superseded by a newer show/clear
            }
            let progress = i as f64 / frames as f64;
            let frame: &[u8] = if i % 2 == 1 {
                effect.render_into(
                    &from,
                    &new_argb,
                    &mut buf_a,
                    snap.w,
                    snap.h,
                    progress,
                    SlideDir::None,
                );
                &buf_a
            } else {
                effect.render_into(
                    &from,
                    &new_argb,
                    &mut buf_b,
                    snap.w,
                    snap.h,
                    progress,
                    SlideDir::None,
                );
                &buf_b
            };
            let ok = {
                let mut guard = self.shared.lock().unwrap();
                match guard.preview_surfaces.get_mut(&snap.key) {
                    Some(prec)
                        if prec.generation == snap.generation
                            && prec.configured
                            && prec.surface.is_some()
                            && prec.layer.is_some() =>
                    {
                        prec.alpha = progress as f32;
                        true
                    }
                    _ => false,
                }
            };
            if !ok {
                return true;
            }
            attach_preview_pixels_locked(
                &self.shared,
                snap.key,
                frame,
                snap.w,
                snap.h,
                snap.generation,
                false,
            );
            if i < frames {
                let deadline = t0 + Duration::from_nanos(frame_ns * i as u64);
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }
            }
        }
        {
            // Final pin, re-checked for currency: a clear landing between
            // the last attach and this pin bumps the generation, and the
            // stale pin must not re-arm the dismissed preview.
            if is_current() {
                let mut guard = self.shared.lock().unwrap();
                if let Some(prec) = guard.preview_surfaces.get_mut(&snap.key) {
                    if prec.generation == snap.generation {
                        prec.mapped = true;
                        prec.alpha = 1.0;
                        // The fade skipped its per-frame copies, so record
                        // the settled frame here as the fade-out source.
                        // `copy_from_slice` reuses the existing allocation
                        // on repeat previews instead of reallocating 8MB.
                        if prec.last_frame.len() == new_argb.len() {
                            prec.last_frame.copy_from_slice(&new_argb);
                        } else {
                            prec.last_frame = new_argb.clone();
                        }
                        prec.last_w = snap.w;
                        prec.last_h = snap.h;
                    }
                }
            }
        }
        // The fade's frame buffers are dead now; give their pages back.
        trim_heap();
        true
    }

    /// Dismiss the preview overlay on a monitor, fading back to transparent
    /// (same duration source as the fade-in) and finishing with a nil
    /// attach that unmaps the overlay, so the untouched background shows
    /// through. Idle monitors — and unknown monitors — return `true`
    /// immediately (idempotent). Generation-guarded like `show_preview`.
    pub fn clear_preview(&self, monitor: &str) -> bool {
        drain_graveyard(&self.shared);
        struct Snapshot {
            key: u32,
            w: u32,
            h: u32,
            from: Vec<u8>,
            generation: u64,
        }
        let snap = {
            let mut guard = self.shared.lock().unwrap();
            let key = match guard.find_key(monitor) {
                // Unknown monitor: nothing to dismiss (idempotent).
                Some(key) => key,
                None => return true,
            };
            let prec = match guard.preview_surfaces.get_mut(&key) {
                Some(prec) => prec,
                None => return true,
            };
            // Abort any in-flight show and drop a pending-but-unmapped
            // preview so a later configure cannot resurrect it.
            prec.generation = prec.generation.wrapping_add(1);
            prec.pending = None;
            if !prec.mapped && prec.last_frame.is_empty() && prec.alpha <= 0.0 {
                // Pending-only/idle record: nothing may be on screen.
                return true;
            }
            if !prec.mapped {
                // Possibly-presented but unmapped (stale frame cache or
                // leftover opacity after surface death/failed attaches):
                // never early-true past it — nil-attach straight to the
                // unmap (no fade; nothing is claimed visible, so fading
                // would only flash pixels back on).
                let generation = prec.generation;
                drop(guard);
                unmap_preview_locked(&self.shared, key, generation);
                return true;
            }
            let generation = prec.generation;
            if prec.last_frame.is_empty() || prec.last_w == 0 || prec.last_h == 0 {
                // Mapped without a frame on record: unmap directly.
                drop(guard);
                unmap_preview_locked(&self.shared, key, generation);
                return true;
            }
            Snapshot {
                key,
                w: prec.last_w,
                h: prec.last_h,
                from: prec.last_frame.clone(),
                generation,
            }
        };
        let is_current = || {
            self.shared
                .lock()
                .unwrap()
                .preview_surfaces
                .get(&snap.key)
                .is_some_and(|prec| prec.generation == snap.generation)
        };
        let no_anim = self.shared.lock().unwrap().transition_name == "none"
            || make_transition("fade")
                .map(|e| e.duration_ms() <= 0)
                .unwrap_or(true);
        if !no_anim {
            let effect = make_transition("fade").expect("checked duration above");
            // Reverse ramp: image back to transparent black.
            let to = vec![0u8; snap.from.len()];
            let frames = (i64::from(effect.duration_ms()) * ANIM_FPS as i64 / 1000).max(1);
            let frame_ns = 1_000_000_000u64 / ANIM_FPS;
            let t0 = Instant::now();
            let mut buf_a = vec![0u8; snap.from.len()];
            let mut buf_b = vec![0u8; snap.from.len()];
            for i in 1..=frames {
                if !is_current() {
                    // Superseded by a newer show/clear. Fall through to the
                    // generation-guarded unmap below: it no-ops when a newer
                    // operation owns the record, and still clears pixels if
                    // the record is somehow orphaned unmapped.
                    break;
                }
                let progress = i as f64 / frames as f64;
                let frame: &[u8] = if i % 2 == 1 {
                    effect.render_into(
                        &snap.from,
                        &to,
                        &mut buf_a,
                        snap.w,
                        snap.h,
                        progress,
                        SlideDir::None,
                    );
                    &buf_a
                } else {
                    effect.render_into(
                        &snap.from,
                        &to,
                        &mut buf_b,
                        snap.w,
                        snap.h,
                        progress,
                        SlideDir::None,
                    );
                    &buf_b
                };
                let ok = {
                    let mut guard = self.shared.lock().unwrap();
                    match guard.preview_surfaces.get_mut(&snap.key) {
                        Some(prec)
                            if prec.generation == snap.generation
                                && prec.configured
                                && prec.surface.is_some()
                                && prec.layer.is_some() =>
                        {
                            prec.alpha = 1.0 - progress as f32;
                            true
                        }
                        _ => false,
                    }
                };
                if !ok {
                    // Record changed shape mid-fade (surface retired,
                    // superseded). Stop fading but never return past a
                    // record that may still hold pixels: fall through to
                    // the generation-guarded unmap.
                    break;
                }
                attach_preview_pixels_locked(
                    &self.shared,
                    snap.key,
                    frame,
                    snap.w,
                    snap.h,
                    snap.generation,
                    false,
                );
                if i < frames {
                    let deadline = t0 + Duration::from_nanos(frame_ns * i as u64);
                    let now = Instant::now();
                    if deadline > now {
                        std::thread::sleep(deadline - now);
                    }
                }
            }
        }
        // Finish with a nil attach: unmap so the background shows through.
        unmap_preview_locked(&self.shared, snap.key, snap.generation);
        true
    }

    /// Instantly dismiss the preview overlay on a monitor: no animation
    /// (cut). Bumps the record generation (aborts any in-flight show/clear
    /// loop via the existing generation guard), drops `pending` so a later
    /// configure cannot resurrect, then nil-attaches/unmaps with
    /// `mapped = false`, `alpha = 0.0`. Unknown/idle monitor → `true`
    /// (idempotent, like `clear_preview`). Used by the workspace-switch
    /// cancel and `set`-supersede paths, where the switch's own transition
    /// carries the motion; explicit `preview-clear` and the 5s watchdog
    /// keep the fading `clear_preview`.
    pub fn drop_preview(&self, monitor: &str) -> bool {
        drain_graveyard(&self.shared);
        let (key, generation) = {
            let mut guard = self.shared.lock().unwrap();
            let key = match guard.find_key(monitor) {
                // Unknown monitor: nothing to dismiss (idempotent).
                Some(key) => key,
                None => return true,
            };
            let prec = match guard.preview_surfaces.get_mut(&key) {
                Some(prec) => prec,
                None => return true,
            };
            prec.generation = prec.generation.wrapping_add(1);
            prec.pending = None;
            // Pending-only/idle records early-return; a possibly-presented
            // but unmapped record (stale frame cache/opacity) falls through
            // to the unmap below — never early-true past it.
            if !prec.mapped && prec.last_frame.is_empty() && prec.alpha <= 0.0 {
                return true;
            }
            (key, prec.generation)
        };
        unmap_preview_locked(&self.shared, key, generation);
        true
    }

    /// Whether the monitor currently shows a preview overlay (mapped with
    /// nonzero opacity).
    pub fn has_preview(&self, monitor: &str) -> bool {
        drain_graveyard(&self.shared);
        let guard = self.shared.lock().unwrap();
        guard
            .find_key(monitor)
            .and_then(|key| guard.preview_surfaces.get(&key))
            .is_some_and(|prec| prec.mapped && prec.alpha > 0.0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wayland_client::protocol::wl_display::WlDisplay;

    struct Fixture {
        shared: Arc<Mutex<Shared>>,
        key: u32,
        img: Arc<DecodedImage>,
        // Kept alive: proxies reference the backend, and the socket peer
        // stands in for a compositor (proxy creation only enqueues
        // requests, so no server traffic is needed).
        _peer: UnixStream,
        _conn: Connection,
        _queue: EventQueue<WlState>,
    }

    fn test_image() -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: 2,
            height: 2,
            rgba: vec![9u8; 2 * 2 * 4],
        })
    }

    /// Headless backend state with one fabricated output (global 7,
    /// "DP-1") holding a live layer surface and one pooled SHM slot.
    fn fixture() -> Fixture {
        let (local, peer) = UnixStream::pair().expect("socketpair");
        let conn = Connection::from_socket(local).expect("client backend");
        let queue: EventQueue<WlState> = conn.new_event_queue();
        let qh = queue.handle();
        let display: WlDisplay = conn.display();
        let registry = display.get_registry(&qh, ());
        // Fake global names; binds only enqueue requests client-side.
        let compositor: wl_compositor::WlCompositor = registry.bind(1, 4, &qh, ());
        let shm: wl_shm::WlShm = registry.bind(2, 1, &qh, ());
        let output: wl_output::WlOutput = registry.bind(3, 3, &qh, 7u32);
        let xdg_mgr: zxdg_output_manager_v1::ZxdgOutputManagerV1 = registry.bind(4, 3, &qh, ());
        let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = registry.bind(5, 4, &qh, ());
        let surface: wl_surface::WlSurface = compositor.create_surface(&qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Background,
            "test".to_owned(),
            &qh,
            7u32,
        );
        let xdg = xdg_mgr.get_xdg_output(&output, &qh, 7u32);
        let file = create_shm_file(16).expect("shm file");
        let pool = shm.create_pool(file.as_fd(), 16, &qh, ());
        let shared = Arc::new(Mutex::new(Shared {
            log: None,
            conn: Some(conn.clone()),
            qh: Some(qh),
            display: Some(display),
            compositor: Some(compositor),
            shm: Some(shm),
            layer_shell: Some(layer_shell),
            xdg_mgr: Some(xdg_mgr),
            outputs: HashMap::new(),
            preview_surfaces: HashMap::new(),
            graveyard: Vec::new(),
            pending_by_monitor: HashMap::new(),
            transition_name: "slide".to_owned(),
        }));
        let img = test_image();
        shared.lock().unwrap().outputs.insert(
            7,
            OutputRecord {
                output,
                global: 7,
                wl_version: 3,
                xdg: Some(xdg),
                name: "DP-1".to_owned(),
                mode_w: 4,
                mode_h: 4,
                scale: 1,
                cfg_w: 4,
                cfg_h: 4,
                configured: true,
                surface: Some(surface),
                layer: Some(layer),
                slots: [
                    Some(ShmSlot {
                        file,
                        size: 16,
                        pool: Some(pool),
                        buffer: None,
                        busy: false,
                        seq: 0,
                    }),
                    None,
                ],
                slot_idx: 0,
                buf_seq: 0,
                mapped: true,
                pending: Some(Arc::clone(&img)),
                last_frame: Vec::new(),
                last_w: 0,
                last_h: 0,
                scaled: ScaledCache::new(),
                generation: 1,
            },
        );
        Fixture {
            shared,
            key: 7,
            img,
            _peer: peer,
            _conn: conn,
            _queue: queue,
        }
    }

    /// [`WaylandBackend`] wrapping a fixture [`Shared`] (headless: no
    /// dispatch thread, proxy traffic only enqueues).
    fn test_backend(shared: &Arc<Mutex<Shared>>) -> WaylandBackend {
        WaylandBackend {
            shared: Arc::clone(shared),
            running: Arc::new(AtomicBool::new(false)),
            dispatch: Mutex::new(None),
        }
    }

    /// Headless backend from [`fixture`] plus a fabricated Overlay preview
    /// record for "DP-1" (surface/layer are enqueued proxies, configured so
    /// preview attaches proceed without compositor traffic).
    fn preview_fixture() -> Fixture {
        let f = fixture();
        let (compositor, layer_shell, qh, output) = {
            let guard = f.shared.lock().unwrap();
            (
                guard.compositor.clone().unwrap(),
                guard.layer_shell.clone().unwrap(),
                guard.qh.clone().unwrap(),
                guard.outputs.get(&f.key).unwrap().output.clone(),
            )
        };
        let surface: wl_surface::WlSurface = compositor.create_surface(&qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Overlay,
            "walleclipse-preview".to_owned(),
            &qh,
            PreviewLayerTag(f.key),
        );
        f.shared.lock().unwrap().preview_surfaces.insert(
            f.key,
            PreviewRecord {
                surface: Some(surface),
                layer: Some(layer),
                slots: [None, None],
                slot_idx: 0,
                buf_seq: 0,
                configured: true,
                cfg_w: 4,
                cfg_h: 4,
                mapped: false,
                pending: None,
                last_frame: Vec::new(),
                last_w: 0,
                last_h: 0,
                scaled: ScaledCache::new(),
                alpha: 0.0,
                generation: 1,
            },
        );
        f
    }

    #[test]
    fn closed_callback_only_retires_handles() {
        let f = fixture();
        on_surface_closed(&f.shared, f.key);
        {
            let guard = f.shared.lock().unwrap();
            let rec = guard.outputs.get(&f.key).expect("record survives close");
            assert!(!rec.configured && !rec.mapped);
            assert!(rec.pending.is_some(), "pending survives for repaint");
            assert!(rec.surface.is_none() && rec.layer.is_none());
            // Handles moved — not destroyed: exactly one graveyard entry
            // holding the surface, layer and pool.
            assert_eq!(guard.graveyard.len(), 1);
            let dead = &guard.graveyard[0];
            assert!(dead.surface.is_some() && dead.layer.is_some());
            assert_eq!(dead.pools.len(), 1);
            assert!(dead.output.is_none() && dead.xdg.is_none());
        }
        // Destruction happens only in the drain, outside the callback.
        drain_graveyard(&f.shared);
        assert!(f.shared.lock().unwrap().graveyard.is_empty());
    }

    #[test]
    fn global_remove_only_retires_handles() {
        let f = fixture();
        on_global_remove(&f.shared, f.key);
        {
            let guard = f.shared.lock().unwrap();
            assert!(guard.outputs.get(&f.key).is_none());
            assert!(
                guard
                    .pending_by_monitor
                    .get("DP-1")
                    .is_some_and(|p| Arc::ptr_eq(p, &f.img)),
                "pending stashed by monitor name"
            );
            assert_eq!(guard.graveyard.len(), 1);
            let dead = &guard.graveyard[0];
            assert!(dead.surface.is_some() && dead.layer.is_some());
            assert!(dead.xdg.is_some());
            assert!(dead.output.is_some());
        }
        drain_graveyard(&f.shared);
        assert!(f.shared.lock().unwrap().graveyard.is_empty());
    }

    #[test]
    fn release_clears_busy_with_seq_guard() {
        let f = fixture();
        let tag = SlotTag {
            output: f.key,
            slot: 0,
            seq: 41,
        };
        let (pool, qh) = {
            let guard = f.shared.lock().unwrap();
            let rec = guard.outputs.get(&f.key).unwrap();
            (
                rec.slots[0].as_ref().unwrap().pool.clone().unwrap(),
                guard.qh.clone().unwrap(),
            )
        };
        let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, &qh, tag);
        {
            let mut guard = f.shared.lock().unwrap();
            let slot = guard.outputs.get_mut(&f.key).unwrap().slots[0]
                .as_mut()
                .unwrap();
            slot.buffer = Some(buffer);
            slot.busy = true;
            slot.seq = 41;
        }
        on_buffer_release(&f.shared, &tag);
        {
            let guard = f.shared.lock().unwrap();
            let slot = guard.outputs.get(&f.key).unwrap().slots[0]
                .as_ref()
                .unwrap();
            assert!(!slot.busy);
            assert!(slot.buffer.is_none(), "released buffer retired");
            assert_eq!(guard.graveyard.len(), 1);
            assert_eq!(guard.graveyard[0].buffers.len(), 1);
        }
        // Stale tag (previous generation) is a no-op.
        {
            let mut guard = f.shared.lock().unwrap();
            let slot = guard.outputs.get_mut(&f.key).unwrap().slots[0]
                .as_mut()
                .unwrap();
            slot.busy = true;
            slot.seq = 42;
        }
        on_buffer_release(&f.shared, &tag); // seq 41 != 42
        {
            let guard = f.shared.lock().unwrap();
            let slot = guard.outputs.get(&f.key).unwrap().slots[0]
                .as_ref()
                .unwrap();
            assert!(slot.busy, "stale release must not clear");
            assert_eq!(guard.graveyard.len(), 1, "no new retirement");
        }
        drain_graveyard(&f.shared);
    }

    #[test]
    fn free_slot_preferred_over_busy() {
        let f = fixture();
        let (shm, qh) = {
            let guard = f.shared.lock().unwrap();
            (guard.shm.clone().unwrap(), guard.qh.clone().unwrap())
        };
        let file = create_shm_file(16).unwrap();
        let pool = shm.create_pool(file.as_fd(), 16, &qh, ());
        {
            let mut guard = f.shared.lock().unwrap();
            let rec = guard.outputs.get_mut(&f.key).unwrap();
            rec.slots[0].as_mut().unwrap().busy = true;
            rec.slots[1] = Some(ShmSlot {
                file,
                size: 16,
                pool: Some(pool),
                buffer: None,
                busy: false,
                seq: 0,
            });
        }
        assert_eq!(
            wait_for_free_slot(&f.shared, f.key, 16, Duration::ZERO),
            Some(1)
        );
        // All busy with zero budget: immediate round-robin fallback, no sleep.
        {
            f.shared
                .lock()
                .unwrap()
                .outputs
                .get_mut(&f.key)
                .unwrap()
                .slots[1]
                .as_mut()
                .unwrap()
                .busy = true;
        }
        let fallback = wait_for_free_slot(&f.shared, f.key, 16, Duration::ZERO);
        assert!(fallback == Some(0) || fallback == Some(1));
        drain_graveyard(&f.shared);
    }

    #[test]
    fn reset_live_clears_stale_state() {
        let f = fixture();
        {
            let mut guard = f.shared.lock().unwrap();
            guard
                .pending_by_monitor
                .insert("DP-1".to_owned(), Arc::clone(&f.img));
            reset_live_locked(&mut guard);
            assert!(guard.outputs.is_empty());
            assert!(guard.graveyard.is_empty());
            assert!(guard.compositor.is_none());
            assert!(guard.conn.is_none() && guard.qh.is_none());
            // Sticky config survives for the retry.
            assert!(guard.pending_by_monitor.contains_key("DP-1"));
            assert_eq!(guard.transition_name, "slide");
        }
    }

    #[test]
    fn show_preview_unknown_monitor_returns_false() {
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(!backend.show_preview("NOPE", Arc::clone(&f.img)));
        assert!(!backend.has_preview("NOPE"));
    }

    #[test]
    fn clear_preview_idle_returns_true() {
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(!backend.has_preview("DP-1"));
        assert!(backend.clear_preview("DP-1"));
        assert!(backend.clear_preview("NOPE"));
    }

    #[test]
    fn preview_state_tracks_show_clear() {
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(!backend.has_preview("DP-1"));
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        assert_eq!(f.shared.lock().unwrap().preview_surfaces.len(), 1);
        // Re-preview replaces in place (no second record).
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        assert_eq!(f.shared.lock().unwrap().preview_surfaces.len(), 1);
        assert!(backend.clear_preview("DP-1"));
        assert!(!backend.has_preview("DP-1"));
        // Clearing an idle monitor stays a no-op true.
        assert!(backend.clear_preview("DP-1"));
    }

    #[test]
    fn drop_preview_cuts_instantly() {
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        let fade_ms = make_transition("fade")
            .map(|e| e.duration_ms())
            .unwrap_or(0);
        let t0 = Instant::now();
        assert!(backend.drop_preview("DP-1"));
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(fade_ms.max(1) as u64),
            "drop_preview must cut without the {fade_ms}ms fade (took {elapsed:?})"
        );
        assert!(!backend.has_preview("DP-1"));
        let guard = f.shared.lock().unwrap();
        let prec = guard.preview_surfaces.get(&f.key).unwrap();
        assert!(!prec.mapped);
        assert_eq!(prec.alpha, 0.0);
        drop(guard);
        // Unknown monitor stays a no-op true.
        assert!(backend.drop_preview("NOPE"));
    }

    #[test]
    fn preview_instant_show_stays_pending_until_attach() {
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        f.shared.lock().unwrap().transition_name = "none".to_owned();
        // Take the surface (layer kept, so no rebuild): the instant attach
        // cannot proceed and the record must stay pending, not shown.
        let surface = f
            .shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get_mut(&f.key)
            .unwrap()
            .surface
            .take();
        assert!(surface.is_some());
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(
            !backend.has_preview("DP-1"),
            "no pixels attached: accepted/pending, not presenting"
        );
        assert!(
            f.shared
                .lock()
                .unwrap()
                .preview_surfaces
                .get(&f.key)
                .unwrap()
                .pending
                .is_some(),
            "pending image survives for the configure handler"
        );
        // Surface back (configure ack): re-show presents.
        f.shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get_mut(&f.key)
            .unwrap()
            .surface = surface;
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        assert!(backend.clear_preview("DP-1"));
        assert!(!backend.has_preview("DP-1"));
    }

    /// Mark the fixture's preview record unconfigured, returning its key.
    /// The next `show_preview` then pends (accepted, nothing presented)
    /// exactly like a show racing surface creation on a live compositor.
    fn pend_preview_surface(shared: &Arc<Mutex<Shared>>, key: u32) {
        shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get_mut(&key)
            .unwrap()
            .configured = false;
    }

    /// Drive the compositor configure the pending show is waiting for:
    /// mark configured and run the configure handler's attach path.
    fn drive_preview_configure(shared: &Arc<Mutex<Shared>>, key: u32) {
        shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get_mut(&key)
            .unwrap()
            .configured = true;
        attach_preview_locked(shared, key);
    }

    fn prec_mapped(shared: &Arc<Mutex<Shared>>, key: u32) -> bool {
        shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get(&key)
            .unwrap()
            .mapped
    }

    fn prec_pending_is_some(shared: &Arc<Mutex<Shared>>, key: u32) -> bool {
        shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get(&key)
            .unwrap()
            .pending
            .is_some()
    }

    #[test]
    fn pending_show_then_cut_stays_cleared_after_configure() {
        // Control arm: pending show with NO cut resurrects on configure
        // (proves the fixture can actually resurrect — `mapped` flips).
        let c = preview_fixture();
        let c_backend = test_backend(&c.shared);
        pend_preview_surface(&c.shared, c.key);
        assert!(c_backend.show_preview("DP-1", Arc::clone(&c.img)));
        assert!(
            !c_backend.has_preview("DP-1"),
            "pending show must not present yet"
        );
        drive_preview_configure(&c.shared, c.key);
        assert!(
            prec_mapped(&c.shared, c.key),
            "control: configure attaches the pending show"
        );
        // Fixed arm: the cut path (`drop_preview`, unconditional) clears
        // `pending` + bumps generation, so the same configure resurrects
        // nothing.
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        pend_preview_surface(&f.shared, f.key);
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(!backend.has_preview("DP-1"));
        assert!(
            prec_pending_is_some(&f.shared, f.key),
            "show pends for configure"
        );
        assert!(backend.drop_preview("DP-1"));
        assert!(
            !prec_pending_is_some(&f.shared, f.key),
            "cut must drop the pending image even though mapped == false"
        );
        drive_preview_configure(&f.shared, f.key);
        assert!(!backend.has_preview("DP-1"));
        assert!(
            !prec_mapped(&f.shared, f.key),
            "cut preview must not resurrect on configure"
        );
        // Same invariant through the fading dismiss path: `clear_preview`
        // on the unmapped record still drops `pending`.
        let g = preview_fixture();
        let g_backend = test_backend(&g.shared);
        pend_preview_surface(&g.shared, g.key);
        assert!(g_backend.show_preview("DP-1", Arc::clone(&g.img)));
        assert!(g_backend.clear_preview("DP-1"));
        assert!(
            !prec_pending_is_some(&g.shared, g.key),
            "clear must drop pending even though mapped == false"
        );
        drive_preview_configure(&g.shared, g.key);
        assert!(!g_backend.has_preview("DP-1"));
        assert!(
            !prec_mapped(&g.shared, g.key),
            "cleared preview must not resurrect on configure"
        );
    }

    #[test]
    fn superseded_pin_does_not_rearm_preview() {
        // Fault injection for the animated show's final pin: the loop is
        // synchronous, so a clear cannot be interleaved mid-call without
        // restructuring it (expressly out of scope). Instead, present via
        // the real path, land a real superseding clear (generation bump +
        // unmap — exactly what a clear between the last frame and the pin
        // does to the record), then assert the pin's currency guard leaves
        // the dismissed preview down.
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        let stale_gen = f
            .shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get(&f.key)
            .unwrap()
            .generation;
        // The superseding clear.
        assert!(backend.drop_preview("DP-1"));
        assert!(!backend.has_preview("DP-1"));
        // The stale pin's guard (`is_current`, i.e. generation equality)
        // must now read stale, so its `mapped = true, alpha = 1.0` store
        // must not run. Mirror the pin's guard exactly against the live
        // record: the store fires only when current.
        {
            let mut guard = f.shared.lock().unwrap();
            if let Some(prec) = guard.preview_surfaces.get_mut(&f.key) {
                let current = prec.generation == stale_gen;
                assert!(!current, "clear must disarm the in-flight show's pin");
                if current {
                    prec.mapped = true;
                    prec.alpha = 1.0;
                }
            }
        }
        assert!(
            !backend.has_preview("DP-1"),
            "stale pin must not re-arm a dismissed preview"
        );
        assert!(
            !prec_mapped(&f.shared, f.key),
            "record stays unmapped after the stale pin"
        );
    }

    #[test]
    fn show_on_unconfigured_record_attaches_nothing_until_configure() {
        // Attach-before-configure gate: on a fresh (never-configured)
        // record every show path must pend with ZERO buffer attaches until
        // the configure handler runs. Observed via the attach counter
        // (`buf_seq` bumps only on the write/create/attach path) and slot
        // state (no pools/buffers may exist before the first legal attach).
        for transition in ["slide", "none"] {
            let f = preview_fixture();
            let backend = test_backend(&f.shared);
            f.shared.lock().unwrap().transition_name = transition.to_owned();
            pend_preview_surface(&f.shared, f.key); // configured = false
            assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
            assert!(
                !backend.has_preview("DP-1"),
                "nothing may present before configure ({transition})"
            );
            {
                let guard = f.shared.lock().unwrap();
                let prec = guard.preview_surfaces.get(&f.key).unwrap();
                assert!(!prec.mapped, "unconfigured show stays unmapped");
                assert_eq!(
                    prec.buf_seq, 0,
                    "zero attaches issued before configure ({transition})"
                );
                assert!(
                    prec.slots.iter().all(|s| s.is_none()),
                    "no SHM slots may exist before configure ({transition})"
                );
                assert!(
                    prec.pending.is_some(),
                    "image pends for the configure handler"
                );
            }
            drive_preview_configure(&f.shared, f.key);
            assert!(
                backend.has_preview("DP-1"),
                "pending show presents once configured ({transition})"
            );
            assert!(backend.clear_preview("DP-1"));
            assert!(!backend.has_preview("DP-1"));
        }
    }

    #[test]
    fn reshow_after_unmap_waits_for_a_fresh_configure() {
        // Root cause of the dead-connection bug (Wayland trace,
        // 2026-10-14): a NULL commit by clear/drop unmaps the layer
        // surface, which makes it unconfigured again — but the record
        // kept `configured = true`, so the next show attached a buffer
        // straight away and the compositor raised
        //   "layerSurface was not configured, but a buffer was attached"
        // and killed the whole wl_display connection. Everything then
        // looked alive (IPC still replied OK) while nothing was painted:
        // black background and no further previews.
        // After an unmap the record must read unconfigured, and the next
        // show must pend with zero attaches until a fresh configure.
        for transition in ["slide", "none"] {
            let f = preview_fixture();
            let backend = test_backend(&f.shared);
            f.shared.lock().unwrap().transition_name = transition.to_owned();

            assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
            assert!(backend.has_preview("DP-1"));

            // A dismiss nil-attaches the surface: the overlay is unmapped.
            assert!(backend.drop_preview("DP-1"));
            assert!(!prec_mapped(&f.shared, f.key));

            let attaches_at_unmap = {
                let guard = f.shared.lock().unwrap();
                let prec = guard.preview_surfaces.get(&f.key).unwrap();
                assert!(
                    !prec.configured,
                    "an unmapped layer surface is unconfigured again ({transition})"
                );
                prec.buf_seq
            };

            // Second show must not attach anything before the reconfigure.
            assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
            assert!(
                !backend.has_preview("DP-1"),
                "post-unmap show must pend, never present early ({transition})"
            );
            {
                let guard = f.shared.lock().unwrap();
                let prec = guard.preview_surfaces.get(&f.key).unwrap();
                assert_eq!(
                    prec.buf_seq, attaches_at_unmap,
                    "no buffer may attach between unmap and reconfigure ({transition})"
                );
                assert!(
                    prec.pending.is_some(),
                    "image pends for the configure handler ({transition})"
                );
            }

            // The fresh configure the empty commit asked for arrives.
            drive_preview_configure(&f.shared, f.key);
            assert!(
                backend.has_preview("DP-1"),
                "post-unmap show presents once reconfigured ({transition})"
            );
        }
    }

    #[test]
    fn aborted_show_then_clear_still_dismisses() {
        // A show orphaned mid-fade by a racing clear's generation bump must
        // not strand pixels: the following clear still runs to unmap (not
        // an early-true without unmap).
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        {
            let mut guard = f.shared.lock().unwrap();
            let prec = guard.preview_surfaces.get_mut(&f.key).unwrap();
            prec.generation = prec.generation.wrapping_add(1);
        }
        assert!(backend.clear_preview("DP-1"));
        assert!(!backend.has_preview("DP-1"));
        let guard = f.shared.lock().unwrap();
        let prec = guard.preview_surfaces.get(&f.key).unwrap();
        assert!(!prec.mapped);
        assert!(prec.last_frame.is_empty());
        assert!(prec.pending.is_none());
    }

    #[test]
    fn clear_and_drop_on_desynced_record_still_unmap() {
        // Flag/state desync guard: pixels attached (live frame cache +
        // opacity) but `mapped` reading false. Neither dismiss path may
        // early-true past a possibly-presented record — both must unmap.
        // (Fault injection mirrors `superseded_pin_does_not_rearm_preview`.)
        for dismiss in ["clear", "drop"] {
            let f = preview_fixture();
            let backend = test_backend(&f.shared);
            assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
            assert!(backend.has_preview("DP-1"));
            {
                let mut guard = f.shared.lock().unwrap();
                guard.preview_surfaces.get_mut(&f.key).unwrap().mapped = false;
            }
            let ok = if dismiss == "clear" {
                backend.clear_preview("DP-1")
            } else {
                backend.drop_preview("DP-1")
            };
            assert!(ok, "{dismiss} stays idempotent-true");
            assert!(!backend.has_preview("DP-1"));
            let guard = f.shared.lock().unwrap();
            let prec = guard.preview_surfaces.get(&f.key).unwrap();
            assert!(!prec.mapped, "{dismiss}: stays unmapped");
            assert_eq!(prec.alpha, 0.0, "{dismiss}: opacity reset");
            assert!(
                prec.last_frame.is_empty(),
                "{dismiss}: must unmap past a possibly-presented record, not early-true"
            );
            assert!(prec.pending.is_none(), "{dismiss}: pending dropped");
        }
    }

    #[test]
    fn stale_attach_cannot_rearm_after_unmap() {
        // Root-cause guard for the stuck overlay: an aborted show's frame
        // can reach the attach after a clear already unmapped the record.
        // A generation-guarded attach must refuse it outright — no buffer
        // commit, no `mapped` pin — so pixels cannot reappear over a
        // dismissal and leave `mapped` lying about what is on screen.
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        let stale_gen = f
            .shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get(&f.key)
            .unwrap()
            .generation;
        // A clear supersedes (generation bump) and unmaps.
        assert!(backend.drop_preview("DP-1"));
        assert!(!prec_mapped(&f.shared, f.key));
        let argb = vec![0u8; 4 * 4 * 4];
        assert!(
            !attach_preview_pixels_locked(&f.shared, f.key, &argb, 4, 4, stale_gen, true),
            "stale-generation attach must be refused"
        );
        assert!(
            !prec_mapped(&f.shared, f.key),
            "stale attach must not re-arm the dismissed record"
        );
        let guard = f.shared.lock().unwrap();
        let prec = guard.preview_surfaces.get(&f.key).unwrap();
        assert!(
            prec.last_frame.is_empty(),
            "stale attach must not store a frame"
        );
        drop(guard);
        // Control: a current-generation attach on the same fixture shape
        // does land, so the assertion above is sensitive.
        let c = preview_fixture();
        let c_gen = c
            .shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get(&c.key)
            .unwrap()
            .generation;
        assert!(
            attach_preview_pixels_locked(&c.shared, c.key, &argb, 4, 4, c_gen, true),
            "control: current-generation attach lands"
        );
        assert!(prec_mapped(&c.shared, c.key));
    }

    #[test]
    fn clear_aborting_fade_still_unmaps() {
        // `clear_preview` must never early-return past a possibly-presented
        // record. Simulate the record going unconfigured between the clear
        // snapshot (which snapshots a mapped frame) and its first fade
        // frame: the fade gate aborts, but the generation-guarded unmap
        // must still run instead of returning `true` with pixels intact.
        let f = preview_fixture();
        let backend = test_backend(&f.shared);
        assert!(backend.show_preview("DP-1", Arc::clone(&f.img)));
        assert!(backend.has_preview("DP-1"));
        f.shared
            .lock()
            .unwrap()
            .preview_surfaces
            .get_mut(&f.key)
            .unwrap()
            .configured = false;
        assert!(backend.clear_preview("DP-1"));
        assert!(!backend.has_preview("DP-1"));
        let guard = f.shared.lock().unwrap();
        let prec = guard.preview_surfaces.get(&f.key).unwrap();
        assert!(!prec.mapped, "aborted clear must still unmap");
        assert_eq!(prec.alpha, 0.0, "aborted clear resets opacity");
        assert!(
            prec.last_frame.is_empty(),
            "aborted clear must clear the frame cache"
        );
        assert!(prec.pending.is_none(), "aborted clear drops pending");
    }
}
