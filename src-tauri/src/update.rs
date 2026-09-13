//! Check for and install a newer `dsh` from the npm registry.
//!
//! The shell supervises a globally installed CLI, so "updating the shell's core" means
//! updating that CLI: query dist-tags, compare with prerelease-aware semver, then run
//! `npm install -g <pkg>@<version>` using the npm that belongs to the resolved node
//! (a GUI-launched app has no Homebrew PATH, so npm must be resolved explicitly).

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The CLI this shell supervises.
pub const PACKAGE: &str = "@deepseek-ai/dsh";

/// Registry query budget. Deliberately short: an offline machine should fall back to the
/// installed CLI quickly instead of stalling startup for npm's default timeout.
pub const FETCH_TIMEOUT_MS: u32 = 8_000;

/// A failed query is retried after this many minutes even inside the normal interval.
pub const FAILED_RETRY_MINUTES: u64 = 5;

/// CLI versions this shell has actually been built and tested against. The shell drives the CLI
/// through `--profile web --patch … --no-open --port N` and parses its `dsh web:` line, so a
/// version outside this window may rename a flag or change that line. Saying so at startup beats
/// failing later with a confusing timeout.
pub const TESTED_MIN: &str = "0.1.5-rc.1";
pub const TESTED_MAX_EXCLUSIVE: &str = "0.2.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
}

