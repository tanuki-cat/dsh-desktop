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

/// Executable names `pnpm` can have on this platform, in the order a shell would try them.
#[cfg(windows)]
const PNPM_NAMES: &[&str] = &["pnpm.cmd", "pnpm.exe", "pnpm.bat", "pnpm"];
#[cfg(not(windows))]
const PNPM_NAMES: &[&str] = &["pnpm"];

/// `pnpm` as the CLI's own plugin command resolves it: the first executable match on `path`.
///
/// `dsh plugin add` is a thin wrapper around pnpm (the CLI just spawns it in the profile
/// directory), so a plugin install can only work when this returns something. The shell has to
/// know that *before* it stops the running Harness: without pnpm the CLI exits 127 and the
/// launch loses its session for nothing (review A1).
pub fn find_pnpm(path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        for name in PNPM_NAMES {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// A file this process can actually execute. On Unix the exec bit decides: a merely readable
/// `pnpm` would fail inside the CLI with EACCES instead of resolving.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
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
    cache: Option<&Path>,
) -> Result<(), String> {
    let mut command = npm_command(npm);
    command.args(["install", "-g", "--no-fund", "--no-audit"]);
    // The dependency tree is ~289 MB: point npm at this app's own cache instead of the
    // user's global one (plan §4, review P2-11).
    if let Some(dir) = cache {
        command.arg("--cache").arg(dir);
    }
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

/// The plugin market this shell keeps current alongside the CLI.
///
/// It is a profile's only installation entry (the bundled template ships it, plan §2.5), so a
/// stale copy is the difference between "you can install plugins" and "you cannot".
pub const MARKET_PLUGIN: &str = "dshmarket";

/// The version of `package` installed in a dsh profile, when it is there.
///
/// A profile is a pnpm project: `node_modules/<package>/package.json` is what the CLI loads,
/// while the range in the profile's own `package.json` is only the user's intent.
pub fn installed_plugin(profile_dir: &Path, package: &str) -> Option<String> {
    let manifest = read_package_json(&profile_dir.join("node_modules").join(package))?;
    let version = manifest.get("version")?.as_str()?.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// Does the profile still declare `package`? A plugin the user removed must not be reinstalled
/// by the updater.
pub fn declares_plugin(profile_dir: &Path, package: &str) -> bool {
    read_package_json(profile_dir)
        .and_then(|manifest| manifest.get("dependencies")?.get(package).cloned())
        .is_some()
}

fn read_package_json(dir: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Add or update a plugin through the CLI's own plugin command.
///
/// `dsh plugin --profile <name> add <package>@<version>` forwards to pnpm inside the profile
/// directory, so the CLI owns the profile layout and the pnpm invocation; the shell only decides
/// *when* to run it (after the instance was stopped, exactly like a core update).
///
/// `path` is the caller's assembled PATH, not the app's own: pnpm ships in the bundled tools
/// prefix, which is not on a Finder-launched app's PATH (review A1).
pub fn install_plugin(
    node: &Path,
    dsh_js: &Path,
    profile: &str,
    package: &str,
    version: &str,
    dsh_home: Option<&Path>,
    path: &OsStr,
) -> Result<(), String> {
    let mut command = Command::new(node);
    command
        .arg(dsh_js)
        .arg("plugin")
        .arg("--profile")
        .arg(profile)
        .arg("add")
        .arg(format!("{package}@{version}"))
        // The CLI forwards to pnpm, another `#!/usr/bin/env node` script: node's directory has
        // to lead PATH for the same reason `npm` needs it (see `npm_path`).
        .env("PATH", npm_path(node, Some(path)));
    if let Some(home) = dsh_home {
        command.env("DSH_HOME", home);
    }
    let output = command
        .output()
        .map_err(|error| format!("无法执行 dsh plugin: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let tail: String = String::from_utf8_lossy(&output.stderr)
        .lines()
        .take(5)
        .collect::<Vec<_>>()
        .join(" | ");
    Err(format!("dsh plugin add 失败: {tail}"))
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
    /// Latest version whose install was already attempted while the supervised CLI stayed put
    /// (npm wrote the package somewhere this shell does not run it from). Suppresses a second
    /// install of the same answer; newer than existing cache files, hence optional on the wire.
    #[serde(default)]
    pub attempted: Option<String>,
    /// Version whose install *failed* outright — the CLI could not run it at all — with the
    /// time of that failure. A failure is usually transient (a registry hiccup, a locked
    /// profile, a broken pnpm), so unlike [`Cache::attempted`] this marker expires after
    /// [`FAILED_RETRY_MINUTES`]: the shell tries again instead of leaving the version behind
    /// for as long as the registry head stays put.
    #[serde(default)]
    pub failed: Option<String>,
    #[serde(default)]
    pub failed_at: u64,
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

    /// True when `status` is the cached "update available" answer whose install was already
    /// attempted and changed nothing.
    pub fn attempted_install(&self, status: &Status) -> bool {
        match status {
            Status::UpdateAvailable { to, .. } => self.attempted.as_deref() == Some(to.as_str()),
            _ => false,
        }
    }

    /// True when `status` is an update whose install failed recently enough that the shell
    /// should not stop the Harness for it yet.
    pub fn failed_recently(&self, now: u64, status: &Status) -> bool {
        match status {
            Status::UpdateAvailable { to, .. } => {
                self.failed.as_deref() == Some(to.as_str())
                    && now.saturating_sub(self.failed_at) < FAILED_RETRY_MINUTES * 60
            }
            _ => false,
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the core CLI's cached registry answer lives.
pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update-check.json")
}

/// The plugin check keeps its own file: a plugin answer must not consume the core's cache
/// window (or the other way round), and the two compare different installed versions.
pub fn plugin_cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("plugin-check.json")
}

pub fn read_cache(data_dir: &Path) -> Option<Cache> {
    read_cache_at(&cache_path(data_dir))
}

pub fn write_cache(data_dir: &Path, cache: &Cache) -> std::io::Result<()> {
    write_cache_at(&cache_path(data_dir), cache)
}

fn read_cache_at(path: &Path) -> Option<Cache> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache_at(path: &Path, cache: &Cache) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(cache)?)
}

/// Result of a cache-aware check.
pub struct Checked {
    pub status: Status,
    /// True when the answer came from the cache instead of the network.
    pub cached: bool,
    /// True when this answer already led to an install that changed nothing, so the caller
    /// must report it without installing again.
    pub attempted: bool,
    /// True when the last install of this answer failed and its short retry window has not
    /// passed yet. Weaker than `attempted`: the caller must leave it alone *this* launch, but
    /// the marker disappears on its own — a failed install is usually worth retrying.
    pub failed_recently: bool,
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
    check_cached_at(
        &cache_path(data_dir),
        npm,
        package,
        wanted_tags,
        current,
        interval_minutes,
    )
}

/// The same check against the plugin cache file (the plugin market, see [`MARKET_PLUGIN`]).
pub fn check_plugin_cached(
    npm: &Path,
    package: &str,
    wanted_tags: &[String],
    current: &str,
    data_dir: &Path,
    interval_minutes: u64,
) -> Checked {
    check_cached_at(
        &plugin_cache_path(data_dir),
        npm,
        package,
        wanted_tags,
        current,
        interval_minutes,
    )
}

fn check_cached_at(
    cache_file: &Path,
    npm: &Path,
    package: &str,
    wanted_tags: &[String],
    current: &str,
    interval_minutes: u64,
) -> Checked {
    let now = now_secs();
    let previous = read_cache_at(cache_file);
    if let Some(cache) = previous.as_ref() {
        if cache.is_fresh(now, current, interval_minutes) {
            let status = match cache.latest.as_deref() {
                Some(latest) => judge(latest, current),
                None => Status::Failed {
                    reason: "上次查询失败（缓存结果）".to_string(),
                },
            };
            let attempted = cache.attempted_install(&status);
            let failed_recently = cache.failed_recently(now, &status);
            return Checked {
                status,
                cached: true,
                attempted,
                failed_recently,
            };
        }
    }
    let status = check(npm, package, wanted_tags, current);
    let latest = match &status {
        Status::UpdateAvailable { to, .. } => Some(to.clone()),
        Status::UpToDate { version } => Some(version.clone()),
        _ => None,
    };
    // An expired window is not news about the CLI: keep the marker while the answer is still the
    // version that was already tried, so a mismatched npm prefix does not reinstall for ever.
    let attempted = carried_attempt(previous.as_ref(), latest.as_deref());
    let failed = carried_failure(previous.as_ref(), latest.as_deref(), now);
    let fresh = Cache {
        checked_at: now,
        installed: current.to_string(),
        latest,
        attempted,
        failed: failed.as_ref().map(|(version, _)| version.clone()),
        failed_at: failed.as_ref().map(|(_, at)| *at).unwrap_or(0),
    };
    // Asked of the answer that is about to be stored, so both paths through this function
    // answer the caller the same way.
    let suppress = fresh.attempted_install(&status);
    let failed_recently = fresh.failed_recently(now, &status);
    let _ = write_cache_at(cache_file, &fresh);
    Checked {
        status,
        cached: false,
        attempted: suppress,
        failed_recently,
    }
}

/// Which "already attempted" marker a fresh registry answer should carry forward.
///
/// Only a different version reopens the install decision; a failed query learned nothing new,
/// so it carries the marker too.
pub fn carried_attempt(previous: Option<&Cache>, latest: Option<&str>) -> Option<String> {
    let tried = previous?.attempted.clone()?;
    match latest {
        None => Some(tried),
        Some(latest) if latest == tried => Some(tried),
        Some(_) => None,
    }
}

/// Which "the install failed" marker a fresh registry answer should carry forward.
///
/// Like [`carried_attempt`] it only survives the same version — a new release is a new
/// decision — but it is also dropped once its window has passed, so a transient failure cannot
/// pin the shell to an old version until the next release.
fn carried_failure(
    previous: Option<&Cache>,
    latest: Option<&str>,
    now: u64,
) -> Option<(String, u64)> {
    let previous = previous?;
    let failed = previous.failed.clone()?;
    let same_version = latest == Some(failed.as_str());
    let in_window = now.saturating_sub(previous.failed_at) < FAILED_RETRY_MINUTES * 60;
    (same_version && in_window).then_some((failed, previous.failed_at))
}

/// Remember that installing `latest` did not change the CLI this shell runs, so the cached
/// answer is reported but not installed again.
pub fn mark_attempt_ineffective(data_dir: &Path, latest: &str) {
    mark_attempt_in(&cache_path(data_dir), latest);
}

/// The plugin counterpart of [`mark_attempt_ineffective`].
pub fn mark_plugin_attempt_ineffective(data_dir: &Path, latest: &str) {
    mark_attempt_in(&plugin_cache_path(data_dir), latest);
}

/// Remember that installing the plugin market `latest` failed. The install ran and the CLI
/// exited with an error (network, registry, a pnpm that is there but broken), which is usually
/// transient: this suppresses the next attempt only for [`FAILED_RETRY_MINUTES`], so a bad
/// minute does not cost the project every later release of the plugin.
pub fn mark_plugin_attempt_failed(data_dir: &Path, latest: &str) {
    mark_failed_in(&plugin_cache_path(data_dir), latest);
}

fn mark_failed_in(cache_file: &Path, latest: &str) {
    let Some(mut cache) = read_cache_at(cache_file) else {
        return;
    };
    cache.failed = Some(latest.to_string());
    cache.failed_at = now_secs();
    let _ = write_cache_at(cache_file, &cache);
}

fn mark_attempt_in(cache_file: &Path, latest: &str) {
    let Some(mut cache) = read_cache_at(cache_file) else {
        return;
    };
    cache.attempted = Some(latest.to_string());
    let _ = write_cache_at(cache_file, &cache);
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
            attempted: None,
            failed: None,
            failed_at: 0,
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
            attempted: Some("0.1.5-rc.2".into()),
            failed: None,
            failed_at: 0,
        };
        write_cache(&dir, &cache).unwrap();
        assert_eq!(read_cache(&dir), Some(cache));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache file written before the `attempted` field existed must keep working.
    #[test]
    fn cache_files_without_the_attempt_field_still_parse() {
        let dir = std::env::temp_dir().join("dsh-desktop-update-cache-legacy-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            cache_path(&dir),
            r#"{"checked_at":42,"installed":"0.1.5-rc.1","latest":"0.1.5-rc.2"}"#,
        )
        .unwrap();
        let cache = read_cache(&dir).expect("legacy cache must parse");
        assert_eq!(cache.attempted, None);
        assert_eq!(cache.failed, None);
        assert_eq!(cache.failed_at, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window expiring must not reopen an install that already proved useless.
    #[test]
    fn carried_attempt_follows_the_registry_answer() {
        let cache = |attempted: Option<&str>, latest: Option<&str>| Cache {
            checked_at: 42,
            installed: "0.1.5-rc.1".into(),
            latest: latest.map(str::to_string),
            attempted: attempted.map(str::to_string),
            failed: None,
            failed_at: 0,
        };
        assert_eq!(
            carried_attempt(
                Some(&cache(Some("0.1.5-rc.2"), Some("0.1.5-rc.2"))),
                Some("0.1.5-rc.2")
            ),
            Some("0.1.5-rc.2".to_string())
        );
        assert_eq!(
            carried_attempt(Some(&cache(Some("0.1.5-rc.2"), None)), None),
            Some("0.1.5-rc.2".to_string())
        );
        assert_eq!(
            carried_attempt(
                Some(&cache(Some("0.1.5-rc.2"), Some("0.1.5-rc.2"))),
                Some("0.1.5-rc.3")
            ),
            None
        );
        assert_eq!(carried_attempt(None, Some("0.1.5-rc.2")), None);
        assert_eq!(
            carried_attempt(Some(&cache(None, Some("0.1.5-rc.2"))), Some("0.1.5-rc.2")),
            None
        );
    }

    /// End to end through `check_cached`: an expired window on a machine whose npm writes the
    /// package elsewhere must not reinstall.
    #[cfg(unix)]
    #[test]
    fn an_expired_window_does_not_reopen_an_ineffective_install() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("dsh-desktop-update-window-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A stand-in for npm: `npm view <pkg> dist-tags --json` prints the file below.
        let tags = dir.join("dist-tags.json");
        let npm = dir.join("npm");
        std::fs::write(&npm, format!("#!/bin/sh\ncat {}\n", tags.display())).unwrap();
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();

        let window_ago = now_secs() - 2 * 60 * 60;
        let seed = |latest: &str| Cache {
            checked_at: window_ago,
            installed: "0.1.5-rc.1".into(),
            latest: Some(latest.into()),
            attempted: Some("0.1.5-rc.2".into()),
            failed: None,
            failed_at: 0,
        };
        let wanted = vec!["latest".to_string()];
        std::fs::write(&tags, r#"{"latest":"0.1.5-rc.2"}"#).unwrap();
        write_cache(&dir, &seed("0.1.5-rc.2")).unwrap();

        let checked = check_cached(&npm, PACKAGE, &wanted, "0.1.5-rc.1", &dir, 60);
        assert!(
            !checked.cached,
            "the window is over, so the registry is queried"
        );
        assert!(checked.attempted, "the same answer must not install again");
        assert_eq!(
            read_cache(&dir).unwrap().attempted.as_deref(),
            Some("0.1.5-rc.2"),
            "the marker survives the new query"
        );

        // A new version reopens the decision.
        std::fs::write(&tags, r#"{"latest":"0.1.5-rc.3"}"#).unwrap();
        write_cache(&dir, &seed("0.1.5-rc.2")).unwrap();
        let checked = check_cached(&npm, PACKAGE, &wanted, "0.1.5-rc.1", &dir, 60);
        assert!(!checked.attempted, "a new version is a new decision");
        assert_eq!(read_cache(&dir).unwrap().attempted, None);

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// The repeat-install bug: npm wrote the package somewhere this shell does not run it
    /// from, so the version never changes and every launch inside the cache window used to
    /// stop the Harness and rebuild the tree.
    #[test]
    fn an_ineffective_install_is_not_retried_inside_the_window() {
        let dir = std::env::temp_dir().join("dsh-desktop-update-attempt-test");
        let _ = std::fs::remove_dir_all(&dir);
        // The cached branch never executes npm, so the path only has to exist as a value.
        let npm = Path::new("/nonexistent/npm");
        let tags = vec!["latest".to_string()];

        // What check_cached stores when it finds an update.
        write_cache(
            &dir,
            &Cache {
                checked_at: now_secs(),
                installed: "0.1.5-rc.1".into(),
                latest: Some("0.1.5-rc.2".into()),
                attempted: None,
                failed: None,
                failed_at: 0,
            },
        )
        .unwrap();
        let first = check_cached(npm, PACKAGE, &tags, "0.1.5-rc.1", &dir, 60);
        assert!(first.cached);
        assert!(
            !first.attempted,
            "a fresh answer must be allowed to install"
        );

        // The install ran and the supervised CLI did not change.
        mark_attempt_ineffective(&dir, "0.1.5-rc.2");
        let second = check_cached(npm, PACKAGE, &tags, "0.1.5-rc.1", &dir, 60);
        assert!(second.cached);
        assert!(
            second.attempted,
            "the next launch in the same window must not install again"
        );
        assert_eq!(
            second.status,
            Status::UpdateAvailable {
                from: "0.1.5-rc.1".into(),
                to: "0.1.5-rc.2".into()
            }
        );

        // A newer version than the one attempted is a new decision, not a repeat.
        write_cache(
            &dir,
            &Cache {
                checked_at: now_secs(),
                installed: "0.1.5-rc.1".into(),
                latest: Some("0.1.5-rc.3".into()),
                attempted: Some("0.1.5-rc.2".into()),
                failed: None,
                failed_at: 0,
            },
        )
        .unwrap();
        let third = check_cached(npm, PACKAGE, &tags, "0.1.5-rc.1", &dir, 60);
        assert!(!third.attempted, "only the attempted version is suppressed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed install is not the same as an install that changed nothing: the first is
    /// usually transient, so it must not pin the plugin (or the CLI) to an old version until
    /// the registry head moves on (review A5).
    #[test]
    fn a_failed_install_only_suppresses_its_short_window() {
        let dir = std::env::temp_dir().join("dsh-desktop-plugin-failed-attempt-test");
        let _ = std::fs::remove_dir_all(&dir);
        // The cached branch never executes npm, so the path only has to exist as a value.
        let npm = Path::new("/nonexistent/npm");
        let tags = vec!["latest".to_string()];
        let now = now_secs();
        let cache = |failed_at: u64| Cache {
            checked_at: now,
            installed: "1.45.1".into(),
            latest: Some("1.46.1".into()),
            attempted: None,
            failed: Some("1.46.1".into()),
            failed_at,
        };

        // Just failed: this launch must report the update without stopping the Harness.
        let plugin_cache = plugin_cache_path(&dir);
        write_cache_at(&plugin_cache, &cache(now)).unwrap();
        let just_failed = check_plugin_cached(npm, MARKET_PLUGIN, &tags, "1.45.1", &dir, 60);
        assert!(just_failed.failed_recently);
        assert!(
            !just_failed.attempted,
            "a failure is not the long-lived ineffective marker"
        );

        // What the shell records when the install itself fails: the failure marker only, never
        // the long-lived "environment is wrong" one.
        mark_plugin_attempt_failed(&dir, "1.46.1");
        let stored = read_cache_at(&plugin_cache).unwrap();
        assert_eq!(stored.failed.as_deref(), Some("1.46.1"));
        assert_eq!(stored.attempted, None);
        assert!(
            stored.failed_at > 0,
            "the marker carries the time it was set"
        );

        // Past the short window the shell tries again, even though the answer is still cached.
        write_cache_at(&plugin_cache, &cache(now - FAILED_RETRY_MINUTES * 60 - 1)).unwrap();
        let later = check_plugin_cached(npm, MARKET_PLUGIN, &tags, "1.45.1", &dir, 60);
        assert!(later.cached, "the answer itself is still inside its window");
        assert!(
            !later.failed_recently,
            "one bad minute must not pin this version for ever"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_attempt_is_carried_only_inside_its_window() {
        let failed = |at: u64| Cache {
            checked_at: at,
            installed: "1.45.1".into(),
            latest: Some("1.46.1".into()),
            attempted: None,
            failed: Some("1.46.1".into()),
            failed_at: at,
        };

        // Same version, inside the window: the marker survives a fresh registry answer.
        assert_eq!(
            carried_failure(Some(&failed(1_000)), Some("1.46.1"), 1_000 + 60),
            Some(("1.46.1".to_string(), 1_000))
        );
        // Window over: dropped, so the next launch may install.
        assert_eq!(
            carried_failure(
                Some(&failed(1_000)),
                Some("1.46.1"),
                1_000 + FAILED_RETRY_MINUTES * 60
            ),
            None
        );
        // A newer version is a new decision, not a repeat.
        assert_eq!(
            carried_failure(Some(&failed(1_000)), Some("1.46.2"), 1_000),
            None
        );
        assert_eq!(carried_failure(None, Some("1.46.1"), 1_000), None);
    }

    #[test]
    fn the_market_plugin_is_read_from_the_profile_it_would_load() {
        let dir = std::env::temp_dir().join("dsh-desktop-plugin-profile-test");
        let _ = std::fs::remove_dir_all(&dir);
        let plugin = dir.join("node_modules").join(MARKET_PLUGIN);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"dsh-profile-web","dependencies":{"dshmarket":"^1.45.1"}}"#,
        )
        .unwrap();
        std::fs::write(plugin.join("package.json"), r#"{"version":"1.46.1"}"#).unwrap();

        assert!(declares_plugin(&dir, MARKET_PLUGIN));
        // The installed version (what the CLI loads) wins over the range in the profile.
        assert_eq!(
            installed_plugin(&dir, MARKET_PLUGIN).as_deref(),
            Some("1.46.1")
        );

        std::fs::remove_file(plugin.join("package.json")).unwrap();
        assert_eq!(installed_plugin(&dir, MARKET_PLUGIN), None);

        // A plugin the user removed must not be reinstalled by the updater.
        std::fs::write(dir.join("package.json"), r#"{"dependencies":{}}"#).unwrap();
        assert!(!declares_plugin(&dir, MARKET_PLUGIN));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plugin_check_keeps_its_own_cache_window() {
        let dir = std::env::temp_dir().join("dsh-desktop-plugin-cache-test");
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(cache_path(&dir), plugin_cache_path(&dir));

        // The core cache holds an update for the CLI; the plugin window is separate.
        write_cache(
            &dir,
            &Cache {
                checked_at: now_secs(),
                installed: "0.1.5-rc.1".into(),
                latest: Some("0.1.5-rc.2".into()),
                attempted: None,
                failed: None,
                failed_at: 0,
            },
        )
        .unwrap();
        let core_before = std::fs::read_to_string(cache_path(&dir)).unwrap();

        std::fs::write(
            plugin_cache_path(&dir),
            serde_json::to_vec(&Cache {
                checked_at: now_secs(),
                installed: "1.45.1".into(),
                latest: Some("1.46.1".into()),
                attempted: None,
                failed: None,
                failed_at: 0,
            })
            .unwrap(),
        )
        .unwrap();

        // A fresh plugin answer is served from the plugin file without touching the network
        // (`npm` here does not exist) and without consuming the core answer.
        let checked = check_plugin_cached(
            Path::new("/nonexistent/npm"),
            MARKET_PLUGIN,
            &["latest".to_string()],
            "1.45.1",
            &dir,
            60,
        );
        assert!(checked.cached);
        assert_eq!(
            checked.status,
            Status::UpdateAvailable {
                from: "1.45.1".into(),
                to: "1.46.1".into()
            }
        );
        assert_eq!(
            std::fs::read_to_string(cache_path(&dir)).unwrap(),
            core_before,
            "the core cache must not be touched by a plugin check"
        );

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
    fn find_pnpm_takes_the_first_executable_match_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("dsh-desktop-find-pnpm-test");
        let empty = root.join("empty");
        let good = root.join("good");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        let pnpm = good.join("pnpm");
        std::fs::write(&pnpm, "#!/bin/sh").unwrap();

        let path = std::env::join_paths([&empty, &good]).unwrap();
        // Readable but not executable: the CLI would die with EACCES instead of resolving it.
        assert_eq!(find_pnpm(Some(&path)), None);

        let mut mode = std::fs::metadata(&pnpm).unwrap().permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&pnpm, mode).unwrap();
        assert_eq!(find_pnpm(Some(&path)), Some(pnpm.clone()));

        // A directory of that name is not a command either.
        let dir_like = root.join("dir-like");
        std::fs::create_dir_all(dir_like.join("pnpm")).unwrap();
        let only_a_directory = std::env::join_paths([&dir_like]).unwrap();
        assert_eq!(find_pnpm(Some(&only_a_directory)), None);

        // The still-valid match survives the checks above, and no PATH means no pnpm.
        assert_eq!(find_pnpm(Some(&path)), Some(pnpm));
        assert_eq!(find_pnpm(None), None);
        assert_eq!(find_pnpm(Some(OsStr::new(""))), None);

        let _ = std::fs::remove_dir_all(&root);
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
