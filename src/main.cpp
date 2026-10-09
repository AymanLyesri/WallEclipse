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
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <thread>

#include "walleclipse/ConfigStore.hpp"
#include "walleclipse/HyprListener.hpp"
#include "walleclipse/ImageLoader.hpp"
#include "walleclipse/Ipc.hpp"
#include "walleclipse/MediaDelegate.hpp"
#include "walleclipse/Preloader.hpp"
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
              << "  " << argv0 << " current|list     query daemon\n";
}

std::vector<std::string> queryMonitors() {
    // One hyprctl snapshot per call (no N+1), same as wallpaper-loop.c.
    std::vector<std::string> out;
    std::unique_ptr<FILE, decltype(&pclose)> pipe(
        popen("hyprctl monitors -j | jq -r '.[].name'", "r"), pclose);
    if (!pipe)
        return out;
    char* line = nullptr;
    size_t cap = 0;
    while (getline(&line, &cap, pipe.get()) > 0) {
        std::string s(line);
        while (!s.empty() && (s.back() == '\n' || s.back() == '\r'))
            s.pop_back();
        if (!s.empty())
            out.push_back(s);
    }
    free(line);
    return out;
}

} // namespace

int main(int argc, char** argv) {
    // Client subcommands that talk to a running daemon.
    if (argc >= 2 && std::strcmp(argv[1], "set") == 0) {
        if (argc < 5) {
            std::cerr << "Usage: walleclipse set <monitor> <workspace> <path>\n";
            return 2;
        }
        std::string req = "SET " + std::string(argv[2]) + " " + argv[3] + " " + argv[4];
        std::cout << Ipc::call(req);
        return 0;
    }
    if (argc >= 2 && std::strcmp(argv[1], "preload") == 0) {
        if (argc < 3) {
            std::cerr << "Usage: walleclipse preload <path>\n";
            return 2;
        }
        std::cout << Ipc::call("PRELOAD " + std::string(argv[2]));
        return 0;
    }
    if (argc >= 2 &&
        (std::strcmp(argv[1], "current") == 0 || std::strcmp(argv[1], "list") == 0)) {
        std::cout << Ipc::call(argv[1]);
        return 0;
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
        std::string expanded;
        ConfigStore::expandPath(path, expanded);
        auto img = loadImage(expanded);
        if (!img)
            img = loadImage(path);
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

    std::string currentConf = store.baseDir() + "/current.conf";

    HyprListener::Handlers handlers;
    handlers.listMonitors = [&] { return queryMonitors(); };
    handlers.lookup = [&](const std::string& mon, int ws, std::string& out) {
        auto v = store.getWallpaper(mon, ws);
        if (!v)
            return false;
        out = *v;
        return true;
    };
    handlers.showStatic = [&](const std::string& mon, const std::string& path) {
        // Animated slipped into a static slot? Delegate (never render garbage).
        if (isMediaWallpaper(path)) {
            backend.hideMonitor(mon);
            std::string expanded;
            ConfigStore::expandPath(path, expanded);
            MediaDelegate::playOnMonitor(mon, expanded.empty() ? path : expanded);
            MediaDelegate::applyTheme(path);
            return;
        }
        auto img = preloader.get(path);
        if (!img) { // cache miss (SET during runtime): decode on demand, then cache
            preloader.preloadOne(path);
            img = preloader.get(path);
        }
        if (!img) {
            logLine("change_wallpaper", "cannot decode '" + path + "' — keeping previous");
            return;
        }
        MediaDelegate::stopForMonitor(mon); // leaving video -> kill mpvpaper first
        if (!backend.setWallpaper(mon, *img))
            logLine("change_wallpaper", "unknown monitor '" + mon + "'");
        {
            std::string expanded;
            ConfigStore::expandPath(path, expanded);
            if (FILE* fp = std::fopen(currentConf.c_str(), "w")) {
                std::fputs((expanded.empty() ? path : expanded).c_str(), fp);
                std::fputc('\n', fp);
                std::fclose(fp);
            }
        }
        std::thread([path] { MediaDelegate::applyTheme(path); }).detach();
    };
    handlers.showAnimated = [&](const std::string& mon, const std::string& path) {
        backend.hideMonitor(mon);
        std::string expanded;
        ConfigStore::expandPath(path, expanded);
        MediaDelegate::playOnMonitor(mon, expanded.empty() ? path : expanded);
        MediaDelegate::applyTheme(path);
    };
    handlers.onError = [](const std::string& w, const std::string& m) { logLine(w, m); };
    handlers.onInfo = [](const std::string& w, const std::string& m) { logLine(w, m); };

    HyprListener listener(std::move(handlers));

    // IPC thread: SET updates config + shows immediately (compat shim for
    // set-wallpaper.sh callers during migration).
    std::atomic<bool> ipcRun = true;
    std::thread ipcThread([&] {
        Ipc::Handlers ih;
        ih.preload = [&](const std::string& p) { preloader.preloadOne(p); };
        ih.set = [&](const std::string& mon, int ws, const std::string& path) {
            if (!store.setWallpaper(mon, ws, path))
                return false;
            std::string out = path;
            if (isMediaWallpaper(out)) {
                backend.hideMonitor(mon);
                std::string expanded;
                ConfigStore::expandPath(out, expanded);
                MediaDelegate::playOnMonitor(mon, expanded.empty() ? out : expanded);
            } else {
                preloader.preloadOne(out);
                auto img = preloader.get(out);
                if (img) {
                    MediaDelegate::stopForMonitor(mon);
                    backend.setWallpaper(mon, *img);
                }
            }
            MediaDelegate::applyTheme(out);
            return true;
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
        Ipc::serve(std::move(ih));
    });
    ipcThread.detach();

    std::cout << "Listening for workspace changes...\n";
    listener.run(); // blocks until socket EOF
    return 0;
}
