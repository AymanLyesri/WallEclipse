#pragma once

#include <map>
#include <optional>
#include <string>
#include <vector>

// Reads/writes per-monitor `w-<id>=<path>` defaults.conf files,
// byte-compatible with ArchEclipse's wallpaper-daemon scripts.
// Writes are serialized with flock on `<config>.lock`, matching
// set-wallpaper.sh semantics.
class ConfigStore {
public:
    explicit ConfigStore(std::string baseDir);

    // Default location: $WALLECLIPSE_CONFIG_DIR or ~/.config/walleclipse/config
    static ConfigStore defaultLocation();

    // Ensure config/<monitor>/defaults.conf exists (seeded from
    // config/defaults.conf) for every known monitor.
    void ensureMonitors(const std::vector<std::string>& monitors);

    std::vector<std::string> monitors() const;
    std::map<int, std::string> wallpapersFor(const std::string& monitor) const;
    std::optional<std::string> getWallpaper(const std::string& monitor, int workspace) const;

    // flock-guarded read-modify-write. Creates w-<ws> key if missing.
    bool setWallpaper(const std::string& monitor, int workspace, const std::string& path);

    const std::string& baseDir() const { return baseDir_; }

    static void expandPath(const std::string& in, std::string& out);

private:
    std::string monitorConf(const std::string& monitor) const;
    std::string baseDir_;
};
