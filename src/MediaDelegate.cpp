#include "walleclipse/MediaDelegate.hpp"
#include "walleclipse/Util.hpp"

#include <chrono>
#include <cstdlib>
#include <dirent.h>
#include <fcntl.h>
#include <signal.h>
#include <thread>
#include <unistd.h>

namespace {

std::string procRead(pid_t pid, const char* file) {
    std::string path = "/proc/" + std::to_string(pid) + file;
    int fd = ::open(path.c_str(), O_RDONLY);
    if (fd < 0)
        return "";
    char buf[4096];
    ssize_t n = ::read(fd, buf, sizeof(buf) - 1);
    ::close(fd);
    return n <= 0 ? "" : std::string(buf, static_cast<size_t>(n));
}

bool isMpvpaper(pid_t pid) {
    std::string comm = procRead(pid, "/comm");
    while (!comm.empty() && (comm.back() == '\n' || comm.back() == '\0'))
        comm.pop_back();
    return comm == "mpvpaper";
}

} // namespace

void MediaDelegate::stopForMonitor(const std::string& monitor) {
    DIR* proc = opendir("/proc");
    if (!proc)
        return;
    while (dirent* e = readdir(proc)) {
        char* end = nullptr;
        long pid = std::strtol(e->d_name, &end, 10);
        if (!end || *end != '\0' || pid <= 0)
            continue;
        if (isMpvpaper(static_cast<pid_t>(pid)) &&
            procRead(static_cast<pid_t>(pid), "/cmdline").find(monitor) !=
                std::string::npos)
            kill(static_cast<pid_t>(pid), SIGTERM);
    }
    closedir(proc);
}

bool MediaDelegate::playOnMonitor(const std::string& monitor,
                                  const std::string& path) {
    stopForMonitor(monitor);
    spawnDetached({"mpvpaper", "-o",
                   "no-audio --loop --fs --panscan=1.0 --hwdec=auto-safe",
                   monitor, path});
    return true;
}

void MediaDelegate::applyTheme(const std::string& wallpaper) {
    const char* home = std::getenv("HOME");
    std::string script =
        std::string(home ? home : "") + "/.config/hypr/theme/scripts/wal-theme.sh";
    if (access(script.c_str(), X_OK) != 0)
        return;
    spawnDetached({script, wallpaper});
}

bool MediaDelegate::foreignSurfaceListed(const std::string& jqOutput) {
    for (auto line : splitLines(jqOutput)) {
        line = trimWs(line);
        if (!line.empty() && line != "walleclipse")
            return true;
    }
    return false;
}

bool MediaDelegate::waitForPlayer(const std::string& monitor, int timeoutMs) {
    // Monitor names come from hyprctl (DP-2, HDMI-A-1); refuse anything
    // that would break the single-quoted shell embedding below.
    if (monitor.empty() || monitor.find('\'') != std::string::npos)
        return false;
    auto deadline =
        std::chrono::steady_clock::now() + std::chrono::milliseconds(timeoutMs);
    std::string cmd = "hyprctl layers -j 2>/dev/null | jq -r --arg m '" + monitor +
                      "' '.[$m].levels.\"0\"[].namespace // empty' 2>/dev/null";
    while (std::chrono::steady_clock::now() < deadline) {
        if (foreignSurfaceListed(execCapture(cmd.c_str()))) {
            // Surface exists but mpv may not have attached its first
            // frame yet: hold our poster one beat longer so the handoff
            // never shows a blank frame in between.
            std::this_thread::sleep_for(std::chrono::milliseconds(400));
            return true;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }
    return false;
}
