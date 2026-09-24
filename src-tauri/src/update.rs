//! Check for and install a newer `dsh` from the npm registry.
//!
//! The shell supervises a globally installed CLI, so "updating the shell's core" means
//! updating that CLI: query dist-tags, compare with prerelease-aware semver, then run
//! `npm install -g <pkg>@<version>` using the npm that belongs to the resolved node
//! (a GUI-launched app has no Homebrew PATH, so npm must be resolved explicitly).

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
/// Whole-process budget for an npm/pnpm operation.
///
/// `--fetch-timeout` only bounds one registry request. npm still hangs on a proxy that accepts
/// and stalls, a DNS server that never answers, a lock held by another npm, a lifecycle script
/// waiting on stdin, or a child of its own that outlives it — none of which the fetch budget
/// covers. The supervised Harness is already stopped when these run, so an unbounded call leaves
/// the user with no window and no Harness until they kill the app.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// A shorter budget for a read-only registry query: the same stalls apply, and the startup path
/// is blocked on the answer.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the last of a killed command's output before giving up on it.
///
/// The bytes are only ever used to explain a failure, so a straggler must not become a second
/// unbounded wait after the first one was already cut short.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Bytes kept from the command's stdout.
///
/// The only reader is `npm view … --json`, whose document is a few hundred bytes; the cap exists
/// so that a package manager which prints for its whole budget cannot turn that budget into
/// resident memory. Nothing here needs the whole stream, so an over-cap stdout is reported as a
/// failure to parse rather than being buffered.
const STDOUT_LIMIT: usize = 4 * 1024 * 1024;

/// Bytes kept from the command's stderr.
///
/// Only a handful of lines ever reach the user (`Output::summary`), so this is generous: it is
/// the difference between a diagnostic that names the failure and one that does not.
const STDERR_LIMIT: usize = 1024 * 1024;

/// A bounded slice of one child pipe.
struct Captured {
    /// The newest bytes read, at most the pipe's limit.
    bytes: Vec<u8>,
    /// True when the pipe produced more than `bytes` holds.
    truncated: bool,
}

impl Captured {
    fn empty() -> Self {
        Captured {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    /// The first `lines` lines of what was kept, joined, marked when it is not the whole story.
    fn summary(&self, lines: usize) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        let head: String = text.lines().take(lines).collect::<Vec<_>>().join(" | ");
        if self.truncated {
            format!("{head} | …（输出过长，已截断）")
        } else {
            head
        }
    }
}

/// What one finished command produced.
struct Output {
    status: std::process::ExitStatus,
    stdout: Captured,
    stderr: Captured,
}

/// Run a command with a whole-process timeout, killing it and its children on expiry.
///
/// `Command::output()` waits for ever, so a hung npm parks the startup thread and leaves the user
/// on a splash page with no way out. The child gets its own process group (its own console group on
/// Windows) so the kill reaches whatever npm spawned: signalling only the parent leaves a node
/// child holding the registry connection.
fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法执行命令: {error}"))?;
    let pid = child.id();

    // Drain both pipes on their own threads: a child that fills a pipe buffer blocks for ever,
    // and the timeout below would then fire on a process that was only waiting to be read.
    let stdout = child.stdout.take().map(|pipe| drain(pipe, STDOUT_LIMIT));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, STDERR_LIMIT));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                crate::process::kill_tree(pid);
                let _ = child.wait();
                return Err(format!("命令超过 {} 秒未结束，已终止", timeout.as_secs()));
            }
            Err(error) => {
                crate::process::kill_tree(pid);
                let _ = child.wait();
                return Err(format!("等待命令结束失败: {error}"));
            }
        }
    };
    Ok(Output {
        status,
        stdout: collected(stdout),
        stderr: collected(stderr),
    })
}

/// Read one of the child's pipes to the end on its own thread, reporting through a channel.
///
/// A channel rather than a `JoinHandle`: the reader ends when the last writer closes the pipe, and
/// a grandchild that survived the kill would hold it open for ever. Waiting on a join there would
/// reintroduce exactly the unbounded wait the timeout exists to remove, so the caller bounds it.
///
/// The pipe is still read to the end — a reader that stopped early would leave the child blocked
/// on a full pipe for the rest of its budget — but only `limit` bytes are kept.
fn drain<R: Read + Send + 'static>(mut pipe: R, limit: usize) -> mpsc::Receiver<Captured> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut captured = Captured::empty();
        let mut chunk = [0u8; 8 * 1024];
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let room = limit.saturating_sub(captured.bytes.len());
            let take = room.min(read);
            captured.bytes.extend_from_slice(&chunk[..take]);
            if take < read {
                captured.truncated = true;
            }
        }
        let _ = tx.send(captured);
    });
    rx
}

/// Whatever a drain thread managed to read before its budget ran out.
fn collected(rx: Option<mpsc::Receiver<Captured>>) -> Captured {
    rx.and_then(|rx| rx.recv_timeout(DRAIN_GRACE).ok())
        .unwrap_or_else(Captured::empty)
}

