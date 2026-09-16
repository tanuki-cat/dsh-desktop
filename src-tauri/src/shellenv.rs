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
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Give a slow rc file some room, but never stall startup on it.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

/// Bytes kept from the `env` dump. Measured at ~46 variables / a few KB; the cap only stops a
/// shell that prints a whole document instead of its environment.
const ENV_LIMIT: usize = 1024 * 1024;

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
    // `output_within` rather than a local reader loop: it bounds the wait for a finished child's
    // pipe (a background process the rc file started inherits stdout and would otherwise hold
    // `join` open indefinitely), caps the bytes, and decodes lossily — `read_to_string` returned
    // `Err` without writing anything when any variable held a non-UTF-8 byte, which emptied the
    // whole import and lost the very API key it exists to carry.
    let out = crate::process::output_within(
        Command::new(shell).args([flag, "env"]),
        CAPTURE_TIMEOUT,
        ENV_LIMIT,
    )?;
    if out.truncated {
        // Names only, like every other line about the environment: the partial line is already
        // gone, so nothing cut mid-value (an API key) is imported.
        crate::harness::app_log(&format!(
            "{} {flag} 的环境输出不完整（管道未关闭或超出上限），只导入完整的行",
            shell.display()
        ));
    }
    out.status
        .filter(|status| status.success())
        .map(|_| out.text)
}
#[cfg(test)]
mod tests;
