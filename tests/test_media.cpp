// MediaDelegate unit tests: layer-surface readiness detection.
// Pure logic, no compositor needed (liveness checked via canned
// `hyprctl layers` jq output, not a running mpvpaper).
#include <iostream>
#include <string>

#include "walleclipse/MediaDelegate.hpp"

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
    // Empty query output: player not up.
    CHECK(!MediaDelegate::foreignSurfaceListed(""));
    CHECK(!MediaDelegate::foreignSurfaceListed("\n"));
    // Only our own poster surface: still not ready (don't hide yet).
    CHECK(!MediaDelegate::foreignSurfaceListed("walleclipse\n"));
    // Foreign background surface (mpvpaper, any namespace): ready.
    CHECK(MediaDelegate::foreignSurfaceListed("mpvpaper\n"));
    CHECK(MediaDelegate::foreignSurfaceListed("walleclipse\nmpvpaper\n"));
    CHECK(MediaDelegate::foreignSurfaceListed("mpvpaper-holder\n"));
    // Blank lines tolerated.
    CHECK(MediaDelegate::foreignSurfaceListed("\nwalleclipse\n\nmpvpaper\n"));

    if (failures == 0)
        std::cout << "test_media: all passed\n";
    return failures == 0 ? 0 : 1;
}
