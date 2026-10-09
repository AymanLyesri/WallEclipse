#pragma once

#include <functional>
#include <map>
#include <string>
#include <vector>

// Hyprland event listener + wallpaper switch orchestrator.
// Merges wallpaper-loop.c semantics: socket2.sock event filter
// (workspace/focusedmon), 100ms debounce, single-snapshot monitor
// query, per-monitor workspace state, empty-slot skip.
class HyprListener {
public:
    // Callbacks wired by main(): render static, delegate animated.
    struct Handlers {
        std::function<std::vector<std::string>()> listMonitors;
        std::function<bool(const std::string& monitor, int workspace, std::string& outPath)> lookup;
        std::function<void(const std::string& monitor, const std::string& path)> showStatic;
        std::function<void(const std::string& monitor, const std::string& path)> showAnimated;
        std::function<void(const std::string& where, const std::string& msg)> onError;
        std::function<void(const std::string& where, const std::string& msg)> onInfo;
    };

    explicit HyprListener(Handlers h);

    // One-shot: query monitors once and apply current wallpapers.
    void applyCurrent();

    // Blocking event loop (returns on socket EOF/error).
    void run();

    // Pure helper, unit-tested: parse `hyprctl monitors -j | jq` lines
    // of "<name> <workspaceId>" into pairs. Skips blanks/special.
    static std::vector<std::pair<std::string, int>>
    parseSnapshotLines(const std::string& text);

    // Pure helper: does a socket2 line merit a wallpaper refresh?
    // Mirrors wallpaper-loop.c ("workspace" substring incl. moveworkspace,
    // plus focusedmon; workspacev2 covered by substring).
    static bool isRefreshEvent(const std::string& line);

private:
    void handleRefresh();

    Handlers h_;
    std::map<std::string, int> lastWorkspace_;
    std::map<std::string, std::string> currentWallpaper_;
};
