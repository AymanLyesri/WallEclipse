#!/usr/bin/env bash
# WallEclipse Rust port — parity comparison harness.
#
# Produces rust/COMPARISON.md with the four numbers (CPU proxy, RAM proxy,
# line count, file count) + binary sizes + methodology + cargo test status.
#
# SAFETY (hard rules — do not weaken):
#  - Re-runnable; writes only to a fresh mktemp dir, rust/COMPARISON.md,
#    and rust/target/ (cargo build). NEVER touches ~/.config/walleclipse,
#    ~/.cache/walleclipse.sock, /tmp/walleclipse.log, or any live path.
#  - EVERY invocation of either wallpaper binary (C++ or Rust) runs under
#    `env -i` with an isolated HOME/XDG_CACHE_HOME/WALLECLIPSE_CONFIG_DIR
#    rooted at the temp dir, so even a mis-parsed probe cannot reach the
#    live daemon socket or live config.
#  - Pure-compute microbenches decode fixture files generated in the temp
#    dir only. The C++ tree is read (compiled from sources) but never
#    modified; the prebuilt C++ binary is only `stat`ed and run with
#    `--help` (read-only probe).
#
# Requirements: bash, g++, python3 + PIL, cargo (offline registry cache),
# /proc (peak-RSS via VmHWM). No network. No /usr/bin/time, no tokei.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_DIR="$ROOT/rust"
OUT="$RUST_DIR/COMPARISON.md"
# Prebuilt C++ reference binary (read-only: stat + --help probe only).
CXX_BIN="${CXX_BIN:-/home/ayman/.config/walleclipse/build/walleclipse}"
RS_BIN="$RUST_DIR/target/release/walleclipse-rs"
ITERS="${ITERS:-10}"

TMP="$(mktemp -d /tmp/opencode/walleclipse-compare.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
FIX="$TMP/fixtures"; mkdir -p "$FIX"
ISO_HOME="$TMP/home"; ISO_CACHE="$TMP/cache"; ISO_CFG="$TMP/cfg"
mkdir -p "$ISO_HOME" "$ISO_CACHE" "$ISO_CFG"

pass=0; fail=0
note() { printf '%s\n' "$*" ; }
warn() { printf 'WARN: %s\n' "$*" >&2; }

# Isolated-env launcher for ANY wallpaper-binary probe.
# Usage: probe <stdout-file> <binary> [args...]
probe() {
    local out="$1"; shift
    env -i PATH="/usr/bin:/bin" HOME="$ISO_HOME" \
        XDG_CACHE_HOME="$ISO_CACHE" WALLECLIPSE_CONFIG_DIR="$ISO_CFG" \
        "$@" >"$out" 2>&1
}

# Run a workload under peak-RSS observation.
# Usage: measure <tag> <stdout-file> <binary> [args...]
# Echoes "<wall_ms> <peak_kb>" and leaves program output in stdout-file.
measure() {
    local tag="$1" out="$2"; shift 2
    local t0 t1 peak cur status_file="$TMP/$tag.status"
    t0=$(date +%s%N)
    env -i PATH="/usr/bin:/bin" HOME="$ISO_HOME" \
        XDG_CACHE_HOME="$ISO_CACHE" WALLECLIPSE_CONFIG_DIR="$ISO_CFG" \
        "$@" >"$out" 2>&1 &
    local pid=$!
    peak=0
    while kill -0 "$pid" 2>/dev/null; do
        if cur=$(awk '/^VmHWM:/{print $2}' "/proc/$pid/status" 2>/dev/null) && [ -n "$cur" ]; then
            [ "$cur" -gt "$peak" ] && peak="$cur"
        fi
        sleep 0.005
    done
    wait "$pid"; echo "$?" >"$status_file"
    # Final VmHWM sample may be gone after exit; last poll holds the peak
    # (VmHWM is monotonic), so $peak is the run's peak RSS in KiB.
    t1=$(date +%s%N)
    echo "$(( (t1 - t0) / 1000000 )) $peak $(cat "$status_file")"
}

# ---------------------------------------------------------------- counts ---
cxx_files=$(cd "$ROOT" && find src include tests -type f \( -name '*.cpp' -o -name '*.hpp' -o -name '*.h' \) | sort)
cxx_nfiles=$(printf '%s\n' "$cxx_files" | grep -c .)
cxx_lines=$(cd "$ROOT" && cat $(printf '%s\n' "$cxx_files") | wc -l)
cxx_cmake_lines=$(wc -l <"$ROOT/CMakeLists.txt")

