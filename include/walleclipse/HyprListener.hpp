#pragma once

#include <functional>
#include <map>
#include <optional>
#include <string>
#include <vector>

#include "walleclipse/Transition.hpp"

// Hyprland event listener + wallpaper switch orchestrator.
// Merges wallpaper-loop.c semantics: socket2.sock event filter
// (workspace/focusedmon), 100ms debounce, single-snapshot monitor
// query, per-monitor workspace state, empty-slot skip.
class HyprListener {
public:
    // Callbacks wired by main(): render static, delegate animated.
    // showStatic receives the slide direction derived from workspace order
    // (None on first paint) plus the from/to workspace ids for logging.
    struct Handlers {
        std::function<std::vector<std::string>()> listMonitors;
        std::function<bool(const std::string& monitor, int workspace, std::string& outPath)> lookup;
        std::function<void(const std::string& monitor, const std::string& path, SlideDir dir,
                           int fromWs, int toWs)>
            showStatic;
        std::function<void(const std::string& monitor, const std::string& path)> showAnimated;
        std::function<void(const std::string& where, const std::string& msg)> onError;
        std::function<void(const std::string& where, const std::string& msg)> onInfo;
    };

    explicit HyprListener(Handlers h);

    // One-shot: query monitors once and apply current wallpapers.
    void applyCurrent();

    // Record an out-of-band show (IPC `set` bypasses the event flow);
    // keeps animated-restore state accurate.
    void noteShown(const std::string& monitor, const std::string& path);

    // Blocking event loop (returns on socket EOF/error).
    void run();

    // Pure helper, unit-tested: parse `hyprctl monitors -j | jq` lines
    // of "<name> <workspaceId>" into pairs. Skips blanks/special.
    // Entries named FALLBACK (Hyprland's transient resume state) are
    // also skipped — they are not real monitors.
    static std::vector<std::pair<std::string, int>>
    parseSnapshotLines(const std::string& text);

    // What to do on a workspace switch. `shown` = path currently displayed
    // (nullopt when unknown), `target` = new mapping (nullopt = no key,
    // "" = empty slot), `lastStatic` = last static path shown on this
    // monitor. RestoreStatic re-shows the last static image when the
    // surface is unmapped (an animated wallpaper was showing and the new
    // slot is empty) — otherwise the screen goes black.
    enum class SwitchAction { Skip, ShowStatic, ShowAnimated, RestoreStatic };
    static SwitchAction decide(const std::optional<std::string>& shown,
                               const std::optional<std::string>& target,
                               const std::optional<std::string>& lastStatic);

    // Pure helper: does a socket2 line merit a wallpaper refresh?
    // Mirrors wallpaper-loop.c ("workspace" substring incl. moveworkspace,
    // plus focusedmon; workspacev2 covered by substring). Monitor
    // add/remove (sleep/resume, hotplug) also trigger a refresh.
    static bool isRefreshEvent(const std::string& line);

private:
    void handleRefresh();

    Handlers h_;
    std::map<std::string, int> lastWorkspace_;
    std::map<std::string, std::string> currentWallpaper_;
    // Last static path per monitor (animated workspaces unmap the layer
    // surface, so an empty slot must restore this instead of black-screen).
    std::map<std::string, std::string> lastStaticWallpaper_;
};
