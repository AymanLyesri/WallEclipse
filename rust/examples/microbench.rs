//! Headless-safe decode + cover-scale microbench driver.
//!
//! Workload (mirrors `ImageLoader::loadImage` + `scaleCoverArgb` hot path):
//! for each fixture file, `N` times: decode to RGBA, cover-scale to
//! 1920x1080 ARGB, fold a checksum. Prints `microbench: ...` line with
//! wall time and checksum. Reads only the fixture dir argv[1]; writes
//! nothing; never touches daemon sockets or live config paths.

use std::time::Instant;
use walleclipse_rs::image::{load_image, scale_cover_argb};

fn main() {
    let dir = std::env::args().nth(1).expect("usage: microbench <fixture-dir> [iters]");
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .expect("fixture dir unreadable")
        .filter_map(|e| e.ok().map(|e| e.path().to_string_lossy().into_owned()))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures in {dir}");
    let t0 = Instant::now();
    let mut acc: u64 = 0;
    let mut decodes: usize = 0;
    for _ in 0..iters {
        for f in &files {
            let img = load_image(f).unwrap_or_else(|| panic!("decode failed: {f}"));
            let out = scale_cover_argb(&img, 1920, 1080);
            for chunk in out.chunks(4) {
                acc = acc.wrapping_add(chunk[0] as u64 + chunk[1] as u64 * 3 + chunk[2] as u64 * 7);
            }
            decodes += 1;
        }
    }
    let dt = t0.elapsed();
    println!(
        "microbench: files={} iters={} decodes={} wall_ms={} checksum={acc}",
        files.len(),
        iters,
        decodes,
        dt.as_millis()
    );
}
