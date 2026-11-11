//! Keep the desktop shell itself current, independently of the `dsh` core it supervises.
//!
//! Two update paths already exist in this repo and both are about the supervised runtime: the CLI
//! is kept current through the npm registry (`update` / `update_flow`) and the plugin market
//! through the profile. This module is the third object — the `DSH Desktop` executable or `.app`
//! the user is looking at — and it shares none of their state: its own cache file, its own
//! failure window, its own log lines.
//!
//! The Tauri updater plugin is deliberately not used. It wants a signed manifest (`latest.json`
//! plus a minisign signature) and, on macOS, a `.app.tar.gz`; this repository publishes unsigned
//! `.app.zip` / `.deb` / portable `.zip` artifacts through `release.yml`, so adopting the plugin
//! would either stall the release pipeline or ship a verification path that can never pass. What
//! is used instead is the channel that already exists: the GitHub Release, with the `SHA256SUMS`
//! asset the pipeline already assembles as the integrity check.

use crate::harness;
use crate::update::{failure_window_secs, now_secs, Version};
use crate::{window, Config};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tauri::AppHandle;

/// Owner and repository the release list is read from.
pub const REPOSITORY: &str = "tanuki-cat/dsh-desktop";

/// GitHub refuses an API request that carries no User-Agent, so every transport sets one.
pub const USER_AGENT: &str = concat!("dsh-desktop/", env!("CARGO_PKG_VERSION"));

/// Budget for the whole release-metadata request.
///
/// The check runs on a background thread and never blocks startup, so this only bounds how long a
/// stalled network holds that thread. It is the same order as the registry query in `update`,
/// which the user already waits for at the same point of the launch.
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);

/// Budget for one asset download. A release artifact is tens to hundreds of megabytes, so this is
/// a transfer budget rather than a request budget: slow links are normal, a stalled one is not.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Largest asset this shell is willing to keep. The bundled packages are several hundred
/// megabytes; the cap exists so a URL that answers with an endless stream cannot fill the disk.
const MAX_ASSET_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// `app_update` in config.json: how far an available release is taken automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Do not even look.
    Off,
    /// Check, and say so in the window title and the log. Nothing is fetched.
    Notify,
    /// Download and verify into `ready/`, then leave the install to the user. The default: it buys
    /// the one click under `install` without spending it — a startup path may prepare an update on
    /// its own, but replacing the application the user just launched is their decision.
    #[default]
    Download,
    /// Download, verify, install and relaunch. Only macOS can do the last two in place (see
    /// `install_supported`); elsewhere this behaves as `Policy::Download`.
    Install,
}

impl Policy {
    /// Label for the log line, so one lookup tells which policy ran.
    pub fn label(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Notify => "notify",
            Policy::Download => "download",
            Policy::Install => "install",
        }
    }

    /// Read a policy out of a config string. `None` means the value is not one this shell knows,
    /// which the caller reports instead of silently falling back.
    pub fn parse(value: &str) -> Option<Policy> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Policy::Off),
            "notify" => Some(Policy::Notify),
            "download" => Some(Policy::Download),
            "install" => Some(Policy::Install),
            _ => None,
        }
    }
}

/// `app_update_channel` in config.json: which releases the check considers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// The newest non-prerelease, non-draft release. What `/releases/latest` answers.
    #[default]
    Stable,
    /// The newest release of any kind, prereleases included.
    Prerelease,
}

impl Channel {
    pub fn parse(value: &str) -> Option<Channel> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Channel::Stable),
            "prerelease" => Some(Channel::Prerelease),
            _ => None,
        }
    }

    /// The value stored in the cache, so an entry answers only the channel it was produced for.
    pub fn key(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Prerelease => "prerelease",
        }
    }
}

/// The platform-specific halves of an asset name, as `release.yml` writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetTarget {
    /// `macos`, `linux` or `windows`.
    pub os: &'static str,
    /// `arm64` or `x64`.
    pub arch: &'static str,
    /// True for a build that carries its own runtime, whose artifact name ends in `-bundled`.
    pub bundled: bool,
}

impl AssetTarget {
    /// The target of the running build.
    pub fn current(bundled: bool) -> AssetTarget {
        AssetTarget {
            os: if cfg!(target_os = "macos") {
                "macos"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else {
                "linux"
            },
            arch: if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            },
            bundled,
        }
    }

    /// The file name an asset for this target has, given the release version.
    ///
    /// One function, so the picker and the tests cannot drift from the pipeline: every artifact is
    /// named `dsh-desktop_<version>_<os>-<arch>[-bundled].<suffix>`, and the suffix is the only
    /// part that varies by platform.
    pub fn asset_name(&self, version: &str) -> String {
        format!("dsh-desktop_{version}{}", self.flavour())
    }

