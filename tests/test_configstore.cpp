// ConfigStore unit tests (no Wayland / no compositor needed).
#include <cassert>
#include <filesystem>
#include <fstream>
#include <iostream>

#include "walleclipse/ConfigStore.hpp"

namespace fs = std::filesystem;

static int failures = 0;
#define CHECK(cond)                                              \
    do {                                                         \
        if (!(cond)) {                                           \
            std::cerr << "FAIL line " << __LINE__ << ": " #cond  \
                      << "\n";                                   \
            ++failures;                                          \
        }                                                        \
    } while (0)

int main() {
    std::string tmp = std::string(std::getenv("TMPDIR") ? std::getenv("TMPDIR") : "/tmp") +
                      "/walleclipse-test-cfg";
    fs::remove_all(tmp);

    ConfigStore store(tmp + "/config");
    store.ensureMonitors({"DP-2"});

    // Seeded template has 10 empty slots.
    auto wps = store.wallpapersFor("DP-2");
    CHECK(wps.size() == 10);
    auto w1 = store.getWallpaper("DP-2", 1);
    CHECK(w1.has_value() && w1->empty());

    // Missing workspace key -> nullopt.
    CHECK(!store.getWallpaper("DP-2", 99).has_value());

    // Write + read back.
    CHECK(store.setWallpaper("DP-2", 3, "/a/b.jpg"));
    auto got = store.getWallpaper("DP-2", 3);
    CHECK(got.has_value() && *got == "/a/b.jpg");

    // Path with spaces survives round-trip.
    CHECK(store.setWallpaper("DP-2", 4, "/a/b c/d.png"));
    got = store.getWallpaper("DP-2", 4);
    CHECK(got.has_value() && *got == "/a/b c/d.png");

    // Unknown monitor dir is created on write.
    CHECK(store.setWallpaper("HDMI-1", 1, "/x.jpg"));
    got = store.getWallpaper("HDMI-1", 1);
    CHECK(got.has_value() && *got == "/x.jpg");

    // Tilde expansion.
    std::string out;
    ConfigStore::expandPath("~/wall.jpg", out);
    CHECK(out == std::string(std::getenv("HOME")) + "/wall.jpg");

    fs::remove_all(tmp);
    if (failures == 0)
        std::cout << "test_configstore: all passed\n";
    return failures == 0 ? 0 : 1;
}
