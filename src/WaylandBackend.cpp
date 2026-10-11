#include "walleclipse/WaylandBackend.hpp"

#include <chrono>
#include <cstring>
#include <fcntl.h>
#include <sys/mman.h>
#include <thread>
#include <unistd.h>
#include <wayland-client.h>

// wayland-scanner emits `namespace` as a parameter name in the C header,
// which is a keyword in C++. Rename it around the include (standard hack).
#define namespace ns_placeholder
#include "wlr-layer-shell-unstable-v1-client.h"
#undef namespace
#include "xdg-output-unstable-v1-client.h"

struct OutputInfo {
    WaylandBackend::Impl* impl = nullptr;
    wl_output* output = nullptr;
    uint32_t global = 0; // wl_registry global name for hotplug removal
    uint32_t wlVersion = 0; // bound wl_output version (release iff >= 3)
    zxdg_output_v1* xdg = nullptr;
    std::string name;
    int32_t width = 0;   // from mode (physical px)
    int32_t height = 0;
    int32_t scale = 1;

    wl_surface* surface = nullptr;
    zwlr_layer_surface_v1* layer = nullptr;
    uint32_t configureSerial = 0;
    bool configured = false;
    int cfgW = 0, cfgH = 0;

    // Image waiting for (or surviving) configure. Layer-shell forbids
    // attaching a buffer before the first acked configure, and a `closed`
    // event (e.g. `hyprctl reload`) unmaps us — either way the pending
    // image is (re-)attached when configure arrives, never before.
    std::shared_ptr<const DecodedImage> pending;

    // False until a buffer is successfully attached. Attaching nil
    // (hideMonitor) unmaps the surface: per protocol it returns to the
    // post-get_layer_surface state, so re-showing needs an empty commit
    // + configure + attach, not a direct attach (that's a fatal error).
    bool mapped = false;

    wl_buffer* buffer = nullptr;
    int bufW = 0, bufH = 0;

    // Persistent double-buffered SHM pools: allocated once per output size,
    // reused every frame. Per-frame mkstemp/mmap/munmap caused the visible
    // stutter (page-table churn on 8MB per frame at 1080p). Pool memory stays
    // mapped until resize/close, so a commit never outlives its storage.
    struct Pool {
        int fd = -1;
        void* data = nullptr;
        size_t size = 0;
        wl_shm_pool* pool = nullptr;
    };
    Pool pools[2];
    int poolIdx = 0;

    // Last presented frame (ARGB8888, bufW x bufH) for transitions.
    std::vector<uint8_t> lastFrame;
    // Cover-fit results per (source, output size): scaling costs ~90ms at
    // 1080p, paid once here instead of on every switch.
    ScaledCache scaled;
    // Bumps on every setWallpaper; animation loops abort when stale.
    uint64_t generation = 0;
};

struct WaylandBackend::Impl {
    WaylandBackend::LogFn log;
    wl_display* display = nullptr;
    wl_registry* registry = nullptr;
    wl_compositor* compositor = nullptr;
    wl_shm* shm = nullptr;
    zwlr_layer_shell_v1* layerShell = nullptr;
    zxdg_output_manager_v1* xdgOutMgr = nullptr;
    std::map<wl_output*, std::unique_ptr<OutputInfo>> outputs;
    std::map<uint32_t, wl_output*> globalToOutput;
    // Pending images stashed by monitor name when their wl_output global
    // is removed (sleep/disconnect). Restored when the monitor re-appears
    // (xdg name event) so the wallpaper self-heals even if the workspace
    // is unchanged and no setWallpaper arrives.
    std::map<std::string, std::shared_ptr<const DecodedImage>> pendingByMonitor;
    // Retired outputs whose proxies are destroyed but whose listener
    // userdata may still fire from already-queued Wayland events. Kept
    // alive (detached, impl=null) so those late callbacks are safe no-ops
    // instead of use-after-free. Bounded below.
    std::vector<std::unique_ptr<OutputInfo>> dead;
    std::mutex mutex;
    std::thread dispatchThread;
    bool running = false;
    std::string transitionName = "slide";

    void vlog(const std::string& m) {
        if (log)
            log(m);
    }
};

