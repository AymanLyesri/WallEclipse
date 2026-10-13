//! Hyprland event listener + wallpaper switch orchestrator (port of
//! `HyprListener.hpp/cpp`, merging wallpaper-loop.c semantics):
//! socket2.sock event filter (workspace/focusedmon), 100ms debounce,
//! single-snapshot monitor query, per-monitor workspace state,
//! empty-slot skip with animated-restore.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::image::is_media_wallpaper;
use crate::transition::{SlideDir, slide_dir_for_workspaces};
use crate::util::{exec_capture, split_lines, trim_ws};

/// What to do on a workspace switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchAction {
    Skip,
    ShowStatic,
    ShowAnimated,
    RestoreStatic,
}

/// Parse `hyprctl monitors -j | jq` lines of `"<name> <workspaceId>"`
/// into pairs. Skips blanks, malformed lines, `special` workspaces and
/// `FALLBACK` entries (Hyprland's transient resume state, not a real
/// monitor).
pub fn parse_snapshot_lines(text: &str) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    for raw in split_lines(text) {
        let line = trim_ws(&raw);
        if line.is_empty() {
            continue;
        }
        let Some(sp) = line.rfind(' ') else {
            continue;
        };
        let name = &line[..sp];
        if name.is_empty() || name == "FALLBACK" || name.contains("special") {
            continue;
        }
        if let Ok(ws) = line[sp + 1..].parse::<i32>() {
            out.push((name.to_string(), ws));
        }
    }
    out
}

/// Does a socket2 line merit a wallpaper refresh? `"workspace"`
/// substring (covers `workspacev2`, `moveworkspace`) plus `focusedmon`
/// and monitor add/remove (sleep/resume, hotplug).
pub fn is_refresh_event(line: &str) -> bool {
    ["workspace", "focusedmon", "monitoradded", "monitorremoved"]
        .iter()
        .any(|k| line.contains(k))
}

/// Switch decision. `shown` = path currently displayed (`None` when
/// unknown), `target` = new mapping (`None` = no key, `""` = empty
/// slot), `last_static` = last static path shown on this monitor.
/// `RestoreStatic` re-shows the last static image when the surface is
/// unmapped (an animated wallpaper was showing and the new slot is
/// empty) — otherwise the screen goes black.
pub fn decide(
    shown: &Option<String>,
    target: &Option<String>,
    last_static: &Option<String>,
) -> SwitchAction {
    let showing_animated = shown.as_deref().is_some_and(is_media_wallpaper);
    match target {
        None => {
            if showing_animated
                && last_static.as_deref().is_some_and(|s| !s.is_empty())
            {
                SwitchAction::RestoreStatic
            } else {
                SwitchAction::Skip
            }
        }
        Some(t) if t.is_empty() => {
            if showing_animated
                && last_static.as_deref().is_some_and(|s| !s.is_empty())
            {
                SwitchAction::RestoreStatic
            } else {
                SwitchAction::Skip
            }
        }
        Some(t) => {
            if shown.as_deref() == Some(t.as_str()) {
                SwitchAction::Skip
            } else if is_media_wallpaper(t) {
                SwitchAction::ShowAnimated
            } else {
                SwitchAction::ShowStatic
            }
        }
    }
}

/// Default snapshot query: `hyprctl monitors -j` piped through `jq`.
pub fn default_list_monitors() -> Vec<(String, i32)> {
    parse_snapshot_lines(&exec_capture(
        "hyprctl monitors -j | jq -r '.[] | \"\\(.name) \\(.activeWorkspace.id)\"'",
    ))
}

fn socket2_path() -> Option<String> {
    let runt = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if runt.is_empty() || sig.is_empty() {
        return None;
    }
    Some(format!("{runt}/hypr/{sig}/.socket2.sock"))
}

