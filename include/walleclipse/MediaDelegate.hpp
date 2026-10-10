#pragma once

#include <string>

// Owns the mpvpaper hand-off for animated wallpapers (gif/mp4/webm),
// mirroring wallpaper-daemon/mpvpaper.sh + hyprpaper.sh kill semantics:
// only mpvpaper instances whose cmdline mentions the monitor are killed.
struct MediaDelegate {
    // Kill stale mpvpaper for a monitor (best-effort).
    static void stopForMonitor(const std::string& monitor);
    // Spawn `mpvpaper <monitor> <path>` detached (double-fork, no shell).
    static bool playOnMonitor(const std::string& monitor, const std::string& path);
    // Run wal-theme hook detached (parity with hyprpaper.sh).
    static void applyTheme(const std::string& wallpaper);
    // True when `hyprctl layers` output (one namespace per line) lists a
    // background surface other than our own ("walleclipse") — i.e. the
    // player is up. Pure helper, unit-tested.
    static bool foreignSurfaceListed(const std::string& jqOutput);
    // Block until the player's layer surface appears on a monitor (it
    // spawns async: hiding our poster before this returns blanks the
    // screen between our unmap and mpvpaper's first frame). Returns
    // false on timeout — callers hide anyway (never wedges the switch).
    static bool waitForPlayer(const std::string& monitor, int timeoutMs = 3000);
};
