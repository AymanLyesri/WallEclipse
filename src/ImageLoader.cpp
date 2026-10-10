#include "walleclipse/ImageLoader.hpp"

#include <algorithm>
#include <cctype>
#include <cmath>
#include <cstdio>
#include <cstring>

// stb_image implements jpg/png (and bmp/tga) decoders in one header.
#define STB_IMAGE_IMPLEMENTATION
#define STBI_ONLY_JPEG
#define STBI_ONLY_PNG
#define STBI_NO_STDIO  // we read the file ourselves for better errors
#include "stb_image.h"

#include <webp/decode.h>

namespace {

std::string lowerExt(const std::string& path) {
    auto dot = path.find_last_of('.');
    if (dot == std::string::npos || dot + 2 >= path.size())
        return "";
    std::string got = path.substr(dot + 1);
    std::transform(got.begin(), got.end(), got.begin(),
                   [](unsigned char c) { return std::tolower(c); });
    return got;
}

std::optional<std::vector<uint8_t>> readFile(const std::string& path) {
    FILE* fp = std::fopen(path.c_str(), "rb");
    if (!fp)
        return std::nullopt;
    std::fseek(fp, 0, SEEK_END);
    long size = std::ftell(fp);
    std::fseek(fp, 0, SEEK_SET);
    if (size <= 0 || size > (128 << 20)) { // 128MB sanity cap
        std::fclose(fp);
        return std::nullopt;
    }
    std::vector<uint8_t> data(static_cast<size_t>(size));
    bool ok = std::fread(data.data(), 1, data.size(), fp) == data.size();
    std::fclose(fp);
    if (!ok)
        return std::nullopt;
    return data;
}

bool validSize(int w, int h) { return w > 0 && h > 0 && w <= 16384 && h <= 16384; }

} // namespace

bool isMediaWallpaper(const std::string& path) {
    auto e = lowerExt(path);
    return e == "gif" || e == "mp4" || e == "webm";
}

std::optional<DecodedImage> loadImage(const std::string& path) {
    auto file = readFile(path);
    if (!file)
        return std::nullopt;

    int w = 0, h = 0;
    const uint8_t* px = nullptr;
    bool webp = lowerExt(path) == "webp";
    stbi_uc* stb = nullptr;
    uint8_t* wp = nullptr;
    if (webp)
        wp = WebPDecodeRGBA(file->data(), file->size(), &w, &h);
    else {
        int comp = 0;
        stb = stbi_load_from_memory(file->data(), static_cast<int>(file->size()),
                                    &w, &h, &comp, 4);
    }
    px = webp ? wp : stb;
    if (!px || !validSize(w, h)) {
        if (stb)
            stbi_image_free(stb);
        if (wp)
            WebPFree(wp);
        return std::nullopt;
    }
    DecodedImage img{w, h, {px, px + static_cast<size_t>(w) * h * 4}};
    if (stb)
        stbi_image_free(stb);
    if (wp)
        WebPFree(wp);
    return img;
}

std::vector<uint8_t> scaleCoverArgb(const DecodedImage& src, int dstW, int dstH) {
    std::vector<uint8_t> dst(static_cast<size_t>(dstW) * dstH * 4, 0);
    if (src.width <= 0 || src.height <= 0 || dstW <= 0 || dstH <= 0)
        return dst;

    // Cover-fit: scale so the image covers dst, center-crop the overflow.
    double s = std::max(double(dstW) / src.width, double(dstH) / src.height);
    double srcW = dstW / s, srcH = dstH / s;
    double ox = (src.width - srcW) / 2.0, oy = (src.height - srcH) / 2.0;

    for (int y = 0; y < dstH; ++y) {
        double sy = oy + (y + 0.5) / s;
        int y0 = static_cast<int>(std::floor(sy));
        double fy = sy - y0;
        for (int x = 0; x < dstW; ++x) {
            double sx = ox + (x + 0.5) / s;
            int x0 = static_cast<int>(std::floor(sx));
            double fx = sx - x0;
            auto samp = [&](int ix, int iy) -> const uint8_t* {
                ix = std::clamp(ix, 0, src.width - 1);
                iy = std::clamp(iy, 0, src.height - 1);
                return &src.rgba[(static_cast<size_t>(iy) * src.width + ix) * 4];
            };
            const uint8_t* p00 = samp(x0, y0);
            const uint8_t* p10 = samp(x0 + 1, y0);
            const uint8_t* p01 = samp(x0, y0 + 1);
            const uint8_t* p11 = samp(x0 + 1, y0 + 1);
            uint8_t* o = &dst[(static_cast<size_t>(y) * dstW + x) * 4];
            for (int c = 0; c < 4; ++c) {
                double v = p00[c] * (1 - fx) * (1 - fy) + p10[c] * fx * (1 - fy) +
                           p01[c] * (1 - fx) * fy + p11[c] * fx * fy;
                o[c] = static_cast<uint8_t>(std::lround(v));
            }
            // wl_shm ARGB8888 is little-endian: store XRGB order in memory
            // as B,G,R,A. Convert RGBA -> ARGB (opaque wallpapers: A=255).
            uint8_t r = o[0], g = o[1], b = o[2];
            o[0] = b;
            o[1] = g;
            o[2] = r;
            o[3] = 0xFF;
        }
    }
    return dst;
}
