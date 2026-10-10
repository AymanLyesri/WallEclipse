#include "walleclipse/VideoPoster.hpp"

#include <cstdio>
#include <cstdlib>
#include <fcntl.h>
#include <filesystem>
#include <sstream>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

std::string hashHex(const std::string& s) {
    size_t h = std::hash<std::string>{}(s);
    std::ostringstream oss;
    oss << std::hex << h;
    return oss.str();
}

// fork+exec (no shell): paths may contain spaces/quotes.
bool runCmd(const std::vector<std::string>& args) {
    pid_t pid = fork();
    if (pid < 0)
        return false;
    if (pid == 0) {
        std::vector<char*> argv;
        for (auto& a : args)
            argv.push_back(const_cast<char*>(a.c_str()));
        argv.push_back(nullptr);
        // Silence child output; daemon logs stay clean.
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
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

} // namespace

std::string VideoPoster::cacheDir() {
    const char* xdg = std::getenv("XDG_CACHE_HOME");
    std::string base;
    if (xdg && *xdg) {
        base = xdg;
    } else {
        const char* home = std::getenv("HOME");
        base = std::string(home ? home : "/tmp") + "/.cache";
    }
    return base + "/walleclipse/posters";
}

std::string VideoPoster::cachePathFor(const std::string& videoPath,
                                      const std::string& dir) {
    return dir + "/" + hashHex(videoPath) + ".jpg";
}

std::shared_ptr<DecodedImage> VideoPoster::load(const std::string& videoPath) {
    if (videoPath.empty() || !isMediaWallpaper(videoPath))
        return nullptr;
    std::string dir = cacheDir();
    std::error_code ec;
    std::filesystem::create_directories(dir, ec);
    std::string out = cachePathFor(videoPath, dir);

    auto cached = loadImage(out);
    if (cached)
        return std::make_shared<DecodedImage>(std::move(*cached));

    // Extract to a temp file, then atomically rename (parallel
    // preload workers may race on the same video path).
    std::string tmp = out + ".tmp." + std::to_string(getpid());
    bool ok = runCmd({"ffmpegthumbnailer", "-i", videoPath, "-o", tmp, "-s", "0", "-q", "5"});
    if (!ok)
        ok = runCmd({"ffmpeg", "-y", "-loglevel", "error", "-i", videoPath,
                     "-vframes", "1", "-q:v", "3", tmp});
    if (!ok) {
        std::filesystem::remove(tmp, ec);
        return nullptr;
    }
    std::filesystem::rename(tmp, out, ec);
    auto img = loadImage(out);
    if (!img)
        return nullptr;
    return std::make_shared<DecodedImage>(std::move(*img));
}
