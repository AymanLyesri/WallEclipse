#include "walleclipse/ConfigStore.hpp"
#include "walleclipse/Transition.hpp"
#include "walleclipse/Util.hpp"

#include <algorithm>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <sys/file.h>
#include <unistd.h>

namespace fs = std::filesystem;

ConfigStore::ConfigStore(std::string baseDir) : baseDir_(std::move(baseDir)) {}

ConfigStore ConfigStore::defaultLocation() {
    if (const char* env = std::getenv("WALLECLIPSE_CONFIG_DIR")) {
        if (env[0] != '\0')
            return ConfigStore(env);
    }
    const char* home = std::getenv("HOME");
    std::string h = home ? home : "";
    return ConfigStore(h + "/.config/walleclipse/config");
}

void ConfigStore::expandPath(const std::string& in, std::string& out) {
    out = expandHome(in);
}

std::string ConfigStore::monitorConf(const std::string& monitor) const {
    return baseDir_ + "/" + monitor + "/defaults.conf";
}

void ConfigStore::ensureMonitors(const std::vector<std::string>& monitors) {
    std::error_code ec;
    fs::create_directories(baseDir_, ec);
    std::string seed = baseDir_ + "/defaults.conf";
    for (const auto& mon : monitors) {
        if (mon.empty())
            continue;
        std::string conf = baseDir_ + "/" + mon + "/defaults.conf";
        fs::create_directories(fs::path(conf).parent_path(), ec);
        if (fs::exists(conf, ec) && fs::file_size(conf, ec) != 0)
            continue;
        if (fs::exists(seed, ec))
            fs::copy_file(seed, conf, fs::copy_options::overwrite_existing, ec);
        else {
            // No seed: write empty w-1..w-10 template (matches ArchEclipse seed shape).
            std::ofstream out(conf, std::ios::trunc);
            for (int i = 1; i <= 10; ++i)
                out << "w-" << i << "=\n";
        }
    }
}

std::vector<std::string> ConfigStore::monitors() const {
    std::vector<std::string> out;
    std::error_code ec;
    if (!fs::exists(baseDir_, ec))
        return out;
    for (const auto& e : fs::directory_iterator(baseDir_, ec)) {
        if (e.is_directory(ec))
            out.push_back(e.path().filename().string());
    }
    return out;
}

namespace {
bool parseWallpaperLine(std::string line, int& ws, std::string& path) {
    if (!line.empty() && line.back() == '\r')
        line.pop_back();
    if (line.rfind("w-", 0) != 0)
        return false;
    auto eq = line.find('=');
    if (eq == std::string::npos)
        return false;
    try {
        ws = std::stoi(line.substr(2, eq - 2));
    } catch (...) {
        return false;
    }
    path = line.substr(eq + 1);
    return true;
}
} // namespace

std::map<int, std::string> ConfigStore::wallpapersFor(const std::string& monitor) const {
    std::map<int, std::string> out;
    std::ifstream fp(monitorConf(monitor));
    std::string line;
    int ws = 0;
    std::string path;
    while (std::getline(fp, line))
        if (parseWallpaperLine(line, ws, path))
            out[ws] = path;
    return out;
}

std::optional<std::string> ConfigStore::getWallpaper(const std::string& monitor,
                                                     int workspace) const {
    auto map = wallpapersFor(monitor);
    auto it = map.find(workspace);
    if (it == map.end())
        return std::nullopt;
    return it->second;
}

bool ConfigStore::setWallpaper(const std::string& monitor, int workspace,
                               const std::string& path) {
    std::string conf = monitorConf(monitor);
    std::error_code ec;
    fs::create_directories(fs::path(conf).parent_path(), ec);

    int lockFd = ::open((conf + ".lock").c_str(), O_CREAT | O_RDWR, 0644);
    if (lockFd < 0 || flock(lockFd, LOCK_EX) != 0) {
        if (lockFd >= 0)
            ::close(lockFd);
        return false;
    }

    std::vector<std::string> lines;
    std::string line;
    { // Read all lines (may not exist yet).
        std::ifstream fp(conf);
        while (std::getline(fp, line)) {
            if (!line.empty() && line.back() == '\r')
                line.pop_back();
            lines.push_back(line);
        }
    }

    std::string replacement = "w-" + std::to_string(workspace) + "=" + path;
    auto it = std::find_if(lines.begin(), lines.end(), [&](const std::string& l) {
        return l.rfind(replacement.substr(0, replacement.find('=') + 1), 0) == 0;
    });
    if (it != lines.end())
        *it = replacement;
    else
        lines.push_back(replacement);

    bool ok = false;
    if (std::ofstream fp(conf, std::ios::trunc); fp) {
        for (const auto& l : lines)
            fp << l << "\n";
        ok = static_cast<bool>(fp);
    }

    flock(lockFd, LOCK_UN);
    ::close(lockFd);
    return ok;
}

std::string ConfigStore::getTransition() const {
    std::ifstream fp(baseDir_ + "/transition");
    std::string line;
    if (!std::getline(fp, line))
        return "slide";
    line = trimWs(line);
    return makeTransition(line) ? line : "slide";
}

bool ConfigStore::setTransition(const std::string& name) {
    std::string clean = trimWs(name);
    if (!makeTransition(clean))
        return false;
    std::error_code ec;
    fs::create_directories(baseDir_, ec);
    std::ofstream fp(baseDir_ + "/transition", std::ios::trunc);
    if (!fp)
        return false;
    fp << clean << "\n";
    return static_cast<bool>(fp);
}
