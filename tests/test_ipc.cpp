// Ipc TRANSITION tests: query/set over an isolated socket, no daemon/compositor.
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <string>
#include <thread>
#include <unistd.h>

#include "walleclipse/Ipc.hpp"

namespace fs = std::filesystem;

static int failures = 0;
#define CHECK(cond)                                             \
    do {                                                        \
        if (!(cond)) {                                          \
            std::cerr << "FAIL line " << __LINE__ << ": " #cond \
                       << "\n";                                  \
            ++failures;                                         \
        }                                                       \
    } while (0)

int main() {
    std::string tmp = std::string(std::getenv("TMPDIR") ? std::getenv("TMPDIR") : "/tmp") +
                      "/walleclipse-test-ipc-" + std::to_string(::getpid());
    fs::remove_all(tmp);
    fs::create_directories(tmp);
    ::setenv("XDG_CACHE_HOME", tmp.c_str(), 1);

    std::string current = "slide";
    Ipc::Handlers h;
    h.transition = [&](const std::string& name) {
        if (name.empty())
            return current; // query
        if (name != "slide" && name != "fade" && name != "none")
            return std::string(); // reject: serve replies ERR
        current = name;
        return current;
    };
    std::thread srv([h = std::move(h)]() mutable { Ipc::serve(std::move(h)); });
    // serve() binds async: wait for the socket before the first call.
    for (int i = 0; i < 200 && !fs::exists(tmp + "/walleclipse.sock"); ++i)
        std::this_thread::sleep_for(std::chrono::milliseconds(10));

    // Query defaults to slide.
    CHECK(Ipc::call("TRANSITION") == "slide\n");
    // Set fade, query sticks.
    CHECK(Ipc::call("TRANSITION fade") == "fade\n");
    CHECK(Ipc::call("TRANSITION") == "fade\n");
    // Unknown names rejected, previous value kept.
    std::string bad = Ipc::call("TRANSITION bogus");
    CHECK(bad.rfind("ERR", 0) == 0);
    CHECK(Ipc::call("TRANSITION") == "fade\n");
    // Back to slide.
    CHECK(Ipc::call("TRANSITION slide") == "slide\n");

    CHECK(Ipc::call("QUIT") == "OK\n");
    srv.join();
    fs::remove_all(tmp);
    if (failures == 0)
        std::cout << "test_ipc: all passed\n";
    return failures == 0 ? 0 : 1;
}
