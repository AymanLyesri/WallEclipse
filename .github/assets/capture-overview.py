#!/usr/bin/env python3
"""Capture WallEclipse overview: W10 -> W9 -> W7 -> W9 -> W10.

Starts recording on workspace 10, then round-trips 9, 7, 9 and back so the
GIF shows preloaded wallpaper transitions in both directions. The workspaces
keep their existing wallpapers — nothing is applied or changed.

Reference: ~/.github/assets/capture-readme.py (workspace helpers),
~/.github/assets/.togif (ffmpeg mp4 -> gif).

Tunables (env-overridable):
  DELAY=0.5   seconds held on each workspace between switches
  FPS=60      capture rate; the gif is snapped to a representable rate <= FPS
              (GIF delays are whole centiseconds, so 60 -> 50 for uniform frames)

Usage:
  capture-overview.py [--dry-run] [--no-replace] [--output out.gif]
  DELAY=1 FPS=30 python3 capture-overview.py     # per-run override
  Default output: .github/assets/overview.gif (repo root of this checkout).
"""

import argparse
import glob
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.stdout.reconfigure(line_buffering=True)  # keep progress visible when piped

REPO = Path(__file__).resolve().parents[2]
DEFAULT_OUT = REPO / ".github" / "assets" / "overview.gif"
WORKSPACES = (10, 9, 7, 9, 10)

DELAY = float(os.environ.get("DELAY", "0.5"))  # seconds between workspace switches
FPS = int(os.environ.get("FPS", "60"))  # capture rate; gif snaps to a representable rate <= FPS


def run(cmd, timeout=15):
    try:
        return subprocess.run(
            [str(x) for x in cmd],
            capture_output=True, text=True, timeout=timeout, check=True,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError) as e:
        raise RuntimeError(f"Command failed: {cmd}: {getattr(e, 'stderr', '') or e}") from e


SOFTWARE_ARGS = ["-p", "preset=veryfast", "-p", "crf=23", "-p", "tune=zerolatency"]


def recorder_args(fps):
    """wf-recorder args mirroring scripts/screenrecord.sh.

    `-r <fps>` forces constant framerate, and `-D` (no-damage) captures every
    frame: damage-based capture misses frames during a workspace slide (the
    compositor does not signal damage per animation frame), which shows up as a
    ~100ms freeze then a jump. Encode on the GPU (VAAPI) when available.
    """
    render = sorted(glob.glob("/dev/dri/renderD*"))
    if render and "h264_vaapi" in run(["ffmpeg", "-hide_banner", "-encoders"]):
        return [
            "-c", "h264_vaapi", "-d", render[0],
            "-F", "scale_vaapi=format=nv12", "-D", "-r", str(fps),
        ], True
    return SOFTWARE_ARGS + ["-D", "-r", str(fps)], False


def gif_fps(fps):
    """Largest exactly-representable GIF rate <= fps.

    GIF frame delays are integer centiseconds, so only 100/n fps plays
    uniformly; anything else makes ffmpeg alternate e.g. 1cs/2cs frames, which
    judders (and sub-2cs delays are clamped by some viewers). Snapping 60 -> 50
    gives every frame a clean 2cs delay.
    """
    for d in range(1, 101):
        if 100 / d <= fps:
            return 100 // d
    return 1


def start_recorder(args, out):
    """Start wf-recorder detached from the terminal.

    `-y` overwrites the temp path (it already exists from NamedTemporaryFile)
    and stdin=DEVNULL makes the recorder never block on an overwrite prompt.
    """
    return subprocess.Popen(
        ["wf-recorder", "-y", *args, "-f", out],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def focused_monitor():
    import json
    mons = json.loads(run(["hyprctl", "monitors", "-j"]))
    focused = [m for m in mons if m.get("focused")]
    if len(focused) != 1:
        raise RuntimeError("Expected one focused monitor")
    return focused[0]["name"]


def active_workspace():
    import json
    info = json.loads(run(["hyprctl", "activeworkspace", "-j"]))
    if not isinstance(info.get("id"), int):
        raise RuntimeError("Unexpected active workspace: " + str(info)[:200])
    return info["id"]


def ensure_workspace(ws, timeout=10):
    run(["hyprctl", "dispatch", f"hl.dsp.focus({{workspace = {ws}}})"])
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if active_workspace() == ws:
            return
        time.sleep(0.2)
    raise RuntimeError(f"Workspace {ws} did not become active")


def workspace_wallpaper(monitor, ws):
    for line in run(["walleclipse", "list"]).splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0] == monitor and parts[1] == str(ws):
            return parts[2].strip() or None
    return None


