#pragma once

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <vector>

// Wallpaper transition effects. Open for future effects via makeTransition().
// Sliding direction derives from workspace order: higher workspace id =
// Forward (new wallpaper enters from the right), lower = Backward.
enum class SlideDir { Forward, Backward, None };

// Pure helper: direction for a workspace switch. nullopt `from` (first
// show) or equal ids -> None (instant, no slide).
SlideDir slideDirForWorkspaces(std::optional<int> from, int to);

class TransitionEffect {
public:
    virtual ~TransitionEffect() = default;
    // Total animation time. Mirrors Hyprland `workspaces ... slide`
    // (see ~/.config/hypr/config/animations.lua).
    virtual int durationMs() const = 0;
    // Eased progress for linear t in [0,1]. Mirrors Hyprland `ease`
    // bezier (fast start, gentle landing).
    virtual double eased(double t) const = 0;
    // Composite one frame. Buffers are ARGB8888 (w*h*4, same layout as
    // scaleCoverArgb output). progress is linear [0,1], eased internally.
    virtual std::vector<uint8_t> render(const std::vector<uint8_t>& oldFrame,
                                        const std::vector<uint8_t>& newFrame, int w,
                                        int h, double progress,
                                        SlideDir dir) const = 0;
};

// Factory: "slide" (directional), "none" (instant cut).
// Returns nullptr for unknown names. Add future effects here.
std::unique_ptr<TransitionEffect> makeTransition(const std::string& name);
