//! Smoke tests for the Wayland layer-shell backend.
//!
//! Gated: must pass with no compositor running. `init` degrades to
//! `false` headless and every monitor operation is a safe no-op.

use std::sync::Arc;

use walleclipse_rs::backend::WaylandBackend;
use walleclipse_rs::image::DecodedImage;
use walleclipse_rs::transition::SlideDir;

fn sample_image() -> Arc<DecodedImage> {
    Arc::new(DecodedImage {
        width: 4,
        height: 2,
        rgba: vec![200u8; 4 * 2 * 4],
    })
}

#[test]
fn unknown_monitor_returns_false_without_compositor() {
    let backend = WaylandBackend::new();
    assert!(!backend.running());
    // Must not panic headless; result depends on compositor presence.
    let _ = backend.init(None);
    // Unknown monitor holds with or without a compositor.
    assert!(!backend.set_wallpaper("NOPE-NO-SUCH-MONITOR", sample_image()));
    assert!(!backend.set_wallpaper_dir("NOPE-NO-SUCH-MONITOR", sample_image(), SlideDir::Forward));
    assert!(!backend.is_mapped("NOPE-NO-SUCH-MONITOR"));
    assert!(!backend.remap_with_image("NOPE-NO-SUCH-MONITOR", sample_image(), 50));
    // No-ops that must not panic.
    backend.precache("NOPE-NO-SUCH-MONITOR", sample_image());
    backend.hide_monitor("NOPE-NO-SUCH-MONITOR");
    assert!(!backend
        .outputs()
        .iter()
        .any(|n| n == "NOPE-NO-SUCH-MONITOR"));
    backend.shutdown();
    assert!(!backend.running());
}

#[test]
fn unknown_transition_name_ignored() {
    let backend = WaylandBackend::new();
    assert_eq!(backend.transition(), "slide");
    backend.set_transition("zoom");
    assert_eq!(backend.transition(), "slide");
    backend.set_transition("fade");
    assert_eq!(backend.transition(), "fade");
    backend.set_transition("");
    assert_eq!(backend.transition(), "fade");
    backend.set_transition("none");
    assert_eq!(backend.transition(), "none");
}