impl Version {
    pub fn parse(raw: &str) -> Option<Version> {
        let text = raw.trim().trim_start_matches('v');
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').map(|s| s.to_string()).collect()),
            None => (text, Vec::new()),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    fn cmp_pre(a: &[String], b: &[String]) -> Ordering {
        match (a.is_empty(), b.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater, // release outranks prerelease
            (false, true) => Ordering::Less,
            (false, false) => {
                for index in 0..a.len().max(b.len()) {
                    match (a.get(index), b.get(index)) {
                        (None, _) => return Ordering::Less,
                        (_, None) => return Ordering::Greater,
                        (Some(x), Some(y)) => {
                            let xn = x.parse::<u64>().ok();
                            let yn = y.parse::<u64>().ok();
                            let ord = match (xn, yn) {
                                (Some(nx), Some(ny)) => nx.cmp(&ny),
                                (Some(_), None) => Ordering::Less, // numeric < alphanumeric
                                (None, Some(_)) => Ordering::Greater,
                                (None, None) => x.cmp(y),
                            };
                            if ord != Ordering::Equal {
                                return ord;
                            }
                        }
                    }
                }
                Ordering::Equal
            }
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| Version::cmp_pre(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    UpToDate { version: String },
    UpdateAvailable { from: String, to: String },
    Skipped,
    Failed { reason: String },
}

/// How the installed CLI relates to the range this shell was tested against.
#[derive(Debug, PartialEq, Eq)]
pub enum Compatibility {
    Tested,
    Older { version: String },
    Newer { version: String },
    Unknown { version: String },
}

impl Compatibility {
    /// Human-readable reason, for the log and the status page.
    pub fn describe(&self) -> String {
        match self {
            Compatibility::Tested => format!("dsh {} 已在测试区间内", TESTED_MIN),
            Compatibility::Older { version } => format!(
                "dsh {version} 早于本壳测试过的最低版本 {TESTED_MIN}：可能缺少本壳依赖的命令行参数"
            ),
            Compatibility::Newer { version } => format!(
                "dsh {version} 高于本壳测试过的区间（< {TESTED_MAX_EXCLUSIVE}）：上游可能有破坏性变更"
            ),
            Compatibility::Unknown { version } => {
                format!("无法识别 dsh 版本 {version:?}，无法判断是否兼容")
            }
        }
    }
}

/// Compare an installed CLI version against the tested range. Pure: the caller decides whether
/// to warn or refuse.
pub fn compatibility(version: &str) -> Compatibility {
    let Some(parsed) = Version::parse(version) else {
        return Compatibility::Unknown {
            version: version.to_string(),
        };
    };
    // Both constants are literals covered by tests, so parsing cannot fail here.
    let min = Version::parse(TESTED_MIN).expect("TESTED_MIN must parse");
    let max = Version::parse(TESTED_MAX_EXCLUSIVE).expect("TESTED_MAX_EXCLUSIVE must parse");
    if parsed < min {
        Compatibility::Older {
            version: version.to_string(),
        }
    } else if parsed >= max {
        Compatibility::Newer {
            version: version.to_string(),
        }
    } else {
        Compatibility::Tested
    }
}

/// Highest version among the given dist-tags, e.g. `["latest", "next"]`.
pub fn newest_tagged(tags: &[(String, String)]) -> Option<Version> {
    tags.iter().filter_map(|(_, raw)| Version::parse(raw)).max()
}

/// PATH for an npm child process: npm is a `#!/usr/bin/env node` script, and a GUI-launched
/// app inherits launchd PATH, which normally has no `node`. The shebang then died with exit
/// 127 before npm ever ran, so every update check failed. npm sits next to the node that
/// should run it, so that directory goes first.
pub fn npm_path(npm: &Path, existing: Option<&OsStr>) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = npm.parent() {
        if !dir.as_os_str().is_empty() {
            dirs.push(dir.to_path_buf());
        }
    }
    if let Some(value) = existing {
        for dir in std::env::split_paths(value) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    match std::env::join_paths(&dirs) {
        Ok(joined) => joined,
        Err(_) => existing.unwrap_or_default().to_os_string(),
    }
}

/// npm command that starts whatever PATH the app was launched with.
fn npm_command(npm: &Path) -> Command {
    let mut command = Command::new(npm);
    command.env("PATH", npm_path(npm, std::env::var_os("PATH").as_deref()));
    command
}

/// Read `npm view <pkg> dist-tags --json` and return the tags that exist.
pub fn fetch_dist_tags(npm: &Path, package: &str) -> Result<Vec<(String, String)>, String> {
    let timeout = format!("--fetch-timeout={FETCH_TIMEOUT_MS}");
    let output = npm_command(npm)
        .args([
            "view",
            package,
            "dist-tags",
            "--json",
            timeout.as_str(),
            "--fetch-retries=1",
        ])
        .output()
        .map_err(|error| format!("无法执行 npm: {error}"))?;
    if !output.status.success() {
        let tail: String = String::from_utf8_lossy(&output.stderr)
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(format!("npm view 失败: {tail}"));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("dist-tags 解析失败: {error}"))?;
    let mut tags = Vec::new();
    if let Some(object) = parsed.as_object() {
        for (name, value) in object {
            if let Some(text) = value.as_str() {
                tags.push((name.clone(), text.to_string()));
            }
        }
    }
    Ok(tags)
}

/// Decide what a known registry head means for the installed CLI. Pure, so both a fresh
/// answer and a cached one go through the same rules.
pub fn judge(latest: &str, current: &str) -> Status {
    let current_version = match Version::parse(current) {
        Some(version) => version,
        None => {
            return Status::Failed {
                reason: format!("无法解析已安装版本 {current:?}"),
            }
        }
    };
    let latest_version = match Version::parse(latest) {
        Some(version) => version,
        None => {
            return Status::Failed {
                reason: format!("无法解析 registry 版本 {latest:?}"),
            }
        }
    };
    if latest_version > current_version {
        Status::UpdateAvailable {
            from: current.to_string(),
            to: format!("{latest_version}"),
        }
    } else {
        Status::UpToDate {
            version: format!("{latest_version}"),
        }
    }
}

/// Compare the installed CLI against the newest of the configured tags.
pub fn check(npm: &Path, package: &str, wanted_tags: &[String], current: &str) -> Status {
    let tags = match fetch_dist_tags(npm, package) {
        Ok(tags) => tags,
        Err(reason) => return Status::Failed { reason },
    };
    let selected: Vec<(String, String)> = tags
        .into_iter()
        .filter(|(name, _)| wanted_tags.iter().any(|wanted| wanted == name))
        .collect();
    if selected.is_empty() {
        return Status::Failed {
            reason: format!("没有匹配的 dist-tag: {}", wanted_tags.join(",")),
        };
    }
    match newest_tagged(&selected) {
        None => Status::Failed {
            reason: "dist-tag 里没有可解析的版本号".to_string(),
        },
        Some(latest) => judge(&format!("{latest}"), current),
    }
}

/// `npm` that belongs to the same installation as the resolved node.
pub fn npm_for(node: &Path) -> Option<PathBuf> {
    if let Some(dir) = node.parent() {
        // `bin/npm` on Unix, `npm.cmd` beside `node.exe` on Windows. The bare name is checked
        // last: npm also ships a POSIX wrapper next to its `.cmd`, and running that fails
        // with `os error 193`.
        for name in ["npm.cmd", "npm.exe", "npm"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(found) = crate::locator::path_lookup("npm") {
        return Some(found);
    }
    login_shell_npm()
}

/// Last resort: ask the user's login shell where npm is. Unix only.
fn login_shell_npm() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        None
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let output = Command::new(shell)
            .args(["-lc", "command -v npm"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let candidate = PathBuf::from(text);
        candidate.is_file().then_some(candidate)
    }
}

/// Global prefix implied by the location of the installed CLI
/// (`/opt/homebrew/lib/node_modules/@deepseek-ai/dsh` -> `/opt/homebrew`).
pub fn install_prefix(dsh_js: &Path) -> Option<PathBuf> {
    let text = dsh_js.to_string_lossy();
    let marker = "/lib/node_modules/";
    let index = text.find(marker)?;
    Some(PathBuf::from(&text[..index]))
}

fn global_prefix(npm: &Path) -> Option<String> {
    let output = npm_command(npm).args(["prefix", "-g"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Run the global install, mirroring the launcher script: pin the resolved version and, when
/// npm's global prefix differs from where the CLI actually lives, install into that prefix.
pub fn install(
    npm: &Path,
    package: &str,
    version: &str,
    prefix: Option<&Path>,
) -> Result<(), String> {
    let mut command = npm_command(npm);
    command.args(["install", "-g", "--no-fund", "--no-audit"]);
    if let Some(wanted) = prefix {
        let differs = global_prefix(npm)
            .map(|current| current != wanted.to_string_lossy())
            .unwrap_or(true);
        if differs {
            command.arg("--prefix").arg(wanted);
        }
    }
    command.arg(format!("{package}@{version}"));
    let output = command
        .output()
        .map_err(|error| format!("无法执行 npm install: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let tail: String = String::from_utf8_lossy(&output.stderr)
        .lines()
        .take(5)
        .collect::<Vec<_>>()
        .join(" | ");
    Err(format!("npm install 失败: {tail}"))
}

/// Cached outcome of one registry query, so most launches skip the ~1.9s network round trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cache {
    /// Unix seconds when the query ran.
    pub checked_at: u64,
    /// Installed version at that moment; a manual upgrade invalidates the entry.
    pub installed: String,
    /// Newest version seen, or None when the query failed.
    pub latest: Option<String>,
}

impl Cache {
    pub fn is_fresh(&self, now: u64, installed: &str, interval_minutes: u64) -> bool {
        if self.installed != installed {
            return false;
        }
        // A failed query is retried quickly instead of being trusted for the whole interval.
        let ttl_minutes = if self.latest.is_some() {
            interval_minutes
        } else {
            interval_minutes.min(FAILED_RETRY_MINUTES)
        };
        if ttl_minutes == 0 {
            return false;
        }
        now.saturating_sub(self.checked_at) < ttl_minutes * 60
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update-check.json")
}

pub fn read_cache(data_dir: &Path) -> Option<Cache> {
    let raw = std::fs::read_to_string(cache_path(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_cache(data_dir: &Path, cache: &Cache) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(cache_path(data_dir), serde_json::to_vec_pretty(cache)?)
}

/// Result of a cache-aware check.
pub struct Checked {
    pub status: Status,
    /// True when the answer came from the cache instead of the network.
    pub cached: bool,
}

/// Consult the cache first; query the registry only when the entry is stale, then remember
/// the answer. Failures are cached too, with the short retry window from FAILED_RETRY_MINUTES.
pub fn check_cached(
    npm: &Path,
    package: &str,
    wanted_tags: &[String],
    current: &str,
    data_dir: &Path,
    interval_minutes: u64,
) -> Checked {
    let now = now_secs();
    if let Some(cache) = read_cache(data_dir) {
        if cache.is_fresh(now, current, interval_minutes) {
            let status = match cache.latest.as_deref() {
                Some(latest) => judge(latest, current),
                None => Status::Failed {
                    reason: "上次查询失败（缓存结果）".to_string(),
                },
            };
            return Checked {
                status,
                cached: true,
            };
        }
    }
    let status = check(npm, package, wanted_tags, current);
    let latest = match &status {
        Status::UpdateAvailable { to, .. } => Some(to.clone()),
        Status::UpToDate { version } => Some(version.clone()),
        _ => None,
    };
    let _ = write_cache(
        data_dir,
        &Cache {
            checked_at: now,
            installed: current.to_string(),
            latest,
        },
    );
    Checked {
        status,
        cached: false,
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(formatter, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Version {
        Version::parse(raw).expect("version should parse")
    }

    #[test]
    fn orders_numeric_segments_numerically() {
        assert!(parse("1.2.10") > parse("1.2.9"));
        assert!(parse("0.1.5") > parse("0.1.4"));
    }

    #[test]
    fn release_outranks_prerelease() {
        assert!(parse("1.0.0") > parse("1.0.0-rc.1"));
    }

    #[test]
    fn orders_prerelease_identifiers() {
        assert!(parse("0.1.5-rc.2") > parse("0.1.5-rc.1"));
        assert!(parse("0.1.5-rc.1") > parse("0.1.5-alpha.2"));
        assert!(parse("1.0.0-alpha.1") < parse("1.0.0-alpha.beta"));
    }

    #[test]
    fn picks_newest_among_dist_tags() {
        let tags = vec![
            ("alpha".to_string(), "0.1.5-alpha.2".to_string()),
            ("latest".to_string(), "0.1.5-rc.1".to_string()),
            ("next".to_string(), "0.1.5-rc.2".to_string()),
        ];
        let filtered: Vec<(String, String)> = tags
            .into_iter()
            .filter(|(name, _)| name == "latest" || name == "next")
            .collect();
        assert_eq!(newest_tagged(&filtered).unwrap().to_string(), "0.1.5-rc.2");
    }

    #[test]
    fn rejects_malformed_versions() {
        assert!(Version::parse("not-a-version").is_none());
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
    }

    #[test]
    fn judge_reports_updates_and_up_to_date() {
        assert_eq!(
            judge("0.1.5-rc.2", "0.1.5-rc.1"),
            Status::UpdateAvailable {
                from: "0.1.5-rc.1".into(),
                to: "0.1.5-rc.2".into()
            }
        );
        assert_eq!(
            judge("0.1.5-rc.1", "0.1.5-rc.2"),
            Status::UpToDate {
                version: "0.1.5-rc.1".into()
            }
        );
        assert_eq!(
            judge("0.1.5-rc.1", "0.1.5-rc.1"),
            Status::UpToDate {
                version: "0.1.5-rc.1".into()
            }
        );
        assert!(matches!(
            judge("garbage", "0.1.5-rc.1"),
            Status::Failed { .. }
        ));
        assert!(matches!(
            judge("0.1.5-rc.1", "garbage"),
            Status::Failed { .. }
        ));
    }

    #[test]
    fn cache_freshness_follows_interval_and_installed_version() {
        let entry = |latest: Option<&str>, installed: &str| Cache {
            checked_at: 1_000,
            installed: installed.to_string(),
            latest: latest.map(|text| text.to_string()),
        };
        // Inside the window, same installed version -> fresh.
        assert!(entry(Some("0.1.5-rc.2"), "0.1.5-rc.1").is_fresh(
            1_000 + 59 * 60,
            "0.1.5-rc.1",
            60
        ));
        // Past the window -> stale.
        assert!(!entry(Some("0.1.5-rc.2"), "0.1.5-rc.1").is_fresh(
            1_000 + 61 * 60,
            "0.1.5-rc.1",
            60
        ));
        // Manual upgrade changed the installed version -> stale.
        assert!(!entry(Some("0.1.5-rc.2"), "0.1.5-rc.1").is_fresh(1_000 + 60, "0.1.6", 60));
        // Interval 0 disables caching entirely.
        assert!(!entry(Some("0.1.5-rc.2"), "0.1.5-rc.1").is_fresh(1_000, "0.1.5-rc.1", 0));
        // A failed query is retried after the short window, not the full interval.
        assert!(!entry(None, "0.1.5-rc.1").is_fresh(1_000 + 6 * 60, "0.1.5-rc.1", 360));
        assert!(entry(None, "0.1.5-rc.1").is_fresh(1_000 + 4 * 60, "0.1.5-rc.1", 360));
    }

    #[test]
    fn cache_round_trips_on_disk() {
        let dir = std::env::temp_dir().join("dsh-desktop-update-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache {
            checked_at: 42,
            installed: "0.1.5-rc.1".into(),
            latest: Some("0.1.5-rc.2".into()),
        };
        write_cache(&dir, &cache).unwrap();
        assert_eq!(read_cache(&dir), Some(cache));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compatibility_flags_versions_outside_the_tested_range() {
        assert_eq!(compatibility(TESTED_MIN), Compatibility::Tested);
        assert_eq!(compatibility("0.1.5-rc.2"), Compatibility::Tested);
        assert_eq!(compatibility("0.1.9"), Compatibility::Tested);
        assert!(matches!(
            compatibility("0.1.4"),
            Compatibility::Older { .. }
        ));
        assert!(matches!(
            compatibility("0.2.0"),
            Compatibility::Newer { .. }
        ));
        assert!(matches!(
            compatibility("1.0.0"),
            Compatibility::Newer { .. }
        ));
        assert!(matches!(
            compatibility("未知"),
            Compatibility::Unknown { .. }
        ));
        // The range must stay a real range, or every version would be "Newer".
        assert!(
            Version::parse(TESTED_MIN).unwrap() < Version::parse(TESTED_MAX_EXCLUSIVE).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn npm_path_puts_the_node_directory_first() {
        let existing = OsStr::new("/usr/bin:/opt/homebrew/bin");
        let path = npm_path(Path::new("/opt/homebrew/bin/npm"), Some(existing));
        assert_eq!(path.to_string_lossy(), "/opt/homebrew/bin:/usr/bin");
        // No PATH at all still yields the directory npm itself lives in.
        assert_eq!(
            npm_path(Path::new("/opt/homebrew/bin/npm"), None).to_string_lossy(),
            "/opt/homebrew/bin"
        );
    }

    #[test]
    fn derives_install_prefix_from_cli_path() {
        let prefix = install_prefix(Path::new(
            "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        ));
        assert_eq!(prefix, Some(PathBuf::from("/opt/homebrew")));
        assert_eq!(install_prefix(Path::new("/tmp/plain/bin.js")), None);
    }
}
