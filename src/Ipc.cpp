#include "walleclipse/Ipc.hpp"
#include "walleclipse/Util.hpp"

#include <algorithm>
#include <cctype>
#include <cstring>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

std::string Ipc::socketPath() { return cacheBaseDir() + "/walleclipse.sock"; }

namespace {

// Split "SET <mon> <ws> <path...>": path may contain spaces but not
// newlines (parity with ConfigStore; newlines in paths are rejected).
bool parseSet(const std::string& line, std::string& mon, int& ws,
              std::string& path) {
    // line starts with "SET "
    size_t b = line.find(' ', 4);
    if (b == std::string::npos)
        return false;
    size_t c = line.find(' ', b + 1);
    if (c == std::string::npos)
        return false;
    mon = line.substr(4, b - 4);
    try {
        ws = std::stoi(line.substr(b + 1, c - b - 1));
    } catch (...) {
        return false;
    }
    path = trimWs(line.substr(c + 1));
    return !mon.empty() && !path.empty() && path.find('\n') == std::string::npos;
}

std::string upperOf(const std::string& s) {
    std::string u = s;
    std::transform(u.begin(), u.end(), u.begin(),
                   [](unsigned char c) { return std::toupper(c); });
    return u;
}

} // namespace

bool Ipc::serve(Handlers h) {
    std::string spath = socketPath();
    ::unlink(spath.c_str());

    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    if (srv < 0)
        return false;
    // Spawned helpers (mpvpaper, wal-theme) must not inherit IPC fds:
    // an inherited client socket delays the client's EOF until the
    // helper exits (wal-theme on video takes seconds).
    fcntl(srv, F_SETFD, FD_CLOEXEC);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, spath.c_str(), sizeof(addr.sun_path) - 1);
    if (bind(srv, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0 ||
        listen(srv, 8) < 0) {
        close(srv);
        return false;
    }

    bool stop = false;
    while (!stop) {
        int cli = accept(srv, nullptr, nullptr);
        if (cli < 0)
            continue;
        fcntl(cli, F_SETFD, FD_CLOEXEC);
        std::string line = trimWs(splitLines(readAllFd(cli))[0]);
        std::string reply = "ERR unknown command\n";

        if (line.rfind("PRELOAD ", 0) == 0) {
            if (h.preload)
                h.preload(trimWs(line.substr(8)));
            reply = "OK\n";
        } else if (line.rfind("SET ", 0) == 0) {
            std::string mon, path;
            int ws = 0;
            if (!parseSet(line, mon, ws, path))
                reply = "ERR bad SET syntax (SET <monitor> <ws> <path>)\n";
            else if (h.set)
                reply = h.set(mon, ws, path) ? "OK\n" : "ERR set failed\n";
            else
                reply = "ERR set failed\n";
        } else if (upperOf(line) == "CURRENT") {
            reply = h.current ? h.current() : "\n";
        } else if (upperOf(line) == "LIST") {
            reply = h.list ? h.list() : "\n";
        } else if (upperOf(line) == "RELOAD") {
            if (h.reload)
                h.reload();
            reply = "OK\n";
        } else if (upperOf(line) == "QUIT") {
            reply = "OK\n";
            stop = true;
            if (h.quit)
                h.quit();
        }
        writeAllFd(cli, reply);
        close(cli);
    }
    close(srv);
    ::unlink(spath.c_str());
    return true;
}

std::string Ipc::call(const std::string& payload) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0)
        return "ERR no socket\n";
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, socketPath().c_str(), sizeof(addr.sun_path) - 1);
    if (connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        close(fd);
        return "ERR daemon not running\n";
    }
    std::string msg = payload;
    if (msg.empty() || msg.back() != '\n')
        msg += '\n';
    writeAllFd(fd, msg);
    shutdown(fd, SHUT_WR);
    std::string reply = readAllFd(fd);
    close(fd);
    return reply;
}
