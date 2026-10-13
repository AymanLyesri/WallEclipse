use std::sync::Arc;
use walleclipse_rs::image::{DecodedImage, is_media_wallpaper, load_image, scale_cover_argb};
use walleclipse_rs::transition::{SlideDir, effect_name_for, make_transition, slide_dir_for_workspaces, ScaledCache};

#[test]
fn detects_gif_mp4_webm_as_media() {
    assert!(is_media_wallpaper("a.mp4"));
    assert!(is_media_wallpaper("a.gif"));
    assert!(is_media_wallpaper("a.webm"));
    assert!(is_media_wallpaper("A.MP4"));
    assert!(!is_media_wallpaper("a.png"));
    assert!(!is_media_wallpaper("a.jpg"));
    assert!(!is_media_wallpaper("a.webp"));
}

#[test]
fn slide_dir_forward_backward_none() {
    assert_eq!(slide_dir_for_workspaces(None, 1), SlideDir::None);
    assert_eq!(slide_dir_for_workspaces(Some(1), 1), SlideDir::None);
    assert_eq!(slide_dir_for_workspaces(Some(1), 3), SlideDir::Forward);
    assert_eq!(slide_dir_for_workspaces(Some(3), 1), SlideDir::Backward);
}

#[test]
fn unknown_transition_returns_none() {
    assert!(make_transition("zoom").is_none());
}

#[test]
fn scaled_cache_stores_and_finds() {
    let mut cache = ScaledCache::new();
    let src = Arc::new(DecodedImage { width: 2, height: 2, rgba: vec![10; 2 * 2 * 4] });
    let argb = vec![20; 2 * 2 * 4];
    cache.store(Arc::clone(&src), 2, 2, argb.clone());
    let found = cache.find(&src, 2, 2).expect("same Arc ptr dims must hit");
    assert_eq!(found, &argb);
    // Different Arc with identical contents must miss (identity keying).
    let twin = Arc::new(DecodedImage { width: 2, height: 2, rgba: vec![10; 2 * 2 * 4] });
    assert!(cache.find(&twin, 2, 2).is_none());
    // Wrong dims must miss.
    assert!(cache.find(&src, 4, 4).is_none());
    assert_eq!(cache.size(), 1);
}

#[test]
fn effect_name_for_fade_on_none_dir() {
    assert_eq!(effect_name_for(SlideDir::None, "slide"), "fade");
    assert_eq!(effect_name_for(SlideDir::None, "none"), "none");
    assert_eq!(effect_name_for(SlideDir::Forward, "slide"), "slide");
    assert_eq!(effect_name_for(SlideDir::Backward, "slide"), "slide");
}

#[test]
fn known_transitions_render_endpoints() {
    let w = 4u32;
    let h = 1u32;
    let old = vec![0u8; (w * h * 4) as usize];
    let new = vec![255u8; (w * h * 4) as usize];
    // "none" is an instant cut: always the new frame (mirrors C++).
    let none = make_transition("none").unwrap();
    assert_eq!(none.render(&old, &new, w, h, 0.0, SlideDir::Forward), new);
    assert_eq!(none.render(&old, &new, w, h, 1.0, SlideDir::Forward), new);
    for name in ["slide", "fade"] {
        let t = make_transition(name).unwrap_or_else(|| panic!("{name} must exist"));
        assert_eq!(t.render(&old, &new, w, h, 0.0, SlideDir::Forward), old, "{name} p=0");
        assert_eq!(t.render(&old, &new, w, h, 1.0, SlideDir::Forward), new, "{name} p=1");
    }
    // Slide with None dir is an instant cut.
    let slide = make_transition("slide").unwrap();
    assert_eq!(slide.render(&old, &new, w, h, 0.5, SlideDir::None), new);
    // Easing endpoints exact.
    assert_eq!(slide.eased(0.0), 0.0);
    assert_eq!(slide.eased(1.0), 1.0);
    assert_eq!(make_transition("none").unwrap().duration_ms(), 0);
}

