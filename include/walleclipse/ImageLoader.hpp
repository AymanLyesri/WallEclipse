#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <vector>

struct DecodedImage {
    int width = 0;
    int height = 0;
    std::vector<uint8_t> rgba; // width*height*4, RGBA byte order
};

// Decode jpg/png (stb_image) and webp (libwebp) into 8-bit RGBA.
// Returns nullopt when the file is missing or undecodable — callers
// keep the previous wallpaper up and log, never black-screen.
std::optional<DecodedImage> loadImage(const std::string& path);

// Animated wallpapers stay on mpvpaper (parity with wallpaper-loop.c).
bool isMediaWallpaper(const std::string& path);

// Cover-fit scale of src into dstW x dstH (ARGB8888, premultiplied off).
// Used to fill SHM buffers sized to the output mode.
std::vector<uint8_t> scaleCoverArgb(const DecodedImage& src, int dstW, int dstH);