    /// The stable part of an asset name: everything that follows the version in it.
    ///
    /// Windows is the one platform whose artifact name does not distinguish a bundled build: the
    /// portable package always carries its runtime (see windows-portable.yml), so it is called
    /// -portable there and bundled is ignored. Getting this wrong is silent — the release would
    /// read as no asset for this machine — which is why the test below pins the exact names a
    /// published release has.
    pub fn flavour(&self) -> String {
        if self.os == "windows" {
            return format!("_windows-{}-portable.zip", self.arch);
        }
        let bundled = if self.bundled { "-bundled" } else { "" };
        let suffix = if self.os == "macos" { "app.zip" } else { "deb" };
        format!("_{}-{}{bundled}.{suffix}", self.os, self.arch)
    }

    /// The asset that carries the checksums for every platform.
    pub const SUMS: &'static str = "SHA256SUMS";
}

/// The version embedded in an asset name this pipeline produced.
///
/// A package that was downloaded in an earlier session has only its file name to say what it is,
/// and the `.app` inside has to report exactly that version (see `macos::verify_bundle`), so the
/// slice is returned verbatim instead of being re-formatted from the parsed value.
pub fn version_of_asset_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("dsh-desktop_")?;
    let version = rest.split('_').next()?;
    Version::parse(version)?;
    Some(version.to_string())
}

/// One entry of the GitHub release list, reduced to what this shell reads.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// `browser_download_url`; optional so a hand-written test fixture needs only the name.
    #[serde(default)]
    pub browser_download_url: Option<String>,
}

impl Release {
    /// The version this release publishes, or `None` when the tag is not one.
    pub fn version(&self) -> Option<Version> {
        Version::parse(&self.tag_name)
    }

    /// The version as it appears in asset names: the tag without a leading `v`.
    pub fn version_label(&self) -> String {
        self.tag_name
            .trim()
            .trim_start_matches('v')
            .trim()
            .to_string()
    }

    /// The asset for `target`, when the release offers exactly that name.
    ///
    /// Exact matching is the point: a release that only carries an x64 package must not be
    /// installed on arm64, so the absent name is reported rather than replaced by a near miss.
    pub fn asset_for(&self, target: &AssetTarget, version: &str) -> Option<&Asset> {
        let wanted = target.asset_name(version);
        self.assets.iter().find(|asset| asset.name == wanted)
    }

    /// The checksum manifest bundled with the release.
    pub fn sums(&self) -> Option<&Asset> {
        self.assets
            .iter()
            .find(|asset| asset.name == AssetTarget::SUMS)
    }
}

/// Which release a release list offers for a channel.
///
/// Pure, so the selection is testable without a network. The interesting cases are "the newest
/// release is a draft", "the channel is stable and the newest is a prerelease", and "the newest
/// tag is not a version this shell understands".
pub fn select(releases: &[Release], channel: Channel) -> Option<&Release> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter(|release| channel == Channel::Prerelease || !release.prerelease)
        .filter_map(|release| release.version().map(|version| (version, release)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, release)| release)
}

/// What one check learned about the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    UpToDate {
        version: String,
    },
    UpdateAvailable {
        from: String,
        to: String,
    },
    /// A release exists but carries no artifact for this platform and build flavour. Reported
    /// rather than ignored: "0.4.16 is out but has no macos-arm64 package" is a real answer, and
    /// substituting another asset would install the wrong architecture.
    NoAsset {
        to: String,
        wanted: String,
    },
    Failed {
        reason: String,
    },
}

/// Compare the installed shell version against a release.
pub fn judge(current: &str, release: &Release, target: &AssetTarget) -> Status {
    let Some(latest) = release.version() else {
        return Status::Failed {
            reason: format!("无法解析 release 版本 {:?}", release.tag_name),
        };
    };
    let Some(installed) = Version::parse(current) else {
        return Status::Failed {
            reason: format!("无法解析当前版本 {current:?}"),
        };
    };
    if latest <= installed {
        return Status::UpToDate {
            version: format!("{latest}"),
        };
    }
    match release.asset_for(target, &release.version_label()) {
        Some(_) => Status::UpdateAvailable {
            from: current.to_string(),
            to: format!("{latest}"),
        },
        None => Status::NoAsset {
            to: format!("{latest}"),
            wanted: target.asset_name(&release.version_label()),
        },
    }
}

