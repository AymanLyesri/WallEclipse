//! Line-based Unix-socket IPC at `$XDG_CACHE_HOME/walleclipse.sock`
//! (fallback `~/.cache`, mirrors `Ipc.hpp/cpp`). One command per
//! connection: PRELOAD / SET / CURRENT / LIST / RELOAD / TRANSITION /
//! PREVIEW / PREVIEW_CLEAR / QUIT. Replies carry a trailing newline;
//! errors use an `ERR` prefix.

use std::io::{Read, Write};
use std::sync::Arc;

use crate::util::{cache_base_dir, split_lines, trim_ws};

/// Server-side command handlers, wired by `main()`.
pub struct Handlers {
    pub preload: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub set: Option<Arc<dyn Fn(&str, i32, &str) -> bool + Send + Sync>>,
    pub current: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    pub list: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    pub reload: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Empty name = query (return current); otherwise validate, persist
    /// and apply, returning the new value (empty = rejected -> ERR).
    pub transition: Option<Arc<dyn Fn(&str) -> String + Send + Sync>>,
    /// Fullscreen hover preview (mon, path -> shown).
    pub preview: Option<Arc<dyn Fn(&str, &str) -> bool + Send + Sync>>,
    /// Dismiss preview (mon, "" = all -> had-any-or-none, always true).
    pub preview_clear: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
    pub quit: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for Handlers {
    fn default() -> Self {
        Self {
            preload: None,
            set: None,
            current: None,
            list: None,
            reload: None,
            transition: None,
            preview: None,
            preview_clear: None,
            quit: None,
        }
    }
}

/// Socket path, honoring `$XDG_CACHE_HOME` with `~/.cache` fallback.
pub fn socket_path() -> String {
    cache_base_dir() + "/walleclipse.sock"
}

// Split "SET <mon> <ws> <path...>": path may contain spaces but not
// newlines. Returns (monitor, workspace, path).
fn parse_set(line: &str) -> Option<(String, i32, String)> {
    // line starts with "SET "
    let b = line[4..].find(' ').map(|i| i + 4)?;
    let c = line[b + 1..].find(' ')? + b + 1;
    let mon = line[4..b].to_string();
    let ws: i32 = line[b + 1..c].parse().ok()?;
    let path = trim_ws(&line[c + 1..]);
    if mon.is_empty() || path.is_empty() || path.contains('\n') {
        return None;
    }
    Some((mon, ws, path))
}

// Split "PREVIEW <mon> <path...>": same rule as `parse_set` — path
// may contain spaces but not newlines. Returns (monitor, path).
fn parse_preview(line: &str) -> Option<(String, String)> {
    // line starts with "PREVIEW "
    let rest = &line[8..];
    let b = rest.find(' ')?;
    let mon = rest[..b].to_string();
    let path = trim_ws(&rest[b + 1..]);
    if mon.is_empty() || path.is_empty() || path.contains('\n') {
        return None;
    }
    Some((mon, path))
}

fn upper_of(s: &str) -> String {
    s.to_ascii_uppercase()
}

fn read_all(mut stream: &std::os::unix::net::UnixStream) -> String {
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Blocking server loop. Returns after QUIT (`true`) or fatal error
/// (`false`).
pub fn serve(h: Handlers) -> bool {
    use std::os::unix::net::UnixListener;
    let spath = socket_path();
    if let Some(parent) = std::path::Path::new(&spath).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&spath);
    let listener = match UnixListener::bind(&spath) {
        Ok(l) => l,
        Err(_) => return false,
    };
    let mut stop = false;
    while !stop {
        let (mut cli, _) = match listener.accept() {
            Ok(c) => c,
            Err(_) => continue,
        };
        // One connection, one command: read to EOF (a client that
        // drops mid-line just yields a truncated first line, answered
        // ERR below — the loop keeps serving).
        let text = read_all(&cli);
        let line = trim_ws(split_lines(&text).first().map(String::as_str).unwrap_or(""));
        let mut reply = "ERR unknown command\n".to_string();
        if line.starts_with("PRELOAD ") {
            if let Some(f) = &h.preload {
                f(&trim_ws(&line[8..]));
            }
            reply = "OK\n".to_string();
        } else if line.starts_with("SET ") {
            match parse_set(&line) {
                None => {
                    reply = "ERR bad SET syntax (SET <monitor> <ws> <path>)\n".to_string();
                }
                Some((mon, ws, path)) => {
                    let ok = h.set.as_ref().is_some_and(|f| f(&mon, ws, &path));
                    reply = if ok {
                        "OK\n".to_string()
                    } else {
                        "ERR set failed\n".to_string()
                    };
                }
            }
        } else if line.starts_with("PREVIEW ") {
            match parse_preview(&line) {
                None => {
                    reply = "ERR bad PREVIEW syntax (PREVIEW <monitor> <path>)\n".to_string();
                }
                Some((mon, path)) => {
                    let ok = h.preview.as_ref().is_some_and(|f| f(&mon, &path));
                    reply = if ok {
                        "OK\n".to_string()
                    } else {
                        "ERR preview failed\n".to_string()
                    };
                }
            }
        } else if upper_of(line.get(..13).unwrap_or("")).as_str() == "PREVIEW_CLEAR" {
            // Word boundary: bare or ASCII-whitespace-separated monitor
            // arg (mirrors TRANSITION). `PREVIEW_CLEARFOO` is a syntax
            // error, not a clear for monitor "FOO".
            let boundary_ok = match line.as_bytes().get(13) {
                None => true,
                Some(b) => b.is_ascii_whitespace(),
            };
            if !boundary_ok {
                reply = "ERR bad PREVIEW_CLEAR syntax (PREVIEW_CLEAR [monitor])\n".to_string();
            } else {
                // Optional monitor arg (mirrors TRANSITION): trimmed
                // remainder, "" = all.
                let mon = trim_ws(if line.len() > 13 { &line[13..] } else { "" });
                let ok = h.preview_clear.as_ref().map(|f| f(&mon)).unwrap_or(true);
                reply = if ok {
                    "OK\n".to_string()
                } else {
                    "ERR preview-clear failed\n".to_string()
                };
            }
        } else if upper_of(&line) == "CURRENT" {
            reply = h.current.as_ref().map(|f| f()).unwrap_or_else(|| "\n".to_string());
        } else if upper_of(&line) == "LIST" {
            reply = h.list.as_ref().map(|f| f()).unwrap_or_else(|| "\n".to_string());
        } else if upper_of(&line) == "RELOAD" {
            if let Some(f) = &h.reload {
                f();
            }
            reply = "OK\n".to_string();
        } else if upper_of(line.get(..10).unwrap_or("")).as_str() == "TRANSITION" {
            let arg = trim_ws(if line.len() > 10 { &line[10..] } else { "" });
            let res = h.transition.as_ref().map(|f| f(&arg)).unwrap_or_default();
            reply = if res.is_empty() {
                "ERR bad transition (slide|fade|none)\n".to_string()
            } else {
                res + "\n"
            };
        } else if upper_of(&line) == "QUIT" {
            reply = "OK\n".to_string();
            stop = true;
            if let Some(f) = &h.quit {
                f();
            }
        }
        let _ = cli.write_all(reply.as_bytes());
    }
    drop(listener);
    let _ = std::fs::remove_file(&spath);
    true
}

/// One-shot client: send lines, return server reply.
pub fn call(payload: &str) -> String {
    call_at(std::path::Path::new(&socket_path()), payload)
}

fn call_at(sock: &std::path::Path, payload: &str) -> String {
    use std::os::unix::net::UnixStream;
    let Ok(mut fd) = UnixStream::connect(sock) else {
        return "ERR daemon not running\n".to_string();
    };
    let mut msg = payload.to_string();
    if !msg.ends_with('\n') {
        msg.push('\n');
    }
    // Mirror C++ `Ipc::call` (Ipc.cpp:114-133): `"ERR no socket"` is
    // returned only when `socket()` itself fails. Every later-stage
    // failure (connect refused = dead daemon, write to a dead peer, ...)
    // must read as daemon-down so scripts see the `ERR` prefix and exit
    // 1. (C++ ignores the write result and surfaces whatever reply
    // arrives — possibly empty, which `runClient` would mistake for
    // success; the daemon string is the observable-behavior equivalent.)
    if fd.write_all(msg.as_bytes()).is_err() {
        return "ERR daemon not running\n".to_string();
    }
    let _ = fd.shutdown(std::net::Shutdown::Write);
    read_all(&fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Serializes the `serve` tests below: they isolate the socket by
    /// pointing process-global `XDG_CACHE_HOME` at a temp dir, and
    /// parallel threads would otherwise cross `call` traffic between
    /// the two servers (wrong replies + mixed handler logs).
    static SERVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Write-failure path (server accepted, then vanished: dead daemon)
    /// must read as daemon-down — not `"ERR no socket"` (which C++
    /// reserves for `socket()` failure, Ipc.cpp:115-117) and not an empty
    /// reply (which `run_client` would mistake for success, exit 0).
    /// Uses an isolated socket path under a temp dir, never the live one.
    #[test]
    fn write_failure_reports_daemon_not_running() {
        let dir = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-ipc-writefail-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("walleclipse.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        // Server: accept one connection, then close it immediately, so
        // the client's write hits a dead peer (EPIPE; SIGPIPE is ignored
        // by the Rust runtime, so `write_all` returns `Err`).
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            drop(conn);
        });
        // Oversized payload: early bytes may still fit the socket buffers
        // before the RST lands, so `write_all` needs many chunks to
        // deterministically observe EPIPE. (A single small write could
        // succeed into the buffer and then hang in `read_all`.)
        // `accept` blocks until our connect arrives and the close always
        // follows, so one attempt deterministically hits the dead peer:
        // the 2MB can never fully buffer with no reader.
        let big = "PING ".to_string() + &"x".repeat(2 * 1024 * 1024);
        let reply = call_at(&sock, &big);
        assert_eq!(reply, "ERR daemon not running\n");
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_and_preview_clear_roundtrip() {
        let _guard = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Temp-socket `serve` test (isolated XDG_CACHE_HOME, never live).
        let dir = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-ipc-preview-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        let got_preview = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let got_clear = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let gp = std::sync::Arc::clone(&got_preview);
        let gc = std::sync::Arc::clone(&got_clear);
        let server = std::thread::spawn(move || {
            serve(Handlers {
                preview: Some(std::sync::Arc::new(move |mon: &str, path: &str| {
                    gp.lock().unwrap().push((mon.to_string(), path.to_string()));
                    true
                })),
                preview_clear: Some(std::sync::Arc::new(move |mon: &str| {
                    gc.lock().unwrap().push(mon.to_string());
                    true
                })),
                ..Handlers::default()
            })
        });
        // Wait for the socket to appear (serve binds after thread start).
        for _ in 0..100 {
            if std::path::Path::new(&socket_path()).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let reply = call("PREVIEW DP-1 /a/b.jpg");
        assert_eq!(reply, "OK\n");
        assert_eq!(
            *got_preview.lock().unwrap(),
            vec![("DP-1".to_string(), "/a/b.jpg".to_string())]
        );
        // Path with spaces survives (same split rule as SET).
        let reply = call("PREVIEW DP-1 /a/b c.jpg");
        assert_eq!(reply, "OK\n");
        assert_eq!(
            got_preview.lock().unwrap()[1],
            ("DP-1".to_string(), "/a/b c.jpg".to_string())
        );
        let reply = call("PREVIEW_CLEAR");
        assert_eq!(reply, "OK\n");
        assert_eq!(*got_clear.lock().unwrap(), vec!["".to_string()]);
        let _ = call("QUIT");
        let _ = server.join();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_failure_replies_err() {
        let _guard = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-ipc-preview-fail-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        let server = std::thread::spawn(move || {
            serve(Handlers {
                preview: Some(std::sync::Arc::new(|_: &str, _: &str| false)),
                ..Handlers::default()
            })
        });
        for _ in 0..100 {
            if std::path::Path::new(&socket_path()).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let reply = call("PREVIEW DP-1 /a/b.jpg");
        assert_eq!(reply, "ERR preview failed\n");
        let _ = call("QUIT");
        let _ = server.join();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_clear_requires_word_boundary() {
        let _guard = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-ipc-preview-boundary-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        let got_clear = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let gc = std::sync::Arc::clone(&got_clear);
        let server = std::thread::spawn(move || {
            serve(Handlers {
                preview_clear: Some(std::sync::Arc::new(move |mon: &str| {
                    gc.lock().unwrap().push(mon.to_string());
                    true
                })),
                ..Handlers::default()
            })
        });
        for _ in 0..100 {
            if std::path::Path::new(&socket_path()).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // No word boundary after the verb: syntax error, handler untouched.
        let reply = call("PREVIEW_CLEARX");
        assert_eq!(
            reply,
            "ERR bad PREVIEW_CLEAR syntax (PREVIEW_CLEAR [monitor])\n"
        );
        assert!(got_clear.lock().unwrap().is_empty());
        // Bare verb and verb + monitor still reply OK.
        let reply = call("PREVIEW_CLEAR");
        assert_eq!(reply, "OK\n");
        let reply = call("PREVIEW_CLEAR DP-1");
        assert_eq!(reply, "OK\n");
        assert_eq!(
            *got_clear.lock().unwrap(),
            vec!["".to_string(), "DP-1".to_string()]
        );
        let _ = call("QUIT");
        let _ = server.join();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Storm shape: 10 rapid PREVIEW + 1 PREVIEW_CLEAR against handlers
    /// doing 350ms of work (simulating the fade) must still reply fast.
    /// Takes SERVE_LOCK (remaps XDG_CACHE_HOME, never live). The stubs
    /// get the same async treatment as the real preview handlers in
    /// `main.rs` (reply fast, work on detached workers): with synchronous
    /// handlers the serial accept loop pushes later replies past 3.5s and
    /// the <1s assertion fails (verified RED: 9 late, 3.89s total).
    #[test]
    fn preview_storm_replies_stay_fast() {
        let _guard = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-ipc-preview-storm-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        let got_preview =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let got_clear = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let gp = std::sync::Arc::clone(&got_preview);
        let gc = std::sync::Arc::clone(&got_clear);
        let server = std::thread::spawn(move || {
            serve(Handlers {
                preview: Some(std::sync::Arc::new(move |mon: &str, path: &str| {
                    // Async shape (mirrors `main.rs`): reply immediately,
                    // do the 350ms fade stand-in on a detached worker.
                    let gp = std::sync::Arc::clone(&gp);
                    let rec = (mon.to_string(), path.to_string());
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(350));
                        gp.lock().unwrap().push(rec);
                    });
                    true
                })),
                preview_clear: Some(std::sync::Arc::new(move |mon: &str| {
                    let gc = std::sync::Arc::clone(&gc);
                    let rec = mon.to_string();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(350));
                        gc.lock().unwrap().push(rec);
                    });
                    true
                })),
                ..Handlers::default()
            })
        });
        for _ in 0..100 {
            if std::path::Path::new(&socket_path()).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Fire the whole storm concurrently; every reply is timed from
        // the shared t0, so a serial loop shows up as late arrivals.
        let t0 = std::time::Instant::now();
        let mut jobs = Vec::new();
        for i in 0..10 {
            let payload = format!("PREVIEW DP-1 /storm/{i}.jpg");
            jobs.push(std::thread::spawn(move || call(&payload)));
        }
        jobs.push(std::thread::spawn(|| call("PREVIEW_CLEAR DP-1")));
        let mut bad = 0u32;
        let mut late = 0u32;
        for j in jobs {
            let reply = j.join().unwrap();
            if reply != "OK\n" {
                bad += 1;
            }
            if t0.elapsed() >= std::time::Duration::from_secs(1) {
                late += 1;
            }
        }
        assert_eq!(bad, 0, "all storm replies must be OK");
        assert_eq!(
            late, 0,
            "every storm reply must arrive in <1s (serial handling pushes later ones past 3.5s)"
        );
        // The work still happens, just off-loop: all 11 records land.
        for _ in 0..200 {
            let done =
                got_preview.lock().unwrap().len() == 10 && got_clear.lock().unwrap().len() == 1;
            if done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            got_preview.lock().unwrap().len(),
            10,
            "all 10 shows dispatched"
        );
        assert_eq!(
            *got_clear.lock().unwrap(),
            vec!["DP-1".to_string()],
            "clear dispatched"
        );
        let _ = call("QUIT");
        let _ = server.join();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
