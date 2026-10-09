# WallEclipse

Preloaded layer-shell wallpaper daemon for Hyprland. Extension of [ArchEclipse](https://github.com/AymanLyesri/ArchEclipse).
Replaces `hyprpaper` + `wallpaper-loop.c`; `mpvpaper` stays for animated wallpapers (gif/mp4/webm).

## Why

hyprpaper 0.8 removed `preload`, so every workspace switch re-loads from disk
(`hyprctl hyprpaper wallpaper`). WallEclipse decodes **all** workspace
wallpapers once at startup into RAM (threadpool) and switches with a local
`wl_surface.attach` — no compositor round-trip on the hot path.

## Layout

```
CMakeLists.txt          C++20, wayland-scanner codegen
protocols/              vendored wlr-layer-shell-unstable-v1.xml
third_party/stb_image.h jpg/png decoder (webp via system libwebp)
include/walleclipse/   ConfigStore, ImageLoader, Preloader,
src/                    WaylandBackend, HyprListener, MediaDelegate, Ipc
tests/                  CTest unit tests (no compositor needed)
config/defaults.conf    seed template (w-1=..w-10=), copied from ArchEclipse
docs/superpowers/plans/ implementation plan
```

## Config merge

Drop-in compatible with `~/.config/hypr/wallpaper-daemon/config`:
per-monitor `config/<monitor>/defaults.conf` with `w-<workspace>=<path>` lines,
`flock` on `<config>.lock` for writers. Seed once:

```sh
mkdir -p ~/.config/walleclipse/config/DP-2
cp ~/.config/hypr/wallpaper-daemon/config/DP-2/defaults.conf ~/.config/walleclipse/config/DP-2/
```

Or let the daemon seed empty slots from `config/defaults.conf` automatically.

## Build

```sh
cmake -S . -B build && cmake --build build
ctest --test-dir build
```

Deps: `wayland-client`, `wlr-protocols` not required (vendored),
`libpng`, `libjpeg`, `libwebp`, `wayland-scanner`.

## Run

```sh
./build/walleclipse                  # daemon: preload-all, then Hyprland events
./build/walleclipse --once DP-2 ~/wall.jpg   # smoke test one render
./build/walleclipse set DP-2 3 ~/wall.jpg
./build/walleclipse preload ~/wall.jpg
./build/walleclipse reload   # re-read config + re-apply current
./build/walleclipse list
```

Logs: `/tmp/walleclipse.log`. IPC: `${XDG_CACHE_HOME:-~/.cache}/walleclipse.sock`.

## Migration from hyprpaper
1. Keep `hyprpaper` installed until stable; stop it (`pkill hyprpaper`) only for test runs.
2. Replace the `wallpaper-loop` `exec-once` with `walleclipse` in Hyprland config.
3. `set-wallpaper.sh` callers can switch to `walleclipse set <mon> <ws> <path>`.
4. Animated wallpapers keep working via `mpvpaper` automatically.

## License

GPL-3.0-only, see [LICENSE](LICENSE). Vendored `third_party/stb_image.h` is
MIT-licensed (compatible, notice preserved in-file).
