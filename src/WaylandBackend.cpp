#include "walleclipse/WaylandBackend.hpp"

#include <cstring>
#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>

// wayland-scanner emits `namespace` as a parameter name in the C header,
// which is a keyword in C++. Rename it around the include (standard hack).
#define namespace ns_placeholder
#include "wlr-layer-shell-unstable-v1-client.h"
#undef namespace
#include "xdg-output-unstable-v1-client.h"

struct OutputInfo {
    wl_output* output = nullptr;
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

    wl_buffer* buffer = nullptr;
    void* poolData = nullptr;
    size_t poolSize = 0;
    int bufW = 0, bufH = 0;
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
    std::mutex mutex;
    std::thread dispatchThread;
    bool running = false;

    void vlog(const std::string& m) {
        if (log)
            log(m);
    }
};

namespace {

void layerConfigure(void* data, zwlr_layer_surface_v1* layer, uint32_t serial,
                     uint32_t width, uint32_t height) {
    auto* out = static_cast<OutputInfo*>(data);
    zwlr_layer_surface_v1_ack_configure(layer, serial);
    out->configureSerial = serial;
    out->configured = true;
    out->cfgW = static_cast<int>(width);
    out->cfgH = static_cast<int>(height);
}

void layerClosed(void* data, zwlr_layer_surface_v1* /*layer*/) {
    auto* out = static_cast<OutputInfo*>(data);
    out->configured = false;
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
    if (name)
        static_cast<OutputInfo*>(data)->name = name;
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
        info->output = o;
        wl_output_add_listener(o, &kOutputListener, info.get());
        if (self->xdgOutMgr) {
            info->xdg = zxdg_output_manager_v1_get_xdg_output(self->xdgOutMgr, o);
            zxdg_output_v1_add_listener(info->xdg, &kXdgListener, info.get());
        }
        self->outputs[o] = std::move(info);
    } else if (std::strcmp(interface, zwlr_layer_shell_v1_interface.name) == 0) {
        self->layerShell = static_cast<zwlr_layer_shell_v1*>(wl_registry_bind(
            registry, name, &zwlr_layer_shell_v1_interface, 4));
    } else if (std::strcmp(interface, zxdg_output_manager_v1_interface.name) == 0) {
        self->xdgOutMgr = static_cast<zxdg_output_manager_v1*>(wl_registry_bind(
            registry, name, &zxdg_output_manager_v1_interface, 3));
    }
}

void registryRemove(void* data, wl_registry*, uint32_t) {
    (void)data;
    // Hotplug-removal destroys surfaces lazily on next setWallpaper/hide.
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

    // Create one background layer surface per output.
    for (auto& [wlOut, info] : impl_->outputs) {
        info->surface = wl_compositor_create_surface(impl_->compositor);
        info->layer = zwlr_layer_shell_v1_get_layer_surface(
            impl_->layerShell, info->surface, info->output,
            ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND, "walleclipse");
        zwlr_layer_surface_v1_set_size(info->layer, 0, 0);
        zwlr_layer_surface_v1_set_anchor(
            info->layer, ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP |
                             ZWLR_LAYER_SURFACE_V1_ANCHOR_BOTTOM |
                             ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT |
                             ZWLR_LAYER_SURFACE_V1_ANCHOR_RIGHT);
        zwlr_layer_surface_v1_set_exclusive_zone(info->layer, -1);
        zwlr_layer_surface_v1_add_listener(info->layer, &kLayerListener, info.get());
        wl_surface_commit(info->surface);
    }
    wl_display_roundtrip(impl_->display);

    impl_->running = true;
    impl_->dispatchThread = std::thread([impl = impl_.get()] {
        while (impl->running) {
            if (wl_display_dispatch(impl->display) < 0)
                break;
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

static OutputInfo* findOutput(WaylandBackend::Impl* impl, const std::string& monitor) {
    for (auto& [wlOut, info] : impl->outputs) {
        if (info->name == monitor)
            return info.get();
    }
    return nullptr;
}

bool WaylandBackend::setWallpaper(const std::string& monitor, const DecodedImage& img) {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    OutputInfo* out = findOutput(impl_.get(), monitor);
    if (!out || !out->surface || !out->layer)
        return false;

    int w = out->cfgW > 0 ? out->cfgW : out->width;
    int h = out->cfgH > 0 ? out->cfgH : out->height;
    w *= 1;
    h *= 1;
    if (w <= 0 || h <= 0) {
        w = 1920;
        h = 1080;
    }

    auto argb = scaleCoverArgb(img, w, h);
    size_t stride = static_cast<size_t>(w) * 4;
    size_t size = stride * h;

    int fd = createShmFile(size);
    if (fd < 0)
        return false;
    void* data =
        mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (data == MAP_FAILED) {
        close(fd);
        return false;
    }
    std::memcpy(data, argb.data(), size);

    wl_shm_pool* pool = wl_shm_create_pool(impl_->shm, fd, static_cast<int32_t>(size));
    wl_buffer* buffer = wl_shm_pool_create_buffer(pool, 0, w, h,
                                                  static_cast<int32_t>(stride),
                                                  WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);

    wl_surface_attach(out->surface, buffer, 0, 0);
    wl_surface_damage_buffer(out->surface, 0, 0, w, h);
    wl_surface_commit(out->surface);
    wl_display_flush(impl_->display);

    // Retire previous buffer resources.
    if (out->buffer)
        wl_buffer_destroy(out->buffer);
    if (out->poolData)
        munmap(out->poolData, out->poolSize);
    out->buffer = buffer;
    out->poolData = data;
    out->poolSize = size;
    out->bufW = w;
    out->bufH = h;
    return true;
}

void WaylandBackend::hideMonitor(const std::string& monitor) {
    std::lock_guard<std::mutex> lock(impl_->mutex);
    OutputInfo* out = findOutput(impl_.get(), monitor);
    if (!out || !out->surface)
        return;
    wl_surface_attach(out->surface, nullptr, 0, 0);
    wl_surface_commit(out->surface);
    wl_display_flush(impl_->display);
}

bool WaylandBackend::running() const {
    return impl_->running;
}
