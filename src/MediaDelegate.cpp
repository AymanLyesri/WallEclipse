#include "walleclipse/MediaDelegate.hpp"

#include <array>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <dirent.h>
#include <fcntl.h>
#include <memory>
#include <signal.h>
#include <string>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace {

bool cmdlineMentions(pid_t pid, const std::string& needle) {
    std::string path = "/proc/" + std::to_string(pid) + "/cmdline";
    int fd = ::open(path.c_str(), O_RDONLY);
    if (fd < 0)
        return false;
    char buf[4096];
    ssize_t n = ::read(fd, buf, sizeof(buf) - 1);
    ::close(fd);
    if (n <= 0)
        return false;
    std::string cmd(buf, static_cast<size_t>(n));
    // cmdline is NUL-separated; needle match across args is fine.
    return cmd.find(needle) != std::string::npos;
}

bool isMpvpaper(pid_t pid) {
    char buf[64] = {0};
    std::string commPath = "/proc/" + std::to_string(pid) + "/comm";
    int fd = ::open(commPath.c_str(), O_RDONLY);
    if (fd < 0)
        return false;
    ssize_t n = ::read(fd, buf, sizeof(buf) - 1);
    ::close(fd);
    if (n <= 0)
        return false;
    std::string comm(buf, static_cast<size_t>(n));
    while (!comm.empty() && (comm.back() == '\n' || comm.back() == '\0'))
        comm.pop_back();
    return comm == "mpvpaper";
}

// Double-fork detach: grandchild reparented to init, no zombies,
// no SIGCHLD games (same pattern as wallpaper-loop.c spawn_detached).
void spawnDetached(std::vector<char*> argv) {
    argv.push_back(nullptr);
    pid_t pid = fork();
    if (pid < 0)
        return;
    if (pid == 0) {
        if (fork() == 0) {
            int devnull = open("/dev/null", O_WRONLY);
            if (devnull >= 0) {
                dup2(devnull, STDOUT_FILENO);
                dup2(devnull, STDERR_FILENO);
                if (devnull > STDERR_FILENO)
                    close(devnull);
            }
            execvp(argv[0], argv.data());
            _exit(127);
        }
        _exit(0);
    }
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
    }
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
            cmdlineMentions(static_cast<pid_t>(pid), monitor)) {
            kill(static_cast<pid_t>(pid), SIGTERM);
        }
    }
    closedir(proc);
}

bool MediaDelegate::playOnMonitor(const std::string& monitor, const std::string& path) {
    stopForMonitor(monitor);
    std::string m = monitor, p = path;
    std::string opts = "no-audio --loop --fs --panscan=1.0 --hwdec=auto-safe";
    std::vector<char*> argv = {const_cast<char*>("mpvpaper"),
                               const_cast<char*>("-o"), const_cast<char*>(opts.data()),
                               const_cast<char*>(m.data()), const_cast<char*>(p.data())};
    // Note: opts/m/p must outlive exec — they do (exec copies before return).
    spawnDetached(argv);
    return true;
}

void MediaDelegate::applyTheme(const std::string& wallpaper) {
    const char* home = std::getenv("HOME");
    std::string script = std::string(home ? home : "") + "/.config/hypr/theme/scripts/wal-theme.sh";
    if (access(script.c_str(), X_OK) != 0)
        return;
    std::string w = wallpaper;
    std::vector<char*> argv = {const_cast<char*>(script.data()),
                               const_cast<char*>(w.data())};
    spawnDetached(argv);
}

bool MediaDelegate::foreignSurfaceListed(const std::string& jqOutput) {
    size_t pos = 0;
    while (pos < jqOutput.size()) {
        size_t end = jqOutput.find('\n', pos);
        std::string line = jqOutput.substr(pos, end == std::string::npos ? end : end - pos);
        if (end == std::string::npos)
            pos = jqOutput.size();
        else
            pos = end + 1;
        size_t s = line.find_first_not_of(" \t\r");
        if (s == std::string::npos)
            continue;
        size_t e = line.find_last_not_of(" \t\r");
        line = line.substr(s, e - s + 1);
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
    while (std::chrono::steady_clock::now() < deadline) {
        std::string cmd = "hyprctl layers -j 2>/dev/null | jq -r --arg m '" + monitor +
                          "' '.[$m].levels.\"0\"[].namespace // empty' 2>/dev/null";
        std::string out;
        std::unique_ptr<FILE, decltype(&pclose)> pipe(popen(cmd.c_str(), "r"), pclose);
        if (pipe) {
            std::array<char, 256> buf{};
            size_t n = 0;
            while ((n = std::fread(buf.data(), 1, buf.size(), pipe.get())) > 0)
                out.append(buf.data(), n);
        }
        if (foreignSurfaceListed(out)) {
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
