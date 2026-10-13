# WallEclipse

Preloaded layer-shell wallpaper daemon for Hyprland. Extension of [ArchEclipse](https://github.com/AymanLyesri/ArchEclipse).
Replaces `hyprpaper` + `wallpaper-loop.c`; `mpvpaper` stays for animated wallpapers (gif/mp4/webm).
Workspace slide transitions also cover animated wallpapers via a static
poster bridge: the first switch to/from a video slides to/from its
extracted poster frame (cached in `~/.cache/walleclipse/posters`),
then hands off to `mpvpaper`.

## Why

hyprpaper 0.8 removed `preload`, so every workspace switch re-loads from disk
(`hyprctl hyprpaper wallpaper`). WallEclipse decodes **all** workspace
wallpapers once at startup into RAM (threadpool) and switches with a local
`wl_surface.attach` — no compositor round-trip on the hot path.

## Layout

```
Cargo.toml              workspace root (binary `walleclipse`, lib `walleclipse_rs`)
src/                    ConfigStore, ImageLoader, Preloader, WaylandBackend,
                        HyprListener, MediaDelegate, VideoPoster, Util, Ipc
tests/                  `cargo test` integration tests (no compositor needed)
examples/microbench.rs  decode+scale microbench used for COMPARISON.md
COMPARISON.md           C++ vs Rust numbers (historical record)
config/defaults.conf    seed template (w-1=..w-10=), copied from ArchEclipse
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
cargo build --release
cargo test
```

Always run the **release** binary — debug builds are ~12x slower at
decode/scale and cannot hold the 120fps transition cadence.
Deps: a recent stable Rust toolchain, system `ffmpegthumbnailer`/`ffmpeg`
(poster extraction), `mpvpaper` (animated wallpapers), `hyprland`, `jq`.

## Run

```sh
./target/release/walleclipse                  # daemon: preload-all, then Hyprland events
./target/release/walleclipse --once DP-2 ~/wall.jpg   # smoke test one render
./target/release/walleclipse set DP-2 3 ~/wall.jpg
./target/release/walleclipse preload ~/wall.jpg
./target/release/walleclipse reload   # re-read config + re-apply current
./target/release/walleclipse list
```

Logs: `/tmp/walleclipse.log`. IPC: `${XDG_CACHE_HOME:-~/.cache}/walleclipse.sock`.

## Migration from hyprpaper
1. Keep `hyprpaper` installed until stable; stop it (`pkill hyprpaper`) only for test runs.
2. Replace the `wallpaper-loop` `exec-once` with `walleclipse` in Hyprland config.
3. `set-wallpaper.sh` callers can switch to `walleclipse set <mon> <ws> <path>`.
4. Animated wallpapers keep working via `mpvpaper` automatically.

## License

GPL-3.0-only, see [LICENSE](LICENSE).
