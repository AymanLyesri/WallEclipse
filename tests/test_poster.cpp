// VideoPoster unit tests: poster disk-cache path derivation + in-memory cache.
// Pure logic, no compositor or ffmpeg needed.
#include <iostream>
#include <string>

#include "walleclipse/VideoPoster.hpp"
#include "walleclipse/Preloader.hpp"

static int failures = 0;
#define CHECK(cond)                                             \
    do {                                                        \
        if (!(cond)) {                                          \
            std::cerr << "FAIL line " << __LINE__ << ": " #cond \
                      << "\n";                                  \
            ++failures;                                         \
        }                                                       \
    } while (0)

int main() {
    // Cache path is deterministic per video path.
    std::string a1 = VideoPoster::cachePathFor("/v/a.mp4", "/cache");
    std::string a2 = VideoPoster::cachePathFor("/v/a.mp4", "/cache");
    std::string b = VideoPoster::cachePathFor("/v/b.mp4", "/cache");
    CHECK(!a1.empty());
    CHECK(a1 == a2);
    CHECK(a1 != b);
    CHECK(a1.rfind("/cache/", 0) == 0);
    CHECK(a1.size() > 4 && a1.substr(a1.size() - 4) == ".jpg");

    // Different extensions, same stem -> different entries (content differs).
    std::string g = VideoPoster::cachePathFor("/v/a.gif", "/cache");
    CHECK(g != a1);

    // Poster cache: miss returns nullptr, no crash on unknown paths.
    {
        Preloader p(1);
        CHECK(p.getPoster("/v/nonexistent.mp4") == nullptr);
    }

    // Poster load failures stay null (never crash the switch path):
    // non-media rejected, missing video file extracts to nothing.
    CHECK(VideoPoster::load("/a/b.jpg") == nullptr);
    CHECK(VideoPoster::load("") == nullptr);
    CHECK(VideoPoster::load("/nonexistent/walleclipse-poster.mp4") == nullptr);

    if (failures == 0)
        std::cout << "test_poster: all passed\n";
    return failures == 0 ? 0 : 1;
}
