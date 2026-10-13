//! Wallpaper transition effects (port of `Transition.hpp/cpp`).
//!
//! Sliding direction derives from workspace order: higher workspace id =
//! Forward (new wallpaper enters from the right), lower = Backward.

use std::sync::Arc;

use crate::image::DecodedImage;

/// Slide direction for a workspace switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideDir {
    Forward,
    Backward,
    None,
}

/// Direction for a workspace switch. `None` `from` (first show) or equal
/// ids -> `None` (instant, no slide).
pub fn slide_dir_for_workspaces(from: Option<i32>, to: i32) -> SlideDir {
    match from {
        None => SlideDir::None,
        Some(f) if f == to => SlideDir::None,
        Some(f) if to > f => SlideDir::Forward,
        Some(_) => SlideDir::Backward,
    }
}

/// Which transition effect to run. Directional switches use the active
/// transition ("slide"); set/reload/init arrive with `None` and cross-fade
/// unless transitions are disabled ("none").
pub fn effect_name_for(dir: SlideDir, active: &str) -> String {
    if dir == SlideDir::None && active != "none" {
        return "fade".to_owned();
    }
    active.to_owned()
}

/// Mirrors Hyprland workspaces slide duration (~300-400ms feel).
pub const SLIDE_DURATION_MS: i32 = 350;

fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// A wallpaper transition effect. Buffers are ARGB8888
/// (`w*h*4`, same layout as `scale_cover_argb` output).
pub trait TransitionEffect {
    /// Total animation time in ms.
    fn duration_ms(&self) -> i32;
    /// Eased progress for linear `t` in [0,1].
    fn eased(&self, t: f64) -> f64;
    /// Composite one frame into `out` (`w*h*4` bytes, reused across
    /// frames); `progress` is linear [0,1], eased internally.
    #[allow(clippy::too_many_arguments)]
    fn render_into(
        &self,
        old_frame: &[u8],
        new_frame: &[u8],
        out: &mut [u8],
        w: u32,
        h: u32,
        progress: f64,
        dir: SlideDir,
    );
    /// Composite one frame; convenience wrapper allocating the buffer.
    /// The animation loop uses [`TransitionEffect::render_into`] with
    /// reused buffers instead (no per-frame allocation churn).
    fn render(
        &self,
        old_frame: &[u8],
        new_frame: &[u8],
        w: u32,
        h: u32,
        progress: f64,
        dir: SlideDir,
    ) -> Vec<u8> {
        let mut out = vec![0u8; w as usize * h as usize * 4];
        self.render_into(old_frame, new_frame, &mut out, w, h, progress, dir);
        out
    }
}

struct SlidingTransition;
struct NoneTransition;
struct FadeTransition;

impl TransitionEffect for SlidingTransition {
    fn duration_ms(&self) -> i32 {
        SLIDE_DURATION_MS
    }
    fn eased(&self, t: f64) -> f64 {
        ease_out_cubic(t)
    }
    fn render_into(
        &self,
        old_frame: &[u8],
        new_frame: &[u8],
        out: &mut [u8],
        w: u32,
        h: u32,
        progress: f64,
        dir: SlideDir,
    ) {
        if dir == SlideDir::None || progress >= 1.0 {
            out.copy_from_slice(new_frame);
            return;
        }
        if progress <= 0.0 {
            out.copy_from_slice(old_frame);
            return;
        }
        let split = (self.eased(progress) * f64::from(w)) as u32;
        if split == 0 {
            out.copy_from_slice(old_frame);
            return;
        }
        if split >= w
            || old_frame.len() != new_frame.len()
            || old_frame.len() != w as usize * h as usize * 4
        {
            out.copy_from_slice(new_frame);
            return;
        }
        // Row-based composite: one contiguous old run + one contiguous
        // new run per row.
        let row_bytes = w as usize * 4;
        let new_bytes = split as usize * 4;
        let old_bytes = row_bytes - new_bytes;
        for y in 0..h as usize {
            let o_row = &old_frame[y * row_bytes..(y + 1) * row_bytes];
            let n_row = &new_frame[y * row_bytes..(y + 1) * row_bytes];
            let d_row = &mut out[y * row_bytes..(y + 1) * row_bytes];
            if dir == SlideDir::Backward {
                // New enters from the left.
                d_row[..new_bytes].copy_from_slice(&n_row[..new_bytes]);
                d_row[new_bytes..].copy_from_slice(&o_row[new_bytes..]);
            } else {
                // New enters from the right.
                d_row[..old_bytes].copy_from_slice(&o_row[..old_bytes]);
                d_row[old_bytes..].copy_from_slice(&n_row[old_bytes..]);
            }
        }
    }
}

