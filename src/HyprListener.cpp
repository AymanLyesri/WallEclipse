#include "walleclipse/HyprListener.hpp"
#include "walleclipse/ImageLoader.hpp"

#include <array>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <memory>
#include <sys/socket.h>
#include <sys/un.h>
#include <thread>
#include <unistd.h>

namespace {

std::string execCapture(const char* cmd) {
    std::string out;
    std::unique_ptr<FILE, decltype(&pclose)> pipe(popen(cmd, "r"), pclose);
    if (!pipe)
        return out;
    std::array<char, 4096> buf{};
    size_t n = 0;
    while ((n = std::fread(buf.data(), 1, buf.size(), pipe.get())) > 0)
        out.append(buf.data(), n);
    return out;
}

} // namespace

HyprListener::HyprListener(Handlers h) : h_(std::move(h)) {}

std::vector<std::pair<std::string, int>>
HyprListener::parseSnapshotLines(const std::string& text) {
    std::vector<std::pair<std::string, int>> out;
    size_t pos = 0;
    while (pos < text.size()) {
        size_t end = text.find('\n', pos);
        std::string line = text.substr(pos, end == std::string::npos ? end : end - pos);
        if (end == std::string::npos)
            pos = text.size();
        else
            pos = end + 1;
        size_t s = line.find_first_not_of(" \t\r");
        if (s == std::string::npos)
            continue;
        line = line.substr(s);
        auto sp = line.find_last_of(' ');
        if (sp == std::string::npos)
            continue;
        std::string name = line.substr(0, sp);
        if (name.empty() || name.find("special") != std::string::npos)
            continue;
        try {
            int ws = std::stoi(line.substr(sp + 1));
            out.emplace_back(name, ws);
        } catch (...) {
        }
    }
    return out;
}

bool HyprListener::isRefreshEvent(const std::string& line) {
    return line.find("workspace") != std::string::npos ||
           line.find("focusedmon") != std::string::npos;
}

void HyprListener::handleRefresh() {
    std::string text = execCapture("hyprctl monitors -j | jq -r '.[] | \"\\(.name) \\(.activeWorkspace.id)\"'");
    for (auto& [monitor, ws] : parseSnapshotLines(text)) {
        auto lw = lastWorkspace_.find(monitor);
        if (lw != lastWorkspace_.end() && lw->second == ws)
            continue; // unchanged workspace

        std::string path;
        if (!h_.lookup(monitor, ws, path)) {
            if (h_.onInfo)
                h_.onInfo("change_wallpaper", "No wallpaper config for '" + monitor +
                                                   "' workspace " + std::to_string(ws));
            lastWorkspace_[monitor] = ws;
            continue;
        }
        if (path.empty()) { // empty slot: expected, keep previous
            lastWorkspace_[monitor] = ws;
            continue;
        }
        auto cw = currentWallpaper_.find(monitor);
        if (cw != currentWallpaper_.end() && cw->second == path) {
            lastWorkspace_[monitor] = ws;
            continue; // same image, new workspace id
        }
        if (h_.showStatic && h_.showAnimated) {
            if (isMediaWallpaper(path))
                h_.showAnimated(monitor, path);
            else
                h_.showStatic(monitor, path);
        }
        currentWallpaper_[monitor] = path;
        lastWorkspace_[monitor] = ws;
    }
}

void HyprListener::applyCurrent() {
    lastWorkspace_.clear();
    currentWallpaper_.clear();
    handleRefresh();
}

void HyprListener::run() {
    applyCurrent();

    const char* runt = std::getenv("XDG_RUNTIME_DIR");
    const char* sig = std::getenv("HYPRLAND_INSTANCE_SIGNATURE");
    if (!runt || !sig) {
        if (h_.onError)
            h_.onError("socket", "XDG_RUNTIME_DIR/HYPRLAND_INSTANCE_SIGNATURE not set");
        return;
    }
    std::string sockPath = std::string(runt) + "/hypr/" + sig + "/.socket2.sock";

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        if (h_.onError)
            h_.onError("socket", std::string("socket() failed: ") + std::strerror(errno));
        return;
    }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, sockPath.c_str(), sizeof(addr.sun_path) - 1);
    if (connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        if (h_.onError)
            h_.onError("socket", "connect " + sockPath + ": " + std::strerror(errno));
        close(fd);
        return;
    }

    FILE* fp = fdopen(fd, "r");
    if (!fp) {
        if (h_.onError)
            h_.onError("socket", "fdopen failed");
        close(fd);
        return;
    }

    char* line = nullptr;
    size_t cap = 0;
    ssize_t n = 0;
    auto lastRun = std::chrono::steady_clock::now() - std::chrono::seconds(10);
    while ((n = getline(&line, &cap, fp)) > 0) {
        std::string ev(line, static_cast<size_t>(n));
        if (!isRefreshEvent(ev))
            continue;
        auto now = std::chrono::steady_clock::now();
        auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(now - lastRun);
        if (elapsed.count() < 100)
            std::this_thread::sleep_for(std::chrono::milliseconds(100 - elapsed.count()));
        lastRun = std::chrono::steady_clock::now();
        handleRefresh();
    }
    free(line);
    fclose(fp); // also closes fd
}
