// ImageLoader + HyprListener pure-logic tests (no compositor needed).
#include <cassert>
#include <cstdio>
#include <iostream>
#include <vector>

#include "walleclipse/HyprListener.hpp"
#include "walleclipse/ImageLoader.hpp"

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
    // Media classification (parity with wallpaper-loop.c is_media_wallpaper).
    CHECK(isMediaWallpaper("/a/b.mp4"));
    CHECK(isMediaWallpaper("/a/b.MP4"));
    CHECK(isMediaWallpaper("/a/b.webm"));
    CHECK(isMediaWallpaper("/a/b.gif"));
    CHECK(!isMediaWallpaper("/a/b.jpg"));
    CHECK(!isMediaWallpaper("/a/b.png"));
    CHECK(!isMediaWallpaper("/a/b.webp"));
    CHECK(!isMediaWallpaper("/a/b"));
    CHECK(!isMediaWallpaper(""));

    // Snapshot parsing: blanks, special workspaces skipped.
    std::string snap = "DP-2 3\nHDMI-1 1\n\nspecial 5\nBADLINE\n";
    auto pairs = HyprListener::parseSnapshotLines(snap);
    CHECK(pairs.size() == 2);
    CHECK(pairs[0].first == "DP-2" && pairs[0].second == 3);
    CHECK(pairs[1].first == "HDMI-1" && pairs[1].second == 1);

    // Event filter: workspace substring (incl. moveworkspace/workspacev2)
    // and focusedmon trigger; everything else ignored.
    CHECK(HyprListener::isRefreshEvent("workspace>>3"));
    CHECK(HyprListener::isRefreshEvent("moveworkspace>>1,DP-2"));
    CHECK(HyprListener::isRefreshEvent("workspacev2>>3,addr"));
    CHECK(HyprListener::isRefreshEvent("focusedmon>>DP-2,1"));
    CHECK(!HyprListener::isRefreshEvent("activewindow>>class,title"));
    CHECK(!HyprListener::isRefreshEvent(""));

    // SwitchAction decision matrix (regression: animated -> empty slot
    // must restore last static instead of black-screening).
    using SA = HyprListener::SwitchAction;
    auto dec = HyprListener::decide;
    std::optional<std::string> none;
    // Empty/missing slot after static: keep persisting buffer.
    CHECK(dec(std::string("/a.jpg"), std::string(""), std::string("/a.jpg")) == SA::Skip);
    CHECK(dec(std::string("/a.jpg"), none, std::string("/a.jpg")) == SA::Skip);
    // Empty/missing slot after animated: restore last static.
    CHECK(dec(std::string("/v.mp4"), std::string(""), std::string("/a.jpg")) == SA::RestoreStatic);
    CHECK(dec(std::string("/v.mp4"), none, std::string("/a.jpg")) == SA::RestoreStatic);
    // Animated showing, empty slot, no static ever: nothing to restore.
    CHECK(dec(std::string("/v.mp4"), std::string(""), none) == SA::Skip);
    // Fresh boot, empty slot: nothing to show.
    CHECK(dec(none, std::string(""), none) == SA::Skip);
    // Normal switches.
    CHECK(dec(std::string("/a.jpg"), std::string("/b.jpg"), std::string("/a.jpg")) == SA::ShowStatic);
    CHECK(dec(std::string("/a.jpg"), std::string("/a.jpg"), std::string("/a.jpg")) == SA::Skip);
    CHECK(dec(std::string("/a.jpg"), std::string("/v.mp4"), std::string("/a.jpg")) == SA::ShowAnimated);
    CHECK(dec(std::string("/v.mp4"), std::string("/v.mp4"), std::string("/a.jpg")) == SA::Skip);
    CHECK(dec(std::string("/v.mp4"), std::string("/a.jpg"), std::string("/a.jpg")) == SA::ShowStatic);
    CHECK(dec(none, std::string("/a.jpg"), none) == SA::ShowStatic);
    CHECK(dec(none, std::string("/v.mp4"), none) == SA::ShowAnimated);

    // Missing file decodes to nullopt (daemon must keep old wallpaper).
    CHECK(!loadImage("/nonexistent/walleclipse-test.jpg").has_value());
    // Synthetic 2x1 red/green image scales to cover 4x4 without crashing,
    // output is ARGB8888 opaque.
    DecodedImage tiny;
    tiny.width = 2;
    tiny.height = 1;
    tiny.rgba = {255, 0, 0, 255, 0, 255, 0, 255};
    auto scaled = scaleCoverArgb(tiny, 4, 4);
    CHECK(scaled.size() == 4 * 4 * 4);
    CHECK(scaled[3] == 0xFF);

    if (failures == 0)
        std::cout << "test_image: all passed\n";
    return failures == 0 ? 0 : 1;
}
