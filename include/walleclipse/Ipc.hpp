#pragma once

#include <functional>
#include <string>

// Line-based Unix-socket IPC at $XDG_CACHE_HOME/walleclipse.sock
// (fallback ~/.cache). Commands (one per connection):
//   PRELOAD <path>            decode now (also auto-cached on SET)
//   SET <monitor> <ws> <path> update config + show immediately
//   CURRENT                   print current.conf content
//   LIST                      print "<monitor> <ws> <path>" lines
//   RELOAD                    re-read config, preload new paths, re-apply current
//   TRANSITION [name]         query (no arg) or set the workspace-switch
//                             transition (slide|fade|none); set persists to
//                             disk so it survives reload + reboot
//   QUIT                      stop the daemon (local user only)
class Ipc {
public:    struct Handlers {
        std::function<void(const std::string& path)> preload;
        std::function<bool(const std::string& mon, int ws, const std::string& path)> set;
        std::function<std::string()> current;
        std::function<std::string()> list;
        std::function<void()> reload;
        // Empty name = query (return current); otherwise validate, persist
        // and apply, returning the new value (empty = rejected -> ERR).
        std::function<std::string(const std::string& name)> transition;
        std::function<void()> quit;
    };

    static std::string socketPath();
    // Blocking server loop. Returns after QUIT (true) or fatal error (false).
    static bool serve(Handlers h);
    // One-shot client: send lines, return server reply.
    static std::string call(const std::string& payload);
};
