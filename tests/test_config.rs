use std::fs;
use std::path::PathBuf;
use walleclipse_rs::config::ConfigStore;
use walleclipse_rs::util;

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "walleclipse-rs-{}-{}-{}",
        name,
        std::process::id(),
        // thread id debug differs per test thread, keeping dirs distinct
        format!("{:?}", std::thread::current().id())
            .replace(['(', ')', ' ', ','], "_")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn transition_missing_reads_back_slide() {
    let dir = tmpdir("transition-missing");
    let store = ConfigStore::new(dir.join("config"));
    assert_eq!(store.get_transition(), "slide");
}

#[test]
fn set_wallpaper_roundtrips_with_flock() {
    let dir = tmpdir("roundtrip");
    let store = ConfigStore::new(dir.join("config"));
    assert!(store.set_wallpaper("DP-2", 3, "/a/b.jpg"));
    assert_eq!(
        store.get_wallpaper("DP-2", 3).as_deref(),
        Some("/a/b.jpg")
    );
    // lock file must exist next to the config (flock discipline)
    assert!(dir.join("config/DP-2/defaults.conf.lock").exists());
}

#[test]
fn rejects_unknown_transition() {
    let dir = tmpdir("reject-transition");
    let store = ConfigStore::new(dir.join("config"));
    assert!(!store.set_transition("zoom"));
    assert_eq!(store.get_transition(), "slide");
}

#[test]
fn crlf_tilde_and_spaces_parse_like_cpp() {
    let dir = tmpdir("crlf");
    let mon = dir.join("config/DP-1");
    fs::create_dir_all(&mon).unwrap();
    fs::write(mon.join("defaults.conf"), "w-1=  ~/a.jpg\r\n").unwrap();
    let store = ConfigStore::new(dir.join("config"));
    let raw = store.get_wallpaper("DP-1", 1).expect("w-1 must parse");
    // C++ keeps the spaces raw (only \\r stripped); trim+expand like callers do.
    assert_eq!(raw, "  ~/a.jpg");
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(
        util::expand_home(&util::trim_ws(&raw)),
        format!("{home}/a.jpg")
    );
}
