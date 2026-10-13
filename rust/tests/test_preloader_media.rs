use std::path::PathBuf;
use walleclipse_rs::media;
use walleclipse_rs::poster;
use walleclipse_rs::preloader::Preloader;

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "walleclipse-rs-{}-{}-{:?}",
        name,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_test_png(path: &std::path::Path) {
    let img = image::RgbImage::new(8, 6);
    img.save(path).unwrap();
}

#[test]
fn preload_one_caches_and_get_hits() {
    let dir = tmpdir("preload-hit");
    let png = dir.join("wall.png");
    write_test_png(&png);
    let path = png.to_string_lossy().into_owned();

    let pre = Preloader::new(2);
    assert!(pre.get(&path).is_none());
    assert_eq!(pre.size(), 0);
    pre.preload_one(&path);
    assert!(pre.get(&path).is_some());
    let img = pre.get(&path).unwrap();
    assert_eq!((img.width, img.height), (8, 6));
    assert_eq!(pre.size(), 1);
    // Second preload is a cached no-op.
    pre.preload_one(&path);
    assert_eq!(pre.size(), 1);
    // Misses and invalid inputs never insert or panic.
    assert!(pre.get("/nonexistent/missing.png").is_none());
    pre.preload_one("");
    pre.preload_one("/nonexistent/missing.png");
    pre.preload_one("/tmp/fake-video.mp4");
    assert_eq!(pre.size(), 1);
}

#[test]
fn cache_path_for_is_deterministic() {
    let dir = tmpdir("poster-dir");
    let dir_s = dir.to_string_lossy().into_owned();
    let a = poster::cache_path_for("/videos/a.mp4", &dir_s);
    let b = poster::cache_path_for("/videos/a.mp4", &dir_s);
    let c = poster::cache_path_for("/videos/b.mp4", &dir_s);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert!(a.starts_with(&format!("{dir_s}/")));
    assert!(a.ends_with(".jpg"));
}

#[test]
fn foreign_surface_listed_detects_mpvpaper() {
    assert!(!media::foreign_surface_listed(""));
    assert!(!media::foreign_surface_listed("walleclipse\n"));
    assert!(!media::foreign_surface_listed("walleclipse\nwalleclipse\n"));
    assert!(media::foreign_surface_listed("mpvpaper\n"));
    assert!(media::foreign_surface_listed("walleclipse\nmpvpaper\n"));
    assert!(!media::foreign_surface_listed("  \n"));
}

#[test]
fn poster_failure_returns_none_without_panic() {
    assert!(poster::load("").is_none());
    assert!(poster::load("/tmp/not-a-video.png").is_none());
    // Bogus media path: extraction fails -> None, no panic.
    let bogus = tmpdir("poster-fail").join("missing-video.mp4");
    assert!(poster::load(&bogus.to_string_lossy()).is_none());
}
