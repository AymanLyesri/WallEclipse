#include "walleclipse/Ipc.hpp"

#include <algorithm>
#include <cctype>
#include <cstdlib>
#include <cstring>
#include <string>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <vector>

std::string Ipc::socketPath() {
    const char* cache = std::getenv("XDG_CACHE_HOME");
    std::string base;
    if (cache && cache[0] != '\0') {
        base = cache;
    } else {
        const char* home = std::getenv("HOME");
        base = std::string(home ? home : "/tmp") + "/.cache";
    }
    return base + "/walleclipse.sock";
}

namespace {

std::string readAll(int fd) {
    std::string out;
    char buf[4096];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0)
        out.append(buf, static_cast<size_t>(n));
    return out;
}

// Split "SET <mon> <ws> <path...>": path may contain spaces but not
// newlines (parity with ConfigStore; newlines in paths are rejected).
bool parseSet(const std::string& line, std::string& mon, int& ws, std::string& path) {
    // line starts with "SET "
    size_t a = 4;
    size_t b = line.find(' ', a);
    if (b == std::string::npos)
        return false;
    size_t c = line.find(' ', b + 1);
    if (c == std::string::npos)
        return false;
    mon = line.substr(a, b - a);
    try {
        ws = std::stoi(line.substr(b + 1, c - b - 1));
    } catch (...) {
        return false;
    }
    path = line.substr(c + 1);
    while (!path.empty() && (path.back() == '\n' || path.back() == '\r'))
        path.pop_back();
    return !mon.empty() && !path.empty() && path.find('\n') == std::string::npos;
}

} // namespace

bool Ipc::serve(Handlers h) {
    std::string spath = socketPath();
    ::unlink(spath.c_str());

    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    if (srv < 0)
        return false;
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, spath.c_str(), sizeof(addr.sun_path) - 1);
    if (bind(srv, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        close(srv);
        return false;
    }
    if (listen(srv, 8) < 0) {
        close(srv);
        return false;
    }

    bool stop = false;
    while (!stop) {
        int cli = accept(srv, nullptr, nullptr);
        if (cli < 0)
            continue;
        std::string req = readAll(cli);
        // First line is the command; reject embedded NULs implicitly via find.
        size_t nl = req.find('\n');
        std::string line = req.substr(0, nl);
        while (!line.empty() && (line.back() == '\r' || line.back() == ' '))
            line.pop_back();
        std::string upper = line;
        std::transform(upper.begin(), upper.end(), upper.begin(),
                       [](unsigned char c) { return std::toupper(c); });
        std::string reply = "ERR unknown command\n";

        if (line.rfind("PRELOAD ", 0) == 0) {
            std::string p = line.substr(8);
            if (h.preload)
                h.preload(p);
            reply = "OK\n";
        } else if (line.rfind("SET ", 0) == 0) {
            std::string mon, path;
            int ws = 0;
            if (parseSet(line, mon, ws, path) && h.set) {
                reply = h.set(mon, ws, path) ? "OK\n" : "ERR set failed\n";
            } else {
                reply = "ERR bad SET syntax (SET <monitor> <ws> <path>)\n";
            }
        } else if (line == "CURRENT" || line == "CURRENT\n" || upper == "CURRENT") {
            reply = h.current ? h.current() : "\n";
        } else if (line == "LIST" || line == "LIST\n" || upper == "LIST") {
            reply = h.list ? h.list() : "\n";
        } else if (upper == "RELOAD") {
            if (h.reload)
                h.reload();
            reply = "OK\n";
        } else if (line == "QUIT" || line == "QUIT\n" || upper == "QUIT") {
            reply = "OK\n";
            stop = true;
            if (h.quit)
                h.quit();
        }
        size_t off = 0;
        while (off < reply.size()) {
            ssize_t n = write(cli, reply.data() + off, reply.size() - off);
            if (n <= 0)
                break;
            off += static_cast<size_t>(n);
        }
        close(cli);
    }
    close(srv);
    ::unlink(spath.c_str());
    return true;
}

std::string Ipc::call(const std::string& payload) {
    std::string spath = socketPath();
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0)
        return "ERR no socket\n";
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, spath.c_str(), sizeof(addr.sun_path) - 1);
    if (connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        close(fd);
        return "ERR daemon not running\n";
    }
    std::string msg = payload;
    if (msg.empty() || msg.back() != '\n')
        msg += '\n';
    size_t off = 0;
    while (off < msg.size()) {
        ssize_t n = write(fd, msg.data() + off, msg.size() - off);
        if (n <= 0)
            break;
        off += static_cast<size_t>(n);
    }
    shutdown(fd, SHUT_WR);
    std::string reply = readAll(fd);
    close(fd);
    return reply;
}
