use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// `$XDG_CACHE_HOME` or `~/.cache` (mirrors `cacheBaseDir()` in Util.cpp).
pub fn cache_base_dir() -> String {
    if let Ok(c) = env::var("XDG_CACHE_HOME") {
        if !c.is_empty() {
            return c;
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".cache")
        .to_string_lossy()
        .into_owned()
}

/// Expand a leading `~` or `$HOME` (mirrors `expandHome()` in Util.cpp).
pub fn expand_home(input: &str) -> String {
    let home = env::var("HOME").unwrap_or_default();
    if let Some(rest) = input.strip_prefix('~') {
        return format!("{home}{rest}");
    }
    if let Some(rest) = input.strip_prefix("$HOME") {
        return format!("{home}{rest}");
    }
    input.to_string()
}

/// Strip leading/trailing `" \t\r\n"` (mirrors `trimWs()` in Util.cpp).
pub fn trim_ws(s: &str) -> String {
    s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n')
        .to_string()
}

/// Split on `'\n'` keeping the C++ shape: a trailing newline yields a
/// final empty element (mirrors `splitLines()` in Util.cpp).
pub fn split_lines(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    s.split('\n').map(str::to_string).collect()
}

/// Run `cmd` via the shell and capture stdout (mirrors `execCapture()`
/// in Util.cpp, which uses `popen` + read-all).
pub fn exec_capture(cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Run `args[0]` with `args[1..]` directly (no shell), wait for exit,
/// silence stdio. Returns true on exit status 0 (mirrors `runWait()`
/// in Util.cpp, which fork+execs and waits).
pub fn run_wait(args: &[&str]) -> bool {
    if args.is_empty() {
        return false;
    }
    match Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Spawn `args` detached so it outlives us, with silent stdio (mirrors
/// `spawnDetached()` in Util.cpp). Implemented via an intermediate
/// `sh -c '... &'` re-parent (double-fork parity: no zombie, no shell
/// quoting risk since args travel as positional parameters).
pub fn spawn_detached(args: &[&str]) {
    if args.is_empty() {
        return;
    }
    let _ = Command::new("sh")
        .arg("-c")
        .arg("exec \"$@\" >/dev/null 2>&1 </dev/null &")
        .arg("walleclipse-detach")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
