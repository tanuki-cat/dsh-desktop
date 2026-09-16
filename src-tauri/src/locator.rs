//! Resolve the three executables the shell needs: dsh launcher, its real JS entry, and node.
//!
//! `dsh` ships as `#!/usr/bin/env node`, and a GUI-launched app has no Homebrew PATH, so
//! resolving the launcher alone is not enough: node must be resolved too.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DshLocation {
    pub launcher: PathBuf,
    pub dsh_js: PathBuf,
    pub node: PathBuf,
}

pub fn locate(
    remembered: Option<PathBuf>,
    override_env: Option<String>,
) -> Result<DshLocation, String> {
    let launcher = find_launcher(remembered, override_env)?;
    let dsh_js = real_path(&launcher);
    let node = find_node(&launcher)?;
    Ok(DshLocation {
        launcher,
        dsh_js,
        node,
    })
}

/// What a node binary can do. The bundled runtime plan §2.4 gates the system installation on
/// two facts, because a mismatch only shows up as a crash much later:
/// the architecture (an x64 node under Rosetta cannot load the bundled arm64 prebuilds) and
/// `module.stripTypeScriptTypes` (the CLI's code-runtime worker needs it; `@deepseek-ai/dsh`
/// declares no `engines.node`, so a capability probe is the only honest check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeFacts {
    pub arch: String,
    pub strip_types: bool,
}

/// A node binary we cannot talk to must not stall startup: this probe runs before the
/// supervised process and before the URL wait, so a hanging shim would freeze the splash page
/// with no timeout to fall back on. Measured cost of a successful probe is ~80 ms.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe node once (~80 ms) for the facts above.
pub fn probe_node(node: &Path) -> Option<NodeFacts> {
    probe_node_within(node, PROBE_TIMEOUT)
}

/// `probe_node` with an explicit budget, so the timeout itself is testable.
fn probe_node_within(node: &Path, timeout: Duration) -> Option<NodeFacts> {
    const SCRIPT: &str =
        "process.stdout.write(process.arch + \" \" + (typeof require(\"module\").stripTypeScriptTypes))";
    let mut child = Command::new(node)
        .arg("-e")
        .arg(SCRIPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on another thread: the pipe must be drained while we wait, or a chatty node fills
    // the buffer and deadlocks against our try_wait loop.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                if !status.success() {
                    return None;
                }
                let mut parts = text.split_whitespace();
                let arch = parts.next()?.to_string();
                let strip_types = parts.next() == Some("function");
                return Some(NodeFacts { arch, strip_types });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => {
                let _ = child.kill();
                let _ = child.wait();
                // The reader is dropped, not joined: a grandchild can still hold the pipe
                // (`sh -c 'sleep 30'`), and waiting for it here would reintroduce the very
                // hang this deadline exists to avoid. The thread ends with the pipe.
                drop(reader);
                let why = match other {
                    Err(error) => format!("探测失败: {error}"),
                    _ => format!("探测超过 {}s 未返回", timeout.as_secs()),
                };
                crate::harness::app_log(&format!("{why}；按不可用处理: {}", node.display()));
                return None;
            }
        }
    }
}

/// The npm package every candidate has to be: a `dsh` that is something else is not this CLI.
///
/// `dsh` is a crowded name — Homebrew ships a distributed shell under it, PyPI a Python tool,
/// and a hand-written shim or a stale dependency can put one anywhere on PATH. The shell used to
/// take whichever file came first and read the version off the nearest `package.json` without
/// asking whose it was, so a foreign binary could be supervised *and* have its version shown as
/// "the dsh version" (field report 2026-09-15: a startup page naming a dsh version that does not
/// exist on npm).
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";

/// The nearest package manifest above an entry script: where it is, what it is called, its version.
///
/// Reading the *nearest* manifest — rather than "the first one on the way up that names dsh" — is
/// the point: a foreign `dsh` living inside some other npm package must be recognised as that
/// package, not adopted merely because no dsh manifest turned up.
struct PackageManifest {
    name: String,
    version: String,
}

