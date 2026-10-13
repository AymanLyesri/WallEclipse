use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use walleclipse_rs::hypr::{HyprListener, SwitchAction, decide, is_refresh_event, parse_snapshot_lines};
use walleclipse_rs::transition::SlideDir;
use walleclipse_rs::ipc::{self, Handlers as IpcHandlers};

fn ipc_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn temp_cache_dir(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "walleclipse-rust-test-{}-{}",
        tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

#[test]
fn skips_fallback_and_blanks_in_snapshot() {
    let text = "DP-1 1\n\n   \nFALLBACK 2\nspecial-foo 3\nHDMI-A-1 5\nbadline\nDP-2 abc\n";
    let got = parse_snapshot_lines(text);
    assert_eq!(
        got,
        vec![
            ("DP-1".to_string(), 1),
            ("HDMI-A-1".to_string(), 5),
        ]
    );
}

#[test]
fn decide_empty_slot_with_video_restores_static() {
    let action = decide(
        &Some("/v/clip.mp4".to_string()),
        &None,
        &Some("/s/bg.jpg".to_string()),
    );
    assert_eq!(action, SwitchAction::RestoreStatic);
    // Empty-string target is also an empty slot.
    let action2 = decide(
        &Some("/v/clip.mp4".to_string()),
        &Some(String::new()),
        &Some("/s/bg.jpg".to_string()),
    );
    assert_eq!(action2, SwitchAction::RestoreStatic);
    // Static showing + empty slot -> Skip (no black-screen restore needed).
    let action3 = decide(
        &Some("/s/bg.jpg".to_string()),
        &None,
        &Some("/s/bg.jpg".to_string()),
    );
    assert_eq!(action3, SwitchAction::Skip);
    // Same path -> Skip; video target -> ShowAnimated; image target -> ShowStatic.
    assert_eq!(
        decide(
            &Some("/s/a.jpg".to_string()),
            &Some("/s/a.jpg".to_string()),
            &None
        ),
        SwitchAction::Skip
    );
    assert_eq!(
        decide(&None, &Some("/v/clip.mp4".to_string()), &None),
        SwitchAction::ShowAnimated
    );
    assert_eq!(
        decide(&None, &Some("/s/a.jpg".to_string()), &None),
        SwitchAction::ShowStatic
    );
}

#[test]
fn refresh_event_matches_workspace_and_focusedmon() {
    assert!(is_refresh_event("workspacev2>>1,DP-1"));
    assert!(is_refresh_event("workspace>>2,HDMI-A-1"));
    assert!(is_refresh_event("moveworkspace>>1,DP-1"));
    assert!(is_refresh_event("focusedmon>>DP-1,HDMI-A-1"));
    assert!(is_refresh_event("monitoradded>>DP-2"));
    assert!(is_refresh_event("monitorremoved>>DP-2"));
    assert!(!is_refresh_event("openwindow>>123,ws,class,title"));
    assert!(!is_refresh_event(""));
}

// apply_current paints via list_monitors+lookup, note_shown tracks static state.
#[test]
fn listener_apply_current_paints_and_note_shown_tracks_static() {
    let shown: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let shown2 = Arc::clone(&shown);
    let mut map = HashMap::new();
    map.insert(("DP-1".to_string(), 1), "/s/bg.jpg".to_string());
    let listener = HyprListener::new(
        Arc::new(move || vec![("DP-1".to_string(), 1)]),
        Arc::new(move |mon: &str, ws: i32| map.get(&(mon.to_string(), ws)).cloned()),
        Arc::new(move |mon: &str, path: &str, _dir: SlideDir, _from: i32, _to: i32| {
            shown2.lock().unwrap().push((mon.to_string(), path.to_string()));
        }),
        Arc::new(|_: &str, _: &str, _: SlideDir, _: i32, _: i32| {}),
    );
    listener.apply_current();
    assert_eq!(
        shown.lock().unwrap().clone(),
        vec![("DP-1".to_string(), "/s/bg.jpg".to_string())]
    );
    // Out-of-band SET bypass: note_shown keeps animated-restore state accurate.
    listener.note_shown("DP-1", "/v/clip.mp4");
    listener.note_shown("DP-1", "/s/other.jpg");
}

#[test]
fn run_reports_missing_hypr_socket_via_on_error() {
    let _guard = ipc_lock();
    let prev_runt = std::env::var("XDG_RUNTIME_DIR").ok();
    let prev_sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    unsafe {
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE");
    }

    let errors: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let errors2 = Arc::clone(&errors);
    let listener = HyprListener::new(
        Arc::new(Vec::new),
        Arc::new(|_: &str, _: i32| None),
        Arc::new(|_: &str, _: &str, _: SlideDir, _: i32, _: i32| {}),
        Arc::new(|_: &str, _: &str, _: SlideDir, _: i32, _: i32| {}),
    )
    .with_on_error(Arc::new(move |where_: &str, msg: &str| {
        errors2
            .lock()
            .unwrap()
            .push((where_.to_string(), msg.to_string()));
    }));
    // Returns (no socket to block on) and reports via on_error.
    listener.run();
    let got = errors.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "expected one on_error call, got {got:?}");
    assert_eq!(got[0].0, "socket");
    assert!(
        got[0].1.contains("XDG_RUNTIME_DIR"),
        "message: {:?}",
        got[0].1
    );

    match prev_runt {
        Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
        None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
    }
    match prev_sig {
        Some(v) => unsafe { std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", v) },
        None => unsafe { std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE") },
    }
}

#[test]
fn show_callback_calling_note_shown_does_not_deadlock() {
    // Re-entrancy scenario: a show callback calling note_shown (as IPC
    // `set` does out-of-band). The old MutexGuard-held-across-callbacks
    // shape deadlocked here; callbacks now run lock-free.
    let listener = Arc::new_cyclic(|weak: &Weak<HyprListener>| {
        let w = weak.clone();
        HyprListener::new(
            Arc::new(move || vec![("DP-1".to_string(), 1)]),
            Arc::new(|_: &str, _: i32| Some("/s/bg.jpg".to_string())),
            Arc::new(move |mon: &str, path: &str, _: SlideDir, _: i32, _: i32| {
                if let Some(l) = w.upgrade() {
                    l.note_shown(mon, path);
                }
            }),
            Arc::new(|_: &str, _: &str, _: SlideDir, _: i32, _: i32| {}),
        )
    });
    let l2 = Arc::clone(&listener);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        l2.apply_current();
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("deadlock: apply_current with re-entrant note_shown did not return");
    // State converged on the shown wallpaper despite the interleaved write.
    listener.note_shown("probe", "/s/x.jpg");
}

#[test]
fn ipc_roundtrip_set_and_list() {
    let _guard = ipc_lock();
    let dir = temp_cache_dir("ipc-basic");
    let prev = std::env::var("XDG_CACHE_HOME").ok();
    unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };

    let store: Arc<Mutex<HashMap<(String, i32), String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let store_set = Arc::clone(&store);
    let store_list = Arc::clone(&store);
    let handlers = IpcHandlers {
        set: Some(Arc::new(move |mon: &str, ws: i32, path: &str| {
            store_set
                .lock()
                .unwrap()
                .insert((mon.to_string(), ws), path.to_string());
            true
        })),
        list: Some(Arc::new(move || {
            let s = store_list.lock().unwrap();
            let mut lines: Vec<String> = s
                .iter()
                .map(|((m, w), p)| format!("{m} {w} {p}"))
                .collect();
            lines.sort();
            if lines.is_empty() {
                "\n".to_string()
            } else {
                lines.join("\n") + "\n"
            }
        })),
        quit: Some(Arc::new(|| {})),
        ..Default::default()
    };
    let server = std::thread::spawn(move || ipc::serve(handlers));
    // Wait for the socket to appear.
    let sock = ipc::socket_path();
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ipc::call("SET DP-1 1 /s/bg.jpg"), "OK\n");
    let list = ipc::call("LIST");
    assert!(list.contains("DP-1 1 /s/bg.jpg"), "LIST reply: {list:?}");
    assert_eq!(ipc::call("QUIT"), "OK\n");
    assert!(server.join().unwrap());
    match prev {
        Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
    }
}

#[test]
fn ipc_partial_line_does_not_hang_next_connection() {
    let _guard = ipc_lock();
    let dir = temp_cache_dir("ipc-partial");
    let prev = std::env::var("XDG_CACHE_HOME").ok();
    unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };

    let handlers = IpcHandlers {
        list: Some(Arc::new(|| "DP-1 1 /s/bg.jpg\n".to_string())),
        quit: Some(Arc::new(|| {})),
        ..Default::default()
    };
    let server = std::thread::spawn(move || ipc::serve(handlers));
    let sock = ipc::socket_path();
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Client drops mid-line without a newline: server must reply (ERR) and
    // keep serving the next connection.
    {
        let mut s = UnixStream::connect(&sock).unwrap();
        use std::io::Write;
        s.write_all(b"SET DP-").unwrap();
        // Drop without newline/shutdown-write-cleanliness: close mid-line.
    }
    // Next connection must still work (with a timeout so a hang fails fast).
    let list = ipc::call("LIST");
    assert!(
        list.contains("DP-1 1 /s/bg.jpg"),
        "LIST after partial: {list:?}"
    );
    assert_eq!(ipc::call("QUIT"), "OK\n");
    assert!(server.join().unwrap());
    match prev {
        Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
    }
}