namespace {

// Forward declarations (defined below createShmFile).
// Call with out->impl->mutex held.
void attachLocked(OutputInfo* out);
void attachPixelsLocked(OutputInfo* out, const std::vector<uint8_t>& argb, int w, int h);
// (Re)create the layer surface + initial empty commit. True when a layer
// surface exists afterwards (buffer still waits for configure).
bool ensureSurfaceLocked(WaylandBackend::Impl* impl, OutputInfo* out);
static OutputInfo* findOutput(WaylandBackend::Impl* impl, const std::string& monitor);
static void outSize(OutputInfo* out, int& w, int& h);

void layerConfigure(void* data, zwlr_layer_surface_v1* layer, uint32_t serial,
                     uint32_t width, uint32_t height) {
    auto* out = static_cast<OutputInfo*>(data);
    zwlr_layer_surface_v1_ack_configure(layer, serial);
    if (!out->impl)
        return;
    std::lock_guard<std::mutex> lock(out->impl->mutex);
    out->configureSerial = serial;
    out->configured = true;
    out->cfgW = static_cast<int>(width);
    out->cfgH = static_cast<int>(height);
    if (out->pending)
        attachLocked(out);
}

void destroyPoolsLocked(OutputInfo* out) {
    for (auto& p : out->pools) {
        if (p.pool) {
            wl_shm_pool_destroy(p.pool);
            p.pool = nullptr;
        }
        if (p.data) {
            munmap(p.data, p.size);
            p.data = nullptr;
        }
        if (p.fd >= 0) {
            close(p.fd);
            p.fd = -1;
        }
        p.size = 0;
    }
}

// Call with impl->mutex held. Tears down the layer surface/buffer/pools
// but keeps pending so a later configure or setWallpaper re-attaches.
void teardownSurfaceLocked(OutputInfo* out) {
    out->configured = false;
    out->mapped = false;
    ++out->generation; // cancel any in-flight slide
    if (out->layer) {
        zwlr_layer_surface_v1_destroy(out->layer);
        out->layer = nullptr;
    }
    if (out->surface) {
        wl_surface_destroy(out->surface);
        out->surface = nullptr;
    }
    if (out->buffer) {
        wl_buffer_destroy(out->buffer);
        out->buffer = nullptr;
    }
    destroyPoolsLocked(out);
}

void layerClosed(void* data, zwlr_layer_surface_v1* /*layer*/) {
    auto* out = static_cast<OutputInfo*>(data);
    if (!out->impl)
        return;
    // Surface is gone (reload/reconfigure/sleep): tear down so the next
    // setWallpaper recreates it and waits for configure before attaching.
    std::lock_guard<std::mutex> lock(out->impl->mutex);
    teardownSurfaceLocked(out);
    // Visible in [wayland] logs so the next blank-screen report shows
    // whether the compositor closed us (sleep/DPMS/reload) and we are
    // waiting on a repaint. pending survives: the next configure or
    // setWallpaper re-attaches it.
    if (out->impl)
        out->impl->vlog("layer surface closed, will repaint on next setWallpaper/configure");
}

const zwlr_layer_surface_v1_listener kLayerListener = {layerConfigure, layerClosed};

void outputGeometry(void*, wl_output*, int32_t, int32_t, int32_t, int32_t, int32_t,
                    const char*, const char*, int32_t) {}
void outputMode(void* data, wl_output*, uint32_t, int32_t w, int32_t h, int32_t) {
    auto* out = static_cast<OutputInfo*>(data);
    // Preferred mode wins; wl_output sends current+preferred with flags,
    // but taking the last announced mode matches swaybg behaviour closely
    // enough for fullscreen wallpapers (configure size is authoritative).
    if (w > 0 && h > 0) {
        out->width = w;
        out->height = h;
    }
}
void outputDone(void*, wl_output*) {}
void outputScale(void* data, wl_output*, int32_t s) {
    static_cast<OutputInfo*>(data)->scale = s > 0 ? s : 1;
}
void outputName(void*, wl_output*, const char*) {}
void outputDescription(void*, wl_output*, const char*) {}
const wl_output_listener kOutputListener = {outputGeometry, outputMode, outputDone,
                                            outputScale, outputName,
                                            outputDescription};

void xdgName(void* data, zxdg_output_v1*, const char* name) {
    if (!name)
        return;
    auto* out = static_cast<OutputInfo*>(data);
    if (!out->impl)
        return;
    std::lock_guard<std::mutex> lock(out->impl->mutex);
    out->name = name;
    // Monitor (re)appeared after sleep/hotplug: restore the stashed image
    // and eagerly recreate the layer surface so the wallpaper self-heals
    // even when the workspace is unchanged (listener would skip repaint).
    // The configure handler attaches pending; never attach here (fatal
    // before first acked configure).
    auto* impl = out->impl;
    auto it = impl->pendingByMonitor.find(out->name);
    if (it != impl->pendingByMonitor.end() && it->second) {
        if (!out->pending)
            out->pending = it->second;
        if (impl->compositor && impl->layerShell && out->output && !out->layer) {
            if (ensureSurfaceLocked(impl, out))
                impl->vlog("output '" + out->name +
                           "' reconnected, restoring wallpaper");
        }
    }
}
void xdgLogicalPosition(void*, zxdg_output_v1*, int32_t, int32_t) {}
void xdgLogicalSize(void*, zxdg_output_v1*, int32_t, int32_t) {}
void xdgDone(void*, zxdg_output_v1*) {}
void xdgDescription(void*, zxdg_output_v1*, const char*) {}
const zxdg_output_v1_listener kXdgListener = {xdgLogicalPosition, xdgLogicalSize,
                                              xdgDone, xdgName, xdgDescription};

void registryGlobal(void* data, wl_registry* registry, uint32_t name,
                    const char* interface, uint32_t version) {
    auto* self = static_cast<WaylandBackend::Impl*>(data);
    if (std::strcmp(interface, wl_compositor_interface.name) == 0) {
        self->compositor = static_cast<wl_compositor*>(
            wl_registry_bind(registry, name, &wl_compositor_interface, 4));
    } else if (std::strcmp(interface, wl_shm_interface.name) == 0) {
        self->shm = static_cast<wl_shm*>(
            wl_registry_bind(registry, name, &wl_shm_interface, 1));
    } else if (std::strcmp(interface, wl_output_interface.name) == 0) {
        uint32_t v = std::min(version, 3u);
        wl_output* o = static_cast<wl_output*>(
            wl_registry_bind(registry, name, &wl_output_interface, v));
        auto info = std::make_unique<OutputInfo>();
        info->impl = self;
        info->output = o;
        info->global = name;
        info->wlVersion = v;
        wl_output_add_listener(o, &kOutputListener, info.get());
        if (self->xdgOutMgr) {
            info->xdg = zxdg_output_manager_v1_get_xdg_output(self->xdgOutMgr, o);
            zxdg_output_v1_add_listener(info->xdg, &kXdgListener, info.get());
        }
        {
            std::lock_guard<std::mutex> lock(self->mutex);
            self->globalToOutput[name] = o;
            self->outputs[o] = std::move(info);
        }
        self->vlog("output global " + std::to_string(name) + " added");
    } else if (std::strcmp(interface, zwlr_layer_shell_v1_interface.name) == 0) {
        self->layerShell = static_cast<zwlr_layer_shell_v1*>(wl_registry_bind(
            registry, name, &zwlr_layer_shell_v1_interface, 4));
    } else if (std::strcmp(interface, zxdg_output_manager_v1_interface.name) == 0) {
        self->xdgOutMgr = static_cast<zxdg_output_manager_v1*>(wl_registry_bind(
            registry, name, &zxdg_output_manager_v1_interface, 3));
    }
}

void registryRemove(void* data, wl_registry*, uint32_t name) {
    // The compositor retires the wl_output proxy: any layer surface anchored
    // to it is dead and will never configure again. Destroy the OutputInfo
    // now (stashing pending by monitor name) so a later findOutput cannot
    // return the dead entry and paint into the void (black screen). The
    // re-added global creates a fresh OutputInfo; xdgName restores pending.
    auto* self = static_cast<WaylandBackend::Impl*>(data);
    if (!self)
        return;
    self->vlog("output global " + std::to_string(name) +
               " removed (hotplug/sleep?)");
    std::lock_guard<std::mutex> lock(self->mutex);
    auto git = self->globalToOutput.find(name);
    if (git == self->globalToOutput.end())
        return; // not an output global (compositor/shm/...)
    wl_output* dead = git->second;
    self->globalToOutput.erase(git);
    auto oit = self->outputs.find(dead);
    if (oit == self->outputs.end())
        return;
    OutputInfo* out = oit->second.get();
    if (!out->name.empty() && out->pending)
        self->pendingByMonitor[out->name] = out->pending;
    teardownSurfaceLocked(out);
    if (out->xdg) {
        zxdg_output_v1_destroy(out->xdg);
        out->xdg = nullptr;
    }
    // wl_output.release exists since v3 and destroys the proxy; older
    // binds fall back to a plain proxy destroy.
    if (out->wlVersion >= 3)
        wl_output_release(dead);
    else
        wl_output_destroy(dead);
    out->output = nullptr;
    out->impl = nullptr; // detach: late compositor events become no-ops
    // Retire instead of freeing: already-queued events may still carry
    // the dead OutputInfo* as userdata. Bounded graveyard, negligible size
    // (pools/surfaces already destroyed).
    self->dead.push_back(std::move(oit->second));
    self->outputs.erase(oit);
    while (self->dead.size() > 16)
        self->dead.erase(self->dead.begin());
    // Wake the display so a racing roundtrip notices the removal promptly.
    if (self->display)
        wl_display_flush(self->display);
}

const wl_registry_listener kRegistryListener = {registryGlobal, registryRemove};

int createShmFile(size_t size) {
    const char* runt = std::getenv("XDG_RUNTIME_DIR");
    std::string dir = runt ? runt : "/tmp";
    std::string tmpl = dir + "/walleclipse-shm-XXXXXX";
    std::vector<char> buf(tmpl.begin(), tmpl.end());
    buf.push_back('\0');
    int fd = mkstemp(buf.data());
    if (fd < 0)
        return -1;
    unlink(buf.data());
    if (ftruncate(fd, static_cast<off_t>(size)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

void attachPixelsLocked(OutputInfo* out, const std::vector<uint8_t>& argb, int w,
                          int h) {
    if (!out->surface || !out->layer || !out->configured || !out->impl ||
        !out->impl->shm || !out->impl->display)
        return;
    if (w <= 0 || h <= 0)
        return;
    if (argb.size() != static_cast<size_t>(w) * h * 4)
        return;
    size_t stride = static_cast<size_t>(w) * 4;
    size_t size = stride * h;

    // (Re)allocate the pool pair on size change; otherwise reuse mapped
    // memory — no mkstemp/mmap/munmap on the frame path.
    if (out->pools[0].size != size || out->pools[0].data == nullptr) {
        destroyPoolsLocked(out);
        for (auto& p : out->pools) {
            int fd = createShmFile(size);
            if (fd < 0) {
                destroyPoolsLocked(out);
                return;
            }
            void* data =
                mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            if (data == MAP_FAILED) {
                close(fd);
                destroyPoolsLocked(out);
                return;
            }
            wl_shm_pool* pool = wl_shm_create_pool(out->impl->shm, fd,
                                                   static_cast<int32_t>(size));
            if (!pool) {
                munmap(data, size);
                close(fd);
                destroyPoolsLocked(out);
                return;
            }
            p.fd = fd;
            p.data = data;
            p.size = size;
            p.pool = pool;
        }
        out->poolIdx = 0;
    }

    auto& slot = out->pools[out->poolIdx++ % 2];
    std::memcpy(slot.data, argb.data(), size);

    wl_buffer* buffer = wl_shm_pool_create_buffer(slot.pool, 0, w, h,
                                                  static_cast<int32_t>(stride),
                                                  WL_SHM_FORMAT_ARGB8888);
    if (!buffer)
        return;

    wl_surface_attach(out->surface, buffer, 0, 0);
    wl_surface_damage_buffer(out->surface, 0, 0, w, h);
    wl_surface_commit(out->surface);
    wl_display_flush(out->impl->display);

    if (out->buffer)
        wl_buffer_destroy(out->buffer);
    out->buffer = buffer;
    out->bufW = w;
    out->bufH = h;
    out->mapped = true;
    out->lastFrame = argb;
}

void attachLocked(OutputInfo* out) {
    if (!out->surface || !out->layer || !out->configured || !out->pending ||
        !out->impl || !out->impl->shm || !out->impl->display)
        return;

    int w = 0, h = 0;
    outSize(out, w, h);

    // Configure path: cache is usually warm (startup precache + switch path
    // stores). Inline scale on miss only — rare, keeps this simple.
    if (const auto* hit = out->scaled.find(out->pending, w, h)) {
        attachPixelsLocked(out, *hit, w, h);
        return;
    }
    auto argb = scaleCoverArgb(*out->pending, w, h);
    out->scaled.store(out->pending, w, h, argb);
    attachPixelsLocked(out, argb, w, h);
}

bool ensureSurfaceLocked(WaylandBackend::Impl* impl, OutputInfo* out) {
    if (!impl || !impl->compositor || !impl->layerShell || !out->output)
        return false;
    if (out->layer)
        return true;
    out->surface = wl_compositor_create_surface(impl->compositor);
    if (!out->surface)
        return false;
    out->layer = zwlr_layer_shell_v1_get_layer_surface(
        impl->layerShell, out->surface, out->output,
        ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND, "walleclipse");
    if (!out->layer) {
        wl_surface_destroy(out->surface);
        out->surface = nullptr;
        return false;
    }
    zwlr_layer_surface_v1_set_size(out->layer, 0, 0);
    zwlr_layer_surface_v1_set_anchor(
        out->layer, ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP |
                        ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM |
                        ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT |
                        ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
    zwlr_layer_surface_v1_set_exclusive_zone(out->layer, -1);
    zwlr_layer_surface_v1_add_listener(out->layer, &kLayerListener, out);
    out->configured = false;
    // Initial commit WITHOUT a buffer: compositor replies with configure,
    // only then may a buffer be attached.
    wl_surface_commit(out->surface);
    return true;
}


static OutputInfo* findOutput(WaylandBackend::Impl* impl, const std::string& monitor) {
    for (auto& [wlOut, info] : impl->outputs)
        if (info->name == monitor)
            return info.get();
    return nullptr;
}

static void outSize(OutputInfo* out, int& w, int& h) {
    w = out->cfgW > 0 ? out->cfgW : out->width;
    h = out->cfgH > 0 ? out->cfgH : out->height;
    if (w <= 0 || h <= 0) {
        w = 1920;
        h = 1080;
    }
}

// Cover-fit cached per (source, size). Lookup under lock, scale outside it.
static std::vector<uint8_t> coverCached(WaylandBackend::Impl* impl,
                                        const std::string& monitor,
                                        std::shared_ptr<const DecodedImage> img,
                                        int w, int h) {
    {
        std::lock_guard<std::mutex> lock(impl->mutex);
        if (OutputInfo* out = findOutput(impl, monitor))
            if (const auto* hit = out->scaled.find(img, w, h))
                return *hit;
    }
    auto argb = scaleCoverArgb(*img, w, h);
    std::lock_guard<std::mutex> lock(impl->mutex);
    if (OutputInfo* out = findOutput(impl, monitor))
        if (!out->scaled.find(img, w, h))
            out->scaled.store(img, w, h, argb);
    return argb;
}

static bool validImg(const std::shared_ptr<const DecodedImage>& img) {
    return img && img->width > 0 && img->height > 0;
}

} // namespace

WaylandBackend::WaylandBackend() : impl_(std::make_unique<Impl>()) {}
WaylandBackend::~WaylandBackend() {
    shutdown();
}

bool WaylandBackend::init(LogFn log) {
    impl_->log = std::move(log);
    impl_->display = wl_display_connect(nullptr);
    if (!impl_->display) {
        impl_->vlog("wl_display_connect failed (no Wayland display?)");
        return false;
    }
    impl_->registry = wl_display_get_registry(impl_->display);
    wl_registry_add_listener(impl_->registry, &kRegistryListener, impl_.get());
    wl_display_roundtrip(impl_->display);
    // Second roundtrip so xdg-output names arrive before we create surfaces.
    wl_display_roundtrip(impl_->display);

    if (!impl_->compositor || !impl_->shm || !impl_->layerShell) {
        impl_->vlog("compositor missing wl_compositor/wl_shm/layer-shell");
        return false;
    }

    // Create one background layer surface per output (initial empty
    // commit; buffers attach only after configure — see attachLocked).
    {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        for (auto& [wlOut, info] : impl_->outputs)
            ensureSurfaceLocked(impl_.get(), info.get());
    }
    wl_display_roundtrip(impl_->display);

    impl_->running = true;
    impl_->dispatchThread = std::thread([impl = impl_.get()] {
        while (impl->running) {
            if (wl_display_dispatch(impl->display) < 0) {
                // Sleep/DPMS must NOT kill event flow silently: without
                // dispatch, configure/close/global events stop and every
                // later paint pends forever (black screen). Log so the
                // next report shows whether the loop died.
                if (impl->running)
                    impl->vlog("wl_display_dispatch error; event loop stopped");
                break;
            }
        }
    });
    return true;
}

void WaylandBackend::shutdown() {
    if (impl_->running) {
        impl_->running = false;
        // Wake the dispatch thread; full teardown happens on process exit.
    }
    if (impl_->dispatchThread.joinable()) {
        // Best-effort: dispatch thread blocks in poll; detach on shutdown
        // path since the daemon exits right after (avoids wedging exit).
        impl_->dispatchThread.detach();
    }
}

std::vector<std::string> WaylandBackend::outputs() {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    std::vector<std::string> names;
    for (auto& [wlOut, info] : impl_->outputs) {
        names.push_back(info->name.empty() ? "(unnamed)" : info->name);
    }
    return names;
}

bool WaylandBackend::setWallpaper(const std::string& monitor, const DecodedImage& img) {
    return setWallpaper(monitor, std::make_shared<DecodedImage>(img), SlideDir::None);
}

bool WaylandBackend::setWallpaper(const std::string& monitor,
                                  std::shared_ptr<const DecodedImage> img) {
    return setWallpaper(monitor, std::move(img), SlideDir::None);
}

bool WaylandBackend::setWallpaper(const std::string& monitor,
                                  std::shared_ptr<const DecodedImage> img,
                                  SlideDir dir) {
    if (!validImg(img))
        return false;

    int w = 0, h = 0;
    bool wasConfigured = false, wasMapped = false;
    std::vector<uint8_t> oldFrame;
    std::string transName;
    uint64_t myGen = 0;
    {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        OutputInfo* out = findOutput(impl_.get(), monitor);
        if (!out || !ensureSurfaceLocked(impl_.get(), out))
            return false;
        out->pending = img;
        impl_->pendingByMonitor[monitor] = img;
        myGen = ++out->generation;
        wasConfigured = out->configured;
        wasMapped = out->mapped;
        outSize(out, w, h);
        oldFrame = out->lastFrame;
        transName = impl_->transitionName;
    }

    std::vector<uint8_t> newArgb = coverCached(impl_.get(), monitor, img, w, h);

    auto isCurrent = [&] {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        OutputInfo* out = findOutput(impl_.get(), monitor);
        return out && out->generation == myGen;
    };

    // Set/reload/init arrive with None and cross-fade (old->new, or
    // black->new on first paint); workspace switches slide. "none"
    // disables all animation.
    std::string want = effectNameFor(dir, transName);
    auto effect = makeTransition(want);
    bool haveOld = wasMapped && !oldFrame.empty() &&
                   oldFrame.size() == newArgb.size();
    // First paint fades in from black (opaque). Only for never-shown
    // surfaces: a stale frame + unmapped means hidden-for-video, which
    // needs the empty-commit remap below (direct attach is a protocol
    // error after a nil attach).
    std::vector<uint8_t> from = oldFrame;
    bool fadeFromBlack = (dir == SlideDir::None && oldFrame.empty() &&
                          !wasMapped && effect && effect->durationMs() > 0);
    if (fadeFromBlack) {
        from.assign(newArgb.size(), 0);
        for (size_t i = 3; i < from.size(); i += 4)
            from[i] = 0xFF;
    }
    // Instant when: transitions disabled, surface not configured yet
    // (pends until configure), slide with no frame, or size mismatch.
    if (!effect || effect->durationMs() <= 0 || !wasConfigured ||
        (!haveOld && !fadeFromBlack)) {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        OutputInfo* out = findOutput(impl_.get(), monitor);
        if (!out || out->generation != myGen)
            return true; // superseded by a newer switch
        if (!out->configured)
            return true; // pends until configure (handler attaches)
        if (out->mapped) {
            attachPixelsLocked(out, newArgb, w, h);
        } else if (out->surface) {
            // Unmapped (hidden for mpvpaper): empty commit asks the
            // compositor to remap; the configure handler attaches pending.
            wl_surface_commit(out->surface);
            wl_display_flush(impl_->display);
        }
        return true;
    }

    int duration = effect->durationMs();
    // 120fps target: even cadence on 60Hz and 144Hz panels. Deadline-based
    // pacing (not fixed sleep) so frame work time doesn't drag the rate.
    constexpr int kFps = 120;
    int frames = std::max(1, duration * kFps / 1000);
    auto frameNs = std::chrono::nanoseconds(1'000'000'000LL / kFps);
    auto t0 = std::chrono::steady_clock::now();
    for (int i = 1; i <= frames; ++i) {
        if (!isCurrent())
            return true; // superseded (or hidden) mid-animation
        double progress = static_cast<double>(i) / frames;
        auto frame = effect->render(from, newArgb, w, h, progress, dir);
        {
            std::lock_guard<std::mutex> lock(impl_->mutex);
            OutputInfo* out = findOutput(impl_.get(), monitor);
            if (!out || out->generation != myGen)
                return true;
            if (!out->configured || !out->surface)
                return true;
            attachPixelsLocked(out, frame, w, h);
        }
        if (i < frames)
            std::this_thread::sleep_until(t0 + frameNs * i);
    }
    return true;
}

void WaylandBackend::precache(const std::string& monitor,
                              std::shared_ptr<const DecodedImage> img) {
    if (!validImg(img))
        return;
    int w = 0, h = 0;
    {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        OutputInfo* out = findOutput(impl_.get(), monitor);
        if (!out)
            return;
        outSize(out, w, h);
        if (out->scaled.find(img, w, h))
            return;
    }
    coverCached(impl_.get(), monitor, img, w, h);
}

void WaylandBackend::setTransition(const std::string& name) {
    if (!makeTransition(name))
        return;
    std::lock_guard<std::mutex> lock(impl_->mutex);
    impl_->transitionName = name;
}

std::string WaylandBackend::transition() const {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    return impl_->transitionName;
}

bool WaylandBackend::isMapped(const std::string& monitor) const {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    OutputInfo* out = findOutput(impl_.get(), monitor);
    return out && out->mapped;
}

bool WaylandBackend::remapWithImage(const std::string& monitor,
                                    std::shared_ptr<const DecodedImage> img,
                                    int timeoutMs) {
    if (!validImg(img))
        return false;
    {
        std::lock_guard<std::mutex> lock(impl_->mutex);
        OutputInfo* out = findOutput(impl_.get(), monitor);
        if (!out)
            return false;
        if (out->mapped)
            return true; // already visible: caller slides directly
        if (!ensureSurfaceLocked(impl_.get(), out))
            return false;
        out->pending = std::move(img);
        impl_->pendingByMonitor[monitor] = out->pending;
        ++out->generation;
        if (!out->surface)
            return false;
        // Empty commit asks the compositor to remap; the configure
        // handler attaches pending (same mechanics as the instant path
        // in setWallpaper). No buffer is attached here: after a nil
        // attach the surface needs a fresh configure first.
        wl_surface_commit(out->surface);
        wl_display_flush(impl_->display);
    }
    auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeoutMs);
    while (std::chrono::steady_clock::now() < deadline) {
        if (isMapped(monitor))
            return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    return isMapped(monitor);
}

void WaylandBackend::hideMonitor(const std::string& monitor) {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    OutputInfo* out = findOutput(impl_.get(), monitor);
    if (!out)
        return;
    out->pending.reset(); // stay hidden across future configures
    impl_->pendingByMonitor.erase(monitor);
    ++out->generation;    // cancel any in-flight slide
    if (!out->surface || !out->mapped)
        return; // already unmapped: no commit needed
    wl_surface_attach(out->surface, nullptr, 0, 0);
    wl_surface_commit(out->surface);
    wl_display_flush(impl_->display);
    out->mapped = false;
}

bool WaylandBackend::running() const { return impl_->running; }
