#include "walleclipse/Util.hpp"

#include <array>
#include <cstdio>
#include <cstdlib>
#include <fcntl.h>
#include <memory>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

std::string cacheBaseDir() {
    if (const char* c = std::getenv("XDG_CACHE_HOME"); c && *c)
        return c;
    const char* h = std::getenv("HOME");
    return std::string(h ? h : "/tmp") + "/.cache";
}

std::string expandHome(const std::string& in) {
    const char* h = std::getenv("HOME");
    std::string home = h ? h : "";
    if (!in.empty() && in[0] == '~')
        return home + in.substr(1);
    if (in.rfind("$HOME", 0) == 0)
        return home + in.substr(5);
    return in;
}

std::string trimWs(const std::string& s) {
    size_t a = s.find_first_not_of(" \t\r\n");
    if (a == std::string::npos)
        return "";
    size_t b = s.find_last_not_of(" \t\r\n");
    return s.substr(a, b - a + 1);
}

std::vector<std::string> splitLines(const std::string& s) {
    std::vector<std::string> out;
    size_t pos = 0;
    while (pos <= s.size()) {
        size_t end = s.find('\n', pos);
        if (end == std::string::npos) {
            out.push_back(s.substr(pos));
            break;
        }
        out.push_back(s.substr(pos, end - pos));
        pos = end + 1;
    }
    return out;
}

std::string execCapture(const char* cmd) {
    std::string out;
    struct Closer {
        void operator()(FILE* fp) const { pclose(fp); }
    };
    std::unique_ptr<FILE, Closer> pipe(popen(cmd, "r"));
    if (!pipe)
        return out;
    std::array<char, 4096> buf{};
    size_t n = 0;
    while ((n = std::fread(buf.data(), 1, buf.size(), pipe.get())) > 0)
        out.append(buf.data(), n);
    return out;
}

static void silenceChild() {
    int devnull = open("/dev/null", O_WRONLY);
    if (devnull >= 0) {
        dup2(devnull, STDOUT_FILENO);
        dup2(devnull, STDERR_FILENO);
        if (devnull > STDERR_FILENO)
            close(devnull);
    }
}

static std::vector<char*> toArgv(std::vector<std::string>& args) {
    std::vector<char*> argv;
    for (auto& a : args)
        argv.push_back(a.data());
    argv.push_back(nullptr);
    return argv;
}

bool runWait(const std::vector<std::string>& args) {
    pid_t pid = fork();
    if (pid < 0)
        return false;
    if (pid == 0) {
        std::vector<std::string> mut = args;
        auto argv = toArgv(mut);
        silenceChild();
        execvp(argv[0], argv.data());
        _exit(127);
    }
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

void spawnDetached(std::vector<std::string> args) {
    pid_t pid = fork();
    if (pid < 0)
        return;
    if (pid == 0) {
        if (fork() == 0) {
            auto argv = toArgv(args);
            silenceChild();
            execvp(argv[0], argv.data());
            _exit(127);
        }
        _exit(0);
    }
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
    }
}

std::string readAllFd(int fd) {
    std::string out;
    char buf[4096];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0)
        out.append(buf, static_cast<size_t>(n));
    return out;
}

bool writeAllFd(int fd, const std::string& s) {
    size_t off = 0;
    while (off < s.size()) {
        ssize_t n = write(fd, s.data() + off, s.size() - off);
        if (n <= 0)
            return false;
        off += static_cast<size_t>(n);
    }
    return true;
}
