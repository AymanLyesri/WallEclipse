#pragma once

#include <string>
#include <vector>

// Shared helpers (deduped from ConfigStore/Ipc/VideoPoster/MediaDelegate).
std::string cacheBaseDir(); // $XDG_CACHE_HOME or ~/.cache
std::string expandHome(const std::string& in); // leading ~ / $HOME
std::string trimWs(const std::string& s);
std::vector<std::string> splitLines(const std::string& s);
std::string execCapture(const char* cmd); // popen + read all
bool runWait(const std::vector<std::string>& args); // fork+exec, wait, silent
void spawnDetached(std::vector<std::string> args); // double-fork, silent
std::string readAllFd(int fd);
bool writeAllFd(int fd, const std::string& s);