/// Blocking-event-loop listener. Handlers are wired by `main()`; state
/// is behind a `Mutex` so `note_shown` (IPC `set` bypass) stays
/// accurate across threads.
pub struct HyprListener {
    list_monitors: Arc<dyn Fn() -> Vec<(String, i32)> + Send + Sync>,
    lookup: Arc<dyn Fn(&str, i32) -> Option<String> + Send + Sync>,
    show_static: Arc<dyn Fn(&str, &str, SlideDir, i32, i32) + Send + Sync>,
    show_animated: Arc<dyn Fn(&str, &str, SlideDir, i32, i32) + Send + Sync>,
    on_info: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
    on_error: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
    state: Mutex<ListenerState>,
}

#[derive(Default)]
struct ListenerState {
    last_workspace: HashMap<String, i32>,
    current_wallpaper: HashMap<String, String>,
    last_static_wallpaper: HashMap<String, String>,
}

/// Per-monitor update computed lock-free, applied under re-lock in
/// iteration order (matches the C++ sequential semantics).
struct Pending {
    monitor: String,
    ws: i32,
    /// Path now displayed (`None` = unchanged, i.e. Skip).
    current: Option<String>,
    /// New last-static path (`Some` only when a static show updates it).
    last_static: Option<String>,
}

fn opt_of(map: &HashMap<String, String>, key: &str) -> Option<String> {
    map.get(key).cloned()
}

impl HyprListener {
    pub fn new(
        list_monitors: Arc<dyn Fn() -> Vec<(String, i32)> + Send + Sync>,
        lookup: Arc<dyn Fn(&str, i32) -> Option<String> + Send + Sync>,
        show_static: Arc<dyn Fn(&str, &str, SlideDir, i32, i32) + Send + Sync>,
        show_animated: Arc<dyn Fn(&str, &str, SlideDir, i32, i32) + Send + Sync>,
    ) -> Self {
        Self {
            list_monitors,
            lookup,
            show_static,
            show_animated,
            on_info: None,
            on_error: None,
            state: Mutex::new(ListenerState::default()),
        }
    }

    pub fn with_on_info(
        mut self,
        on_info: Arc<dyn Fn(&str, &str) + Send + Sync>,
    ) -> Self {
        self.on_info = Some(on_info);
        self
    }

    pub fn with_on_error(
        mut self,
        on_error: Arc<dyn Fn(&str, &str) + Send + Sync>,
    ) -> Self {
        self.on_error = Some(on_error);
        self
    }

    fn info(&self, where_: &str, msg: &str) {
        if let Some(f) = &self.on_info {
            f(where_, msg);
        }
    }

    fn error(&self, where_: &str, msg: &str) {
        if let Some(f) = &self.on_error {
            f(where_, msg);
        }
    }

