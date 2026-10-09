#include "walleclipse/ConfigStore.hpp"

#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <fstream>
#include <sstream>
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
    const char* home = std::getenv("HOME");
    std::string h = home ? home : "";
    if (!in.empty() && in[0] == '~') {
        out = h + in.substr(1);
    } else if (in.rfind("$HOME", 0) == 0) {
        out = h + in.substr(5);
    } else {
        out = in;
    }
}

std::string ConfigStore::monitorConf(const std::string& monitor) const {
    return baseDir_ + "/" + monitor + "/defaults.conf";
}

static bool copyFile(const std::string& src, const std::string& dst) {
    std::ifstream in(src, std::ios::binary);
    if (!in)
        return false;
    std::ofstream out(dst, std::ios::binary | std::ios::trunc);
    if (!out)
        return false;
    out << in.rdbuf();
    return static_cast<bool>(out);
}

void ConfigStore::ensureMonitors(const std::vector<std::string>& monitors) {
    std::error_code ec;
    fs::create_directories(baseDir_, ec);
    std::string seed = baseDir_ + "/defaults.conf";
    for (const auto& mon : monitors) {
        if (mon.empty())
            continue;
        std::string dir = baseDir_ + "/" + mon;
        fs::create_directories(dir, ec);
        std::string conf = dir + "/defaults.conf";
        bool needsInit = !fs::exists(conf, ec) || fs::file_size(conf, ec) == 0;
        if (needsInit && fs::exists(seed, ec))
            copyFile(seed, conf);
        else if (needsInit) {
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

std::map<int, std::string> ConfigStore::wallpapersFor(const std::string& monitor) const {
    std::map<int, std::string> out;
    std::ifstream fp(monitorConf(monitor));
    if (!fp)
        return out;
    std::string line;
    while (std::getline(fp, line)) {
        if (!line.empty() && line.back() == '\r')
            line.pop_back();
        if (line.rfind("w-", 0) != 0)
            continue;
        auto eq = line.find('=');
        if (eq == std::string::npos)
            continue;
        try {
            int ws = std::stoi(line.substr(2, eq - 2));
            out[ws] = line.substr(eq + 1);
        } catch (...) {
        }
    }
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

    std::string lockPath = conf + ".lock";
    int lockFd = ::open(lockPath.c_str(), O_CREAT | O_RDWR, 0644);
    if (lockFd < 0)
        return false;
    if (flock(lockFd, LOCK_EX) != 0) {
        ::close(lockFd);
        return false;
    }

    // Read all lines (may not exist yet).
    std::vector<std::string> lines;
    {
        std::ifstream fp(conf);
        std::string line;
        while (std::getline(fp, line)) {
            if (!line.empty() && line.back() == '\r')
                line.pop_back();
            lines.push_back(line);
        }
    }

    std::string key = "w-" + std::to_string(workspace) + "=";
    std::string replacement = key + path;
    bool found = false;
    for (auto& line : lines) {
        if (line.rfind(key, 0) == 0) {
            line = replacement;
            found = true;
            break;
        }
    }
    if (!found)
        lines.push_back(replacement);

    bool ok = false;
    {
        std::ofstream fp(conf, std::ios::trunc);
        if (fp) {
            for (const auto& line : lines)
                fp << line << "\n";
            ok = static_cast<bool>(fp);
        }
    }

    flock(lockFd, LOCK_UN);
    ::close(lockFd);
    return ok;
}