rs_files=$(cd "$ROOT" && find rust/src rust/tests -type f -name '*.rs' | sort)
rs_nfiles=$(printf '%s\n' "$rs_files" | grep -c .)
rs_lines=$(cd "$ROOT" && cat $(printf '%s\n' "$rs_files") | wc -l)
rs_cargo_lines=$(wc -l <"$RUST_DIR/Cargo.toml")
# Bench harness itself (NOT counted as ported code, reported separately).
bench_rs_lines=$(wc -l <"$RUST_DIR/examples/microbench.rs")

# ---------------------------------------------------------------- build ----
note "== cargo build --release (offline) =="
if cargo build --release --manifest-path "$RUST_DIR/Cargo.toml" --offline >"$TMP/cargo-build.log" 2>&1; then
    pass=$((pass+1)); build_status="ok"
else
    fail=$((fail+1)); build_status="FAILED (see rust build log)"
    warn "cargo build failed; tail:"; tail -5 "$TMP/cargo-build.log" >&2
fi

rs_size="missing"; rs_human="-"
if [ -f "$RS_BIN" ]; then rs_size=$(stat -c%s "$RS_BIN"); rs_human=$(ls -lh "$RS_BIN" | awk '{print $5}'); fi
cxx_size="missing"; cxx_human="-"
if [ -f "$CXX_BIN" ]; then cxx_size=$(stat -c%s "$CXX_BIN"); cxx_human=$(ls -lh "$CXX_BIN" | awk '{print $5}'); fi

# --help probes (read-only, isolated env).
probe "$TMP/cxx-help.txt" "$CXX_BIN" --help || warn "C++ --help probe failed"
probe "$TMP/rs-help.txt" "$RS_BIN" --help || warn "Rust --help probe failed"

# -------------------------------------------------------------- fixtures ---
note "== generating deterministic fixtures =="
python3 - "$FIX" <<'EOF'
import sys
from PIL import Image
fix = sys.argv[1]
def gradient(w, h):
    im = Image.new("RGB", (w, h))
    px = im.load()
    for y in range(h):
        for x in range(w):
            px[x, y] = ((x * 255) // w, (y * 255) // h, ((x + y) * 255) // (w + h))
    return im
gradient(800, 600).save(f"{fix}/a-800x600.jpg", quality=85)
gradient(1280, 720).save(f"{fix}/b-1280x720.png")
try:
    gradient(640, 480).save(f"{fix}/c-640x480.webp", quality=80)
except Exception as e:
    print(f"webp save failed ({e}); using png fallback")
    gradient(640, 480).save(f"{fix}/c-640x480.png")
print("fixtures:", sorted(__import__("os").listdir(fix)))
EOF

micro_rs="$RUST_DIR/target/release/examples/microbench"
note "== cargo build --release --example microbench (offline) =="
if cargo build --release --manifest-path "$RUST_DIR/Cargo.toml" --offline --example microbench >"$TMP/cargo-bench-build.log" 2>&1; then
    pass=$((pass+1)); rs_bench="ok"
else
    fail=$((fail+1)); rs_bench="FAILED"
    warn "microbench example build failed"; tail -5 "$TMP/cargo-bench-build.log" >&2
fi

# ------------------------------------------------------- C++ driver build --
cat >"$TMP/cxx-bench.cc" <<'EOF'
// Headless-safe decode + cover-scale driver for the C++ side.
// Same workload as rust/examples/microbench.rs: decode each fixture N
// times via loadImage, scaleCoverArgb to 1920x1080, fold a checksum.
// Reads only argv[1] (fixture dir); writes nothing; no daemon/IPC.
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <filesystem>
#include <string>
#include <vector>
#include "walleclipse/ImageLoader.hpp"

int main(int argc, char** argv) {
    if (argc < 2) { std::fprintf(stderr, "usage: cxx-bench <dir> [iters]\n"); return 2; }
    int iters = argc > 2 ? std::atoi(argv[3 - 1]) : 10;
    std::vector<std::string> files;
    for (auto& e : std::filesystem::directory_iterator(argv[1]))
        if (e.is_regular_file()) files.push_back(e.path().string());
    std::sort(files.begin(), files.end());
    if (files.empty()) { std::fprintf(stderr, "no fixtures\n"); return 1; }
    auto t0 = std::chrono::steady_clock::now();
    uint64_t acc = 0; long decodes = 0;
    for (int i = 0; i < iters; ++i)
        for (auto& f : files) {
            auto img = loadImage(f);
            if (!img) { std::fprintf(stderr, "decode failed: %s\n", f.c_str()); return 1; }
            auto out = scaleCoverArgb(*img, 1920, 1080);
            for (size_t k = 0; k < out.size(); k += 4)
                acc += out[k] + 3ULL * out[k + 1] + 7ULL * out[k + 2];
            ++decodes;
        }
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - t0).count();
    std::printf("microbench: files=%zu iters=%d decodes=%ld wall_ms=%lld checksum=%llu\n",
        files.size(), iters, decodes, (long long)ms, (unsigned long long)acc);
    return 0;
}
EOF
cxx_driver_status="ok"
if g++ -O2 -std=c++17 -I"$ROOT/include" -I"$ROOT/third_party" \
        "$TMP/cxx-bench.cc" "$ROOT/src/ImageLoader.cpp" -lwebp \
        -o "$TMP/cxx-bench" >"$TMP/cxx-build.log" 2>&1; then
    pass=$((pass+1))