/// Read the manifest owning `dsh_js`, up to five levels above it.
///
/// `Ok(None)` means there is no manifest to judge (a shim outside any package, which is how the
/// shell's own launcher can look); `Err` carries one that exists but cannot be read or parsed,
/// which is worth logging instead of pretending either answer.
fn owning_manifest(dsh_js: &Path) -> Result<Option<PackageManifest>, String> {
    let mut dir = match dsh_js.parent() {
        Some(dir) => dir.to_path_buf(),
        None => return Ok(None),
    };
    for _ in 0..5 {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            let raw = std::fs::read_to_string(&manifest)
                .map_err(|error| format!("{} 无法读取: {error}", manifest.display()))?;
            let parsed: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|error| format!("{} 不是合法 JSON: {error}", manifest.display()))?;
            return Ok(Some(PackageManifest {
                name: parsed
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                version: parsed
                    .get("version")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            }));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// The owning manifest when it really is this CLI.
fn dsh_package(dsh_js: &Path) -> Result<Option<PackageManifest>, String> {
    Ok(owning_manifest(dsh_js)?.filter(|manifest| manifest.name == DSH_PACKAGE))
}

/// The version of a CLI tree, when its owning package really is this CLI.
pub fn version_of(dsh_js: &Path) -> Option<String> {
    match dsh_package(dsh_js) {
        Ok(Some(manifest)) if !manifest.version.is_empty() => Some(manifest.version),
        _ => None,
    }
}

/// Version of a CLI tree as a person should read it, with the reason when it cannot be read.
pub fn describe_version(dsh_js: &Path) -> String {
    match dsh_package(dsh_js) {
        Ok(Some(manifest)) if !manifest.version.is_empty() => manifest.version,
        Ok(Some(_)) => "未知（清单没有 version）".to_string(),
        Ok(None) => match owning_manifest(dsh_js) {
            Ok(Some(other)) => format!(
                "未知（{} 属于 {}，不是 {DSH_PACKAGE}）",
                dsh_js.display(),
                other.name.as_str()
            ),
            Ok(None) => "未知（找不到所属 package.json）".to_string(),
            Err(error) => format!("未知（{error}）"),
        },
        Err(error) => format!("未知（{error}）"),
    }
}

/// Version of the installed CLI. Reading the owning package.json costs ~1 ms, while booting
/// node for `--version` costs ~80 ms, so the file wins and the CLI is the fallback.
///
/// The fallback asks the binary itself, which is what names the version on a layout this walk
/// cannot read — and the answer is still parsed as a version, so a foreign `dsh` printing
/// something unrelated cannot put that text on the status page either.
pub fn version(loc: &DshLocation) -> Option<String> {
    if let Some(version) = version_of(&loc.dsh_js) {
        return Some(version);
    }
    let out = Command::new(&loc.node)
        .arg(&loc.dsh_js)
        .arg("--version")
        .output()
        .ok()?;
    parse_version_line(&String::from_utf8_lossy(&out.stdout))
}

/// The version a `dsh --version` line carries, or `None` when it is not one.
fn parse_version_line(raw: &str) -> Option<String> {
    let line = raw.trim().lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let parsed = crate::update::Version::parse(line)?;
    Some(parsed.to_string())
}

/// True when the file looks like a node launcher (`lib/bin.js`, or a `#!…node` shim).
///
/// A launcher on PATH is normally a symlink or a two-line shim, so the identity check needs the
/// file it points at — and a compiled `dsh` from another project is neither.
fn is_js_launcher(path: &Path) -> bool {
    use std::io::Read;
    if path.extension().is_some_and(|extension| extension == "js") {
        return true;
    }
    let mut head = [0u8; 64];
    match std::fs::File::open(path).and_then(|mut file| file.read(&mut head)) {
        Ok(0) | Err(_) => false,
        Ok(read) => {
            let text = String::from_utf8_lossy(&head[..read]);
            text.starts_with("#!") && text.contains("node")
        }
    }
}

/// How a candidate launcher was judged.
#[derive(Debug, PartialEq, Eq)]
pub enum Candidate {
    /// The owning package is [DSH_PACKAGE]: this is the CLI.
    Ours,
    /// A node launcher that answers `--version` with a version but cannot be identified from its
    /// manifest (a shim outside its package, a layout this walk does not know).
    Unidentified(String),
    /// Not a dsh: skipped, with the reason for the log.
    Foreign(String),
}

/// Judge one candidate launcher.
///
/// `probe` runs `--version` and returns its first line, so the pure part stays testable without a
/// node binary.
pub fn judge(launcher: &Path, probe: impl FnOnce(&Path) -> Option<String>) -> Candidate {
    let real = real_path(launcher);
    let manifest = match owning_manifest(&real) {
        Ok(manifest) => manifest,
        Err(error) => return Candidate::Foreign(error),
    };
    if let Some(manifest) = &manifest {
        if manifest.name == DSH_PACKAGE {
            return Candidate::Ours;
        }
        // The nearest manifest names something else. That is an answer, not a reason to keep
        // walking: a \`dsh\` shipped inside another npm package is that package's binary.
        return Candidate::Foreign(format!(
            "{} 属于 {}，不是 {DSH_PACKAGE}",
            real.display(),
            manifest.name.as_str()
        ));
    }
    if !is_js_launcher(&real) {
        return Candidate::Foreign(format!("{} 不是 node 启动脚本", real.display()));
    }
    match probe(&real).and_then(|line| parse_version_line(&line)) {
        Some(version) => Candidate::Unidentified(version),
        None => Candidate::Foreign(format!("{} 不响应 --version", real.display())),
    }
}

/// Every place a `dsh` launcher may live, in the documented order (design §4 step 3).
///
/// Duplicates are dropped after resolving symlinks: the login shell usually prints the same file
/// PATH already offered, and probing one candidate twice costs a node start each time.
fn launcher_candidates(remembered: Option<PathBuf>, override_env: Option<String>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if path.is_file() {
            candidates.push(path);
        }
    };
    if let Some(raw) = override_env {
        push(PathBuf::from(raw));
    }
    if let Some(path) = remembered {
        push(path);
    }
    if let Some(path) = path_lookup("dsh") {
        push(path);
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        push(Path::new(dir).join("dsh"));
    }
    if let Some(path) = login_shell_lookup("dsh") {
        push(path);
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    candidates.retain(|candidate| {
        let real = real_path(candidate);
        if seen.contains(&real) {
            return false;
        }
        seen.push(real);
        true
    });
    candidates
}

/// Run the candidate with its own node and return the first line it prints.
///
/// Bounded by the same budget as the node probe: a launcher that hangs must not park startup.
fn probe_version_line(node: Option<&Path>, entry: &Path) -> Option<String> {
    let node = node?;
    let mut child = Command::new(node)
        .arg(entry)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                return status.success().then_some(text);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return None;
            }
        }
    }
}

