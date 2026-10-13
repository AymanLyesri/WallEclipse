//! Line-based Unix-socket IPC at `$XDG_CACHE_HOME/walleclipse.sock`
//! (fallback `~/.cache`, mirrors `Ipc.hpp/cpp`). One command per
//! connection: PRELOAD / SET / CURRENT / LIST / RELOAD / TRANSITION /
//! QUIT. Replies carry a trailing newline; errors use an `ERR` prefix.

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
}