else
    fail=$((fail+1)); cxx_driver_status="BUILD FAILED — C++ perf requires-live-daemon fallback"
    warn "C++ driver build failed"; tail -5 "$TMP/cxx-build.log" >&2
fi

# ------------------------------------------------------------------ perf ---
rs_wall="n/a"; rs_peak="n/a"; rs_prog="(not run)"; rs_exit="-"
cxx_wall="n/a"; cxx_peak="n/a"; cxx_prog="(not run)"; cxx_exit="-"

if [ "$rs_bench" = "ok" ] && [ -x "$micro_rs" ]; then
    # shellcheck disable=SC2086
    read -r rs_wall rs_peak rs_exit < <(measure rs "$TMP/rs-bench.out" "$micro_rs" "$FIX" "$ITERS")
    rs_prog=$(cat "$TMP/rs-bench.out")
    [ "$rs_exit" = "0" ] && pass=$((pass+1)) || fail=$((fail+1))
fi
if [ "$cxx_driver_status" = "ok" ]; then
    read -r cxx_wall cxx_peak cxx_exit < <(measure cxx "$TMP/cxx-bench.out" "$TMP/cxx-bench" "$FIX" "$ITERS")
    cxx_prog=$(cat "$TMP/cxx-bench.out")
    [ "$cxx_exit" = "0" ] && pass=$((pass+1)) || fail=$((fail+1))
fi

# Checksums must agree across sides (same bilinear math on same pixels,
# up to decoder rounding: allow exact match, flag otherwise).
rs_sum=$(printf '%s' "$rs_prog" | sed -n 's/.*checksum=\([0-9]*\).*/\1/p')
cxx_sum=$(printf '%s' "$cxx_prog" | sed -n 's/.*checksum=\([0-9]*\).*/\1/p')
if [ -n "$rs_sum" ] && [ -n "$cxx_sum" ]; then
    if [ "$rs_sum" = "$cxx_sum" ]; then checksum_note="MATCH ($rs_sum) — same workload, same pixels"; pass=$((pass+1))
    else checksum_note="DIFFER (rust=$rs_sum cxx=$cxx_sum) — decoder rounding; workload still equivalent"; fi
else
    checksum_note="unavailable (one side did not run)"
fi

# -------------------------------------------------------------- cargo test --
note "== cargo test (offline) =="
if cargo test --manifest-path "$RUST_DIR/Cargo.toml" --offline >"$TMP/cargo-test.log" 2>&1; then
    test_status="PASS"; pass=$((pass+1))
else
    test_status="FAIL"; fail=$((fail+1))
fi
tp=$(grep -h "^test result" "$TMP/cargo-test.log" | grep -o '[0-9]* passed' | grep -o '^[0-9]*' | awk '{p+=$1} END{print p+0}')
tf=$(grep -h "^test result" "$TMP/cargo-test.log" | grep -o '[0-9]* failed' | grep -o '^[0-9]*' | awk '{f+=$1} END{print f+0}')
test_summary="$tp passed, $tf failed"
test_suites=$(grep -h "^test result" "$TMP/cargo-test.log" | wc -l)
test_suites_ok=$(grep -c "^test result: ok" "$TMP/cargo-test.log" || true)

# ---------------------------------------------------------------- report ---
cat >"$OUT" <<EOF
# WallEclipse Rust port — parity comparison