/// The cached answer, so a launch that just checked does not query GitHub again.
///
/// Structurally the same as `update::Cache`, and read by the same freshness rules (a changed
/// installed version or a passed interval invalidates it). It is a separate file because the two
/// answers describe different objects and expire on their own schedules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cache {
    /// Unix seconds when the query ran.
    pub checked_at: u64,
    /// The shell version at that moment; installing a new one invalidates the entry.
    pub installed: String,
    /// The release version seen, or `None` for a failed query.
    pub latest: Option<String>,
    /// The channel this answer was produced for: a different channel is a different question.
    #[serde(default)]
    pub channel: String,
    /// The asset flavour this answer was produced for (see `AssetTarget::flavour`).
    #[serde(default)]
    pub flavour: String,
    /// Version whose download failed, with the time and the count — the same marker the CLI
    /// update keeps, for the same reason: a dead network must not be retried on every launch.
    #[serde(default)]
    pub failed: Option<String>,
    #[serde(default)]
    pub failed_at: u64,
    #[serde(default)]
    pub failures: u32,
}

impl Cache {
    /// Whether this entry answers the question being asked now.
    pub fn is_fresh(
        &self,
        now: u64,
        installed: &str,
        channel: Channel,
        target: &AssetTarget,
        interval_minutes: u64,
    ) -> bool {
        if self.installed != installed {
            return false;
        }
        if self.channel != channel.key() {
            return false;
        }
        if !self.flavour.is_empty() && self.flavour != target.flavour() {
            return false;
        }
        // A failed query is retried quickly instead of being trusted for the whole interval.
        let ttl = if self.latest.is_some() {
            interval_minutes
        } else {
            interval_minutes.min(crate::update::FAILED_RETRY_MINUTES)
        };
        if ttl == 0 {
            return false;
        }
        now.saturating_sub(self.checked_at) < ttl.saturating_mul(60)
    }

    /// Whether this version's download failed recently enough that it must not be retried yet.
    pub fn failed_recently(&self, now: u64, to: &str) -> bool {
        self.failed.as_deref() == Some(to)
            && now.saturating_sub(self.failed_at) < failure_window_secs(self.failures)
    }
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("app-update-check.json")
}

pub fn read_cache(data_dir: &Path) -> Option<Cache> {
    let raw = std::fs::read_to_string(cache_path(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_cache(data_dir: &Path, cache: &Cache) {
    let path = cache_path(data_dir);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec_pretty(cache) {
        if std::fs::write(&path, text).is_ok() {
            crate::process::restrict(&path);
        }
    }
}

/// Remember that `version` failed, growing the wait with every repeat.
pub fn mark_failed(data_dir: &Path, version: &str) {
    let now = now_secs();
    let mut cache = read_cache(data_dir).unwrap_or(Cache {
        checked_at: now,
        installed: String::new(),
        latest: None,
        channel: String::new(),
        flavour: String::new(),
        failed: None,
        failed_at: 0,
        failures: 0,
    });
    cache.failures = if cache.failed.as_deref() == Some(version) {
        cache.failures.saturating_add(1)
    } else {
        1
    };
    cache.failed = Some(version.to_string());
    cache.failed_at = now;
    write_cache(data_dir, &cache);
}

/// Where downloads and verified packages live, under the app data directory.
pub struct Paths {
    /// In-progress and rejected downloads.
    pub downloads: PathBuf,
    /// Packages that passed the checksum, ready to install.
    pub ready: PathBuf,
    /// The installation an update replaced, kept as a rollback.
    pub backup: PathBuf,
}

impl Paths {
    pub fn new(data_dir: &Path) -> Paths {
        let root = data_dir.join("app-update");
        Paths {
            downloads: root.join("downloads"),
            ready: root.join("ready"),
            backup: root.join("backup"),
        }
    }

    /// Create every directory the update path writes into. Idempotent; a read-only data directory
    /// is reported to the caller instead of panicking.
    pub fn create(&self) -> std::io::Result<()> {
        for dir in [&self.downloads, &self.ready, &self.backup] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Parse a `SHA256SUMS` document into `(name, hex)` pairs.
///
/// The format is the one `sha256sum` produces and the release pipeline concatenates: one
/// `<hex>  <name>` line per file. A line that does not match is dropped rather than failing the
/// whole document — a trailing blank line is normal, and one malformed line must not invalidate
/// every checksum in the file.
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let digest = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            Some((name.to_string(), digest.to_ascii_lowercase()))
        })
        .collect()
}

/// The checksum `sums` records for `name`.
pub fn checksum_for<'a>(sums: &'a [(String, String)], name: &str) -> Option<&'a str> {
    sums.iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, digest)| digest.as_str())
}