    fn handle_refresh(&self) {
        let pairs = (self.list_monitors)();
        if pairs.is_empty() {
            self.info("change_wallpaper", "empty monitor snapshot; keeping previous");
            return;
        }
        // Snapshot state, then decide and invoke callbacks lock-free: a
        // slow render never blocks note_shown, and a re-entrant callback
        // (e.g. show -> note_shown, as IPC `set` does) cannot deadlock.
        // Local maps are seeded from the snapshot and updated as we go,
        // so even a duplicate-monitor snapshot behaves sequentially.
        let (mut last_ws, mut current, mut last_static) = {
            let state = self.state.lock().unwrap();
            (
                state.last_workspace.clone(),
                state.current_wallpaper.clone(),
                state.last_static_wallpaper.clone(),
            )
        };
        let mut pending: Vec<Pending> = Vec::new();
        let mut seen = HashSet::new();
        for (monitor, ws) in &pairs {
            seen.insert(monitor.clone());
            if last_ws.get(monitor) == Some(ws) {
                continue; // unchanged workspace
            }
            let target = (self.lookup)(monitor, *ws);
            if target.is_none() {
                self.info(
                    "change_wallpaper",
                    &format!("No wallpaper config for '{monitor}' workspace {ws}"),
                );
            }
            let from_ws = last_ws.get(monitor).copied();
            let dir: SlideDir = slide_dir_for_workspaces(from_ws, *ws);
            let from_id = from_ws.unwrap_or(*ws);
            let mut cur: Option<String> = None;
            let mut sta: Option<String> = None;
            match decide(
                &opt_of(&current, monitor),
                &target,
                &opt_of(&last_static, monitor),
            ) {
                SwitchAction::Skip => {}
                SwitchAction::ShowStatic => {
                    let t = target.clone().unwrap();
                    (self.show_static)(monitor, &t, dir, from_id, *ws);
                    sta = Some(t.clone());
                    cur = Some(t);
                }
                SwitchAction::ShowAnimated => {
                    let t = target.clone().unwrap();
                    (self.show_animated)(monitor, &t, dir, from_id, *ws);
                    cur = Some(t);
                }
                SwitchAction::RestoreStatic => {
                    // Target empty: re-show the last static path. The
                    // mapping still points at the video; record what's
                    // displayed so returning to it re-spawns the player.
                    let ls = last_static.get(monitor).cloned().unwrap();
                    (self.show_static)(monitor, &ls, dir, from_id, *ws);
                    cur = Some(ls);
                }
            }
            if let Some(s) = &sta {
                last_static.insert(monitor.clone(), s.clone());
            }
            if let Some(c) = &cur {
                current.insert(monitor.clone(), c.clone());
            }
            last_ws.insert(monitor.clone(), *ws);
            pending.push(Pending {
                monitor: monitor.clone(),
                ws: *ws,
                current: cur,
                last_static: sta,
            });
        }
        // Re-lock: apply the computed updates in order, then prune
        // disconnected monitors so a reconnect forces a repaint
        // instead of being skipped as "unchanged workspace".
        {
            let mut state = self.state.lock().unwrap();
            for p in pending {
                if let Some(s) = p.last_static {
                    state.last_static_wallpaper.insert(p.monitor.clone(), s);
                }
                if let Some(c) = p.current {
                    state.current_wallpaper.insert(p.monitor.clone(), c);
                }
                state.last_workspace.insert(p.monitor, p.ws);
            }
            let gone: Vec<String> = state
                .last_workspace
                .keys()
                .filter(|m| !seen.contains(*m))
                .cloned()
                .collect();
            for m in gone {
                state.last_workspace.remove(&m);
                state.current_wallpaper.remove(&m);
                state.last_static_wallpaper.remove(&m);
            }
        }
    }

    /// One-shot: query monitors once and apply current wallpapers.
    pub fn apply_current(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.last_workspace.clear();
            state.current_wallpaper.clear();
        }
        self.handle_refresh();
    }

    /// Record an out-of-band show (IPC `set` bypasses the event flow);
    /// keeps animated-restore state accurate.
    pub fn note_shown(&self, monitor: &str, path: &str) {
        let mut state = self.state.lock().unwrap();
        state.current_wallpaper.insert(monitor.to_string(), path.to_string());
        if !is_media_wallpaper(path) {
            state.last_static_wallpaper.insert(monitor.to_string(), path.to_string());
        }
    }

    /// Blocking event loop (returns on socket EOF/error or when the
    /// Hyprland runtime env is missing; failures report via on_error,
    /// mirroring the C++ `onError("socket", ...)` call sites).
    pub fn run(&self) {
        self.apply_current();
        let Some(sock) = socket2_path() else {
            self.error(
                "socket",
                "XDG_RUNTIME_DIR/HYPRLAND_INSTANCE_SIGNATURE not set",
            );
            return;
        };
        // Rust builds the socket inside `connect`, and `BufReader`
        // construction is infallible, so one site covers the C++
        // socket()/connect()/fdopen() failures.
        let stream = match std::os::unix::net::UnixStream::connect(&sock) {
            Ok(s) => s,
            Err(e) => {
                self.error("socket", &format!("connect {sock}: {e}"));
                return;
            }
        };
        let reader = std::io::BufReader::new(stream);
        let mut last_run = Instant::now() - Duration::from_secs(10);
        for line in reader.lines().map_while(Result::ok) {
            if !is_refresh_event(&line) {
                continue;
            }
            let elapsed = last_run.elapsed();
            if elapsed < Duration::from_millis(100) {
                std::thread::sleep(Duration::from_millis(100) - elapsed);
            }
            last_run = Instant::now();
            self.handle_refresh();
        }
    }
}