/// The first candidate that really is this CLI, or an error naming what was rejected.
fn find_launcher(
    remembered: Option<PathBuf>,
    override_env: Option<String>,
) -> Result<PathBuf, String> {
    let candidates = launcher_candidates(remembered, override_env);
    if candidates.is_empty() {
        return Err(
            "找不到 dsh。可在 config.json 里设置 dsh_path，或用环境变量 DSH_DESKTOP_DSH 指定绝对路径。"
                .into(),
        );
    }
    let mut rejected: Vec<String> = Vec::new();
    let mut unidentified: Option<(PathBuf, String)> = None;
    for candidate in &candidates {
        let node = find_node_without_env(candidate);
        match judge(candidate, |entry| {
            probe_version_line(node.as_deref(), entry)
        }) {
            Candidate::Ours => return Ok(candidate.clone()),
            Candidate::Unidentified(version) => {
                if unidentified.is_none() {
                    unidentified = Some((candidate.clone(), version));
                }
            }
            Candidate::Foreign(reason) => {
                crate::harness::app_log(&format!("跳过候选 dsh：{reason}"));
                rejected.push(reason);
            }
        }
    }
    // Nothing identified itself. A launcher that at least answers `--version` with a version is
    // still the user's installation on a layout this walk cannot read — refusing it would be
    // worse than the bug the identity check fixes — so it is used, loudly.
    if let Some((launcher, version)) = unidentified {
        crate::harness::app_log(&format!(
            "无法从清单确认 {} 属于 {DSH_PACKAGE}，但它报告版本 {version}，按用户的安装处理",
            launcher.display()
        ));
        return Ok(launcher);
    }
    Err(format!(
        "PATH 上的 dsh 都不是 {DSH_PACKAGE}：{}。请安装 npm i -g {DSH_PACKAGE}，或用 DSH_DESKTOP_DSH / config.json 的 dsh_path 指定。",
        rejected.join("；")
    ))
}

