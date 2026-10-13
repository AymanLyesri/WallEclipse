//! WallEclipse — layer-shell wallpaper daemon with startup preloading.
//! Port of `src/main.cpp`: client subcommands over [`ipc`], `--once`
//! smoke path, and the daemon (preload-all → backend init → precache →
//! current.conf writer → show closures → [`HyprListener`] + detached IPC
//! thread). Exit codes: 0 ok, 1 ERR/decode, 2 usage.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, Weak};

use walleclipse_rs::backend::{LogFn, WaylandBackend};
use walleclipse_rs::config::ConfigStore;
use walleclipse_rs::hypr::{self, HyprListener};
use walleclipse_rs::image::{is_media_wallpaper, load_image, DecodedImage};
use walleclipse_rs::ipc;
use walleclipse_rs::media;
use walleclipse_rs::preloader::Preloader;
use walleclipse_rs::transition::SlideDir;
use walleclipse_rs::util::{exec_capture, expand_home, split_lines, trim_ws};

const LOG_FILE: &str = "/tmp/walleclipse.log";

fn log_line(where_: &str, msg: &str) {
    let line = format!("[{where_}] {msg}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Ok(mut fp) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
    {
        let _ = fp.write_all(line.as_bytes());
    }
}

/// Exact port of C++ `printHelp` (main.cpp:49-59); `{argv0}` stands in
/// for `argv[0]`. Keep byte-identical when editing.
fn help_text(argv0: &str) -> String {
    format!(
        "WallEclipse {} — preloaded layer-shell wallpapers\n\
         \n\
         Usage:\n\
         \x20 {argv0}                  run daemon\n\
         \x20 {argv0} --once MON PATH  render once, exit\n\
         \x20 {argv0} set MON WS PATH  set workspace wallpaper\n\
         \x20 {argv0} preload PATH     preload into daemon cache\n\
         \x20 {argv0} reload           re-read config + re-apply current\n\
         \x20 {argv0} transition [name] query/set workspace transition\n\
         \x20 {argv0} current|list     query daemon\n",
        env!("CARGO_PKG_VERSION"),
    )
}

/// One hyprctl snapshot per call (no N+1), same as wallpaper-loop.c.
fn query_monitors() -> Vec<String> {
    let mut out = Vec::new();
    for l in split_lines(&exec_capture("hyprctl monitors -j | jq -r '.[].name'")) {
        let l = trim_ws(&l);
        if !l.is_empty() {
            out.push(l);
        }
    }
    out
}

fn resolve_image(path: &str) -> Option<DecodedImage> {
    let expanded = expand_home(path);
    if let Some(img) = load_image(if expanded.is_empty() { path } else { &expanded }) {
        return Some(img);
    }
    if !expanded.is_empty() && expanded != path {
        return load_image(path);
    }
    None
}

fn run_client(req: &str) -> i32 {
    let reply = ipc::call(req);
    print!("{reply}");
    let _ = std::io::stdout().flush();
    if reply.starts_with("ERR") { 1 } else { 0 }
}

/// Shared daemon show-path state (C++ lambda captures, made explicit).
struct ShowCtx {
    store: Arc<ConfigStore>,
    preloader: Arc<Preloader>,
    backend: Arc<WaylandBackend>,
    shown: Arc<Mutex<HashMap<String, String>>>,
    current_conf: std::path::PathBuf,
}

fn get_shown(shown: &Arc<Mutex<HashMap<String, String>>>, mon: &str) -> String {
    shown
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(mon)
        .cloned()
        .unwrap_or_default()
}

fn set_shown(shown: &Arc<Mutex<HashMap<String, String>>>, mon: &str, path: &str) {
    shown
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(mon.to_string(), path.to_string());
}

/// Poster with on-demand extraction (first visit pays ~250ms once,
/// then disk + RAM cached). None when extraction fails — callers fall
/// back to the previous instant cut.
fn poster_for(preloader: &Preloader, path: &str) -> Option<Arc<DecodedImage>> {
    if let Some(p) = preloader.get_poster(path) {
        return Some(p);
    }
    preloader.preload_poster_one(path);
    preloader.get_poster(path)
}

/// Hand the monitor to mpvpaper without a blank frame in between:
/// the player spawns async, so our poster stays mapped until its
/// layer surface appears (plus a beat for its first frame) and only
/// then is hidden.
fn handoff_to_video(backend: &WaylandBackend, mon: &str, path: &str) {
    let expanded = expand_home(path);
    media::play_on_monitor(mon, if expanded.is_empty() { path } else { &expanded });
    if !media::wait_for_player(mon, 3000) {
        log_line(
            "change_wallpaper",
            &format!("mpvpaper slow on '{mon}' — hid poster before first frame"),
        );
    }
    backend.hide_monitor(mon);
}

fn track_shown(
    ctx: &ShowCtx,
    listener: Option<&HyprListener>,
    mon: &str,
    path: &str,
) {
    if let Some(l) = listener {
        l.note_shown(mon, path);
    }
    set_shown(&ctx.shown, mon, path);
    let expanded = expand_home(path);
    let content = format!("{}\n", if expanded.is_empty() { path } else { &expanded });
    let _ = std::fs::write(&ctx.current_conf, content);
    media::apply_theme(path);
}

fn show_static_now(
    ctx: &ShowCtx,
    listener: Option<&HyprListener>,
    mon: &str,
    path: &str,
    dir: SlideDir,
) -> bool {
    // Animated slipped into a static slot? Delegate (never render garbage).
    if is_media_wallpaper(path) {
        show_animated_now(ctx, listener, mon, path, dir);
        return true;
    }
    let mut img = ctx.preloader.get(path);
    if img.is_none() {
        // Cache miss (SET during runtime): decode on demand.
        ctx.preloader.preload_one(path);
        img = ctx.preloader.get(path);
    }
    let Some(img) = img else {
        log_line(
            "change_wallpaper",
            &format!("cannot decode '{path}' — keeping previous"),
        );
        return false;
    };
    if !ctx.backend.is_mapped(mon) {
        // Coming from video: restore its poster under mpvpaper first
        // (invisible), so killing the player reveals the poster and
        // the slide starts there instead of cutting from black.
        let prev = get_shown(&ctx.shown, mon);
        if is_media_wallpaper(&prev) {
            if let Some(poster) = poster_for(&ctx.preloader, &prev) {
                ctx.backend.remap_with_image(mon, poster, 1000);
            }
        }
    }
    media::stop_for_monitor(mon); // leaving video -> kill mpvpaper first
    if !ctx.backend.set_wallpaper_dir(mon, img, dir) {
        log_line("change_wallpaper", &format!("unknown monitor '{mon}'"));
    }
    track_shown(ctx, listener, mon, path);
    true
}

fn show_animated_now(
    ctx: &ShowCtx,
    listener: Option<&HyprListener>,
    mon: &str,
    path: &str,
    dir: SlideDir,
) -> bool {
    if ctx.backend.is_mapped(mon) {
        // Static -> animated: slide to the video's poster, then hand off.
        if let Some(poster) = poster_for(&ctx.preloader, path) {
            ctx.backend.set_wallpaper_dir(mon, poster, dir);
        } else {
            // Poster extraction failed: C++ (main.cpp:294-298, `if (auto
            // poster = posterFor(path))`) skips the slide here too and
            // hands off anyway — the hide below blanks until the player's
            // first frame. Behavior kept identical; log so the missing
            // poster surfaces instead of a silent black cut.
            log_line(
                "change_wallpaper",
                &format!("cannot extract poster for '{path}' — handing off without poster frame"),
            );
        }
    } else if {
        let prev = get_shown(&ctx.shown, mon);
        is_media_wallpaper(&prev)
    } {
        // Animated -> animated: reveal old poster, slide poster->poster.
        let prev = get_shown(&ctx.shown, mon);
        let old_poster = poster_for(&ctx.preloader, &prev);
        let new_poster = poster_for(&ctx.preloader, path);
        if let (Some(old), Some(new)) = (old_poster, new_poster) {
            if ctx.backend.remap_with_image(mon, old, 1000) {
                media::stop_for_monitor(mon); // reveal old poster
                ctx.backend.set_wallpaper_dir(mon, new, dir);
            }
        } else {
            // Same C++ parity (main.cpp:299-307: both posters required):
            // missing poster(s) skip the poster->poster slide and the
            // handoff below still hides. Log for observability.
            log_line(
                "change_wallpaper",
                &format!("cannot extract poster for '{path}' — handing off without poster frame"),
            );
        }
    }
    handoff_to_video(&ctx.backend, mon, path);
    track_shown(ctx, listener, mon, path);
    true
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().cloned().unwrap_or_else(|| "walleclipse".to_string());

    // Client subcommands that talk to a running daemon.
    if args.len() >= 2 && args[1] == "set" {
        if args.len() < 5 {
            eprintln!("Usage: walleclipse set <monitor> <workspace> <path>");
            return 2;
        }
        return run_client(&format!("SET {} {} {}", args[2], args[3], args[4]));
    }
    if args.len() >= 2 && args[1] == "preload" {
        if args.len() < 3 {
            eprintln!("Usage: walleclipse preload <path>");
            return 2;
        }
        return run_client(&format!("PRELOAD {}", args[2]));
    }
    if args.len() >= 2 && (args[1] == "current" || args[1] == "list" || args[1] == "reload") {
        return run_client(&args[1]);
    }
    if args.len() >= 2 && args[1] == "transition" {
        let mut req = "TRANSITION".to_string();
        if args.len() >= 3 {
            req += " ";
            req += &args[2];
        }
        return run_client(&req);
    }
    if args.len() >= 2 && (args[1] == "--help" || args[1] == "-h") {
        print!("{}", help_text(&argv0));
        let _ = std::io::stdout().flush();
        return 0;
    }

    let store = Arc::new(ConfigStore::default_location());
    let preloader = Arc::new(Preloader::new(0));

    // Smoke-test path: render one image without the event loop.
    if args.len() >= 2 && args[1] == "--once" {
        if args.len() < 4 {
            eprintln!("Usage: walleclipse --once <monitor> <path>");
            return 2;
        }
        let (mon, path) = (args[2].clone(), args[3].clone());
        let Some(img) = resolve_image(&path) else {
            eprintln!("cannot decode {path}");
            return 1;
        };
        let backend = WaylandBackend::new();
        let log: LogFn = Arc::new(|m: String| log_line("wayland", &m));
        if !backend.init(Some(log)) {
            return 1;
        }
        if !backend.set_wallpaper(&mon, Arc::new(img)) {
            eprintln!("unknown monitor '{mon}'");
            return 1;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        return 0;
    }

    // Unknown subcommand: fail instead of starting a second daemon.
    if args.len() >= 2 && !args[1].starts_with('-') {
        eprintln!("Unknown command '{}'", args[1]);
        print!("{}", help_text(&argv0));
        let _ = std::io::stdout().flush();
        return 2;
    }

    // ---- Daemon ----
    // Unbuffered startup lines: flush each so logs show them live.
    let say = |msg: &str| {
        println!("{msg}");
        let _ = std::io::stdout().flush();
    };
    say("WallEclipse starting...");

    let mut monitors = query_monitors();
    if monitors.is_empty() {
        log_line("main", "hyprctl returned no monitors; continuing with cached config");
    }
    store.ensure_monitors(&monitors);
    // Also seed from any already-known config dirs (hotplug memory).
    for known in store.monitors() {
        if !monitors.contains(&known) {
            monitors.push(known);
        }
    }

    say("Preloading wallpapers...");
    preloader.preload_all(&store);
    say(&format!("Preloaded {} wallpapers.", preloader.size()));

    let backend = Arc::new(WaylandBackend::new());
    let log: LogFn = Arc::new(|m: String| log_line("wayland", &m));
    if !backend.init(Some(log)) {
        log_line("main", "Wayland backend init failed");
        return 1;
    }
    // Workspace-switch transition persists across reload + reboot
    // (missing/invalid file reads back as slide).
    backend.set_transition(&store.get_transition());

    // Warm the cover-fit cache: the ~90ms scale per wallpaper happens here,
    // once, instead of on the first visit to each workspace. Animated
    // posters are precached too so slides to/from video start instantly.
    for mon in &monitors {
        for (_ws, path) in store.wallpapers_for(mon) {
            if is_media_wallpaper(&path) {
                if let Some(poster) = preloader.get_poster(&path) {
                    backend.precache(mon, poster);
                }
                continue;
            }
            if let Some(img) = preloader.get(&path) {
                backend.precache(mon, img);
            }
        }
    }

    let ctx = Arc::new(ShowCtx {
        store: Arc::clone(&store),
        preloader: Arc::clone(&preloader),
        backend: Arc::clone(&backend),
        shown: Arc::new(Mutex::new(HashMap::new())),
        current_conf: store.base_dir().join("current.conf"),
    });

    let listener: Arc<HyprListener> = Arc::new_cyclic(|weak: &Weak<HyprListener>| {
        let lookup_store = Arc::clone(&store);
        let static_ctx = Arc::clone(&ctx);
        let anim_ctx = Arc::clone(&ctx);
        let w_static: Weak<HyprListener> = weak.clone();
        let w_anim: Weak<HyprListener> = weak.clone();
        HyprListener::new(
            Arc::new(hypr::default_list_monitors),
            Arc::new(move |mon: &str, ws: i32| lookup_store.get_wallpaper(mon, ws)),
            Arc::new(
                move |mon: &str, path: &str, dir: SlideDir, _from: i32, _to: i32| {
                    let l = w_static.upgrade();
                    if is_media_wallpaper(path) {
                        show_animated_now(&static_ctx, l.as_deref(), mon, path, dir);
                    } else {
                        show_static_now(&static_ctx, l.as_deref(), mon, path, dir);
                    }
                    // Keep listener state accurate (handlers run inside listener flow).
                },
            ),
            Arc::new(
                move |mon: &str, path: &str, dir: SlideDir, _from: i32, _to: i32| {
                    let l = w_anim.upgrade();
                    show_animated_now(&anim_ctx, l.as_deref(), mon, path, dir);
                },
            ),
        )
        .with_on_info(Arc::new(|w: &str, m: &str| log_line(w, m)))
        .with_on_error(Arc::new(|w: &str, m: &str| log_line(w, m)))
    });

    // IPC thread: SET updates config + shows immediately (compat shim for
    // set-wallpaper.sh callers during migration). Detached like C++.
    {
        let ctx = Arc::clone(&ctx);
        let listener = Arc::clone(&listener);
        std::thread::spawn(move || {
            let preload_ctx = Arc::clone(&ctx);
            let reload_ctx = Arc::clone(&ctx);
            let reload_listener = Arc::clone(&listener);
            let trans_ctx = Arc::clone(&ctx);
            let set_ctx = Arc::clone(&ctx);
            let set_listener = Arc::clone(&listener);
            let list_store = Arc::clone(&ctx.store);
            let current_conf = ctx.current_conf.clone();
            let handlers = ipc::Handlers {
                preload: Some(Arc::new(move |p: &str| {
                    preload_ctx.preloader.preload_one(p);
                })),
                reload: Some(Arc::new(move || {
                    // Re-read config from disk (picks up external edits), preload
                    // any new paths, then re-apply current workspace wallpapers.
                    reload_ctx.preloader.preload_all(&reload_ctx.store);
                    reload_ctx
                        .backend
                        .set_transition(&reload_ctx.store.get_transition());
                    reload_listener.apply_current();
                    log_line("ipc", "reloaded config");
                })),
                transition: Some(Arc::new(move |name: &str| {
                    if name.is_empty() {
                        return trans_ctx.backend.transition();
                    }
                    if !trans_ctx.store.set_transition(name) {
                        return String::new();
                    }
                    // Apply the canonical stored value (trimmed), not the raw arg.
                    trans_ctx
                        .backend
                        .set_transition(&trans_ctx.store.get_transition());
                    trans_ctx.backend.transition()
                })),
                set: Some(Arc::new(move |mon: &str, ws: i32, path: &str| {
                    if !set_ctx.store.set_wallpaper(mon, ws, path) {
                        return false;
                    }
                    // No slide out-of-band; reuse the same show path with None dir.
                    // Warm the cache first so the next switch slides.
                    if is_media_wallpaper(path) {
                        set_ctx.preloader.preload_poster_one(path);
                    } else {
                        set_ctx.preloader.preload_one(path);
                    }
                    // Publish via listener state, then render without duplicating logic.
                    // Note: show_static_now/show_animated_now already track + theme.
                    let ok = if is_media_wallpaper(path) {
                        show_animated_now(&set_ctx, Some(&set_listener), mon, path, SlideDir::None)
                    } else {
                        show_static_now(&set_ctx, Some(&set_listener), mon, path, SlideDir::None)
                    };
                    if ok {
                        set_listener.note_shown(mon, path);
                    }
                    ok
                })),
                current: Some(Arc::new(move || {
                    let s = std::fs::read_to_string(&current_conf).unwrap_or_default();
                    if s.is_empty() { "\n".to_string() } else { s }
                })),
                list: Some(Arc::new(move || {
                    let mut s = String::new();
                    for mon in list_store.monitors() {
                        for (ws, p) in list_store.wallpapers_for(&mon) {
                            s += &format!("{mon} {ws} {p}\n");
                        }
                    }
                    if s.is_empty() { "\n".to_string() } else { s }
                })),
                quit: Some(Arc::new(|| {
                    log_line("ipc", "quit requested");
                })),
            };
            if !ipc::serve(handlers) {
                log_line(
                    "ipc",
                    &format!(
                        "serve failed (socket {}): {}",
                        ipc::socket_path(),
                        std::io::Error::last_os_error()
                    ),
                );
            }
        });
    }

    say("Listening for workspace changes...");
    listener.run(); // blocks until socket EOF
    0
}

fn main() {
    std::process::exit(run());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_mentions_every_subcommand() {
        let h = help_text("walleclipse");
        for want in [
            "run daemon",
            "--once MON PATH",
            "set MON WS PATH",
            "preload PATH",
            "reload",
            "transition [name]",
            "current|list",
        ] {
            assert!(h.contains(want), "missing {want:?} in:\n{h}");
        }
    }

    #[test]
    fn help_uses_argv0() {
        let h = help_text("walleclipse");
        assert!(h.contains("walleclipse set MON WS PATH"));
    }

    #[test]
    fn missing_poster_returns_none_for_handoff_log_path() {
        // Isolated cache dir: poster extraction probes the disk cache and
        // shells out to ffmpegthumbnailer/ffmpeg (both fail fast on
        // missing input). NEVER the live ~/.cache.
        let dir = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-poster-none-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        let pre = Preloader::new(1);
        // Extraction of a nonexistent video fails -> None. show_animated_now
        // keeps the C++ behavior here (main.cpp:292-312: null poster skips
        // the slide, handoff hides anyway) and logs "cannot extract poster".
        assert!(poster_for(&pre, "/nonexistent/no-poster-here.mp4").is_none());
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