impl TransitionEffect for NoneTransition {
    fn duration_ms(&self) -> i32 {
        0
    }
    fn eased(&self, t: f64) -> f64 {
        t.clamp(0.0, 1.0)
    }
    fn render_into(
        &self,
        _old_frame: &[u8],
        new_frame: &[u8],
        out: &mut [u8],
        _w: u32,
        _h: u32,
        _progress: f64,
        _dir: SlideDir,
    ) {
        out.copy_from_slice(new_frame);
    }
}

impl TransitionEffect for FadeTransition {
    fn duration_ms(&self) -> i32 {
        SLIDE_DURATION_MS
    }
    fn eased(&self, t: f64) -> f64 {
        ease_out_cubic(t)
    }
    fn render_into(
        &self,
        old_frame: &[u8],
        new_frame: &[u8],
        out: &mut [u8],
        w: u32,
        h: u32,
        progress: f64,
        _dir: SlideDir,
    ) {
        if progress <= 0.0 {
            out.copy_from_slice(old_frame);
            return;
        }
        if progress >= 1.0 {
            out.copy_from_slice(new_frame);
            return;
        }
        if old_frame.len() != new_frame.len()
            || old_frame.len() != w as usize * h as usize * 4
        {
            out.copy_from_slice(new_frame);
            return;
        }
        let a = self.eased(progress);
        // Integer blend: one float->int per frame, then pure integer
        // math per byte.
        let wa = (a * 256.0 + 0.5) as i32;
        let wo = 256 - wa;
        for (d, (&o, &n)) in out
            .iter_mut()
            .zip(old_frame.iter().zip(new_frame.iter()))
        {
            *d = ((i32::from(o) * wo + i32::from(n) * wa + 128) >> 8) as u8;
        }
    }
}

/// Factory: "slide" (directional), "fade" (cross-blend), "none" (cut).
/// Returns `None` for unknown names.
pub fn make_transition(name: &str) -> Option<Box<dyn TransitionEffect>> {
    match name {
        "slide" => Some(Box::new(SlidingTransition)),
        "fade" => Some(Box::new(FadeTransition)),
        "none" => Some(Box::new(NoneTransition)),
        _ => None,
    }
}

/// Cover-fit result cache: scaling costs ~90ms at 1080p, so scale once per
/// (source, output size) and reuse on every switch. Keyed by `Arc`
/// identity (never dereferenced for the key: no lifetime risk).
/// Not thread-safe; the owner holds its own lock. Caps at [`MAX`]
/// entries, then drops everything.
pub struct ScaledCache {
    entries: Vec<CacheEntry>,
}

struct CacheEntry {
    src: Arc<DecodedImage>,
    w: u32,
    h: u32,
    argb: Vec<u8>,
}

impl ScaledCache {
    /// Cap; configs have ~10 slots so this is never hit in practice —
    /// it only bounds pathological IPC `set` spam.
    pub const MAX: usize = 12;

    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn find(&self, src: &Arc<DecodedImage>, w: u32, h: u32) -> Option<&Vec<u8>> {
        self.entries
            .iter()
            .find(|e| Arc::ptr_eq(&e.src, src) && e.w == w && e.h == h)
            .map(|e| &e.argb)
    }

    pub fn store(&mut self, src: Arc<DecodedImage>, w: u32, h: u32, argb: Vec<u8>) {
        if self.entries.len() >= Self::MAX {
            self.entries.clear();
        }
        self.entries.push(CacheEntry { src, w, h, argb });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ScaledCache {
    fn default() -> Self {
        Self::new()
    }
}
