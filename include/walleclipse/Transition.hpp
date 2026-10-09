#pragma once

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include "walleclipse/ImageLoader.hpp"

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

// Cover-fit result cache: scaling a multi-megapixel source costs ~90ms at
// 1080p, so scale once per (source, output size) and reuse on every switch.
// Keyed by shared_ptr identity (never dereferenced: no lifetime risk).
// Not thread-safe; the owner holds its own lock. Caps at kMax entries, then
// drops everything (configs have ~10 slots, so the cap is never hit in
// practice — it only bounds pathological IPC `set` spam).
class ScaledCache {
public:
    static constexpr size_t kMax = 12;

    const std::vector<uint8_t>* find(const std::shared_ptr<const DecodedImage>& src, int w,
                                     int h) const;
    void store(std::shared_ptr<const DecodedImage> src, int w, int h,
               std::vector<uint8_t> argb);
    void clear();
    size_t size() const;

private:
    struct Entry {
        std::shared_ptr<const DecodedImage> src;
        int w = 0, h = 0;
        std::vector<uint8_t> argb;
    };
    std::vector<Entry> entries_;
};
