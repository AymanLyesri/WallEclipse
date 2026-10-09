#pragma once

#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "walleclipse/ImageLoader.hpp"
#include "walleclipse/Transition.hpp"

// Direct Wayland backend: one wlr-layer-shell background surface per
// output, SHM buffers scaled to the output mode. Replaces hyprpaper's
// `hyprctl hyprpaper wallpaper` round-trip — switches are a local
// buffer attach since images are preloaded by Preloader.
class WaylandBackend {
public:
    using LogFn = std::function<void(const std::string&)>;

    WaylandBackend();
    ~WaylandBackend();

    bool init(LogFn log = nullptr);
    void shutdown();

    // Currently known output (monitor) names via xdg-output.
    std::vector<std::string> outputs();

    // Show a decoded image fullscreen on a monitor (cover-fit).
    // Returns false when the monitor is unknown or not yet configured.
    // The image is attached immediately when the surface is configured,
    // otherwise it pends until configure arrives (never attaches early:
    // that is a fatal protocol error).
    // The SlideDir overload animates via the active TransitionEffect
    // (default "slide"); SlideDir::None (and the 2-arg overload) attaches
    // instantly. Direction comes from workspace order (see Transition.hpp).
    bool setWallpaper(const std::string& monitor, const DecodedImage& img);
    bool setWallpaper(const std::string& monitor, std::shared_ptr<const DecodedImage> img);
    bool setWallpaper(const std::string& monitor, std::shared_ptr<const DecodedImage> img,
                      SlideDir dir);

    // Swap the transition effect ("slide", "none", ...). Unknown names are
    // ignored. Future effects plug in via makeTransition().
    void setTransition(const std::string& name);
    std::string transition() const;

    // Scale + cache a wallpaper for a monitor without showing it. Warms the
    // cover-fit cache at startup so the first visit to any workspace slides
    // instantly instead of paying ~90ms mid-switch.
    void precache(const std::string& monitor, std::shared_ptr<const DecodedImage> img);

    // Unmap the layer surface (used when mpvpaper takes over audio/video).
    void hideMonitor(const std::string& monitor);

    bool running() const;

    struct Impl;

private:
    std::unique_ptr<Impl> impl_;
};
