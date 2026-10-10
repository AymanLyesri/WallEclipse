// WallEclipse — layer-shell wallpaper daemon with startup preloading.
// Replaces hyprpaper + wallpaper-loop.c; mpvpaper stays for animated.
//
// Usage:
//   walleclipse                  run daemon (preload-all, then Hyprland events)
//   walleclipse --once MON PATH  render one image and exit (backend smoke test)
//   walleclipse set MON WS PATH  update config + show via running daemon
//   walleclipse preload PATH     decode into running daemon's cache
//   walleclipse current|list     query running daemon

#include <atomic>
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <map>
#include <mutex>
#include <optional>
#include <thread>

#include "walleclipse/ConfigStore.hpp"
#include "walleclipse/HyprListener.hpp"
#include "walleclipse/ImageLoader.hpp"
#include "walleclipse/Ipc.hpp"
#include "walleclipse/MediaDelegate.hpp"
#include "walleclipse/Preloader.hpp"
#include "walleclipse/Util.hpp"
#include "walleclipse/VideoPoster.hpp"
#include "walleclipse/WaylandBackend.hpp"

namespace {

constexpr const char* kLogFile = "/tmp/walleclipse.log";

void logLine(const std::string& where, const std::string& msg) {
    std::string line = "[" + where + "] " + msg + "\n";
    std::fwrite(line.data(), 1, line.size(), stderr);
    if (FILE* fp = std::fopen(kLogFile, "a")) {
        std::fwrite(line.data(), 1, line.size(), fp);
        std::fclose(fp);
    }
}

void printHelp(const char* argv0) {
    std::cout << "WallEclipse " << "0.1.0" << " — preloaded layer-shell wallpapers\n\n"
              << "Usage:\n"
              << "  " << argv0 << "                  run daemon\n"
              << "  " << argv0 << " --once MON PATH  render once, exit\n"
              << "  " << argv0 << " set MON WS PATH  set workspace wallpaper\n"
              << "  " << argv0 << " preload PATH     preload into daemon cache\n"
              << "  " << argv0 << " reload           re-read config + re-apply current\n"
              << "  " << argv0 << " current|list     query daemon\n";
}

std::vector<std::string> queryMonitors() {
    // One hyprctl snapshot per call (no N+1), same as wallpaper-loop.c.
    std::vector<std::string> out;
    for (auto l : splitLines(execCapture("hyprctl monitors -j | jq -r '.[].name'"))) {
        l = trimWs(l);
        if (!l.empty())
            out.push_back(l);
    }
    return out;
}

std::optional<DecodedImage> resolveImage(const std::string& path) {
    std::string expanded = expandHome(path);
    if (auto img = loadImage(expanded.empty() ? path : expanded))
        return img;
    if (!expanded.empty() && expanded != path)
        return loadImage(path);
    return std::nullopt;
}

int runClient(const std::string& req) {
    std::string reply = Ipc::call(req);
    std::cout << reply;
    return reply.rfind("ERR", 0) == 0 ? 1 : 0;
}

} // namespace

