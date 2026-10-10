#pragma once

#include <memory>
#include <string>

#include "walleclipse/ImageLoader.hpp"

// Poster bridge for animated wallpapers (gif/mp4/webm): mpvpaper owns
// the video surface, so transitions slide to/from a static poster frame
// (first/representative frame) instead of blending video frames.
//
// Disk cache: ~/.cache/walleclipse/posters/<hash>.jpg, extracted once
// with ffmpegthumbnailer (ffmpeg fallback), then decoded with the
// existing loadImage() (jpg). Failures return nullptr — callers fall
// back to the previous instant cut.
struct VideoPoster {
    // Directory for cached posters (XDG_CACHE_HOME or ~/.cache).
    static std::string cacheDir();
    // Deterministic cache file for a video path under cacheDir.
    static std::string cachePathFor(const std::string& videoPath,
                                    const std::string& dir);
    // Load (or extract + load) the poster for a video path.
    // Returns nullptr when the path is not media or extraction fails.
    static std::shared_ptr<DecodedImage> load(const std::string& videoPath);
};
