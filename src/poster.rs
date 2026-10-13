//! Poster bridge for animated wallpapers (port of `VideoPoster.hpp/cpp`).
//!
//! mpvpaper owns the video surface, so transitions slide to/from a static
//! poster frame instead of blending video frames. Posters live in
//! `<cache>/walleclipse/posters/<hash>.jpg`, extracted once with
//! `ffmpegthumbnailer` (`ffmpeg` fallback), then decoded with
//! [`crate::image::load_image`]. Failures return [`None`] — callers fall
//! back to the previous instant cut.

use crate::image::{is_media_wallpaper, load_image, DecodedImage};
use crate::util::{cache_base_dir, run_wait};

/// Directory for cached posters (`$XDG_CACHE_HOME` or `~/.cache` based).
pub fn cache_dir() -> String {
    cache_base_dir() + "/walleclipse/posters"
}

/// FNV-1a 64-bit: deterministic across runs (unlike `DefaultHasher`,
/// whose SipHash keys are per-process random). The C++ side used
/// `std::hash<std::string>` (implementation-defined), so any stable hex
/// hash satisfies the "same video -> same path" contract.
fn deterministic_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Deterministic cache file for a video path under `dir`.
pub fn cache_path_for(video_path: &str, dir: &str) -> String {
    format!("{dir}/{:016x}.jpg", deterministic_hash(video_path))
}

/// Load (or extract + load) the poster for a video path.
/// Returns [`None`] when the path is not media or extraction fails.
pub fn load(video_path: &str) -> Option<DecodedImage> {
    if video_path.is_empty() || !is_media_wallpaper(video_path) {
        return None;
    }
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let out = cache_path_for(video_path, &dir);

    if let Some(cached) = load_image(&out) {
        return Some(cached);
    }

    // Extract to a temp file, then atomically rename (parallel preload
    // workers may race on the same video path).
    let tmp = format!("{out}.tmp.{}", std::process::id());
    let thumb_ok = run_wait(&[
        "ffmpegthumbnailer",
        "-i",
        video_path,
        "-o",
        &tmp,
        "-s",
        "0",
        "-q",
        "5",
    ]);
    let ffmpeg_ok = if thumb_ok {
        true
    } else {
        run_wait(&[
            "ffmpeg",
            "-y",
            "-loglevel",
            "error",
            "-i",
            video_path,
            "-vframes",
            "1",
            "-q:v",
            "3",
            &tmp,
        ])
    };
    if !ffmpeg_ok {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    let _ = std::fs::rename(&tmp, &out);
    load_image(&out)
}
