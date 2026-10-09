#include "walleclipse/Transition.hpp"

#include <algorithm>
#include <cmath>
#include <cstring>

SlideDir slideDirForWorkspaces(std::optional<int> from, int to) {
    if (!from.has_value() || *from == to)
        return SlideDir::None;
    return to > *from ? SlideDir::Forward : SlideDir::Backward;
}

namespace {

// Mirrors Hyprland workspaces slide duration. animations.lua sets
// `speed = phi * 2 (~3.24)` with the `ease` bezier; Hyprland maps higher
// speed to shorter time (~300-400ms for the default slide feel).
constexpr int kSlideDurationMs = 350;

class SlidingTransition : public TransitionEffect {
public:
    int durationMs() const override {
        return kSlideDurationMs;
    }
    double eased(double t) const override {
        t = std::clamp(t, 0.0, 1.0);
        // easeOutCubic: 1-(1-t)^3. Fast start like Hyprland `ease`
        // bezier (0,1 -> 0.618,1), gentle landing. Endpoints exact.
        if (t <= 0.0)
            return 0.0;
        if (t >= 1.0)
            return 1.0;
        return 1.0 - std::pow(1.0 - t, 3.0);
    }
    std::vector<uint8_t> render(const std::vector<uint8_t>& oldFrame,
                                const std::vector<uint8_t>& newFrame, int w, int h,
                                double progress, SlideDir dir) const override {
        if (dir == SlideDir::None || progress >= 1.0)
            return newFrame;
        if (progress <= 0.0)
            return oldFrame;
        double e = eased(progress);
        if (e <= 0.0)
            return oldFrame;
        if (e >= 1.0)
            return newFrame;
        int split = static_cast<int>(e * w);
        if (split <= 0)
            return oldFrame;
        if (split >= w)
            return newFrame;
        std::vector<uint8_t> out(oldFrame.size());
        if (oldFrame.size() != newFrame.size() ||
            oldFrame.size() != static_cast<size_t>(w) * h * 4)
            return newFrame;
        // Row-based composite: each row is one contiguous old run + one
        // contiguous new run (2 memcpys/row, not per-pixel).
        size_t rowBytes = static_cast<size_t>(w) * 4;
        size_t newBytes = static_cast<size_t>(split) * 4;
        size_t oldBytes = rowBytes - newBytes;
        for (int y = 0; y < h; ++y) {
            const uint8_t* oRow = oldFrame.data() + static_cast<size_t>(y) * rowBytes;
            const uint8_t* nRow = newFrame.data() + static_cast<size_t>(y) * rowBytes;
            uint8_t* dRow = out.data() + static_cast<size_t>(y) * rowBytes;
            if (dir == SlideDir::Backward) { // new enters from the left
                std::memcpy(dRow, nRow, newBytes);
                std::memcpy(dRow + newBytes, oRow + newBytes, oldBytes);
            } else { // new enters from the right
                std::memcpy(dRow, oRow, oldBytes);
                std::memcpy(dRow + oldBytes, nRow + oldBytes, newBytes);
            }
        }
        return out;
    }
};

class NoneTransition : public TransitionEffect {
public:
    int durationMs() const override {
        return 0;
    }
    double eased(double t) const override {
        return std::clamp(t, 0.0, 1.0);
    }
    std::vector<uint8_t> render(const std::vector<uint8_t>&,
                                const std::vector<uint8_t>& newFrame, int, int,
                                double, SlideDir) const override {
        return newFrame;
    }
};

} // namespace

std::unique_ptr<TransitionEffect> makeTransition(const std::string& name) {
    if (name == "slide")
        return std::make_unique<SlidingTransition>();
    if (name == "none")
        return std::make_unique<NoneTransition>();
    return nullptr;
}
