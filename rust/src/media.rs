//! mpvpaper hand-off for animated wallpapers (port of
//! `MediaDelegate.hpp/cpp`, mirroring wallpaper-daemon/mpvpaper.sh +
//! hyprpaper.sh kill semantics): only mpvpaper instances whose cmdline
//! mentions the monitor are killed.

use crate::util::{exec_capture, spawn_detached, split_lines, trim_ws};
use std::time::{Duration, Instant};

fn proc_read(pid: u32, file: &str) -> Vec<u8> {
    std::fs::read("/proc/".to_owned() + &pid.to_string() + file).unwrap_or_default()
}

/// Kill stale mpvpaper processes for a monitor (best-effort): scan
/// `/proc` for tasks whose `comm` is `mpvpaper` and whose cmdline
/// mentions the monitor, then SIGTERM each via `/bin/kill`.
pub fn stop_for_monitor(monitor: &str) {
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
        if cmdline.contains(monitor) {
            let _ = std::process::Command::new("kill")
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

/// Run the wal-theme hook detached (parity with hyprpaper.sh). No-op
/// when the hook script is missing or not executable.
pub fn apply_theme(wallpaper: &str) {
    use std::os::unix::fs::PermissionsExt;
    let home = std::env::var("HOME").unwrap_or_default();
    let script = format!("{home}/.config/hypr/theme/scripts/wal-theme.sh");
    let executable = std::fs::metadata(&script)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);
    if !executable {
        return;
    }
    spawn_detached(&[&script, wallpaper]);
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