#[test]
fn scale_cover_argb_solid_color() {
    // 2x2 solid red RGBA -> 4x4 cover; output is BGRA with A=255.
    let src = DecodedImage { width: 2, height: 2, rgba: [255, 0, 0, 255].repeat(4) };
    let out = scale_cover_argb(&src, 4, 4);
    assert_eq!(out.len(), 4 * 4 * 4);
    for px in out.chunks_exact(4) {
        assert_eq!(px, &[0, 0, 255, 255]);
    }
}

#[test]
fn scale_cover_argb_wide_to_square_center_crops() {
    // 4x2 src, left half red / right half blue -> 4x4 dst.
    // Correct: s = max(4/4, 4/2) = 2, center 2x2 crop, 2x upscale.
    // Pixel (0,0) samples sx=1.25 (fx=0.25 over red|blue), sy=0.25:
    // R = 255*0.75 = 191.25 -> 191, B = 255*0.25 = 63.75 -> 64.
    // The old buggy scale (s=1, oy=-1 stretch) gave pure red [0,0,255,255].
    let mut rgba = Vec::with_capacity(4 * 2 * 4);
    for _ in 0..2 {
        rgba.extend_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255]);
        rgba.extend_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]);
    }
    let src = DecodedImage { width: 4, height: 2, rgba };
    let out = scale_cover_argb(&src, 4, 4);
    assert_eq!(out.len(), 4 * 4 * 4);
    assert_eq!(&out[0..4], &[64, 0, 191, 255]);
}

#[test]
fn scale_cover_argb_tall_to_square_center_crops() {
    // 2x6 src: rows 0-1 red, rows 2-3 green, rows 4-5 blue -> 6x6 dst.
    // Correct: s = max(6/2, 6/6) = 3, center 2x2 crop (rows 2-3), 3x upscale.
    // Dst rows 0-2 sample purely inside green rows 2-3; rows 3-5 blend
    // toward blue (bilinear seam). Red must appear nowhere (top cropped off).
    let mut rgba = Vec::with_capacity(2 * 6 * 4);
    for row in 0..6 {
        let px = match row {
            0 | 1 => [255, 0, 0, 255],
            2 | 3 => [0, 255, 0, 255],
            _ => [0, 0, 255, 255],
        };
        rgba.extend_from_slice(&px);
        rgba.extend_from_slice(&px);
    }
    let src = DecodedImage { width: 2, height: 6, rgba };
    let out = scale_cover_argb(&src, 6, 6);
    assert_eq!(out.len(), 6 * 6 * 4);
    for y in 0..6 {
        for x in 0..6 {
            let px = &out[(y * 6 + x) * 4..][..4];
            assert_eq!(px[2], 0, "no red at ({x},{y}): {px:?}");
            if y < 3 {
                assert_eq!(px, &[0, 255, 0, 255], "pure green at ({x},{y})");
            } else {
                assert!(px[0] > 0, "blue blend at ({x},{y}): {px:?}");
            }
        }
    }
}
#[test]
fn loads_real_png_jpg_webp() {
    let dir = std::env::temp_dir().join(format!("walleclipse-task2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 3x2 gradient image.
    let img = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([(x * 80) as u8, (y * 100) as u8, 128]));
    let png = dir.join("t.png");
    let jpg = dir.join("t.jpg");
    let webp = dir.join("t.webp");
    img.save(&png).unwrap();
    img.save(&jpg).unwrap();
    img.save(&webp).unwrap();
    for p in [&png, &jpg, &webp] {
        let d = load_image(p.to_str().unwrap())
            .unwrap_or_else(|| panic!("must decode {}", p.display()));
        assert_eq!((d.width, d.height), (3, 2));
        assert_eq!(d.rgba.len(), 3 * 2 * 4);
    }
}

#[test]
fn load_missing_returns_none() {
    assert!(load_image("/tmp/walleclipse-task2/does-not-exist.png").is_none());
}