/// Stream a file through SHA-256 and return lowercase hex.
///
/// Streamed rather than read into memory: a bundled package is several hundred megabytes, and
/// holding it in the heap to hash it would be the largest allocation this process ever makes.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("无法打开下载结果: {error}"))?;
    let mut hasher = sha2::Sha256::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("读取下载结果失败: {error}")),
        };
        hasher.update(&chunk[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Compare `path` with the digest the manifest records for it.
///
/// A mismatch deletes the file rather than leaving the removal to the caller: a wrong file on disk
/// is a wrong file the next install could pick up, and every failure path would otherwise have to
/// remember the same line.
pub fn check_digest(path: &Path, expected: &str) -> Result<String, String> {
    let actual = sha256_file(path)?;
    if actual == expected {
        return Ok(actual);
    }
    let _ = std::fs::remove_file(path);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Err(format!(
        "校验失败：{name} 的 SHA256 是 {actual}，{} 记录的是 {expected}",
        AssetTarget::SUMS
    ))
}

/// Whether the shell can replace itself in place on this platform.
///
/// Only macOS: the artifact is an `.app` bundle, which can be swapped on disk while the code runs
/// from the copy already mapped into memory. A Linux `.deb` needs root, and a Windows executable
/// cannot be overwritten while it is executing.
pub fn install_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Refuse a package that would move the application backwards.
///
/// `ready/` holds one package until the user acts on it, and nothing says the running version is
/// still older by then — an update installed by hand, or by the `install` policy on a later
/// launch, leaves the waiting package behind. Installing it would be a downgrade nobody asked
/// for, so it is reported instead.
///
/// macOS only, together with the install it guards: nowhere else can a waiting package be used.
#[cfg(target_os = "macos")]
fn refuse_downgrade(version: &str) -> Result<(), String> {
    let installed = env!("CARGO_PKG_VERSION");
    let (Some(package), Some(installed)) = (Version::parse(version), Version::parse(installed))
    else {
        // An unparseable version is not a reason to refuse: `macos::install` still checks the
        // bundle's own CFBundleShortVersionString against this string.
        return Ok(());
    };
    if package <= installed {
        return Err(format!(
            "更新包是 v{version}，不比当前安装的 v{installed} 新，拒绝降级"
        ));
    }
    Ok(())
}

/// Install an already-verified package in place.
///
/// The version comes from the package's own file name: it is the only record a package left in
/// `ready/` by an earlier session carries, and `macos::install` refuses a bundle that does not
/// report it.
pub fn install_package(package: &Path, paths: &Paths) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let name = package
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let version = version_of_asset_name(&name)
            .ok_or_else(|| format!("无法从文件名 {name} 读出要安装的版本"))?;
        // A package can outlive the release it belongs to: it sits in ready/ until someone asks
        // for it, and the application may have moved past it in the meantime (a manual install, or
        // `install` policy on a later launch). Installing it then is a silent downgrade.
        refuse_downgrade(&version)?;
        macos::install(package, &paths.backup, &version)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (package, paths);
        Err("只有 macOS 能就地安装；本平台请按提示手动安装".to_string())
    }
}

/// How one HTTP request is run: the tool, its arguments, and the temporary file it writes into.
///
/// Two transports exist because the platforms differ in what they ship. macOS and Linux have
/// `curl`; Windows has PowerShell. Both are reached the same way the rest of this repo reaches
/// system tools (`taskkill`, `netstat`, the login shell): a bounded child process whose failure
/// is a log line, not a new dependency.
fn transport(url: &str, destination: &Path, timeout: Duration) -> Result<Command, String> {
    if let Some(dir) = destination.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("无法创建下载目录: {error}"))?;
    }

    #[cfg(windows)]
    {
        let mut command = Command::new("powershell");
        // `-NoProfile` keeps a user profile script from replacing the proxy or the certificate
        // store under the request; the quoting mirrors the existing PowerShell call in `harness`.
        let file = destination.display().to_string().replace('\'', "''");
        let url = url.replace('\'', "''");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            // The reason for a failure has to reach stdout: this shell collects stdout only,
            // and PowerShell writes its errors to stderr (which is discarded). `-ErrorAction
            // Stop` is what turns a non-terminating error into one this catch can see.
            &format!(
                "$ProgressPreference='SilentlyContinue'; \
                 try {{ Invoke-WebRequest -UseBasicParsing -TimeoutSec {seconds} \
                 -Headers @{{'User-Agent'='{USER_AGENT}'}} \
                 -Uri '{url}' -OutFile '{file}' -ErrorAction Stop }} \
                 catch {{ Write-Output $_.Exception.Message; exit 1 }}",
                seconds = timeout.as_secs().max(1)
            ),
        ]);
        Ok(command)
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("curl");
        command.args([
            "-f",
            "-sS",
            "-L",
            // Only HTTPS is followed, including redirects: an asset URL comes from an API
            // response, and letting it downgrade to plain HTTP would make the checksum the only
            // thing standing between the user and a modified package.
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            &timeout.as_secs().to_string(),
            "--max-filesize",
            &MAX_ASSET_BYTES.to_string(),
            // curl writes the reason for a failure to stderr, which this shell does not collect
            // (see `process::output_within`), so without this the log would only ever have the
            // exit status. `-` merges stderr into stdout, where the bounded drain keeps the first
            // line of it. Available since curl 7.63.0 (2018).
            "--stderr",
            "-",
            "-A",
            USER_AGENT,
            "-o",
        ]);
        command.arg(destination).arg(url);
        Ok(command)
    }
}