/// The system `dsh` launcher, resolved without requiring node to exist (review P1-4).
///
/// `remembered` is `config.json`'s `dsh_path`: the documented place to point the shell at an
/// installation the automatic search cannot find.
pub fn system_dsh(remembered: Option<PathBuf>) -> Option<PathBuf> {
    find_launcher(remembered, None)
        .ok()
        .map(|launcher| real_path(&launcher))
}

/// The system `node`, resolved without requiring a `dsh` launcher to exist.
///
/// `DSH_DESKTOP_NODE` is deliberately not consulted: it is an input of its own
/// (`Inputs::env_node`), and feeding it back as "the system node" would push the user's explicit
/// choice through the capability gate that override exists to bypass (review P1-4).
pub fn system_node() -> Option<PathBuf> {
    find_node_without_env(Path::new(""))
}

fn find_node(launcher: &Path) -> Result<PathBuf, String> {
    if let Ok(raw) = std::env::var("DSH_DESKTOP_NODE") {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Ok(p);
        }
    }
    find_node_without_env(launcher).ok_or_else(|| {
        "找不到 node。dsh 以 `#!/usr/bin/env node` 运行，没有 node 时启动会直接失败（exit 127）。"
            .to_string()
    })
}

/// The node belonging to an installation: next to the launcher first, then PATH, then the login
/// shell. Split out from `find_node` so the system probe can run without a launcher (P1-4).
fn find_node_without_env(launcher: &Path) -> Option<PathBuf> {
    if let Some(dir) = launcher.parent() {
        for name in ["node", "node.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    path_lookup("node").or_else(|| login_shell_lookup("node"))
}

pub(crate) fn path_lookup(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    // Windows resolves `node` to `node.exe` and `dsh` to `dsh.cmd`; a bare name would never
    // match there, which made the system runtime look "not installed".
    // Command extensions first: `npm` alone matches the POSIX wrapper npm ships next to
    // `npm.cmd` on Windows, and running that fails with `os error 193`.
    #[cfg(windows)]
    let names: Vec<String> = [".exe", ".cmd", ".bat", ""]
        .iter()
        .map(|extension| format!("{name}{extension}"))
        .collect();
    #[cfg(not(windows))]
    let names: Vec<String> = vec![name.to_string()];
    for dir in std::env::split_paths(&paths) {
        for candidate in &names {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn login_shell_lookup(name: &str) -> Option<PathBuf> {
    // A login shell is a Unix idea: on Windows the app environment and PATH are authoritative,
    // and asking for `/bin/zsh` there only wastes time.
    #[cfg(windows)]
    {
        let _ = name;
        None
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let out = Command::new(shell)
            .args(["-lc", &format!("command -v {name}")])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let p = PathBuf::from(text);
        p.is_file().then_some(p)
    }
}

fn real_path(p: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    crate::unverbatim(&canonical)
}

#[cfg(test)]
mod tests;
