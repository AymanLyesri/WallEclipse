#include "walleclipse/Preloader.hpp"
#include "walleclipse/Util.hpp"
#include "walleclipse/VideoPoster.hpp"

#include <algorithm>
#include <atomic>
#include <thread>

Preloader::Preloader(size_t threads) : threads_(threads) {
    if (threads_ == 0) {
        threads_ = std::thread::hardware_concurrency();
        if (threads_ == 0)
            threads_ = 4;
    }
}

void Preloader::insert(const std::string& path, std::shared_ptr<DecodedImage> img) {
    std::lock_guard<std::mutex> lock(mutex_);
    cache_[path] = std::move(img);
}

static std::shared_ptr<DecodedImage> decodeStatic(const std::string& p) {
    auto opt = loadImage(p);
    return opt ? std::make_shared<DecodedImage>(std::move(*opt)) : nullptr;
}

template <typename F> static std::shared_ptr<DecodedImage> withFallback(
    const std::string& path, F load) {
    std::string expanded = expandHome(path);
    const std::string& first = expanded.empty() ? path : expanded;
    if (auto img = load(first))
        return img;
    if (!expanded.empty() && expanded != path)
        return load(path);
    return nullptr;
}

void Preloader::preloadOne(const std::string& path) {
    if (path.empty() || isMediaWallpaper(path))
        return;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (cache_.count(path))
            return;
    }
    if (auto img = withFallback(path, decodeStatic))
        insert(path, std::move(img));
}

void Preloader::preloadPosterOne(const std::string& path) {
    if (path.empty() || !isMediaWallpaper(path))
        return;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (posters_.count(path))
            return;
    }
    if (auto img = withFallback(path, VideoPoster::load)) {
        std::lock_guard<std::mutex> lock(mutex_);
        posters_[path] = std::move(img);
    }
}

void Preloader::preloadAll(ConfigStore& store) {
    std::vector<std::string> wanted, wantedMedia;
    for (const auto& mon : store.monitors())
        for (const auto& [ws, path] : store.wallpapersFor(mon)) {
            (void)ws;
            if (path.empty())
                continue;
            (isMediaWallpaper(path) ? wantedMedia : wanted).push_back(path);
        }
    auto dedup = [](std::vector<std::string>& v) {
        std::sort(v.begin(), v.end());
        v.erase(std::unique(v.begin(), v.end()), v.end());
    };
    dedup(wanted);
    dedup(wantedMedia);

    auto runPool = [&](const std::vector<std::string>& jobs,
                       void (Preloader::*fn)(const std::string&)) {
        std::atomic<size_t> next = 0;
        std::vector<std::thread> workers;
        for (size_t t = 0, n = std::min(threads_, std::max<size_t>(jobs.size(), 1));
             t < n; ++t)
            workers.emplace_back([&] {
                for (size_t i = next.fetch_add(1); i < jobs.size();
                     i = next.fetch_add(1))
                    (this->*fn)(jobs[i]);
            });
        for (auto& th : workers)
            th.join();
    };
    runPool(wanted, &Preloader::preloadOne);
    // Posters (~250ms ffmpeg each) share the pool so startup stays fast.
    runPool(wantedMedia, &Preloader::preloadPosterOne);
}

std::shared_ptr<DecodedImage> Preloader::get(const std::string& path) const {
    std::lock_guard<std::mutex> lock(mutex_);
    auto it = cache_.find(path);
    return it == cache_.end() ? nullptr : it->second;
}

std::shared_ptr<DecodedImage> Preloader::getPoster(const std::string& path) const {
    std::lock_guard<std::mutex> lock(mutex_);
    auto it = posters_.find(path);
    return it == posters_.end() ? nullptr : it->second;
}

size_t Preloader::size() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return cache_.size() + posters_.size();
}