/// Fetch `url` into `destination` under a whole-request budget.
fn fetch_to_file(url: &str, destination: &Path, timeout: Duration) -> Result<(), String> {
    let _ = std::fs::remove_file(destination);
    let mut command = transport(url, destination, timeout)?;
    // `run_within` sanitizes the environment itself, and it is the variant that keeps *why* the
    // command produced nothing: a machine without the tool used to be reported as a timeout.
    let output = match crate::process::run_within(&mut command, timeout, 64 * 1024) {
        Ok(output) => output,
        Err(crate::process::RunFailure::Spawn(error)) => {
            return Err(format!("无法启动下载工具（{error}）: {url}"))
        }
        Err(crate::process::RunFailure::Timeout) => {
            return Err(format!("下载超时（{} 秒）: {url}", timeout.as_secs()))
        }
    };
    match output.status {
        Some(status) if status.success() => Ok(()),
        Some(status) => {
            let detail = output.text.lines().next().unwrap_or("").trim().to_string();
            Err(format!(
                "下载失败（{status}）: {url}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ))
        }
        None => Err(format!("下载未能完成: {url}")),
    }
}

/// Reject a file larger than `MAX_ASSET_BYTES` before anything tries to read it.
fn check_size(path: &Path) -> Result<u64, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("无法读取下载结果: {error}"))?;
    if metadata.len() > MAX_ASSET_BYTES {
        return Err(format!(
            "下载的文件超过 {} GiB，拒绝处理",
            MAX_ASSET_BYTES / (1024 * 1024 * 1024)
        ));
    }
    if metadata.len() == 0 {
        return Err("下载的文件是空的".to_string());
    }
    Ok(metadata.len())
}

/// One temporary file per request, in the system temp directory.
fn temp_file(extension: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "dsh-desktop-update-{}-{}.{extension}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A small HTTP GET that is parsed in memory (the release list, the checksum manifest).
fn http_get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let file = temp_file("txt");
    let fetched = fetch_to_file(url, &file, timeout);
    let result = fetched.and_then(|()| {
        check_size(&file)?;
        std::fs::read(&file).map_err(|error| format!("无法读取下载结果: {error}"))
    });
    let _ = std::fs::remove_file(&file);
    result
}

/// Query the release list.
///
/// The body is validated before anything reads it: a GitHub error document is valid JSON and
/// would otherwise parse as an empty list, which is indistinguishable from "nothing newer" — the
/// one failure mode that hides itself completely.
pub fn fetch_releases() -> Result<Vec<Release>, String> {
    let body = http_get(&releases_url(), METADATA_TIMEOUT)?;
    let text = String::from_utf8_lossy(&body);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("release 列表解析失败: {error}"))?;
    if let Some(message) = parsed.get("message").and_then(|value| value.as_str()) {
        return Err(format!("GitHub API 拒绝请求: {message}"));
    }
    serde_json::from_value(parsed).map_err(|error| format!("release 列表结构与预期不符: {error}"))
}

/// A small GET, exposed for the live tests.
///
/// The transport is the part of this module that cannot be exercised offline (it shells out to
/// `curl` or PowerShell), so the live test has to go through the same code path rather than its
/// own client — otherwise it proves nothing about the installer.
#[doc(hidden)]
pub fn http_get_for_test(url: &str) -> Result<Vec<u8>, String> {
    http_get(url, METADATA_TIMEOUT)
}

/// The release list URL. Bounded because the API answers newest first and only the head is read.
pub fn releases_url() -> String {
    format!("https://api.github.com/repos/{REPOSITORY}/releases?per_page=20")
}

