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
};