def parse_cli(argv):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--scale", type=int, default=640, help="gif width (ref .togif uses 320; overview uses 640)")
    ap.add_argument("--output", default=str(DEFAULT_OUT))
    ap.add_argument("--no-replace", dest="replace", action="store_false", default=True)
    ap.add_argument("--dry-run", action="store_true")
    return ap.parse_args(argv)


def main(argv=None):
    args = parse_cli(argv)
    monitor = focused_monitor()
    walls = {ws: workspace_wallpaper(monitor, ws) for ws in WORKSPACES}
    missing = [ws for ws, p in walls.items() if not p]
    if missing:
        raise RuntimeError(f"No walleclipse mapping for {monitor} ws{missing}")

    print(f"Monitor: {monitor} | delay {DELAY:g}s | capture {FPS}fps | gif {gif_fps(FPS)}fps")
    for ws in dict.fromkeys(WORKSPACES):
        print(f"  W{ws}: {walls[ws]}")

    path = " -> ".join(f"W{ws}" for ws in WORKSPACES)
    if args.dry_run:
        print(f"DRY-RUN: would record {path} (delay {DELAY:g}s, {FPS}fps)")
        print(f"DRY-RUN: would write {args.output}")
        return 0

    previous = active_workspace()
    mp4 = tempfile.NamedTemporaryFile(suffix=".mp4", delete=False).name
    rec = None
    try:
        ensure_workspace(WORKSPACES[0])
        time.sleep(DELAY)

        rec_args, hw = recorder_args(FPS)
        print(f"Recording to {mp4} ({'vaapi' if hw else 'x264'} @ {FPS}fps)", flush=True)
        rec = start_recorder(rec_args, mp4)
        time.sleep(1.0)
        if rec.poll() is not None and hw:
            print("VAAPI encode failed; retrying with software x264", flush=True)
            rec = start_recorder(SOFTWARE_ARGS + ["-D", "-r", str(FPS)], mp4)
            time.sleep(1.0)
        if rec.poll() is not None:
            raise RuntimeError("wf-recorder exited immediately")

        # Already on W10: hold while we round-trip 9, 7, 9, back to 10.
        time.sleep(DELAY)
        for ws in WORKSPACES[1:]:
            print(f"Switching to workspace {ws}")
            ensure_workspace(ws)
            time.sleep(DELAY)
    finally:
        if rec is not None and rec.poll() is None:
            rec.send_signal(signal.SIGINT)
            try:
                rec.wait(timeout=10)
            except subprocess.TimeoutExpired:
                rec.kill()
        if previous != active_workspace():
            try:
                ensure_workspace(previous)
            except RuntimeError as e:
                print("WARNING: " + str(e), flush=True)

    if not Path(mp4).is_file() or Path(mp4).stat().st_size == 0:
        raise RuntimeError("Recorder produced no output")

    # GIF delays are integer centiseconds, so only 100/n fps is smooth; snap the
    # requested FPS down (60 -> 50) so every frame gets a uniform 2cs delay.
    rate = gif_fps(FPS)
    vf = f"fps={rate},scale={args.scale}:-1:flags=lanczos"
    out = Path(args.output)
    tmp_gif = out.with_suffix(".tmp.gif")
    run(["ffmpeg", "-y", "-i", mp4, "-vf", vf, "-c:v", "gif", str(tmp_gif)], timeout=120)
    if args.replace:
        out.parent.mkdir(parents=True, exist_ok=True)
        tmp_gif.replace(out)
        print(f"Wrote {out} ({out.stat().st_size // 1024} KiB)")
    else:
        print(f"Preview: {tmp_gif}")
    Path(mp4).unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, KeyboardInterrupt) as e:
        print("ERROR: " + (str(e) or "Interrupted"), file=sys.stderr)
        sys.exit(1)