/// Download one release asset, check it against the release's `SHA256SUMS`, and move the verified
/// copy into `ready/`.
///
/// A failed checksum deletes the download: the next attempt has to fetch it again anyway, and the
/// only thing a wrong file left on disk can do is get installed by accident later.
pub fn download_verified(
    release: &Release,
    target: &AssetTarget,
    paths: &Paths,
) -> Result<PathBuf, String> {
    paths
        .create()
        .map_err(|error| format!("无法创建更新目录: {error}"))?;
    let label = release.version_label();
    let asset = release
        .asset_for(target, &label)
        .ok_or_else(|| format!("release v{label} 没有 {} 资产", target.asset_name(&label)))?;
    let sums_asset = release
        .sums()
        .ok_or_else(|| format!("release v{label} 缺少 {}", AssetTarget::SUMS))?;
    let asset_url = asset
        .browser_download_url
        .as_deref()
        .ok_or_else(|| format!("资产 {} 没有下载地址", asset.name))?;
    let sums_url = sums_asset
        .browser_download_url
        .as_deref()
        .ok_or_else(|| format!("资产 {} 没有下载地址", sums_asset.name))?;

    let sums = {
        let body = http_get(sums_url, METADATA_TIMEOUT)?;
        parse_sums(&String::from_utf8_lossy(&body))
    };
    let expected = checksum_for(&sums, &asset.name)
        .ok_or_else(|| format!("{} 里没有 {} 的校验和", AssetTarget::SUMS, asset.name))?
        .to_string();

    // A package already in ready/ with the right digest is exactly what this download would
    // produce, and re-fetching it costs hundreds of megabytes for no change.
    if let Some(verified) = reuse_verified(paths, &asset.name, &expected) {
        harness::app_log(&format!(
            "桌面应用更新包已在本地且校验通过，复用: {}",
            verified.display()
        ));
        return Ok(verified);
    }

    let partial = paths.downloads.join(&asset.name);
    if let Err(reason) = fetch_to_file(asset_url, &partial, DOWNLOAD_TIMEOUT) {
        let _ = std::fs::remove_file(&partial);
        return Err(reason);
    }
    let (size, actual) = match check_size(&partial)
        .and_then(|size| check_digest(&partial, &expected).map(|digest| (size, digest)))
    {
        Ok(result) => result,
        Err(reason) => {
            // `check_digest` removes its own failures; an oversized or unhashable file has not
            // been removed yet.
            let _ = std::fs::remove_file(&partial);
            return Err(reason);
        }
    };

    let verified = paths.ready.join(&asset.name);
    if let Err(error) = std::fs::rename(&partial, &verified) {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("无法保存更新包: {error}"));
    }
    crate::process::restrict(&verified);
    harness::app_log(&format!(
        "桌面应用更新包已校验: {}（{:.1} MiB, sha256 {actual}）",
        verified.display(),
        size as f64 / (1024.0 * 1024.0)
    ));
    prune_ready(paths, &asset.name);
    Ok(verified)
}

/// Keep the verified directory to the package that just arrived.
///
/// An older verified bundle is dead weight (tens to hundreds of megabytes) and is never installed
/// once a newer one exists.
fn prune_ready(paths: &Paths, keep: &str) {
    let Ok(entries) = std::fs::read_dir(&paths.ready) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() != keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// An already-verified copy of `name` in `ready/`, when its digest still matches.
///
/// A copy that no longer matches is deleted here: it is not the package the release describes, so
/// keeping it would only let a later install pick up the wrong file.
fn reuse_verified(paths: &Paths, name: &str, expected: &str) -> Option<PathBuf> {
    let candidate = paths.ready.join(name);
    if !candidate.is_file() {
        return None;
    }
    match sha256_file(&candidate) {
        Ok(actual) if actual == expected => Some(candidate),
        _ => {
            let _ = std::fs::remove_file(&candidate);
            None
        }
    }
}

/// The newest verified package waiting in `ready/`, when there is one.
pub fn verified_package(paths: &Paths) -> Option<PathBuf> {
    std::fs::read_dir(&paths.ready)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        })
}

/// What `apply` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The package is verified and waiting in `ready/`.
    Downloaded { package: PathBuf },
    /// The running installation now holds the new version; the caller relaunches.
    Installed { installed: PathBuf },
}

/// The bundle identifier every build carries. A package with a different one is not this
/// application, whatever its file name says.
pub const BUNDLE_ID: &str = "com.deepseek.dsh.desktop";

/// Download, and install where this platform allows it, then report what happened.
pub fn apply(
    release: &Release,
    target: &AssetTarget,
    policy: Policy,
    paths: &Paths,
) -> Result<Outcome, String> {
    let package = download_verified(release, target, paths)?;
    if policy != Policy::Install || !install_supported() {
        return Ok(Outcome::Downloaded { package });
    }
    #[cfg(target_os = "macos")]
    {
        let installed = macos::install(&package, &paths.backup, &release.version_label())?;
        Ok(Outcome::Installed { installed })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (package, policy);
        unreachable!("install_supported() is only true on macOS")
    }
}