/// The dist-tags this shell follows by default: every channel upstream publishes to.
///
/// `latest` alone is what this shell shipped with, and it is not enough: upstream publishes
/// ahead of it. Measured 2026-09-18, the registry answered `{"latest":"0.1.5-rc.2",
/// "alpha":"0.1.6-alpha.2"}` — the alpha tag held the newest version on the registry and the
/// shell never looked at it.
///
/// `next` is in the list for the same reason, and it is not an edge case: which channel leads
/// changes from release to release, so a fixed subset of them goes blind whenever the newest
/// build lands in one it left out. Measured 2026-09-24, `{"latest":"0.1.5-rc.3",
/// "next":"0.1.7-rc.1","alpha":"0.1.7-alpha.2"}` — `next` led both other tags and was the only
/// version newer than the installed `0.1.7-alpha.2`, so a shell reading `latest` and `alpha`
/// reported "up to date" while an update existed.
///
/// A tag the registry does not publish is not a failure as long as one of the others matches
/// (see [`check`]), which is what lets one list serve both the CLI and the plugin market:
/// `dshmarket` publishes no `alpha`, so it is simply filtered out there.
///
/// This reverses the 2026-09-15 convergence to `["latest"]` and the 2026-09-18 exclusion of
/// `next`; the original rationale and why it no longer holds are in §13.4 of the design
/// document. `require_tested_dsh` is unchanged and still bounds what may be installed: a
/// version outside the tested range is reported, not installed.
pub fn default_tags() -> Vec<String> {
    vec![
        "latest".to_string(),
        "next".to_string(),
        "alpha".to_string(),
    ]
}

/// The CLI this shell supervises.
pub const PACKAGE: &str = "@deepseek-ai/dsh";

/// Registry query budget. Deliberately short: an offline machine should fall back to the
/// installed CLI quickly instead of stalling startup for npm's default timeout.
pub const FETCH_TIMEOUT_MS: u32 = 8_000;

/// A failed query is retried after this many minutes even inside the normal interval. Also the
/// first retry window of a failed install; repeated failures of the same version wait longer
/// (see [`failure_window_secs`]).
pub const FAILED_RETRY_MINUTES: u64 = 5;

/// The longest a repeatedly failing version is kept from being retried.
const FAILED_RETRY_CAP_MINUTES: u64 = 24 * 60;

/// How long a failure marker is remembered after its retry window has passed.
///
/// Longer than the window on purpose: the count of failures has to survive the retry it allowed, or
/// every retry would start again from the shortest wait — which is how a version that can never
/// boot got downloaded, swapped in and rolled back every five minutes. A week later the slate is
/// clean, so an environment problem that has since been fixed is not held against the version.
const FAILED_MEMORY_SECS: u64 = 7 * 24 * 60 * 60;

/// Seconds a failure marker suppresses another attempt, given how often that version has failed.
///
/// Five minutes after the first failure — most failures are a registry hiccup or a locked profile —
/// then four times longer for every repeat, up to a day. `0` is a marker written before the count
/// existed and reads as one failure.
pub fn failure_window_secs(failures: u32) -> u64 {
    let repeats = failures.saturating_sub(1).min(8);
    FAILED_RETRY_MINUTES
        .saturating_mul(4u64.saturating_pow(repeats))
        .min(FAILED_RETRY_CAP_MINUTES)
        * 60
}

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

