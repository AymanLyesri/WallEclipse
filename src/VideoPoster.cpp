#include "walleclipse/VideoPoster.hpp"
#include "walleclipse/Util.hpp"

#include <cstdlib>
#include <filesystem>
#include <sstream>
#include <unistd.h>

std::string VideoPoster::cacheDir() { return cacheBaseDir() + "/walleclipse/posters"; }

std::string VideoPoster::cachePathFor(const std::string& videoPath,
                                      const std::string& dir) {
    std::ostringstream oss;
    oss << std::hex << std::hash<std::string>{}(videoPath);
    return dir + "/" + oss.str() + ".jpg";
}

std::shared_ptr<DecodedImage> VideoPoster::load(const std::string& videoPath) {
    if (videoPath.empty() || !isMediaWallpaper(videoPath))
        return nullptr;
    std::string dir = cacheDir();
    std::error_code ec;
    std::filesystem::create_directories(dir, ec);
    std::string out = cachePathFor(videoPath, dir);

    if (auto cached = loadImage(out))
        return std::make_shared<DecodedImage>(std::move(*cached));

    // Extract to a temp file, then atomically rename (parallel
    // preload workers may race on the same video path).
    std::string tmp = out + ".tmp." + std::to_string(getpid());
    if (!runWait({"ffmpegthumbnailer", "-i", videoPath, "-o", tmp, "-s", "0",
                  "-q", "5"}) &&
        !runWait({"ffmpeg", "-y", "-loglevel", "error", "-i", videoPath,
                  "-vframes", "1", "-q:v", "3", tmp})) {
        std::filesystem::remove(tmp, ec);
        return nullptr;
    }
    std::filesystem::rename(tmp, out, ec);
    auto img = loadImage(out);
    return img ? std::make_shared<DecodedImage>(std::move(*img)) : nullptr;
}