#[cfg(target_os = "macos")]
pub mod macos;

/// Check, and act according to policy, once per launch or on request.
///
/// `force` is the menu entry: it ignores the cached answer and re-downloads a package that is
/// already verified. Everything else goes through the cache, because the common case is a launch
/// that just checked and found nothing new.
pub fn report(app: &AppHandle, data_dir: &Path, config: &Config, bundled: bool, force: bool) {
    if config.app_update == Policy::Off && !force {
        return;
    }
    let current = env!("CARGO_PKG_VERSION");
    // The same knob the registry check reads: one number for "how often may this shell ask the
    // internet", rather than a second interval a user would have to discover separately.
    let interval = config.update_check_interval_minutes;
    let cache = read_cache(data_dir);
    if !force {
        if let Some(cache) = cache.as_ref() {
            if cache.is_fresh(
                now_secs(),
                current,
                config.app_update_channel,
                &AssetTarget::current(bundled),
                interval,
            ) {
                // A cached "there is a newer one" is still worth saying: the user is looking at
                // the same window the first answer would have reached.
                if let Some(latest) = cache.latest.as_deref() {
                    if Version::parse(latest) > Version::parse(current) {
                        // A launch that already downloaded this release left the package in
                        // ready/. Saying "there is a newer one" then points at the check item,
                        // while the useful click is the install one — so say "ready" whenever
                        // the package is actually here.
                        if verified_package(&Paths::new(data_dir)).is_some() {
                            window::announce_app_update_ready(app, latest);
                        } else {
                            window::announce_app_update(app, latest);
                        }
                    }
                }
                harness::app_log(&format!(
                    "桌面应用更新检查: 缓存仍然有效（{interval} 分钟内）"
                ));
                return;
            }
        }
    }
    let releases = match fetch_releases() {
        Ok(releases) => releases,
        Err(reason) => {
            harness::app_log(&format!("桌面应用更新检查失败: {reason}"));
            write_cache(
                data_dir,
                &Cache {
                    checked_at: now_secs(),
                    installed: current.to_string(),
                    latest: None,
                    channel: config.app_update_channel.key().to_string(),
                    flavour: AssetTarget::current(bundled).flavour(),
                    failed: cache.as_ref().and_then(|cache| cache.failed.clone()),
                    failed_at: cache.as_ref().map(|cache| cache.failed_at).unwrap_or(0),
                    failures: cache.as_ref().map(|cache| cache.failures).unwrap_or(0),
                },
            );
            return;
        }
    };
    let Some(release) = select(&releases, config.app_update_channel) else {
        harness::app_log("桌面应用更新检查: 没有任何可识别的 release");
        return;
    };
    let target = AssetTarget::current(bundled);
    let status = judge(current, release, &target);
    write_cache(
        data_dir,
        &Cache {
            checked_at: now_secs(),
            installed: current.to_string(),
            latest: release.version().map(|version| format!("{version}")),
            channel: config.app_update_channel.key().to_string(),
            flavour: target.flavour(),
            failed: cache.as_ref().and_then(|cache| cache.failed.clone()),
            failed_at: cache.as_ref().map(|cache| cache.failed_at).unwrap_or(0),
            failures: cache.as_ref().map(|cache| cache.failures).unwrap_or(0),
        },
    );
    match status {
        Status::UpToDate { version } => {
            harness::app_log(&format!(
                "桌面应用已是最新: {current}（release 头 {version}）"
            ));
            window::clear_app_update_notices(app);
            if force {
                window::app_update_up_to_date(app, current);
            }
        }
        Status::NoAsset { to, wanted } => {
            harness::app_log(&format!(
                "桌面应用有新版 {to}，但这个 release 没有 {wanted}"
            ));
            window::announce_app_update(app, &to);
        }
        Status::Failed { reason } => {
            harness::app_log(&format!("桌面应用更新检查失败: {reason}"));
        }
        Status::UpdateAvailable { from, to } => {
            window::announce_app_update(app, &to);
            harness::app_log(&format!(
                "桌面应用有新版本 {from} -> {to}（策略 {}）",
                config.app_update.label()
            ));
            // The policy decides how far an available release is taken, whether the check was
            // automatic or asked for by hand: a menu click is a request to *check*, not a
            // reason to install something the user configured the shell to only report.
            if config.app_update == Policy::Notify {
                if force {
                    // Asked for by hand, so the answer has to say where the release is: the menu
                    // item is the one place this shell can report "a newer one exists" and be read.
                    let link = format!("https://github.com/{REPOSITORY}/releases/tag/v{to}");
                    let detail = format!(
                        "当前 v{from}，最新 v{to}。\n\n下载地址：{link}\n\n\
                         想让本应用自动处理，就在 config.json 里把 \"app_update\" 设为 \"download\"\
                         （只下载并校验）或 \"install\"（macOS 上安装并重启）。"
                    );
                    window::show_message(app, &format!("桌面应用有新版本 v{to}"), &detail);
                }
                return;
            }
            if let Some(cache) = cache.as_ref() {
                if cache.failed_recently(now_secs(), &to) {
                    harness::app_log(&format!(
                        "桌面应用 {to} 上次下载失败，暂不重试（等待窗口未过）"
                    ));
                    return;
                }
            }
            let paths = Paths::new(data_dir);
            // Progress is only shown for a check the user asked for: an automatic one runs while
            // they are using the Harness, and a status window raised over it for a background
            // download would be worse than the wait it explains.
            if force {
                window::show_message(app, &format!("正在下载桌面应用 v{to}…"), &from);
            } else {
                window::set_status(app, &format!("正在下载桌面应用 v{to}…"), &from);
            }
            match apply(release, &target, config.app_update, &paths) {
                Ok(Outcome::Installed { installed }) => {
                    harness::app_log(&format!(
                        "桌面应用已更新到 {to}，正在重启: {}",
                        installed.display()
                    ));
                    window::show_message(
                        app,
                        &format!("桌面应用已更新到 v{to}"),
                        "正在重启以载入新版本…",
                    );
                    relaunch_into_update(app, &installed);
                }
                Ok(Outcome::Downloaded { package }) => {
                    harness::app_log(&format!(
                        "桌面应用 v{to} 已下载并校验: {}",
                        package.display()
                    ));
                    // One sentence on every platform: the package is on disk and verified, and
                    // the menu item is what to do about it (on macOS it installs it in place;
                    // elsewhere it prints the command). It deliberately does not promise that a
                    // restart installs anything, because nothing on the startup path does.
                    window::announce_app_update_ready(app, &to);
                    if force {
                        window::show_message(
                            app,
                            &format!("桌面应用 v{to} 已下载并通过校验"),
                            &manual_install_hint(&package),
                        );
                    }
                }
                Err(reason) => {
                    mark_failed(data_dir, &to);
                    harness::app_log(&format!("桌面应用更新下载失败，保留 {from}: {reason}"));
                    if force {
                        window::show_message(app, "桌面应用更新失败", &reason);
                    }
                }
            }
        }
    }
}

