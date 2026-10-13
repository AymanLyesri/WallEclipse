use std::env;
use std::path::PathBuf;
use std::process::Command;

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