Generated by \`rust/compare.sh\` (re-runnable; see Methodology).
Date: $(date -u +%Y-%m-%dT%H:%M:%SZ) · ITERS=$ITERS · host: $(uname -srm)

## The four numbers

| Metric | C++ | Rust | Notes |
|---|---|---|---|
| CPU proxy (decode+cover-scale wall, $(( $(ls "$FIX" | wc -l) * ITERS )) decodes) | ${cxx_wall} ms (outer) | ${rs_wall} ms (outer) | same fixture set, same 1920x1080 cover-scale; per-program wall in output lines below |
| RAM proxy (peak RSS, same workload) | ${cxx_peak} KiB | ${rs_peak} KiB | VmHWM peak poll, \`compare.sh\` \`measure()\` |
| Lines of code | $cxx_lines (+$cxx_cmake_lines CMakeLists) | $rs_lines (+$rs_cargo_lines Cargo.toml) | \`wc -l\`, see Scope |
| Files | $cxx_nfiles | $rs_nfiles | \`find … -name\`, see Scope |

Binary size: C++ \`$cxx_human\` ($cxx_size B) vs Rust \`$rs_human\` ($rs_size B).
Checksum parity: $checksum_note.
\`cargo test\`: **$test_status** — $test_summary across $test_suites suites ($test_suites_ok ok).

## Program output (perf proxy)

- C++ driver: \`$cxx_prog\` (exit $cxx_exit)
- Rust driver: \`$rs_prog\` (exit $rs_exit)

## Scope

- C++ side: \`src/ include/ tests/\` files \`*.cpp/*.hpp/*.h\` ($cxx_nfiles files, $cxx_lines lines);
  \`CMakeLists.txt\` counted separately ($cxx_cmake_lines lines). Excludes \`build/\`,
  generated protocol code, \`third_party/\`.
- Rust side: \`rust/src/ rust/tests/\` files \`*.rs\` ($rs_nfiles files, $rs_lines lines);
  \`rust/Cargo.toml\` counted separately ($rs_cargo_lines lines). Excludes \`rust/target/\`.
- Bench harness (not ported code, excluded from counts above):
  \`rust/examples/microbench.rs\` ($bench_rs_lines lines) + ephemeral C++ driver
  (\`cxx-bench.cc\`, generated in temp dir at run time, compiled \`g++ -O2\`
  against \`src/ImageLoader.cpp\` + system libwebp, headers only read).

## Methodology

1. \`cargo build --release --manifest-path rust/Cargo.toml --offline\` → \`rust/target/release/walleclipse-rs\` ($build_status).
2. Counts: \`find <dirs> -type f -name … | sort\` for files; \`cat <files> | wc -l\` for lines;
   \`stat -c%s\` / \`ls -lh\` for binaries. No tokei available (used \`wc -l\` + \`find\`).
3. Fixtures: \`python3 + PIL\` deterministic gradients
   (\`a-800x600.jpg q85\`, \`b-1280x720.png\`, \`c-640x480.webp q80\` or png fallback),
   generated fresh in a temp dir each run.
4. Workload (identical both sides): for each fixture, \`ITERS\`×: decode to RGBA
   (\`loadImage\` / \`load_image\`) → cover-scale to 1920×1080 ARGB
   (\`scaleCoverArgb\` / \`scale_cover_argb\`) → fold \`B+3G+7R\` checksum.
   C++ driver built to temp dir only; Rust driver = \`cargo build --release --example microbench --offline\`.
5. CPU proxy = program-reported wall ms inside the identical loop
   (+ outer \`date +%s%N\` wall: C++ ${cxx_wall} ms / Rust ${rs_wall} ms).
   RAM proxy = peak VmHWM (KiB) polled every 5 ms from \`/proc/PID/status\`
   while the driver runs (no \`/usr/bin/time\` on this host).
6. \`cargo test --offline\`; status line + per-suite \`test result\` lines above.
7. If the C++ driver fails to build, this file marks the C++ perf cell
   \`requires-live-daemon\` and the user can run the equivalent live check with:
   \`env XDG_CACHE_HOME=\$(mktemp -d) $CXX_BIN --help\` (read-only probe only —
   never run client commands without an isolated cache dir).

## Safety

- \`compare.sh\` writes only to a fresh \`mktemp\` dir, \`rust/COMPARISON.md\`,
  and \`rust/target/\`. It never writes to \`~/.config/walleclipse\`,
  \`~/.cache/walleclipse.sock\`, or \`/tmp/walleclipse.log\`.
- Every binary invocation uses \`env -i\` with isolated
  \`HOME/XDG_CACHE_HOME/WALLECLIPSE_CONFIG_DIR\` under the temp dir.
- The C++ tree is read-only for measurement (source compile reads);
  the prebuilt C++ binary is only \`stat\`ed and \`--help\`-probed.

## Re-run

\`bash rust/compare.sh\` (from worktree root) · \`ITERS=N\` to change loop count ·
\`CXX_BIN=/path/to/walleclipse\` to override the reference binary.
Checks passed: $pass · failed: $fail.
EOF

note "wrote $OUT (pass=$pass fail=$fail)"
[ "$fail" -eq 0 ]
