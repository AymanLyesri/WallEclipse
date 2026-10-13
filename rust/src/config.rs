use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::util::{expand_home, trim_ws};

/// Reads/writes per-monitor `w-<id>=<path>` defaults.conf files,
/// byte-compatible with ArchEclipse's wallpaper-daemon scripts.
/// Writes are serialized with flock on `<config>.lock`, matching
/// set-wallpaper.sh semantics (mirrors ConfigStore in ConfigStore.cpp).
pub struct ConfigStore {
    base_dir: PathBuf,
}

impl ConfigStore {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
        }
    }

    /// `$WALLECLIPSE_CONFIG_DIR` or `~/.config/walleclipse/config`.
    pub fn default_location() -> Self {
        if let Ok(dir) = env::var("WALLECLIPSE_CONFIG_DIR") {
            if !dir.is_empty() {
                return Self::new(dir);
            }
        }
        let home = env::var("HOME").unwrap_or_default();
        Self::new(PathBuf::from(home).join(".config/walleclipse/config"))
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn expand_path(input: &str) -> String {
        expand_home(input)
    }

    fn monitor_conf(&self, monitor: &str) -> PathBuf {
        self.base_dir.join(monitor).join("defaults.conf")
    }

    /// Ensure `config/<monitor>/defaults.conf` exists (seeded from
    /// `config/defaults.conf`, else an empty w-1..w-10 template) for
    /// every known monitor.
    pub fn ensure_monitors<I, S>(&self, monitors: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let _ = fs::create_dir_all(&self.base_dir);
        let seed = self.base_dir.join("defaults.conf");
        for mon in monitors {
            let mon = mon.as_ref();
            if mon.is_empty() {
                continue;
            }
            let conf = self.monitor_conf(mon);
            if let Some(parent) = conf.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(meta) = fs::metadata(&conf) {
                if meta.len() != 0 {
                    continue;
                }
            }
            if seed.exists() {
                let _ = fs::copy(&seed, &conf);
            } else if let Ok(mut f) = File::create(&conf) {
                use std::io::Write as _;
                for i in 1..=10 {
                    let _ = writeln!(f, "w-{i}=");
                }
            }
        }
    }

    /// Subdirectories of the base dir (one per known monitor).
    pub fn monitors(&self) -> Vec<String> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.base_dir) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            // metadata (not file_type) follows symlinks, like C++ `e.is_directory(ec)`.
            if fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false) {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        out
    }

    pub fn wallpapers_for(&self, monitor: &str) -> BTreeMap<i32, String> {
        let mut out = BTreeMap::new();
        let content = fs::read_to_string(self.monitor_conf(monitor)).unwrap_or_default();
        for line in content.lines() {
            if let Some((ws, path)) = parse_wallpaper_line(line) {
                out.insert(ws, path);
            }
        }
        out
    }

    pub fn get_wallpaper(&self, monitor: &str, workspace: i32) -> Option<String> {
        self.wallpapers_for(monitor).remove(&workspace)
    }

    /// flock-guarded read-modify-write. Creates the `w-<ws>` key if missing.
    pub fn set_wallpaper(&self, monitor: &str, workspace: i32, path: &str) -> bool {
        let conf = self.monitor_conf(monitor);
        if let Some(parent) = conf.parent() {
            if fs::create_dir_all(parent).is_err() {
                return false;
            }
        }

        let lock_path = conf.with_extension("conf.lock");
        let lock_file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(_) => return false,
        };
        if lock_file.lock_exclusive().is_err() {
            return false;
        }

        let mut lines: Vec<String> = Vec::new();
        if let Ok(content) = fs::read_to_string(&conf) {
            for line in content.lines() {
                lines.push(line.strip_suffix('\r').unwrap_or(line).to_string());
            }
        }

        let replacement = format!("w-{workspace}={path}");
        let prefix = format!("w-{workspace}=");
        match lines.iter_mut().find(|l| l.starts_with(&prefix)) {
            Some(slot) => *slot = replacement,
            None => lines.push(replacement),
        }

        let mut out = String::new();
        for l in &lines {
            out.push_str(l);
            out.push('\n');
        }
        let ok = fs::write(&conf, out).is_ok();

        let _ = FileExt::unlock(&lock_file);
        ok
    }

    /// Workspace-switch transition, persisted in `<baseDir>/transition`.
    /// Missing or invalid content reads back as `"slide"`.
    pub fn get_transition(&self) -> String {
        let mut content = String::new();
        let line = match File::open(self.base_dir.join("transition"))
            .and_then(|mut f| f.read_to_string(&mut content).map(|_| ()))
        {
            Ok(()) => content.lines().next().unwrap_or("").to_string(),
            Err(_) => return "slide".to_string(),
        };
        let clean = trim_ws(&line);
        if is_valid_transition(&clean) {
            clean
        } else {
            "slide".to_string()
        }
    }

    /// Validates against `slide|fade|none`; rejects unknown names.
    pub fn set_transition(&self, name: &str) -> bool {
        let clean = trim_ws(name);
        if !is_valid_transition(&clean) {
            return false;
        }
        if fs::create_dir_all(&self.base_dir).is_err() {
            return false;
        }
        fs::write(self.base_dir.join("transition"), format!("{clean}\n")).is_ok()
    }
}

fn is_valid_transition(name: &str) -> bool {
    matches!(name, "slide" | "fade" | "none")
}

/// Parse one `w-<id>=<path>` line: strip a trailing `\r` (CRLF files),
/// require the `w-` prefix and `=`, integer key like C++ `stoi`
/// (leading whitespace/sign/digits), raw value after `=`.
fn parse_wallpaper_line(line: &str) -> Option<(i32, String)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let rest = line.strip_prefix("w-")?;
    let eq = rest.find('=')?;
    let ws = stoi_prefix(&rest[..eq])?;
    Some((ws, rest[eq + 1..].to_string()))
}

/// C++ `std::stoi` prefix semantics: skip leading whitespace (all ASCII
/// whitespace, like `isspace`), optional sign, then run of digits (at
/// least one digit required).
fn stoi_prefix(s: &str) -> Option<i32> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let len = digits
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    if len == 0 {
        return None;
    }
    let mut val: i64 = digits[..len].parse().ok()?;
    if neg {
        val = -val;
    }
    i32::try_from(val).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stoi_prefix_skips_all_ascii_whitespace() {
        // C++ std::stoi skips isspace (space, \t, \n, \v, \f, \r) before
        // the optional sign + digits.
        assert_eq!(
            parse_wallpaper_line("w-\t3=/x.jpg"),
            Some((3, "/x.jpg".to_string()))
        );
        assert_eq!(
            parse_wallpaper_line("w- \t 4=/y.jpg"),
            Some((4, "/y.jpg".to_string()))
        );
        assert_eq!(
            parse_wallpaper_line("w- 5=/z.jpg"),
            Some((5, "/z.jpg".to_string()))
        );
        assert_eq!(parse_wallpaper_line("w-\t=x.jpg"), None);
    }

    #[test]
    fn monitors_follows_symlinked_dirs() {
        let base = std::env::temp_dir().join(format!(
            "walleclipse-rust-test-monitors-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real-DP-1");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link-DP-2");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let store = ConfigStore::new(&base);
        let mons = store.monitors();
        assert!(mons.contains(&"real-DP-1".to_string()), "{mons:?}");
        assert!(mons.contains(&"link-DP-2".to_string()), "{mons:?}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