/// Hand the process over to the freshly installed bundle.
///
/// The new copy is started before this one leaves, so a launch failure is visible instead of
/// leaving the user with nothing running. `exit` then goes through the ordinary shutdown path:
/// the Harness child is stopped and `state.json` is dropped, which is what the next launch reads.
///
/// The single-instance socket is released **right after the launch, never before it**: the
/// replacement has to find no listener and install its own, otherwise its own single-instance
/// check connects to this process, reports "already running", and exits — and this one is on its
/// way out, so the machine ends up with no instance at all. The plugin removes the socket on
/// `RunEvent::Exit` too, but that is a whole event-loop shutdown later, which is exactly the
/// window the replacement would fall into.
pub fn relaunch_into_update(app: &AppHandle, installed: &Path) {
    #[cfg(target_os = "macos")]
    {
        if let Err(reason) = macos::relaunch(installed) {
            harness::app_log(&format!("新版本已安装，但启动失败: {reason}"));
            window::show_message(
                app,
                "桌面应用更新完成，需要手动重新打开",
                &format!("{reason}\n\n新版本已安装在 {}", installed.display()),
            );
            return;
        }
        tauri_plugin_single_instance::destroy(app);
        app.exit(0);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, installed);
    }
}

/// What the user has to type or do when this platform cannot install the update in place.
pub(crate) fn manual_install_hint(package: &Path) -> String {
    if cfg!(target_os = "windows") {
        format!(
            "已校验的更新包在：\n{}\n\n退出本应用后，用它解压出的目录替换当前安装（本平台无法覆盖正在运行的 exe）。",
            package.display()
        )
    } else if cfg!(target_os = "macos") {
        // macOS can install in place, so this path is reached only when the policy is `download`
        // and the user asks where the package went. `apt install` (the Linux branch) would be a
        // command that does not exist on the machine this text is printed on.
        format!(
            "已校验的更新包在：\n{}\n\n解压后用它替换当前安装的「DSH Desktop.app」即可；\n\
             想让本应用自己装好并重启，把 config.json 的 \"app_update\" 设为 \"install\"。",
            package.display()
        )
    } else {
        format!(
            "已校验的更新包在：\n{}\n\n执行：sudo apt install {}",
            package.display(),
            package.display()
        )
    }
}

#[cfg(test)]
mod tests;
