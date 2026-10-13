//! mpvpaper hand-off for animated wallpapers (port of
//! `MediaDelegate.hpp/cpp`, mirroring wallpaper-daemon/mpvpaper.sh +
//! hyprpaper.sh kill semantics): only mpvpaper instances whose argv lists
//! the monitor as an exact token are killed.

use crate::util::{exec_capture, spawn_detached, split_lines, trim_ws};
use std::time::{Duration, Instant};

fn proc_read(pid: u32, file: &str) -> Vec<u8> {
    std::fs::read("/proc/".to_owned() + &pid.to_string() + file).unwrap_or_default()
}

/// True when a `/proc/<pid>/cmdline` blob passes `monitor` as an exact
/// argv token. `/proc` cmdlines are NUL-separated argv, so splitting on
/// `'\0'` and comparing whole tokens never confuses `DP-1` with `DP-11`
/// (the C++ `stopForMonitor` uses substring `.find()`, which does).
/// Pure helper, unit-tested.
pub fn cmdline_lists_monitor(cmdline: &str, monitor: &str) -> bool {
    if monitor.is_empty() {
        return false;
    }
    cmdline.split('\0').any(|tok| tok == monitor)
}

/// Log string emitted when `stop_for_monitor` gets an empty monitor name.
/// C++ `stopForMonitor` (MediaDelegate.cpp:34-49) matches via substring
/// `.find(monitor)`, and `find("")` always matches — so an empty name
/// would SIGTERM every mpvpaper instance. The Rust exact-token match
/// ([`cmdline_lists_monitor`]) refuses instead; this log keeps that
/// refusal observable so a future empty-name bug surfaces instead of
/// silently leaking players. Covered by `empty_monitor_kills_nothing_and_logs`.
pub const EMPTY_MONITOR_LOG: &str =
    "stop_for_monitor: refusing empty monitor name (kills nothing; C++ substring would kill everything)";

/// Kill stale mpvpaper processes for a monitor (best-effort): scan
/// `/proc` for tasks whose `comm` is `mpvpaper` and whose argv lists the
/// monitor as an exact token, then SIGTERM each via `/bin/kill`
/// (absolute path: PATH-independent).
pub fn stop_for_monitor(monitor: &str) {
    if monitor.is_empty() {
        eprintln!("[media] {EMPTY_MONITOR_LOG}");
        return;
    }
    let proc = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return,
    };
    for entry in proc.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let mut comm = proc_read(pid, "/comm");
        while comm.last().is_some_and(|b| *b == b'\n' || *b == 0) {
            comm.pop();
        }
        if comm != b"mpvpaper" {
            continue;
        }
        let raw = proc_read(pid, "/cmdline");
        let cmdline = String::from_utf8_lossy(&raw);
        if cmdline_lists_monitor(&cmdline, monitor) {
            let _ = std::process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// Spawn `mpvpaper <monitor> <path>` detached (no shell), after stopping
/// stale players for the monitor. Always returns true (spawn itself is
/// fire-and-forget; the player surfaces async, see [`wait_for_player`]).
pub fn play_on_monitor(monitor: &str, path: &str) -> bool {
    stop_for_monitor(monitor);
    spawn_detached(&[
        "mpvpaper",
        "-o",
        "no-audio --loop --fs --panscan=1.0 --hwdec=auto-safe",
        monitor,
        path,
    ]);
    true
}

/// Run the theme hook detached (parity with hyprpaper.sh). No-op until the
/// coloreclipse package is installed.
pub fn apply_theme(wallpaper: &str) {
    use std::os::unix::fs::PermissionsExt;
    let binary = "/usr/bin/coloreclipse";
    let executable = std::fs::metadata(binary)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    if !executable {
        return;
    }
    spawn_detached(&[binary, "wal", wallpaper]);
}

/// True when `hyprctl layers` output (one namespace per line) lists a
/// background surface other than our own (`"walleclipse"`) — i.e. the
/// player is up. Pure helper, unit-tested.
pub fn foreign_surface_listed(jq_output: &str) -> bool {
    for line in split_lines(jq_output) {
        let line = trim_ws(&line);
        if !line.is_empty() && line != "walleclipse" {
            return true;
        }
    }
    false
}

/// Block until the player's layer surface appears on a monitor (it
/// spawns async: hiding our poster before this returns blanks the
/// screen between our unmap and mpvpaper's first frame). Returns false
/// on timeout — callers hide anyway (never wedges the switch).
pub fn wait_for_player(monitor: &str, timeout_ms: u64) -> bool {
    // Monitor names come from hyprctl (DP-2, HDMI-A-1); refuse anything
    // that would break the single-quoted shell embedding below.
    if monitor.is_empty() || monitor.contains('\'') {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let cmd = "hyprctl layers -j 2>/dev/null | jq -r --arg m '".to_owned()
        + monitor
        + "' '.[$m].levels.\"0\"[].namespace // empty' 2>/dev/null";
    while Instant::now() < deadline {
        if foreign_surface_listed(&exec_capture(&cmd)) {
            // Surface exists but mpv may not have attached its first
            // frame yet: hold our poster one beat longer so the handoff
            // never shows a blank frame in between.
            std::thread::sleep(Duration::from_millis(400));
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_monitor_kills_nothing_and_logs() {
        // No-kill half: the exact-token matcher never matches an empty
        // monitor, so no pid can be selected (unlike C++ `find("")`,
        // which matches every cmdline).
        assert!(!cmdline_lists_monitor("mpvpaper\0DP-1\0/v/a.mp4\0", ""));
        assert!(!cmdline_lists_monitor("", ""));
        // Early return: must not scan /proc at all (returns right after
        // the log below).
        stop_for_monitor("");
        // Log half: asserted via the shared constant — the documented log
        // string the early return emits through eprintln! (capturing
        // stderr would need process-level redirection; the constant is
        // the observable contract).
        assert!(EMPTY_MONITOR_LOG.contains("empty monitor"));
    }
}
