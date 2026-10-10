#pragma once

#include <memory>
#include <mutex>
#include <string>
#include <unordered_map>
#include <vector>

#include "walleclipse/ConfigStore.hpp"
#include "walleclipse/ImageLoader.hpp"

// Preload-all cache: at startup every static wallpaper referenced by
// any monitor config is decoded once on a threadpool and kept in RAM,
// so workspace switches never touch disk. Animated posters (first
// frames for mpvpaper wallpapers, see VideoPoster) are cached alongside
// so transitions can slide to/from them. Thread-safe; get() returns
// nullptr on miss.
class Preloader {
public:
    explicit Preloader(size_t threads = 0);

    void preloadAll(ConfigStore& store);
    void preloadOne(const std::string& path);
    void preloadPosterOne(const std::string& path);

    std::shared_ptr<DecodedImage> get(const std::string& path) const;
    std::shared_ptr<DecodedImage> getPoster(const std::string& path) const;
    size_t size() const;

private:
    void insert(const std::string& path, std::shared_ptr<DecodedImage> img);

    mutable std::mutex mutex_;
    std::unordered_map<std::string, std::shared_ptr<DecodedImage>> cache_;
    std::unordered_map<std::string, std::shared_ptr<DecodedImage>> posters_;
    size_t threads_;
};