int main(int argc, char** argv) {
    // Client subcommands that talk to a running daemon.
    if (argc >= 2 && std::strcmp(argv[1], "set") == 0) {
        if (argc < 5) {
            std::cerr << "Usage: walleclipse set <monitor> <workspace> <path>\n";
            return 2;
        }
        return runClient("SET " + std::string(argv[2]) + " " + argv[3] + " " + argv[4]);
    }
    if (argc >= 2 && std::strcmp(argv[1], "preload") == 0) {
        if (argc < 3) {
            std::cerr << "Usage: walleclipse preload <path>\n";
            return 2;
        }
        return runClient("PRELOAD " + std::string(argv[2]));
    }
    if (argc >= 2 &&
        (std::strcmp(argv[1], "current") == 0 || std::strcmp(argv[1], "list") == 0 ||
         std::strcmp(argv[1], "reload") == 0)) {
        return runClient(argv[1]);
    }
    if (argc >= 2 &&
        (std::strcmp(argv[1], "--help") == 0 || std::strcmp(argv[1], "-h") == 0)) {
        printHelp(argv[0]);
        return 0;
    }

    ConfigStore store = ConfigStore::defaultLocation();
    Preloader preloader;

    // Smoke-test path: render one image without the event loop.
    if (argc >= 2 && std::strcmp(argv[1], "--once") == 0) {
        if (argc < 4) {
            std::cerr << "Usage: walleclipse --once <monitor> <path>\n";
            return 2;
        }
        std::string mon = argv[2], path = argv[3];
        auto img = resolveImage(path);
        if (!img) {
            std::cerr << "cannot decode " << path << "\n";
            return 1;
        }
        WaylandBackend backend;
        if (!backend.init([](const std::string& m) { logLine("wayland", m); }))
            return 1;
        if (!backend.setWallpaper(mon, *img)) {
            std::cerr << "unknown monitor '" << mon << "'\n";
            return 1;
        }
        std::this_thread::sleep_for(std::chrono::seconds(2));
        return 0;
    }

    // Unknown subcommand: fail instead of starting a second daemon.
    if (argc >= 2 && argv[1][0] != '-') {
        std::cerr << "Unknown command '" << argv[1] << "'\n";
        printHelp(argv[0]);
        return 2;
    }

    // ---- Daemon ----
    std::cout << std::unitbuf; // unbuffered: startup lines visible live in logs
    std::cout << "WallEclipse starting...\n";

    auto monitors = queryMonitors();
    if (monitors.empty())
        logLine("main", "hyprctl returned no monitors; continuing with cached config");
    store.ensureMonitors(monitors);
    // Also seed from any already-known config dirs (hotplug memory).
    for (const auto& known : store.monitors()) {
        if (std::find(monitors.begin(), monitors.end(), known) == monitors.end())
            monitors.push_back(known);
    }

    std::cout << "Preloading wallpapers...\n";
    preloader.preloadAll(store);
    std::cout << "Preloaded " << preloader.size() << " wallpapers.\n";

    WaylandBackend backend;
    if (!backend.init([](const std::string& m) { logLine("wayland", m); })) {
        logLine("main", "Wayland backend init failed");
        return 1;
    }

    // Warm the cover-fit cache: the ~90ms scale per wallpaper happens here,
    // once, instead of on the first visit to each workspace. Animated
    // posters are precached too so slides to/from video start instantly.
    for (const auto& mon : monitors)
        for (const auto& [ws, path] : store.wallpapersFor(mon)) {
            (void)ws;
            if (isMediaWallpaper(path)) {
                if (auto poster = preloader.getPoster(path))
                    backend.precache(mon, poster);
                continue;
            }
            if (auto img = preloader.get(path))
                backend.precache(mon, img);
        }

    std::string currentConf = store.baseDir() + "/current.conf";
    auto writeCurrentConf = [&](const std::string& path) {
        std::string expanded = expandHome(path);
        if (FILE* fp = std::fopen(currentConf.c_str(), "w")) {
            std::fputs((expanded.empty() ? path : expanded).c_str(), fp);
            std::fputc('\n', fp);
            std::fclose(fp);
        }
    };

    HyprListener::Handlers handlers;
    handlers.listMonitors = [&] { return queryMonitors(); };
    handlers.lookup = [&](const std::string& mon, int ws, std::string& out) {
        auto v = store.getWallpaper(mon, ws);
        if (!v)
            return false;
        out = *v;
        return true;
    };
    // Last shown path per monitor (listener + IPC threads). Needed to
    // find the poster of a running video when leaving it: the layer
    // surface is hidden while mpvpaper shows, so the return slide
    // starts from the poster, not from a stale static frame.
    std::map<std::string, std::string> shown;
    std::mutex shownMutex;
    auto getShown = [&](const std::string& mon) -> std::string {
        std::lock_guard<std::mutex> lock(shownMutex);
        auto it = shown.find(mon);
        return it == shown.end() ? std::string() : it->second;
    };
    auto setShown = [&](const std::string& mon, const std::string& path) {
        std::lock_guard<std::mutex> lock(shownMutex);
        shown[mon] = path;
    };
    // Poster with on-demand extraction (first visit pays ~250ms once,
    // then disk + RAM cached). Null when extraction fails — callers
    // fall back to the previous instant cut.
    auto posterFor = [&](const std::string& path) -> std::shared_ptr<DecodedImage> {
        if (auto p = preloader.getPoster(path))
            return std::shared_ptr<DecodedImage>(p);
        preloader.preloadPosterOne(path);
        return preloader.getPoster(path);
    };
    // Hand the monitor to mpvpaper without a blank frame in between:
    // the player spawns async, so our poster stays mapped until its
    // layer surface appears (plus a beat for its first frame) and only
    // then is hidden. Works regardless of surface stacking order.
    auto handoffToVideo = [&](const std::string& mon, const std::string& path) {
        std::string expanded = expandHome(path);
        MediaDelegate::playOnMonitor(mon, expanded.empty() ? path : expanded);
        if (!MediaDelegate::waitForPlayer(mon))
            logLine("change_wallpaper", "mpvpaper slow on '" + mon +
                                            "' — hid poster before first frame");
        backend.hideMonitor(mon);
    };
    auto trackShown = [&](const std::string& mon, const std::string& path,
                        HyprListener* l = nullptr) {
        if (l)
            l->noteShown(mon, path);
        setShown(mon, path);
        writeCurrentConf(path);
        MediaDelegate::applyTheme(path);
    };
    auto showStaticNow = [&](const std::string& mon, const std::string& path,
                             SlideDir dir) -> bool {
        // Animated slipped into a static slot? Delegate (never render garbage).
        if (isMediaWallpaper(path)) {
            handoffToVideo(mon, path);
            trackShown(mon, path);
            return true;
        }
        auto img = preloader.get(path);
        if (!img) { // cache miss (SET during runtime): decode on demand
            preloader.preloadOne(path);
            img = preloader.get(path);
        }
        if (!img) {
            logLine("change_wallpaper", "cannot decode '" + path + "' — keeping previous");
            return false;
        }
        if (!backend.isMapped(mon)) {
            // Coming from video: restore its poster under mpvpaper first
            // (invisible), so killing the player reveals the poster and
            // the slide starts there instead of cutting from black.
            std::string prev = getShown(mon);
            if (isMediaWallpaper(prev))
                if (auto poster = posterFor(prev))
                    backend.remapWithImage(mon, poster);
        }
        MediaDelegate::stopForMonitor(mon); // leaving video -> kill mpvpaper first
        if (!backend.setWallpaper(mon, std::shared_ptr<const DecodedImage>(img), dir))
            logLine("change_wallpaper", "unknown monitor '" + mon + "'");
        trackShown(mon, path);
        return true;
    };
    auto showAnimatedNow = [&](const std::string& mon, const std::string& path,
                               SlideDir dir) -> bool {
        if (backend.isMapped(mon)) {
            // Static -> animated: slide to the video's poster, then hand off.
            if (auto poster = posterFor(path))
                backend.setWallpaper(mon, std::shared_ptr<const DecodedImage>(poster),
                                     dir);
        } else if (std::string prev = getShown(mon); isMediaWallpaper(prev)) {
            // Animated -> animated: reveal old poster, slide poster->poster.
            auto oldPoster = posterFor(prev);
            auto newPoster = posterFor(path);
            if (oldPoster && newPoster && backend.remapWithImage(mon, oldPoster)) {
                MediaDelegate::stopForMonitor(mon); // reveal old poster
                backend.setWallpaper(mon,
                                     std::shared_ptr<const DecodedImage>(newPoster), dir);
            }
        }
        handoffToVideo(mon, path);
        trackShown(mon, path);
        return true;
    };
    handlers.showStatic = [&](const std::string& mon, const std::string& path,
                              SlideDir dir, int, int) {
        if (isMediaWallpaper(path))
            showAnimatedNow(mon, path, dir);
        else
            showStaticNow(mon, path, dir);
        // Keep listener state accurate (handlers run inside listener flow).
    };
    handlers.showAnimated = [&](const std::string& mon, const std::string& path,
                                SlideDir dir, int, int) { showAnimatedNow(mon, path, dir); };
    handlers.onError = [](const std::string& w, const std::string& m) { logLine(w, m); };
    handlers.onInfo = [](const std::string& w, const std::string& m) { logLine(w, m); };

    HyprListener listener(std::move(handlers));
    HyprListener* activeListener = &listener;

    // IPC thread: SET updates config + shows immediately (compat shim for
    // set-wallpaper.sh callers during migration).
    std::atomic<bool> ipcRun = true;
    std::thread ipcThread([&] {
        Ipc::Handlers ih;
        ih.preload = [&](const std::string& p) { preloader.preloadOne(p); };
        ih.reload = [&] {
            // Re-read config from disk (picks up external edits), preload
            // any new paths, then re-apply current workspace wallpapers.
            preloader.preloadAll(store);
            if (activeListener)
                activeListener->applyCurrent();
            logLine("ipc", "reloaded config");
        };
        ih.set = [&](const std::string& mon, int ws, const std::string& path) {
            if (!store.setWallpaper(mon, ws, path))
                return false;
            // No slide out-of-band; reuse the same show path with None dir.
            // Warm the cache first so the next switch slides.
            if (isMediaWallpaper(path))
                preloader.preloadPosterOne(path);
            else
                preloader.preloadOne(path);
            // Publish via listener state, then render without duplicating logic.
            // Note: showStaticNow/showAnimatedNow already track + theme.
            // Temporarily expose listener for trackShown.
            bool ok = isMediaWallpaper(path)
                            ? showAnimatedNow(mon, path, SlideDir::None)
                            : showStaticNow(mon, path, SlideDir::None);
            if (ok && activeListener)
                activeListener->noteShown(mon, path);
            return ok;
        };
        ih.current = [&] {
            std::ifstream fp(currentConf);
            std::string s((std::istreambuf_iterator<char>(fp)),
                          std::istreambuf_iterator<char>());
            return s.empty() ? std::string("\n") : s;
        };
        ih.list = [&] {
            std::string s;
            for (const auto& mon : store.monitors())
                for (const auto& [ws, p] : store.wallpapersFor(mon))
                    s += mon + " " + std::to_string(ws) + " " + p + "\n";
            return s.empty() ? std::string("\n") : s;
        };
        ih.quit = [&] { ipcRun = false; };
        if (!Ipc::serve(std::move(ih)))
            logLine("ipc", "serve failed (socket " + Ipc::socketPath() + "): " +
                                std::strerror(errno));
    });
    ipcThread.detach();

    std::cout << "Listening for workspace changes...\n";
    listener.run(); // blocks until socket EOF
    return 0;
}
