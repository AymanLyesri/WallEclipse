// Transition unit tests: sliding direction from workspace order + frame compositing.
// Pure logic, no compositor needed.
#include <iostream>
#include <vector>

#include "walleclipse/Transition.hpp"
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

static std::vector<uint8_t> solid(int w, int h, uint8_t r, uint8_t g, uint8_t b) {
    std::vector<uint8_t> v(static_cast<size_t>(w) * h * 4);
    for (size_t i = 0; i < v.size(); i += 4) {
        v[i] = b;
        v[i + 1] = g;
        v[i + 2] = r;
        v[i + 3] = 0xFF;
    }
    return v;
}

int main() {
    // Direction from workspace order: higher id = forward (new from right).
    CHECK(slideDirForWorkspaces(1, 2) == SlideDir::Forward);
    CHECK(slideDirForWorkspaces(1, 3) == SlideDir::Forward);
    CHECK(slideDirForWorkspaces(3, 1) == SlideDir::Backward);
    CHECK(slideDirForWorkspaces(2, 2) == SlideDir::None);
    CHECK(slideDirForWorkspaces(std::nullopt, 1) == SlideDir::None);

    // Factory is open for future effects.
    auto slide = makeTransition("slide");
    CHECK(slide != nullptr);
    CHECK(makeTransition("nope") == nullptr);
    auto none = makeTransition("none");
    CHECK(none != nullptr);

    // Duration mirrors Hyprland workspaces anim (positive, shared default).
    CHECK(slide->durationMs() > 0);
    CHECK(slide->durationMs() == makeTransition("slide")->durationMs());

    // Easing: endpoints exact, monotonic, ease-out (fast start).
    CHECK(slide->eased(0.0) == 0.0);
    CHECK(slide->eased(1.0) == 1.0);
    double eHalf = slide->eased(0.5);
    CHECK(eHalf > 0.5 && eHalf < 1.0);
    CHECK(slide->eased(0.25) < eHalf && eHalf < slide->eased(0.75));

    // Frame compositing: 4x1 old=red, new=blue.
    int w = 4, h = 1;
    auto oldF = solid(w, h, 255, 0, 0);
    auto newF = solid(w, h, 0, 0, 255);
    // progress 0 -> all old, progress 1 -> all new.
    auto f0 = slide->render(oldF, newF, w, h, 0.0, SlideDir::Forward);
    auto f1 = slide->render(oldF, newF, w, h, 1.0, SlideDir::Forward);
    CHECK(f0 == oldF);
    CHECK(f1 == newF);
    // progress 0.5 forward (eased offset ~ middle): left half old, right half new.
    // Use linear probe via eased value to avoid hardcoding curve.
    double e = slide->eased(0.5);
    int split = static_cast<int>(e * w); // columns of new visible from right
    auto fMid = slide->render(oldF, newF, w, h, 0.5, SlideDir::Forward);
    CHECK(fMid.size() == oldF.size());
    for (int x = 0; x < w; ++x) {
        const uint8_t* px = &fMid[static_cast<size_t>(x) * 4];
        bool isNew = x >= w - split;
        if (isNew) {
            CHECK(px[0] == 255 && px[1] == 0 && px[2] == 0); // blue in BGR mem
        } else {
            CHECK(px[0] == 0 && px[1] == 0 && px[2] == 255); // red in BGR mem
        }
    }
    // Backward mirrors: new enters from left.
    auto fMidB = slide->render(oldF, newF, w, h, 0.5, SlideDir::Backward);
    for (int x = 0; x < w; ++x) {
        const uint8_t* px = &fMidB[static_cast<size_t>(x) * 4];
        bool isNew = x < split;
        if (isNew) {
            CHECK(px[0] == 255 && px[1] == 0 && px[2] == 0);
        } else {
            CHECK(px[0] == 0 && px[1] == 0 && px[2] == 255);
        }
    }
    // None direction = instant cut to new.
    auto fNone = slide->render(oldF, newF, w, h, 0.5, SlideDir::None);
    CHECK(fNone == newF);

    // ScaledCache: cover-fit results cached per (source, size), no compositor.
    {
        ScaledCache cache;
        auto src = std::make_shared<DecodedImage>();
        src->width = 2;
        src->height = 1;
        src->rgba = {255, 0, 0, 255, 0, 255, 0, 255};
        CHECK(cache.find(src, 4, 4) == nullptr); // cold miss
        auto argb = scaleCoverArgb(*src, 4, 4);
        cache.store(src, 4, 4, argb);
        auto hit = cache.find(src, 4, 4);
        CHECK(hit != nullptr && *hit == argb);
        CHECK(cache.find(src, 8, 8) == nullptr); // size mismatch = miss
        auto other = std::make_shared<DecodedImage>(*src);
        CHECK(cache.find(other, 4, 4) == nullptr); // different source = miss
        CHECK(cache.size() == 1);
        cache.clear();
        CHECK(cache.size() == 0 && cache.find(src, 4, 4) == nullptr);
    }

    if (failures == 0)
        std::cout << "test_transition: all passed\n";
    return failures == 0 ? 0 : 1;
}
