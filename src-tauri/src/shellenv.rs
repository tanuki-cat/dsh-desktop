//! Import the user's login-shell environment.
//!
//! A GUI-launched app inherits launchd's environment, not the shell's, so anything a user
//! exports from .zprofile/.zshrc (API keys, tool paths) never reaches the supervised CLI --
//! the Web GUI then fails with "no API key for provider". Capturing the login shell's env
//! once at startup restores those variables without us parsing rc files ourselves.
//!
//! Measured on macOS: the capture costs ~160 ms and returns ~46 variables, zero noise lines.
//! Values are never logged: callers log names only.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Give a slow rc file some room, but never stall startup on it.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

/// The shell must not dictate these: the app owns them, or they are session-local noise.
const RESERVED: &[&str] = &[
    "HOME",
    "SHELL",
    "TMPDIR",
    "SHLVL",
    "PWD",
    "OLDPWD",
    "TERM_SESSION_ID",
    "XPC_SERVICE_NAME",
];

pub fn is_reserved(key: &str) -> bool {
    key == "_" || key.starts_with("DSH_") || RESERVED.contains(&key)
}

/// Parse env output into pairs, dropping anything that is not a plain KEY=VALUE line
/// (interactive rc files may print banners). Later duplicates win.
pub fn parse_env(output: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty() || is_reserved(key) {
            continue;
        }
        let mut chars = key.chars();
        let valid = matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if valid {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}
/// Merge PATH sources, first one first.
///
/// Splitting and joining follows the platform (`;` and drive letters on Windows, `:` and a
/// leading slash elsewhere) — a hand-rolled `split(':')` plus a "must start with /" filter
/// silently reduced a Windows PATH to nothing but the macOS fallback directories.
/// Relative entries are dropped: in a child process an empty entry would mean its cwd.
pub fn merge_path(prefix: &[String], shell_path: Option<&str>, app_path: Option<&str>) -> String {
    let mut parts: Vec<PathBuf> = Vec::new();
    let mut add = |value: &str| {
        for part in std::env::split_paths(value) {
            if part.as_os_str().is_empty() || !part.is_absolute() {
                continue;
            }
            let part = crate::unverbatim(&part);
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
    };
    for dir in prefix {
        if !dir.is_empty() {
            add(dir);
        }
    }
    if let Some(value) = shell_path {
        add(value);
    }
    if let Some(value) = app_path {
        add(value);
    }
    match std::env::join_paths(parts) {
        Ok(joined) => joined.to_string_lossy().to_string(),
        // A path containing `"` cannot be joined on Windows; the app's own PATH beats none.
        Err(_) => app_path.unwrap_or_default().to_string(),
    }
}

/// Capture the login shell's exported environment. None when the shell fails or hangs.
/// Returns the flag set that worked, for diagnostics.
pub fn import(shell: &Path) -> Option<(String, BTreeMap<String, String>)> {
    for flag in ["-lic", "-lc"] {
        if let Some(output) = capture(shell, flag) {
            let map = parse_env(&output);
            if !map.is_empty() {
                return Some((flag.to_string(), map));
            }
        }
    }
    None
}

fn capture(shell: &Path, flag: &str) -> Option<String> {
    let mut child = Command::new(shell)
        .args([flag, "env"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().ok()?;
                return if status.success() { Some(output) } else { None };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(40)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_pairs_and_ignores_noise() {
        let output = "PATH=/usr/bin:/bin\nbanner without equals\nDEEPSEEK_API_KEY=sk-abc=def\n\n1BAD=x\n=empty\n";
        let map = parse_env(output);
        assert_eq!(map.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(
            map.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("sk-abc=def")
        );
        assert!(!map.contains_key("1BAD"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn drops_reserved_and_app_owned_variables() {
        let output = "HOME=/Users/x\nTMPDIR=/tmp\nDSH_HOME=/Users/x/.dsh\n_=/usr/bin/env\nGOOD=1\n";
        let map = parse_env(output);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("GOOD"));
    }

    #[test]
    fn last_duplicate_wins() {
        let map = parse_env("KEY=first\nKEY=second\n");
        assert_eq!(map.get("KEY").map(String::as_str), Some("second"));
    }

    #[test]
    fn merges_path_with_absolute_deduplication() {
        // Entries and separator follow the platform: a Unix spelling is not absolute on
        // Windows, which is how a Windows child ended up with a POSIX-only PATH.
        let (opt, usr, local, bin, sep) = if cfg!(windows) {
            (
                r"C:\homebrew\bin",
                r"C:\usr\bin",
                r"C:\usr\local\bin",
                r"C:\bin",
                ";",
            )
        } else {
            (
                "/opt/homebrew/bin",
                "/usr/bin",
                "/usr/local/bin",
                "/bin",
                ":",
            )
        };
        let merged = merge_path(
            &[opt.to_string(), usr.to_string()],
            Some(&format!("{local}{sep}relative{sep}{usr}{sep}")),
            Some(bin),
        );
        assert_eq!(merged, format!("{opt}{sep}{usr}{sep}{local}{sep}{bin}"));
        // A relative entry never survives: in a child it would mean the child's own cwd.
        assert!(!merged.contains("relative"));
    }
}
