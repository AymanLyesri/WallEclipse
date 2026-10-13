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
    guard.vlog("layer surface closed, will repaint on next setWallpaper/configure");
}

/// Handle a `wl_output` global removal (hotplug/sleep). The record is
/// detached from the map immediately (stashing `pending` by monitor name)
/// and its handles move to the graveyard; destruction happens outside
/// the callback via `drain_graveyard_locked`.
fn on_global_remove(shared: &Arc<Mutex<Shared>>, name: u32) {
    let mut guard = shared.lock().unwrap();
    guard.vlog(&format!("output global {name} removed (hotplug/sleep?)"));
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

wayland_client::delegate_noop!(WlState: ignore wl_compositor::WlCompositor);
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
        for i in 1..=frames {
            if !is_current() {
                return true; // superseded (or hidden) mid-animation
            }
            let progress = i as f64 / frames as f64;
            let frame = effect.render(&from, &new_argb, snap.w, snap.h, progress, dir);
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
            attach_pixels_locked(&self.shared, snap.key, &frame, snap.w, snap.h);
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
}
