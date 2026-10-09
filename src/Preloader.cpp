#include "walleclipse/Preloader.hpp"

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

void Preloader::preloadOne(const std::string& path) {
    if (path.empty() || isMediaWallpaper(path))
        return;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (cache_.count(path))
            return;
    }
    std::string expanded;
    ConfigStore::expandPath(path, expanded);
    auto img = loadImage(expanded.empty() ? path : expanded);
    if (!img)
        img = loadImage(path); // fall back to raw path
    if (img)
        insert(path, std::make_shared<DecodedImage>(std::move(*img)));
}

void Preloader::preloadAll(ConfigStore& store) {
    std::vector<std::string> wanted;
    for (const auto& mon : store.monitors()) {
        for (const auto& [ws, path] : store.wallpapersFor(mon)) {
            (void)ws;
            if (!path.empty() && !isMediaWallpaper(path))
                wanted.push_back(path);
        }
    }
    std::sort(wanted.begin(), wanted.end());
    wanted.erase(std::unique(wanted.begin(), wanted.end()), wanted.end());

    std::atomic<size_t> next = 0;
    std::vector<std::thread> workers;
    size_t n = std::min(threads_, std::max<size_t>(wanted.size(), 1));
    for (size_t t = 0; t < n; ++t) {
        workers.emplace_back([&] {
            for (;;) {
                size_t i = next.fetch_add(1);
                if (i >= wanted.size())
                    break;
                preloadOne(wanted[i]);
            }
        });
    }
    for (auto& th : workers)
        th.join();
}

std::shared_ptr<DecodedImage> Preloader::get(const std::string& path) const {
    std::lock_guard<std::mutex> lock(mutex_);
    auto it = cache_.find(path);
    if (it == cache_.end())
        return nullptr;
    return it->second;
}

size_t Preloader::size() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return cache_.size();
}