/// May the shell install `to`, given whether it would refuse to run it afterwards?
///
/// `auto_update` and `require_tested_dsh` pull in opposite directions the moment `latest` moves
/// past the tested window. Installing first and refusing to boot afterwards is the worst of the
/// two outcomes: the user is left with a CLI this shell just wrote there and will not start, and
/// the previous, working version is already gone. The install is therefore bounded by the same
/// range the startup check enforces — what will not be run is not installed.
pub fn may_install(to: &str, require_tested: bool) -> bool {
    !require_tested || matches!(compatibility(to), Compatibility::Tested)
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
    let fetch = format!("--fetch-timeout={FETCH_TIMEOUT_MS}");
    let mut command = npm_command(npm);
    command.args([
        "view",
        package,
        "dist-tags",
        "--json",
        fetch.as_str(),
        "--fetch-retries=1",
    ]);
    // A whole-process budget on top of `--fetch-timeout`: the flag bounds one request, while a
    // stalled proxy or a registry that never answers can park the startup path indefinitely.
    let output =
        run_with_timeout(command, QUERY_TIMEOUT).map_err(|error| format!("npm view: {error}"))?;
    if !output.status.success() {
        return Err(format!("npm view 失败: {}", output.stderr.summary(3)));
    }
    // An over-cap stdout was cut, so parsing it would report a syntax error instead of the
    // real problem: the command printed far more than a dist-tags document.
    if output.stdout.truncated {
        return Err(format!(
            "npm view 输出超过 {} MiB，已截断，无法解析 dist-tags",
            STDOUT_LIMIT / (1024 * 1024)
        ));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout.bytes)
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
        // A login shell sources the user's rc files, so it can hang on anything they can; the
        // budget is the same one a registry query gets.
        let text = crate::process::stdout_within(
            Command::new(shell).args(["-lc", "command -v npm"]),
            QUERY_TIMEOUT,
        )?;
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
    // `npm prefix` is local, but npm still starts a node process that can stall on a lock or a
    // proxy the way the install does, and this runs while the user waits on the splash page.
    let text =
        crate::process::stdout_within(npm_command(npm).args(["prefix", "-g"]), QUERY_TIMEOUT)?;
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
    // A whole-process budget as well as npm's own fetch flags: lifecycle scripts, a lock held
    // by another npm, or a registry that stops answering mid-install all outlive `--fetch-timeout`
    // and would otherwise park this call for ever — with the Harness already stopped.
    command.arg(format!("--fetch-timeout={FETCH_TIMEOUT_MS}"));
    command.arg("--fetch-retries=1");
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
    let output = run_with_timeout(command, INSTALL_TIMEOUT)
        .map_err(|error| format!("npm install: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!("npm install 失败: {}", output.stderr.summary(5)))
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
    // pnpm runs under this call and installs into the user's profile: same budget as the core
    // install, for the same reason (it runs after the Harness was stopped).
    let output = run_with_timeout(command, INSTALL_TIMEOUT)
        .map_err(|error| format!("dsh plugin add: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!("dsh plugin add 失败: {}", output.stderr.summary(5)))
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
    /// The dist-tags this answer was produced from.
    ///
    /// A different set of tags is a different question — an answer built from `latest` alone says
    /// nothing about whether `alpha` has moved — so the entry may not be reused across a change.
    /// Absent in cache files written before this field existed, which reads as "some other set of
    /// tags" and is what makes a changed default take effect on the next launch rather than after
    /// the interval expires.
    #[serde(default)]
    pub tags: Vec<String>,
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
    /// How many times in a row `failed` has failed; sets the retry window.
    #[serde(default)]
    pub failures: u32,
}

impl Cache {
    pub fn is_fresh(
        &self,
        now: u64,
        installed: &str,
        tags: &[String],
        interval_minutes: u64,
    ) -> bool {
        if self.installed != installed {
            return false;
        }
        if self.tags.as_slice() != tags {
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
        // Saturating: `update_check_interval_minutes` comes from config.json, and a large value
        // would otherwise overflow — a panic in a debug build, a wrap in a release one.
        now.saturating_sub(self.checked_at) < ttl_minutes.saturating_mul(60)
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
                    && now.saturating_sub(self.failed_at) < failure_window_secs(self.failures)
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
    /// True when the last install of this answer failed and its retry window has not passed yet.
    /// Weaker than `attempted`: the caller must leave it alone *this* launch, but the window runs
    /// out on its own — a failed install is usually worth retrying. The window grows with each
    /// repeat failure of the same version (see [`failure_window_secs`]).
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
        if cache.is_fresh(now, current, wanted_tags, interval_minutes) {
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
        tags: wanted_tags.to_vec(),
        attempted,
        failed: failed.as_ref().map(|marker| marker.version.clone()),
        failed_at: failed.as_ref().map(|marker| marker.at).unwrap_or(0),
        failures: failed.as_ref().map(|marker| marker.failures).unwrap_or(0),
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

/// A remembered failed install: which version, when it last failed, and how often in a row.
#[derive(Debug, PartialEq, Eq)]
struct FailureMarker {
    version: String,
    at: u64,
    failures: u32,
}

/// Which "the install failed" marker a fresh registry answer should carry forward.
///
/// Like [`carried_attempt`] it only survives the same version — a new release is a new
/// decision. It outlives its retry window (the window only decides whether *this* launch may try
/// again) so that a repeat failure can lengthen the next wait, and it is forgotten after
/// [`FAILED_MEMORY_SECS`], so an old failure cannot pin the shell to an old version.
fn carried_failure(
    previous: Option<&Cache>,
    latest: Option<&str>,
    now: u64,
) -> Option<FailureMarker> {
    let previous = previous?;
    let failed = previous.failed.clone()?;
    let same_version = latest == Some(failed.as_str());
    let remembered = now.saturating_sub(previous.failed_at) < FAILED_MEMORY_SECS;
    (same_version && remembered).then_some(FailureMarker {
        version: failed,
        at: previous.failed_at,
        failures: previous.failures,
    })
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

/// The core counterpart: this version was staged and swapped in, and the tree never booted.
///
/// Without it the cache still reads `installed = from, latest = to`, so every later launch
/// decides the update is available again — re-downloading the whole tree, swapping it in,
/// waiting out the startup timeout and rolling back, once per launch, for ever. The window is
/// the same short one a failed plugin install gets: a version that failed once may well have
/// failed for a transient reason.
pub fn mark_core_attempt_failed(data_dir: &Path, latest: &str) {
    mark_failed_in(&cache_path(data_dir), latest);
}

fn mark_failed_in(cache_file: &Path, latest: &str) {
    let Some(mut cache) = read_cache_at(cache_file) else {
        return;
    };
    // A repeat of the same version lengthens the next wait; anything else starts the count over.
    // A marker from before the count existed (0) is one earlier failure.
    cache.failures = if cache.failed.as_deref() == Some(latest) {
        cache.failures.max(1).saturating_add(1)
    } else {
        1
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
mod tests;
