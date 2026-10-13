//! Image decoding and cover-fit scaling (port of `ImageLoader.hpp/cpp`).
//!
//! Decoded pixels are 8-bit RGBA in memory. [`scale_cover_argb`] converts to
//! ARGB8888 byte order (B,G,R,255) to match the C++ SHM layout.

/// Decoded still image: `width*height*4` bytes of RGBA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn lower_ext(path: &str) -> &str {
    match path.rfind('.') {
        // Mirror C++ `lowerExt`: single-char (or empty) extensions rejected.
        Some(dot) if dot + 2 < path.len() => &path[dot + 1..],
        _ => "",
    }
}

fn ext_eq(path: &str, want: &str) -> bool {
    lower_ext(path).eq_ignore_ascii_case(want)
}

/// Animated wallpapers stay on mpvpaper (parity with wallpaper-loop.c).
pub fn is_media_wallpaper(path: &str) -> bool {
    ext_eq(path, "gif") || ext_eq(path, "mp4") || ext_eq(path, "webm")
}

fn valid_size(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= 16384 && h <= 16384
}

/// Longest side kept in the preload cache. Wallpapers bigger than this
/// (e.g. 8K files on a 1080p panel) cost full-res RAM without any visible
/// benefit: cover-fit downscales them for display anyway. 3840 covers 4K
/// outputs; bigger panels re-decode on demand via the cache-miss path.
pub const MAX_CACHED_SIDE: u32 = 3840;

/// Shrink `img` (aspect-preserving, downscale-only) so its longest side
/// fits [`MAX_CACHED_SIDE`]. Uses the `image` crate's thumbnail filter.
fn apply_size_cap(img: image::DynamicImage) -> image::DynamicImage {
    if img.width().max(img.height()) > MAX_CACHED_SIDE {
        img.thumbnail(MAX_CACHED_SIDE, MAX_CACHED_SIDE)
    } else {
        img
    }
}

/// Decode jpg/png/webp into 8-bit RGBA.
/// Returns `None` when the file is missing or undecodable — callers keep
/// the previous wallpaper up and log, never black-screen.
pub fn load_image(path: &str) -> Option<DecodedImage> {
    let data = std::fs::read(path).ok()?;
    if data.is_empty() || data.len() > (128 << 20) {
        return None;
    }
    let img = image::load_from_memory(&data).ok()?;
    let img = apply_size_cap(img).to_rgba8();
    let (width, height) = (img.width(), img.height());
    if !valid_size(width, height) {
        return None;
    }
    Some(DecodedImage { width, height, rgba: img.into_raw() })
}

/// Cover-fit scale of `src` into `dst_w` x `dst_h` (ARGB8888 stored as
/// B,G,R,255 in memory, premultiplied off). Used to fill SHM buffers
/// sized to the output mode. Bilinear sampling mirrors the C++ version.
pub fn scale_cover_argb(src: &DecodedImage, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let n = dst_w as usize * dst_h as usize * 4;
    let mut dst = vec![0u8; n];
    if !valid_size(src.width, src.height) || dst_w == 0 || dst_h == 0 {
        return dst;
    }
    if src.rgba.len() != src.width as usize * src.height as usize * 4 {
        return dst;
    }

    let sw = f64::from(src.width);
    let sh = f64::from(src.height);
    let s = (f64::from(dst_w) / sw).max(f64::from(dst_h) / sh);
    let src_w = f64::from(dst_w) / s;
    let src_h = f64::from(dst_h) / s;
    let ox = (sw - src_w) / 2.0;
    let oy = (sh - src_h) / 2.0;

    let sample = |ix: i64, iy: i64| -> &[u8] {
        let ix = ix.clamp(0, i64::from(src.width) - 1) as usize;
        let iy = iy.clamp(0, i64::from(src.height) - 1) as usize;
        let off = (iy * src.width as usize + ix) * 4;
        &src.rgba[off..off + 4]
    };

    for y in 0..dst_h {
        let sy = oy + (f64::from(y) + 0.5) / s;
        let y0 = sy.floor() as i64;
        let fy = sy - y0 as f64;
        for x in 0..dst_w {
            let sx = ox + (f64::from(x) + 0.5) / s;
            let x0 = sx.floor() as i64;
            let fx = sx - x0 as f64;
            let p00 = sample(x0, y0);
            let p10 = sample(x0 + 1, y0);
            let p01 = sample(x0, y0 + 1);
            let p11 = sample(x0 + 1, y0 + 1);
            let o = &mut dst[((u64::from(y) * u64::from(dst_w) + u64::from(x)) * 4) as usize..];
            let mut rgb = [0u8; 3];
            for (c, slot) in rgb.iter_mut().enumerate() {
                let v = f64::from(p00[c]) * (1.0 - fx) * (1.0 - fy)
                    + f64::from(p10[c]) * fx * (1.0 - fy)
                    + f64::from(p01[c]) * (1.0 - fx) * fy
                    + f64::from(p11[c]) * fx * fy;
                *slot = v.round().clamp(0.0, 255.0) as u8;
            }
            // wl_shm ARGB8888 is little-endian: store B,G,R,A
            // (opaque wallpapers: A=255).
            o[0] = rgb[2];
            o[1] = rgb[1];
            o[2] = rgb[0];
            o[3] = 0xFF;
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;

    #[test]
    fn size_cap_leaves_small_images_untouched() {
        let img = DynamicImage::new_rgba8(800, 600);
        let capped = apply_size_cap(img);
        assert_eq!((capped.width(), capped.height()), (800, 600));
    }

    #[test]
    fn size_cap_limits_long_side_preserving_aspect() {
        let img = DynamicImage::new_rgba8(5000, 2500);
        let capped = apply_size_cap(img);
        assert_eq!((capped.width(), capped.height()), (3840, 1920));
    }
}
