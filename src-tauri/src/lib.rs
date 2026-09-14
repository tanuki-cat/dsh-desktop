//! dsh-desktop: a Tauri shell that supervises `dsh web` and hosts it in the system WebView.

// Windows is a supported target since `feat/bundled-runtime` merged in (2026-09-13):
// `process.rs` probes liveness with `OpenProcess` + `GetExitCodeProcess`, and
// `.github/workflows/windows-portable.yml` stages a bundled runtime on a Windows runner.

pub mod harness;
pub mod locator;
pub mod process;
pub mod runtime;
pub mod shellenv;
pub mod update;
pub mod window;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Listener, Manager, RunEvent};

/// First run may initialise a profile; later runs are fast (measured ~4s on macOS).
const STARTUP_TIMEOUT_FIRST: Duration = Duration::from_secs(90);
const STARTUP_TIMEOUT_NEXT: Duration = Duration::from_secs(30);
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Fixed port keeps the cookie authority stable so a restart can reuse its session.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Agent workspace root; dsh treats the invoking directory as the workspace root.
    #[serde(default = "default_workspace")]
    pub workspace: PathBuf,
    /// `None` shares `~/.dsh` with CLI usage (keeps marketplace plugins and settings).
    pub dsh_home: Option<PathBuf>,
    /// Remembered `dsh` launcher: checked after `DSH_DESKTOP_DSH` and before PATH
    /// (design §4 step 3). Hand-written for installations the search cannot find; a missing
    /// or relative path is ignored for this run.
    #[serde(default)]
    pub dsh_path: Option<PathBuf>,
    /// When another Harness owns the port and we hold no session, stop it and take over,
    /// so the window always ends up with a valid session. Disable to only warn.
    #[serde(default = "default_take_over")]
    pub take_over_existing: bool,
    /// Check the npm registry before every start and install a newer CLI when there is one.
    #[serde(default = "default_auto_update")]
    pub auto_update: bool,
    /// Dist-tags consulted in auto mode; the highest version among them wins.
    #[serde(default = "default_update_tags")]
    pub update_tags: Vec<String>,
    /// Trust a successful registry answer for this many minutes (0 = query on every start).
    /// Measured cost of a query is ~1.9s, so caching keeps the common launch fast.
    #[serde(default = "default_update_interval")]
    pub update_check_interval_minutes: u64,
    /// Import the login shell's exported environment before spawning the CLI: a GUI app
    /// inherits launchd's environment, so DEEPSEEK_API_KEY and similar would be missing.
    #[serde(default = "default_import_shell_env")]
    pub import_shell_env: bool,
    /// Explicit child-environment entries, applied after the shell import.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Refuse to start a CLI outside the range this shell was tested against, instead of
    /// running it and failing in a confusing way. Off by default: we warn and continue.
    #[serde(default)]
    pub require_tested_dsh: bool,
    /// Which runtime to supervise: `auto` (use an installed one when it passes the gates,
    /// otherwise the bundled halves), `bundled` (always the shipped runtime) or `system`
    /// (pre-bundled behaviour, for development).
    #[serde(default)]
    pub runtime: runtime::Preference,
    /// What to do when the supervised CLI is the user's own installation and a newer version
    /// exists: `install` (default — upgrade it in place, the pre-bundled behaviour) or
    /// `notify` (report it and leave the tree alone, §2.4: never rewrite a prefix we do not
    /// own). Only affects system installations; a bundled runtime always updates its shadow
    /// prefix.
    #[serde(default)]
    pub system_updates: runtime::SystemUpdates,
    /// Keep the plugin market (`dshmarket`) in the profile current, the same way the CLI itself
    /// is kept current: check the dist-tags, stop the instance using the profile, install, and
    /// restart so the new plugin is what loads. It rewrites files in the profile
    /// (`package.json` + lockfile), which is why it has its own switch next to `auto_update`.
    #[serde(default = "default_auto_update")]
    pub auto_update_plugins: bool,
}

fn default_port() -> u16 {
    3080
}

fn default_workspace() -> PathBuf {
    match home_dir() {
        // `home_dir` is the platform-aware lookup: Windows has no `HOME`, and a Git-Bash-style
        // `/c/Users/me` is not a usable working directory there.
        Some(home) => home,
        None => home_workspace(None),
    }
}

/// Workspace for a config that does not name one, when no home directory was found at all.
///
/// Falling back to `/` is worse than failing: the agent runs its `glob` and `grep` from the
/// workspace root, so it would walk the whole filesystem. The temporary directory is still local
/// and private, and the log says why `$HOME` was not used.
fn home_workspace(home: Option<String>) -> PathBuf {
    match home {
        Some(home) if !home.trim().is_empty() => PathBuf::from(home),
        _ => {
            let fallback = std::env::temp_dir();
            harness::app_log(&format!(
                "HOME 不可用，workspace 回落到 {}",
                fallback.display()
            ));
            fallback
        }
    }
}

/// The user's home directory, on every platform we build for.
///
/// `HOME` is a Unix convention: a Windows GUI process normally only has `USERPROFILE`
/// (sometimes `HOMEDRIVE` + `HOMEPATH`), so looking for `HOME` alone left the workspace and
/// the profile check pointing at a drive root.
pub fn home_dir() -> Option<PathBuf> {
    // Git Bash exports a POSIX-style `HOME` (`/c/Users/me`) and other setups use a bare drive
    // (`D:`); both are unusable as a process working directory, so validate what we return.
    #[cfg(windows)]
    let keys = ["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let keys = ["HOME", "USERPROFILE"];
    for key in keys {
        if let Some(value) = std::env::var_os(key) {
            let candidate = PathBuf::from(value);
            if usable_directory(&candidate) {
                return Some(candidate);
            }
        }
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    let mut combined = PathBuf::from(drive);
    combined.push(path);
    usable_directory(&combined).then_some(combined)
}

/// Whether a path may be handed to a child process as its working directory.
///
/// Absolute and named. Rejects the empty string and, on Windows, `D:` — drive-relative, meaning
/// "the current directory on D:" — plus MSYS-style `/c/Users/me`, which is not absolute there.
pub fn usable_directory(path: &Path) -> bool {
    path.is_absolute() && !path.as_os_str().is_empty() && path.parent().is_some()
}

/// Drop a Windows verbatim prefix (`\\?\`, `\\?\UNC\`).
///
/// Rust's canonicalization returns verbatim paths on Windows, and the startup path resolves the
/// seed through APIs that canonicalize — logged as `\\?\D:\…\node.exe` and `\\?\D:\…\bin.js`.
/// Win32 file APIs accept them, but anything that *parses* one breaks: node's `fs.realpathSync`
/// walked `\\?\D:\…` component-wise and died on `lstat 'D:'` (`EISDIR: illegal operation on a
/// directory`) before the CLI ever started. The prefix exists to exceed MAX_PATH, so it is only
/// dropped while the path still fits.
pub(crate) fn unverbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let stripped = match text.strip_prefix(r"\\?\UNC\") {
            Some(rest) => Some(format!(r"\\{rest}")),
            None => text.strip_prefix(r"\\?\").map(str::to_string),
        };
        if let Some(stripped) = stripped {
            if stripped.len() < 260 {
                return PathBuf::from(stripped);
            }
        }
    }
    path.to_path_buf()
}

/// Resolve a path against the app's own working directory.
///
/// Every path we hand to the Harness must be absolute: it becomes either the child's working
/// directory or an argv entry, and Windows resolves a relative one (a bare `PATH` entry, a
/// drive-relative `D:tools`) against the *child's* directory instead.
fn absolute(path: &Path) -> PathBuf {
    let path = unverbatim(path);
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
}

fn default_import_shell_env() -> bool {
    true
}

fn default_take_over() -> bool {
    true
}

fn default_auto_update() -> bool {
    true
}

fn default_update_interval() -> u64 {
    60
}

fn default_update_tags() -> Vec<String> {
    vec!["latest".to_string(), "next".to_string()]
}

impl Config {
    fn load(data_dir: &Path) -> Config {
        let path = data_dir.join("config.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Config>(&raw) {
                Ok(mut config) => {
                    // The `env` map may hold credentials, and older versions wrote 0644.
                    process::restrict(&path);
                    config.repair();
                    return config;
                }
                // Never rewrite a file the user owns: fall back for this run and say why.
                Err(error) => harness::app_log(&format!(
                    "config.json 无法解析，本次使用默认配置且不覆盖该文件: {error}"
                )),
            }
        }
        let mut config = Config {
            port: default_port(),
            workspace: default_workspace(),
            dsh_home: None,
            dsh_path: None,
            take_over_existing: true,
            auto_update: true,
            update_tags: default_update_tags(),
            update_check_interval_minutes: default_update_interval(),
            import_shell_env: true,
            env: BTreeMap::new(),
            require_tested_dsh: false,
            runtime: runtime::Preference::Auto,
            system_updates: runtime::SystemUpdates::Install,
            auto_update_plugins: true,
        };
        // The same repair a loaded file gets, so the value seeded here is already usable
        // (a `HOME` that is relative or gone would otherwise become the workspace).
        config.repair();
        let _ = std::fs::create_dir_all(data_dir);
        if !path.exists() {
            let written = std::fs::write(
                &path,
                serde_json::to_vec_pretty(&config).unwrap_or_default(),
            );
            if written.is_ok() {
                process::restrict(&path);
            }
        }
        config
    }

    /// Replace values that would send the Harness somewhere it cannot start.
    ///
    /// A workspace becomes the child's working directory, so a workspace that is missing,
    /// relative, or drive-relative (`D:` written by an earlier Windows build, which makes node
    /// fail with `EISDIR: lstat 'D:'`) cannot start the Harness. The value is repaired in memory
    /// rather than rewritten — the same principle as an unparsable config.json: this run adapts,
    /// the user's file stays as they left it.
    fn repair(&mut self) {
        if usable_directory(&self.workspace) && self.workspace.is_dir() {
            self.workspace = absolute(&self.workspace);
        } else {
            let fallback = default_workspace();
            harness::app_log(&format!(
                "config.json 的 workspace 不是已存在的绝对路径，本次改用 {}: {}",
                fallback.display(),
                self.workspace.display()
            ));
            self.workspace = fallback;
        }
        if let Some(path) = self.dsh_path.clone() {
            // A relative path would be resolved against the app's own working directory,
            // which is unpredictable when the app is started from Finder.
            if !(path.is_absolute() && path.is_file()) {
                harness::app_log(&format!(
                    "config.json 的 dsh_path 不是已存在的绝对路径，本次忽略: {}",
                    path.display()
                ));
                self.dsh_path = None;
            }
        }
        if let Some(home) = self.dsh_home.clone() {
            if usable_directory(&home) {
                self.dsh_home = Some(absolute(&home));
            } else {
                harness::app_log(&format!(
                    "config.json 中的 dsh_home 不可用，已忽略: {}",
                    home.display()
                ));
                self.dsh_home = None;
            }
        }
    }
}

struct Live {
    pid: u32,
    data_dir: PathBuf,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

/// Set as soon as an exit is under way. A Harness that finishes booting afterwards is then
/// stopped by the startup thread instead of outliving the app.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Remember the supervised Harness so every exit path can stop it.
fn adopt(pid: u32, data_dir: &Path) {
    *LIVE.lock().unwrap() = Some(Live {
        pid,
        data_dir: data_dir.to_path_buf(),
    });
}

/// Stop the supervised Harness and drop its state file. Idempotent, so every exit path may
/// call it: window close reaches us as `ExitRequested`, while Cmd+Q / Dock Quit / `quit app`
/// go through tao application_will_terminate and arrive as `Exit` only.
fn shutdown() {
    EXITING.store(true, Ordering::SeqCst);
    let mut guard = LIVE.lock().unwrap();
    if let Some(live) = guard.take() {
        harness::app_log(&format!("stopping Harness pid {}", live.pid));
        process::terminate(live.pid, TERMINATE_GRACE);
        process::clear_state(&live.data_dir);
        harness::app_log("Harness stopped");
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            for label in [window::HARNESS, window::SPLASH] {
                if let Some(existing) = app.get_webview_window(label) {
                    let _ = existing.show();
                    let _ = existing.set_focus();
                    return;
                }
            }
        }))
        .setup(|app| {
            // The splash page reports what this WebView can run; the listener has to exist
            // before that page loads (see `window::WebviewReport`).
            app.listen(window::PROBE_EVENT, |event| {
                window::record_report(event.payload());
            });
            let handle = app.handle().clone();
            window::create_splash(&handle)?;
            std::thread::spawn(move || startup(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build dsh-desktop")
        .run(|_app, event| {
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                shutdown();
            }
        });
}

/// May the instance currently owning the port be stopped so an update can rewrite the CLI tree
/// it serves from? Node loads modules lazily, so updating a live tree breaks the running
/// Harness on its next `require()` — the tree must not be touched while it is in use.
fn may_stop_before_update(
    probe: &harness::Probe,
    is_ours: bool,
    take_over_existing: bool,
) -> Result<(), String> {
    match probe {
        harness::Probe::Closed => Ok(()),
        harness::Probe::Other => Err("端口被其它程序占用，跳过本次更新".to_string()),
        _ if is_ours || take_over_existing => Ok(()),
        _ => Err("端口上是外部 Harness，且 take_over_existing=false，跳过本次更新".to_string()),
    }
}

/// How the instance owning the port must be stopped before an install rewrites the tree it
/// serves from.
///
/// Only a Harness this shell started lives in the process group we created; a foreign one
/// shares its group with whatever terminal or script launched it, and `process::terminate`
/// signals the whole group first (design §13.1 / review P0-3).
#[derive(Debug, PartialEq, Eq)]
enum StopMode {
    ProcessGroup,
    PidOnly,
}

fn stop_mode(ours: bool) -> StopMode {
    if ours {
        StopMode::ProcessGroup
    } else {
        StopMode::PidOnly
    }
}

impl StopMode {
    fn describe(&self) -> &'static str {
        match self {
            StopMode::ProcessGroup => "process group, our own instance",
            StopMode::PidOnly => "pid only, external instance",
        }
    }
}

/// Stop whatever Harness owns the port, so the install cannot rewrite the tree it is serving
/// from. Returns Err with the reason when the instance must be left alone.
fn stop_instance_before_update(
    app: &AppHandle,
    data_dir: &Path,
    port: u16,
    config: &Config,
) -> Result<(), String> {
    let probe = harness::probe(port);
    if matches!(probe, harness::Probe::Closed) {
        return Ok(());
    }
    let owner = harness::listener_pid(port);
    let ours = process::read_state(data_dir)
        .map(|state| state.pid)
        .filter(|pid| Some(*pid) == owner && process::is_alive(*pid));
    may_stop_before_update(&probe, ours.is_some(), config.take_over_existing)?;

    let Some(pid) = owner else {
        return Err(format!(
            "端口 {port} 上已有 Harness，但无法确定它的进程（lsof 不可用），跳过本次更新"
        ));
    };
    let mode = stop_mode(ours.is_some());
    window::set_status(
        app,
        "更新前先停止正在运行的 Harness…",
        &format!("pid {pid}（{}）", mode.describe()),
    );
    let _ = match mode {
        StopMode::ProcessGroup => process::terminate(pid, TERMINATE_GRACE),
        StopMode::PidOnly => process::terminate_pid(pid, TERMINATE_GRACE),
    };
    let deadline = std::time::Instant::now() + TERMINATE_GRACE;
    while !matches!(harness::probe(port), harness::Probe::Closed) {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "停止 pid {pid} 后端口 {port} 仍被占用，跳过本次更新"
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if ours.is_some() {
        process::clear_state(data_dir);
    }
    harness::app_log(&format!(
        "stopped Harness pid {pid} before updating the CLI ({})",
        mode.describe()
    ));
    Ok(())
}

/// The supervised runtime, resolved once at startup.
struct ResolvedRuntime {
    node: PathBuf,
    dsh_js: PathBuf,
    version: String,
    updates: runtime::Updates,
    /// Where the bundled runtime lives, when this build ships one.
    seed: Option<PathBuf>,
    /// True when the supervised tree is this shell's own: the seed inside the bundle, or the
    /// writable shadow prefix. A tree the user installed or pointed at with an environment
    /// variable is theirs — its child PATH, `npm_config_prefix` and `PNPM_HOME` must not be
    /// rewritten (review P1-5).
    bundled: bool,
}

impl ResolvedRuntime {
    fn bundled(&self) -> bool {
        self.bundled
    }

    /// Where a core update may install (`None` = the CLI is the user's own, only notify).
    fn update_prefix(&self, data_dir: &Path) -> Option<PathBuf> {
        match self.updates {
            runtime::Updates::Shadow => Some(data_dir.join("runtime").join("prefix")),
            runtime::Updates::Notify => None,
        }
    }
}

/// The bundled runtime shipped inside the app, when there is one.
///
/// `DSH_DESKTOP_RUNTIME` wins (useful for testing a staged tree without bundling), then the
/// resource directory of the running app, then `src-tauri/runtime` for `make dev`.
fn seed_root_for(resources: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("DSH_DESKTOP_RUNTIME") {
        candidates.push(PathBuf::from(dir));
    }
    if let Some(resources) = resources {
        candidates.push(resources.join("runtime"));
    }
    // `tauri dev` copies the resources next to the binary: `target/<profile>/runtime` is the
    // parent when running the app binary, and the grandparent for anything under `deps/`.
    if let Ok(exe) = std::env::current_exe() {
        for base in [exe.parent(), exe.ancestors().nth(2)].into_iter().flatten() {
            candidates.push(base.join("runtime"));
        }
    }
    candidates
        .into_iter()
        .map(|dir| unverbatim(&dir))
        .find(|dir| node_in(dir).is_some())
}

/// The node binary of a runtime directory.
///
/// Two layouts exist: the Unix tarballs put it in `bin/`, while the Windows distribution is
/// flat (`node.exe` next to `node_modules/npm`), so both are accepted.
fn node_in(runtime: &Path) -> Option<PathBuf> {
    let node = runtime.join("node");
    ["bin/node", "node", "bin/node.exe", "node.exe"]
        .iter()
        .map(|relative| node.join(relative))
        .find(|candidate| candidate.is_file())
}

/// Relative paths of the CLI entry script inside a prefix.
///
/// npm uses `<prefix>/lib/node_modules` on Unix but `<prefix>/node_modules` on Windows, so a
/// staged tree from either platform has to be found.
const DSH_JS_SUFFIXES: [&str; 2] = [
    "lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
    "node_modules/@deepseek-ai/dsh/lib/bin.js",
];

/// Which CLI a successful install left the shell to run, and the version of that tree.
///
/// A shadow install lands in the prefix, while `resolved.dsh_js` still points at the seed
/// resolved at startup: reading the version back from the seed made an update look ineffective,
/// so the launch kept the old core and reinstalled on every start (review P0-2). Pure apart from
/// file reads, so the switch is testable on a temp tree.
fn installed_cli(prefix: Option<&Path>, supervised: &Path, fallback: &str) -> (PathBuf, String) {
    let installed = prefix.and_then(dsh_js_in).filter(|path| path.is_file());
    let version = installed
        .as_deref()
        .and_then(locator::version_of)
        .or_else(|| locator::version_of(supervised))
        .unwrap_or_else(|| fallback.to_string());
    (
        installed.unwrap_or_else(|| supervised.to_path_buf()),
        version,
    )
}

/// The CLI entry script inside an npm prefix, when it is there.
fn dsh_js_in(prefix: &Path) -> Option<PathBuf> {
    DSH_JS_SUFFIXES
        .iter()
        .map(|suffix| prefix.join(suffix))
        .find(|candidate| candidate.is_file())
}

/// The verdict on the user's own installation (§2.4).
#[derive(Debug, PartialEq, Eq)]
enum SystemGate {
    Accept,
    /// Usable, but the bundled runtime would be a better fit: say so and keep it.
    Warn(String),
    Reject(String),
}

/// Gate the system installation on the two facts `probe_node` reports.
///
/// `module.stripTypeScriptTypes` is fatal either way: the CLI's code-runtime worker needs it and
/// `@deepseek-ai/dsh` declares no `engines.node`, so Node < 22.13 cannot run it (measured). A
/// wrong architecture is different — an x64 node under Rosetta runs a self-consistent x64 tree,
/// so it is only worth rejecting when a bundled runtime can take its place; without one, refusing
/// would turn a slow-but-working setup into an error page.
fn system_runtime_gate(facts: &locator::NodeFacts, host_arch: &str, bundled: bool) -> SystemGate {
    if !facts.strip_types {
        return SystemGate::Reject(format!(
            "node 缺少 module.stripTypeScriptTypes（需要 Node 22.13 以上，当前架构 {}）",
            facts.arch
        ));
    }
    if facts.arch != host_arch {
        let reason = format!("node 架构 {} 与宿主 {} 不一致", facts.arch, host_arch);
        return if bundled {
            SystemGate::Reject(format!("{reason}，改用自带运行时"))
        } else {
            SystemGate::Warn(format!(
                "{reason}，且本构建没有自带运行时，继续使用系统安装"
            ))
        };
    }
    SystemGate::Accept
}

/// Apply the capability gate to the user's own installation.
///
/// Pure, so the matrix stays testable. `None` means there was no system node to judge — a dsh
/// without node is still usable on the bundled node — while a rejected installation drops both
/// halves: the gate judges the installation, not the file (§2.4).
fn keep_system_install(
    node: Option<PathBuf>,
    dsh: Option<PathBuf>,
    gate: Option<SystemGate>,
) -> (Option<PathBuf>, Option<PathBuf>, Option<String>) {
    match gate {
        Some(SystemGate::Reject(reason)) => (None, None, Some(reason)),
        _ => (node, dsh, None),
    }
}

/// Decide which node and which dsh tree to supervise (bundled-runtime plan §2.3/§2.4).
fn resolve_runtime(
    resources: Option<&Path>,
    data_dir: &Path,
    config: &Config,
) -> Result<ResolvedRuntime, String> {
    let seed = seed_root_for(resources);
    let seed_node = seed.as_deref().and_then(node_in);
    let seed_dsh = seed
        .as_deref()
        .and_then(|dir| dsh_js_in(&dir.join("dsh-prefix")));
    let seed_dsh = seed_dsh.filter(|path| path.is_file());
    let seed_version = seed_dsh.as_deref().and_then(locator::version_of);

    let shadow_dsh = dsh_js_in(&data_dir.join("runtime").join("prefix"));
    let shadow_version = shadow_dsh.as_deref().and_then(locator::version_of);

    // The user's own installation, resolved as two independent halves: a machine can have node
    // without dsh, and §2.4 expects "system node + bundled dsh" to work — resolving the pair with
    // one `locate()` made that cell unreachable (review P1-4).
    let preference = std::env::var("DSH_DESKTOP_RUNTIME_PREFERENCE")
        .ok()
        .and_then(|value| runtime::Preference::parse(&value))
        .unwrap_or(config.runtime);
    let env_node = std::env::var("DSH_DESKTOP_NODE")
        .ok()
        .map(|value| absolute(Path::new(&value)))
        .filter(|path| path.is_file());
    let env_dsh = std::env::var("DSH_DESKTOP_DSH")
        .ok()
        .map(|value| absolute(Path::new(&value)))
        .filter(|path| path.is_file());
    // Probing the system installation costs a login shell and up to 5 s of node probing; when the
    // answer cannot change the outcome, skip it (review P2-14).
    let probe_system =
        preference != runtime::Preference::Bundled && !(env_node.is_some() && env_dsh.is_some());
    if !probe_system {
        harness::app_log("跳过系统运行时探测：preference/env 已经决定了用哪棵树");
    }
    let system_node = probe_system.then(locator::system_node).flatten();
    let system_dsh = probe_system
        .then(|| locator::system_dsh(config.dsh_path.clone()))
        .flatten();
    let facts = system_node
        .as_ref()
        .and_then(|node| locator::probe_node(node));
    let host_arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    let has_seed = seed_node.is_some() && seed_dsh.is_some();
    let gate = facts
        .as_ref()
        .map(|facts| system_runtime_gate(facts, host_arch, has_seed));
    if let Some(SystemGate::Warn(reason)) = &gate {
        harness::app_log(&format!("system runtime kept with a warning: {reason}"));
    }
    if let Some(SystemGate::Reject(reason)) = &gate {
        harness::app_log(&format!("system runtime rejected: {reason}"));
    }
    let (system_node, system_dsh, rejection) = keep_system_install(system_node, system_dsh, gate);

    let decision = runtime::decide(&runtime::Inputs {
        preference,
        env_node,
        env_dsh,
        system_node,
        system_dsh,
        seed_node: seed_node.as_deref(),
        seed_dsh: seed_dsh.as_deref(),
        shadow_dsh: shadow_dsh.as_deref(),
        seed_version: seed_version.as_deref(),
        shadow_version: shadow_version.as_deref(),
    })
    .ok_or_else(|| match &rejection {
        // Say which gate failed: "找不到 dsh" would be wrong, and the fix differs (upgrade
        // node vs. install dsh).
        Some(reason) => format!(
            "系统安装的 dsh 不可用（{reason}），本构建也没有自带运行时。请升级 node / 重装 @deepseek-ai/dsh，或用 DSH_DESKTOP_DSH / DSH_DESKTOP_NODE 指定路径。"
        ),
        None => "找不到 dsh，也没有自带运行时。请安装 node 与 @deepseek-ai/dsh，或用 DSH_DESKTOP_DSH / DSH_DESKTOP_NODE 指定路径。"
            .to_string(),
    })?;

    let describe = format!(
        "{} | updates: {:?} (system_updates: {})",
        decision.describe(),
        decision.updates,
        config.system_updates.label()
    );
    harness::app_log(&describe);
    let version = locator::version_of(&decision.dsh.path).unwrap_or_else(|| "未知".into());
    Ok(ResolvedRuntime {
        node: absolute(&decision.node.path),
        dsh_js: absolute(&decision.dsh.path),
        version,
        updates: decision.updates,
        seed,
        bundled: matches!(
            decision.dsh.origin,
            runtime::Origin::Seed | runtime::Origin::Shadow
        ),
    })
}

/// The login-shell capture: the shell that was run, and the variables it printed.
type EnvCapture = std::thread::JoinHandle<(String, Option<(String, BTreeMap<String, String>)>)>;

/// What the first-launch seeding did.
///
/// `seeded` is not just for the log: the launch that has just written the template must not
/// go on to check the registry, or "first start downloads nothing" stops being true — the
/// template already pins a plugin market version (review A2).
#[derive(Default)]
struct SeedOutcome {
    seeded: bool,
    note: Option<String>,
}

/// First launch of a bundled build: install the profile template (which carries the plugin
/// market) into the user's DSH_HOME, unless a profile is already there (plan §2.5).
fn seed_profile_template(config: &Config, seed: Option<&Path>) -> SeedOutcome {
    let mut outcome = SeedOutcome::default();
    let Some(seed) = seed else {
        return outcome;
    };
    let template = seed.join("profile-template");
    if !template.join("package.json").is_file() {
        return outcome;
    }
    let Some(home) = config
        .dsh_home
        .clone()
        .or_else(|| home_dir().map(|home| home.join(".dsh")))
    else {
        return outcome;
    };
    let profile = home.join("profiles").join("web");
    if profile.exists() {
        return outcome;
    }
    match copy_tree(&template, &profile) {
        Ok(()) => {
            outcome.seeded = true;
            outcome.note = Some(format!(
                "seeded profile template into {}",
                profile.display()
            ));
        }
        Err(error) => outcome.note = Some(format!("could not seed the profile template: {error}")),
    }
    outcome
}

/// Copy a tree into `to`, building a sibling `.tmp` directory first.
///
/// A failure half-way (full disk, permissions) must not leave a partial `profiles/web`: the
/// next launch sees that directory exists and neither re-seeds nor repairs it (review P1-6).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut name = to.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    let staging = to.with_file_name(name);
    let _ = std::fs::remove_dir_all(&staging);
    if let Err(error) = copy_tree_into(from, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, to) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(())
}

fn copy_tree_into(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree_into(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Forget a supervised pid without signalling it (used when the process is already gone).
fn disown(pid: u32) {
    let mut guard = LIVE.lock().unwrap();
    if guard.as_ref().is_some_and(|live| live.pid == pid) {
        *guard = None;
    }
}

/// A launch that failed after spawning must not leave a half-started Harness behind: it
/// would keep the port and make the failure page a lie.
fn abort_start(pid: u32, reason: String) -> Result<(), String> {
    process::terminate(pid, TERMINATE_GRACE);
    disown(pid);
    Err(reason)
}

/// Wait for the Harness to end. An unexpected exit leaves the window on a page that can never
/// connect again, so report it instead of letting the app look alive.
fn watch_harness(
    app: AppHandle,
    data_dir: PathBuf,
    pid: u32,
    ring: harness::Ring,
    mut child: Child,
) {
    let status = child.wait();
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    let code = status.as_ref().ok().and_then(|status| status.code());
    harness::app_log(&format!(
        "Harness pid {pid} exited unexpectedly (code {code:?})"
    ));
    disown(pid);
    process::clear_state(&data_dir);
    window::show_failure(
        &app,
        "Harness 已退出",
        &format!(
            "dsh web 进程已结束（退出码 {code:?}）。关闭本窗口即退出应用，重新启动即可恢复。\n\n最近输出:\n{}",
            ring.tail()
        ),
    );
}

/// What step 1 does with the record a crashed shell may have left behind.
#[derive(Debug, PartialEq, Eq)]
enum SelfHeal {
    /// The record is stale (the pid is gone, or it belongs to something else now): drop it.
    Clear,
    /// The record is ours and still owns its port, but this run will use another one: stop it.
    Terminate,
    /// Ours, still serving the port this run will use: keep the file so the detection step can
    /// reuse the instance instead of restarting it.
    Keep,
}

/// Pure decision for step 1, so the three branches are testable without a running process.
///
/// `listener` is the pid that owns `state.port`; `looks_ours` is the secondary identity check
/// for a record whose pid no longer owns its port (see [`looks_like_our_orphan`]).
fn self_heal_action(
    state: &process::HarnessState,
    listener: Option<u32>,
    alive: bool,
    port: u16,
    looks_ours: bool,
) -> SelfHeal {
    if !alive {
        return SelfHeal::Clear;
    }
    if listener != Some(state.pid) {
        // The pid was reused, or the instance lost its port: only a process that still looks
        // like our own orphaned CLI may be signalled.
        return if looks_ours {
            SelfHeal::Terminate
        } else {
            SelfHeal::Clear
        };
    }
    if state.port == port {
        SelfHeal::Keep
    } else {
        SelfHeal::Terminate
    }
}

/// Does this pid still look like the `dsh web` this shell started?
///
/// Only consulted for a record whose pid no longer owns its port, where the other possible
/// reading is pid reuse. Two signals make that misread unlikely: the command line carries the
/// flags this shell passes (`--profile web`, a `dsh` entry point), and nobody owns the process
/// any more — a leftover of a crashed shell is handed to the session supervisor, while a
/// session someone still runs (a terminal, or another shell of ours) keeps its parent.
fn looks_like_our_orphan(pid: u32) -> bool {
    // `-ww` matters: the flags that identify our spawn sit behind a long node path, and some
    // `ps` builds truncate the command column to the terminal width without it.
    let output = match std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "ppid=,command="])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            harness::app_log(&format!(
                "ps 不可用，无法确认残留进程 {pid} 的身份，按无关进程处理: {error}"
            ));
            return false;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((ppid, _)) = parse_ps_identity(&text) else {
        return false;
    };
    looks_like_our_harness(&text, classify_parent(ppid))
}

/// `ps -p <pid> -o ppid=,command=` output -> (parent pid, command line).
fn parse_ps_identity(output: &str) -> Option<(u32, String)> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    let mut fields = line.split_whitespace();
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, fields.collect::<Vec<_>>().join(" ")))
}

/// Who owns the candidate process now?
#[derive(Debug, PartialEq, Eq)]
enum Parent {
    /// The parent is gone: the ordinary outcome of a shell crash on macOS and Linux alike.
    Gone,
    /// A session supervisor adopted it: launchd (pid 1) on macOS, `systemd --user` on most Linux
    /// desktops. The latter is a child subreaper, so the orphan lands on a pid far from 1 —
    /// assuming pid 1 here would leave the Linux leftovers uncleaned (review R1).
    Supervisor,
    /// A live process that is neither: a shell or another `dsh-desktop`, so somebody still runs
    /// this session. Never signal it.
    Live,
}

/// Classify the candidate's parent.
///
/// Anything that cannot be established counts as [`Parent::Live`]: when the answer is unknown,
/// keeping the process is the safe side.
fn classify_parent(ppid: u32) -> Parent {
    if ppid == 0 {
        // The kernel: no userspace parent is left to own it.
        return Parent::Gone;
    }
    let output = match std::process::Command::new("ps")
        .args(["-ww", "-p", &ppid.to_string(), "-o", "command="])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Parent::Live,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(command) = text.lines().find(|line| !line.trim().is_empty()) else {
        return Parent::Gone;
    };
    if is_session_supervisor(command) {
        Parent::Supervisor
    } else {
        Parent::Live
    }
}

/// Names an orphan is handed to on the platforms this shell builds for.
fn is_session_supervisor(command: &str) -> bool {
    command.contains("launchd") || command.contains("systemd") || command.contains("init")
}

/// Is this candidate the orphaned `dsh web` this shell started?
fn looks_like_our_harness(output: &str, parent: Parent) -> bool {
    let Some((_, command)) = parse_ps_identity(output) else {
        return false;
    };
    command.contains("--profile web") && command.contains("dsh") && parent != Parent::Live
}

/// Why the plugin market cannot be updated this launch, when it cannot.
///
/// The decision is taken *before* the running Harness is stopped: `dsh plugin add` is a thin
/// wrapper around pnpm, so without a resolvable pnpm the CLI exits 127 — stopping the instance
/// for an install that cannot succeed would turn a no-op into an outage on every launch
/// (review A1).
#[derive(Debug, PartialEq, Eq)]
enum PluginSkip {
    /// Declared in the profile but not installed: installing it is a repair, not an update, and
    /// the user may be mid-way through their own plugin surgery.
    NotInstalled,
    /// No `pnpm` on the PATH the CLI will run with (the bundled tools prefix is missing, or the
    /// build has no bundled runtime and the machine has no system pnpm).
    NoPnpm,
}

impl PluginSkip {
    fn reason(&self) -> String {
        match self {
            PluginSkip::NotInstalled => {
                format!("{} 已声明但未安装，跳过自动更新", update::MARKET_PLUGIN)
            }
            PluginSkip::NoPnpm => {
                "PATH 上没有 pnpm，跳过插件市场更新（不停止正在运行的 Harness）".to_string()
            }
        }
    }
}

fn plugin_skip_reason(installed: Option<&str>, pnpm: Option<&Path>) -> Option<PluginSkip> {
    if installed.is_none() {
        return Some(PluginSkip::NotInstalled);
    }
    if pnpm.is_none() {
        return Some(PluginSkip::NoPnpm);
    }
    None
}

/// PATH entries that go in front of whatever the app inherited, in priority order.
///
/// The shipped `tools` prefix is what makes plugin management work at all: the CLI forwards to
/// pnpm, which only lives there (and in the writable prefix updates target) — a Finder-launched
/// app inherits launchd's PATH, which has neither (review A1).
fn tool_path_prefix(
    node: &Path,
    seed: Option<&Path>,
    bundled: bool,
    data_dir: &Path,
) -> Vec<String> {
    let mut prefix: Vec<String> = Vec::new();
    if let Some(dir) = node.parent() {
        prefix.push(dir.to_string_lossy().to_string());
    }
    if bundled {
        // npm puts shims in `bin/` on Unix and directly in the prefix on Windows; PATH entries
        // that do not exist are harmless, so both are offered.
        let tools = data_dir.join("runtime").join("tools");
        prefix.push(tools.join("bin").to_string_lossy().to_string());
        prefix.push(tools.to_string_lossy().to_string());
        if let Some(seed) = seed {
            let shipped = seed.join("tools");
            prefix.push(shipped.join("bin").to_string_lossy().to_string());
            prefix.push(shipped.to_string_lossy().to_string());
        }
    }
    #[cfg(target_os = "macos")]
    {
        // A macOS GUI app inherits launchd's PATH; Homebrew lives here.
        prefix.push("/opt/homebrew/bin".to_string());
        prefix.push("/usr/local/bin".to_string());
    }
    #[cfg(windows)]
    {
        // A launcher may hand us a sanitized environment; `cmd.exe` and friends still expect
        // the system directories to be on PATH.
        if let Some(root) = std::env::var_os("SystemRoot") {
            let root = PathBuf::from(root);
            prefix.push(root.join("System32").to_string_lossy().to_string());
            prefix.push(root.to_string_lossy().to_string());
        }
    }
    prefix
}

/// The environment this shell's children run with, assembled once.
///
/// `path` is what the plugin install gets, and it is the very value the pnpm probe looked at,
/// so the answer to "is pnpm there?" and the install cannot disagree. The npm calls that check
/// and install the CLI keep their own node-first PATH: they only ever run npm, which needs
/// nothing beyond the node sitting next to it.
struct ChildEnv {
    /// `PATH` as a string, for the commands the shell runs itself.
    path: String,
    /// The full environment handed to the Harness.
    vars: Vec<(String, String)>,
}

impl ChildEnv {
    /// `imported` is the login shell capture, when there was one.
    fn assemble(
        config: &Config,
        node: &Path,
        seed: Option<&Path>,
        bundled: bool,
        data_dir: &Path,
        imported: &BTreeMap<String, String>,
    ) -> ChildEnv {
        let prefix = tool_path_prefix(node, seed, bundled, data_dir);
        let merged = shellenv::merge_path(
            &prefix,
            imported.get("PATH").map(String::as_str),
            std::env::var("PATH").ok().as_deref(),
        );
        let mut vars: Vec<(String, String)> = imported
            .iter()
            .filter(|(key, _)| key.as_str() != "PATH")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        vars.push(("PATH".to_string(), merged.clone()));
        if bundled {
            // Recentred global installs: with the read-only seed in front of PATH, `npm i -g`
            // from the harness would target the app bundle. Point it (and pnpm) at the writable
            // tools prefix.
            let tools = data_dir.join("runtime").join("tools");
            vars.retain(|(key, _)| key != "npm_config_prefix" && key != "PNPM_HOME");
            vars.push((
                "npm_config_prefix".to_string(),
                tools.to_string_lossy().to_string(),
            ));
            vars.push((
                "PNPM_HOME".to_string(),
                tools.join("bin").to_string_lossy().to_string(),
            ));
        }
        for (key, value) in &config.env {
            vars.retain(|(existing, _)| existing != key);
            vars.push((key.clone(), value.clone()));
        }
        ChildEnv { path: merged, vars }
    }
}

fn startup(app: AppHandle) {
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(error) => return fail(&app, "无法获取应用数据目录", &error.to_string()),
    };
    let _ = std::fs::create_dir_all(&data_dir);

    window::set_status(&app, "正在检查运行环境…", "");
    if let Err(reason) = start(&app, &data_dir) {
        fail(&app, "DeepSeek Harness 启动失败", &reason);
    }
}

fn start(app: &AppHandle, data_dir: &Path) -> Result<(), String> {
    harness::init_app_log(&data_dir.join("logs").join("harness.log"));

    // The config comes first: step 1 needs the port this run will use to tell a leftover
    // instance it may hand to the reuse branch from one it must stop.
    let config = Config::load(data_dir);
    let port = config.port;
    // The two paths every startup failure is traced back to; cheap to log, and the only way
    // to diagnose a machine we cannot run on.
    harness::app_log(&format!(
        "app data dir = {} | workspace = {}",
        data_dir.display(),
        config.workspace.display()
    ));

    // 1) Self-heal: an instance left behind by a crashed shell is dealt with before anything
    //    else. The state file survives a crash (only a normal exit and the watchdog remove it),
    //    so the recorded pid is trusted only while it still owns the recorded port — a reboot can
    //    hand the same low pid to an unrelated process, and `terminate` signals a whole group.
    //    A record that is still serving the port this run will use is *kept*: the detection step
    //    below then reuses a live session (its cookie is still valid) instead of restarting it.
    if let Some(state) = process::read_state(data_dir) {
        let alive = process::is_alive(state.pid);
        let listener = harness::listener_pid(state.port);
        // The `ps` call is only worth making when the record is alive but no longer owns its
        // port, which is exactly the case where the alternative reading is pid reuse.
        let looks_ours = alive && listener != Some(state.pid) && looks_like_our_orphan(state.pid);
        match self_heal_action(&state, listener, alive, port, looks_ours) {
            SelfHeal::Keep => harness::app_log(&format!(
                "state.json 记录的 Harness pid {} 仍在端口 {} 服务，交给复用分支处理",
                state.pid, state.port
            )),
            SelfHeal::Terminate => {
                window::set_status(
                    app,
                    "正在清理上次残留的 Harness…",
                    &format!("pid {}", state.pid),
                );
                process::terminate(state.pid, TERMINATE_GRACE);
                process::clear_state(data_dir);
            }
            SelfHeal::Clear => {
                if alive {
                    harness::app_log(&format!(
                        "state.json 记录的 pid {} 仍存活，但不是本应用启动的 Harness（监听端口 {} 的是 {:?}），只清理状态文件",
                        state.pid, state.port, listener
                    ));
                }
                process::clear_state(data_dir);
            }
        }
    }

    // The login-shell capture costs ~160 ms and depends on nothing that follows, so it runs
    // alongside the version lookup and update check and is joined just before the child env
    // is assembled.
    // A Windows GUI process already inherits the user's variables and has no login shell to
    // capture; asking for `/bin/zsh` there only delayed startup and changed nothing.
    #[cfg(windows)]
    let env_capture: Option<EnvCapture> = {
        if config.import_shell_env {
            harness::app_log(
                "使用应用环境：Windows 的 GUI 进程已继承用户环境，无登录 shell 可捕获",
            );
        }
        None
    };
    #[cfg(not(windows))]
    let env_capture: Option<EnvCapture> = config.import_shell_env.then(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        std::thread::spawn(move || {
            let imported = shellenv::import(Path::new(&shell));
            (shell, imported)
        })
    });

    // 3) Resolve the runtime: bundled seed / shadow prefix / the user's install (§2.3/§2.4).
    window::set_status(app, "正在解析运行时…", "");
    let mut resolved =
        resolve_runtime(app.path().resource_dir().ok().as_deref(), data_dir, &config)?;
    let mut version = resolved.version.clone();

    // The profile directory decides the startup timeout, so look at it *before* seeding: a
    // freshly seeded profile made the very first launch take the 30 s "warm" timeout instead of
    // the 90 s budget meant for a first start (review P1-6).
    let home = config
        .dsh_home
        .clone()
        .or_else(|| home_dir().map(|home| home.join(".dsh")))
        .unwrap_or_else(|| PathBuf::from(".dsh"));
    let first_launch = !home.join("profiles").join("web").exists();

    // First launch of a bundled build: put the profile template (plugin market included) in
    // place before the CLI starts, so the marketplace exists without any download (§2.5).
    // Only for the shell's own runtime: the template pins a dshmarket version, and a user who
    // installed or pointed at their own tree should not be given one (review P1-7).
    let mut seeded_this_run = false;
    if resolved.bundled() {
        let outcome = seed_profile_template(&config, resolved.seed.as_deref());
        seeded_this_run = outcome.seeded;
        if let Some(note) = outcome.note {
            harness::app_log(&note);
        }
    }

    // The update path installs into `runtime/prefix` and keeps npm's cache there: create both
    // up front (idempotent), like the plan's §3 step 2 asks (review P2-11).
    let runtime_root = data_dir.join("runtime");
    for name in ["prefix", "tools", "npm-cache"] {
        if let Err(error) = std::fs::create_dir_all(runtime_root.join(name)) {
            harness::app_log(&format!(
                "无法创建 {}/{}: {error}",
                runtime_root.display(),
                name
            ));
        }
    }

    // 3b) Update the supervised CLI before booting it, so "core upgrade" needs no terminal.
    let mut just_updated = false;
    if config.auto_update && !version.is_empty() {
        match update::npm_for(&resolved.node) {
            None => harness::app_log("找不到 npm，跳过更新检查"),
            Some(npm) => {
                let interval = config.update_check_interval_minutes;
                let checked = update::check_cached(
                    &npm,
                    update::PACKAGE,
                    &config.update_tags,
                    &version,
                    data_dir,
                    interval,
                );
                if checked.cached {
                    harness::app_log(&format!(
                        "update check: cached answer (interval {interval} min)"
                    ));
                } else {
                    window::set_status(app, "正在检查 dsh 更新…", &format!("当前 dsh {version}"));
                }
                // A cached answer whose install already changed nothing must not be installed
                // again: npm wrote the package somewhere this shell does not run it from, so
                // every launch would stop the Harness and rebuild a 289 MB tree (review P0-2).
                let already_attempted = checked.attempted;
                match checked.status {
                    update::Status::UpdateAvailable { to, .. } if already_attempted => {
                        harness::app_log(&format!(
                            "update {to} was already attempted and changed nothing; not installing again"
                        ));
                    }
                    update::Status::UpdateAvailable { from, to } => {
                        window::set_status(
                            app,
                            &format!("发现新版本 v{to}，正在更新…"),
                            &format!("v{from} -> v{to}"),
                        );
                        // The user's own installation is upgraded in place by default (that is
                        // what this shell always did); `system_updates: notify` in config.json
                        // switches to reporting only, for anyone who would rather run their own
                        // npm upgrade (§2.4).
                        if matches!(resolved.updates, runtime::Updates::Notify)
                            && config.system_updates == runtime::SystemUpdates::Notify
                        {
                            harness::app_log(&format!(
                                "update available: {from} -> {to}; policy {} leaves the user install alone",
                                config.system_updates.label()
                            ));
                            window::set_status(
                                app,
                                &format!("有新版本 v{to}（system_updates=notify，未自动更新）"),
                                &format!("v{from} -> v{to}"),
                            );
                        } else {
                            harness::app_log(&format!(
                                "update available: {from} -> {to}, installing"
                            ));
                            // npm rewrites the CLI tree in place. Anything serving from it must be
                            // stopped first, or the live session breaks on its next lazy require().
                            // A deferred update only skips the install: startup continues and the
                            // instance keeps running (its tree was never touched).
                            match stop_instance_before_update(app, data_dir, port, &config) {
                                Err(reason) => harness::app_log(&format!(
                                    "update deferred, keeping v{from}: {reason}"
                                )),
                                Ok(()) => {
                                    let prefix = resolved
                                        .update_prefix(data_dir)
                                        .or_else(|| update::install_prefix(&resolved.dsh_js));
                                    let cache = runtime_root.join("npm-cache");
                                    match update::install(
                                        &npm,
                                        update::PACKAGE,
                                        &to,
                                        prefix.as_deref(),
                                        Some(&cache),
                                    ) {
                                        Ok(()) => {
                                            // A shadow install lands in a different tree than the
                                            // seed resolved at startup (review P0-2).
                                            let (installed_path, installed_version) = installed_cli(
                                                prefix.as_deref(),
                                                &resolved.dsh_js,
                                                &to,
                                            );
                                            version = installed_version;
                                            if version == from {
                                                // npm installed the package somewhere other than where
                                                // this CLI lives (custom prefix, pnpm/yarn/volta layout),
                                                // so the supervised binary is unchanged. Say so instead of
                                                // claiming an update and restarting for nothing, and
                                                // remember the attempt so the cached answer does not
                                                // repeat it on the next launch.
                                                update::mark_attempt_ineffective(data_dir, &to);
                                                harness::app_log(&format!(
                                                "update installed but the supervised CLI is still {from}; check npm global prefix"
                                            ));
                                            } else {
                                                resolved.dsh_js = installed_path;
                                                just_updated = true;
                                                harness::app_log(&format!(
                                                    "dsh updated: {from} -> {to}"
                                                ));
                                            }
                                        }
                                        Err(reason) => harness::app_log(&format!(
                                            "update failed, keeping v{from}: {reason}"
                                        )),
                                    }
                                }
                            }
                        }
                    }
                    update::Status::UpToDate { version: current } => {
                        harness::app_log(&format!("dsh is up to date: {current}"));
                    }
                    update::Status::Skipped => {}
                    update::Status::Failed { reason } => {
                        harness::app_log(&format!(
                            "update check failed, keeping v{version}: {reason}"
                        ));
                    }
                }
            }
        }
    }

    // 3b4) Assemble the environment every child of this shell gets: a GUI-launched app inherits
    //      launchd's environment, not the login shell's, so the CLI would miss DEEPSEEK_API_KEY
    //      and friends. This runs before the plugin step because `dsh plugin add` forwards to
    //      pnpm, and pnpm only exists in the bundled tools prefix or on the login shell PATH
    //      (review A1); the Harness then gets this very PATH, so the shell and everything it
    //      spawns share one toolchain.
    let mut imported: BTreeMap<String, String> = BTreeMap::new();
    if let Some(handle) = env_capture {
        let captured = handle.join().ok();
        let shell = captured
            .as_ref()
            .map(|(shell, _)| shell.clone())
            .unwrap_or_else(|| "登录 shell".to_string());
        match captured.and_then(|(_, imported)| imported) {
            Some((flag, vars)) => {
                let names: Vec<String> = vars.keys().cloned().collect();
                // Names only: values may be credentials.
                harness::app_log(&format!(
                    "imported {} env vars via {shell} {flag}: {}",
                    names.len(),
                    names.join(", ")
                ));
                imported = vars;
            }
            None => harness::app_log(&format!(
                "login shell env import failed ({shell}); using the app environment"
            )),
        }
    }
    let child = ChildEnv::assemble(
        &config,
        &resolved.node,
        resolved.seed.as_deref(),
        resolved.bundled(),
        data_dir,
        &imported,
    );
    harness::app_log(&format!("child PATH = {}", child.path));

    // 3b3) The plugin market lives in the user profile rather than in the CLI tree, but it ages
    //      the same way: check the dist-tags, stop the instance that is using the profile, install,
    //      and let the new plugin load. `auto_update_plugins` turns the whole step off.
    let check_market = config.auto_update && config.auto_update_plugins;
    if check_market && seeded_this_run {
        // The template this launch just wrote already pins a plugin market version; asking the
        // registry right after it would make a first start download, which is the one thing the
        // seeded template exists to avoid (review A2).
        harness::app_log("刚播种 profile 模板，本轮不检查插件市场（下次启动再查）");
    }
    if check_market && !seeded_this_run {
        let profile_dir = home.join("profiles").join("web");
        if update::declares_plugin(&profile_dir, update::MARKET_PLUGIN) {
            let installed = update::installed_plugin(&profile_dir, update::MARKET_PLUGIN);
            // Resolve pnpm before anything is stopped: it is what actually installs a plugin,
            // and without it the CLI exits 127 after the instance is already gone (review A1).
            let pnpm = update::find_pnpm(Some(OsStr::new(&child.path)));
            if let Some(skip) = plugin_skip_reason(installed.as_deref(), pnpm.as_deref()) {
                harness::app_log(&skip.reason());
            } else if let Some(current) = installed {
                match update::npm_for(&resolved.node) {
                    None => harness::app_log("找不到 npm，跳过插件市场更新检查"),
                    Some(npm) => {
                        let interval = config.update_check_interval_minutes;
                        let checked = update::check_plugin_cached(
                            &npm,
                            update::MARKET_PLUGIN,
                            &config.update_tags,
                            &current,
                            data_dir,
                            interval,
                        );
                        match checked.status {
                            update::Status::UpdateAvailable { to, .. } if checked.attempted => {
                                harness::app_log(&format!(
                                    "plugin {} {to} was already attempted and changed nothing; not installing again",
                                    update::MARKET_PLUGIN
                                ));
                            }
                            update::Status::UpdateAvailable { to, .. }
                                if checked.failed_recently =>
                            {
                                harness::app_log(&format!(
                                    "plugin {} {to} failed to install last time; not retrying for {} minutes",
                                    update::MARKET_PLUGIN,
                                    update::FAILED_RETRY_MINUTES
                                ));
                            }
                            update::Status::UpdateAvailable { from, to } => {
                                window::set_status(
                                    app,
                                    "正在更新插件市场…",
                                    &format!("v{from} -> v{to}"),
                                );
                                harness::app_log(&format!(
                                    "plugin update available: {} {from} -> {to}, installing",
                                    update::MARKET_PLUGIN
                                ));
                                // Same hazard as a core update: pnpm rewrites profile
                                // node_modules in place and a running harness would break on its
                                // next lazy require().
                                match stop_instance_before_update(app, data_dir, port, &config) {
                                    Err(reason) => harness::app_log(&format!(
                                        "plugin update deferred, keeping v{from}: {reason}"
                                    )),
                                    Ok(()) => match update::install_plugin(
                                        &resolved.node,
                                        &resolved.dsh_js,
                                        "web",
                                        update::MARKET_PLUGIN,
                                        &to,
                                        config.dsh_home.as_deref(),
                                        OsStr::new(&child.path),
                                    ) {
                                        Ok(()) => {
                                            let after = update::installed_plugin(
                                                &profile_dir,
                                                update::MARKET_PLUGIN,
                                            )
                                            .unwrap_or_default();
                                            if after == from {
                                                // pnpm wrote the package somewhere the profile
                                                // does not load it from: report and remember the
                                                // attempt instead of retrying on every launch.
                                                update::mark_plugin_attempt_ineffective(
                                                    data_dir, &to,
                                                );
                                                harness::app_log(&format!(
                                                    "plugin installed but the profile still loads {} {from}",
                                                    update::MARKET_PLUGIN
                                                ));
                                            } else {
                                                just_updated = true;
                                                harness::app_log(&format!(
                                                    "plugin updated: {} {from} -> {after}",
                                                    update::MARKET_PLUGIN
                                                ));
                                            }
                                        }
                                        Err(reason) => {
                                            // A failed install is usually transient, so it only
                                            // suppresses the next attempt for a few minutes —
                                            // long enough not to stop the Harness again right
                                            // away, short enough to recover on its own
                                            // (review A1/A5).
                                            update::mark_plugin_attempt_failed(data_dir, &to);
                                            harness::app_log(&format!(
                                                "plugin update failed, keeping v{from}: {reason}"
                                            ));
                                        }
                                    },
                                }
                            }
                            update::Status::UpToDate { version } => harness::app_log(&format!(
                                "plugin {} is up to date: {version}",
                                update::MARKET_PLUGIN
                            )),
                            update::Status::Skipped => {}
                            update::Status::Failed { reason } => harness::app_log(&format!(
                                "plugin update check failed, keeping v{current}: {reason}"
                            )),
                        }
                    }
                }
            }
        }
    }

    // 3b2) The shell drives the CLI through `--profile web --patch … --no-open --port N` and reads
    //      its startup line. Versions outside the range we test against may change either, so say so
    //      now instead of failing later with a timeout that hides the real reason.
    let compatibility = update::compatibility(&version);
    let untested = !matches!(compatibility, update::Compatibility::Tested);
    if untested {
        harness::app_log(&format!("warning: {}", compatibility.describe()));
        if config.require_tested_dsh {
            return Err(format!(
                "{}\n\n如需强行使用，请在 config.json 里设置 \"require_tested_dsh\": false。",
                compatibility.describe()
            ));
        }
    }

    // 3c) Detection. Runs after the update so a freshly installed CLI is what we boot. The startup URL carries a per-process token that no other process can
    //     recover, so a foreign instance can never hand us a session.
    match harness::probe(port) {
        harness::Probe::HarnessWithSession | harness::Probe::HarnessNoSession => {
            let owner = harness::listener_pid(port);
            let ours = process::read_state(data_dir)
                .map(|state| state.pid)
                .filter(|pid| Some(*pid) == owner && process::is_alive(*pid));

            match ours {
                // Our own instance from an earlier run, and no update touched its CLI tree: its
                // cookie is still valid for this authority (verified across restarts), so reuse
                // it as-is instead of restarting it.
                Some(pid) if !just_updated => {
                    if window::unsupported_webview().is_some() {
                        // A browser needs an authenticated URL, and the session token is per
                        // launch: the reused instance's cookie lives in this WebView, which is
                        // exactly the thing that cannot render the UI. Restart it instead — the
                        // fresh launch prints a URL the browser can use (handled after spawn).
                        window::set_status(
                            app,
                            "系统 WebView 太旧，正在重启 Harness 以便在浏览器中打开…",
                            &format!("pid {pid}"),
                        );
                        process::terminate(pid, TERMINATE_GRACE);
                    } else {
                        let url = url::Url::parse(&format!("http://127.0.0.1:{port}/"))
                            .map_err(|e| e.to_string())?;
                        window::set_status(
                            app,
                            "复用本应用上次启动的 Harness…",
                            &format!("pid {pid}"),
                        );
                        // Adopted, but still ours: quitting must stop it rather than leave an
                        // orphan.
                        adopt(pid, data_dir);
                        return window::create_harness(app, &url, port).map_err(|e| e.to_string());
                    }
                }
                // Our own instance that a fresh update just made obsolete: it is our child, so
                // its whole process group goes down together, exactly as at exit.
                Some(pid) => {
                    window::set_status(app, "更新完成，正在重启 Harness…", &format!("pid {pid}"));
                    process::terminate(pid, TERMINATE_GRACE);
                }
                None => {
                    if !config.take_over_existing {
                        window::open_external(&format!("http://127.0.0.1:{port}/"));
                        return Err(format!(
                            "127.0.0.1:{port} 已被另一个 Harness 占用（不是本应用启动的），已改用系统浏览器打开。                     如要让本应用接管，请保持 take_over_existing=true 或先停止该实例。"
                        ));
                    }

                    // Foreign instance: stop it (the auth fence already proved it is a Harness),
                    // then start our own so the window receives a fresh authenticated URL. Never a
                    // group signal: its process group belongs to whatever started it.
                    window::set_status(
                        app,
                        "检测到其它 Harness，正在接管…",
                        &format!("127.0.0.1:{port}"),
                    );
                    if let Some(pid) = owner {
                        process::terminate_pid(pid, TERMINATE_GRACE);
                    } else {
                        return Err(format!(
                            "127.0.0.1:{port} 上已有 Harness，但无法确定它的进程（lsof 不可用）。请先手动停止它。"
                        ));
                    }
                }
            }
            let deadline = std::time::Instant::now() + TERMINATE_GRACE;
            while !matches!(harness::probe(port), harness::Probe::Closed) {
                if std::time::Instant::now() > deadline {
                    return Err(format!("接管失败：端口 {port} 仍被占用。"));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        harness::Probe::Other => {
            return Err(format!(
                "端口 {port} 被其它程序占用，请在 config.json 里换一个端口。"
            ));
        }
        harness::Probe::Closed => {}
    }

    // 4) Force the startup URL line with a launcher-level overlay patch.
    let overlay = data_dir.join("force-print-url.yml");
    std::fs::write(
        &overlay,
        "- id: web-runtime\n  config:\n    openBrowser: !!js ctx.webStartup.openBrowser\n    printUrl: true\n    surfaceContext: true\n    trustedHosts: !!js ctx.webStartup.trustedHosts\n",
    )
    .map_err(|e| format!("写入 overlay 失败: {e}"))?;

    let log_path = data_dir.join("logs").join("harness.log");

    let options = harness::SpawnOptions {
        workspace: &config.workspace,
        overlay: &overlay,
        dsh_home: config.dsh_home.as_deref(),
        port,
        log_path: &log_path,
        env: &child.vars,
    };

    // The window may already be closed: spawning now would leave an orphan nobody stops.
    if EXITING.load(Ordering::SeqCst) {
        return Ok(());
    }
    window::set_status(
        app,
        "正在启动 Harness…",
        &format!(
            "dsh {version}{} · 端口 {port}",
            if untested {
                "（未测试版本）"
            } else {
                ""
            }
        ),
    );
    let spawned = harness::spawn(&resolved.node, &resolved.dsh_js, &options)
        .map_err(|e| format!("启动进程失败: {e}"))?;
    let pid = spawned.child.id();
    adopt(pid, data_dir);
    // Both sides use SeqCst, so either this thread sees the exit flag or `shutdown` sees the
    // freshly registered pid: the Harness cannot slip past the app exit unnoticed.
    if EXITING.load(Ordering::SeqCst) {
        shutdown();
        return Ok(());
    }

    // A profile that does not exist yet is initialised on first use, which is slower
    // (measured ~4s on a warm machine, but plugin installs can take much longer).
    // `first_launch` was captured before seeding, so a bundled first start still gets 90 s
    // (review P1-6).
    let timeout = if first_launch {
        STARTUP_TIMEOUT_FIRST
    } else {
        STARTUP_TIMEOUT_NEXT
    };

    let url = match spawned.wait_for_url(timeout) {
        Ok(url) => url,
        Err(reason) => {
            // The CLI may still be starting even though nothing answered in time. Stopping it
            // keeps the failure page honest and leaves the port free for the next attempt.
            let tail = spawned.ring.tail();
            return abort_start(pid, format!("{reason}\n\n最近输出:\n{tail}"));
        }
    };

    let actual_port = url.port().unwrap_or(port);
    let _ = process::write_state(
        data_dir,
        &process::HarnessState {
            pid,
            port: actual_port,
            cwd: config.workspace.to_string_lossy().to_string(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        },
    );

    if hand_the_gui_to_the_browser(app, &url, &version) {
        return Ok(());
    }
    if let Err(error) = window::create_harness(app, &url, actual_port) {
        return abort_start(pid, error.to_string());
    }

    // The CLI outlives this function, so hand the child to a watchdog: an unexpected exit
    // must be visible rather than leaving the window on a dead page.
    let ring = spawned.ring.clone();
    let child = spawned.into_child();
    let handle = app.clone();
    let data_dir = data_dir.to_path_buf();
    std::thread::spawn(move || watch_harness(handle, data_dir, pid, ring, child));
    Ok(())
}

fn fail(app: &AppHandle, status: &str, detail: &str) {
    window::set_status(app, status, detail);
}

/// Hand the UI to the system browser when this WebView cannot run it.
///
/// Returns true when it did, so the caller stops before opening a window that could only show
/// the harness's plugin error. The browser is the *same* path `dsh web` has always used on
/// these machines, and the only one with a current JavaScript engine: the system WebView is
/// frozen at the WebKit that shipped with the running macOS.
///
/// The splash page probes for this at load (see [`window::WebviewReport`]); the answer is
/// normally there long before the Harness is up. A missing answer counts as supported, so a
/// lost diagnostic can never lock the user out of a working GUI.
fn hand_the_gui_to_the_browser(app: &AppHandle, url: &url::Url, version: &str) -> bool {
    let Some(report) = window::unsupported_webview() else {
        return false;
    };
    harness::app_log(&format!(
        "WebView 缺少 dsh 前端必需的能力（{}），改用默认浏览器打开 {url}",
        report.missing.join("、")
    ));
    window::open_external(url.as_str());
    window::show_failure(
        app,
        "系统 WebView 太旧，界面已改在浏览器中打开",
        &report.browser_fallback_detail(version, url.as_str()),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_heal_keeps_a_record_it_can_reuse_and_clears_a_stale_one() {
        let state = |pid: u32, port: u16| process::HarnessState {
            pid,
            port,
            cwd: "/tmp".into(),
            started_at: 1,
        };
        // Ours, still serving the port this run uses: hand it to the reuse branch.
        assert_eq!(
            self_heal_action(&state(4242, 3080), Some(4242), true, 3080, false),
            SelfHeal::Keep
        );
        // Ours, but this run uses another port: stop it and drop the record.
        assert_eq!(
            self_heal_action(&state(4242, 3080), Some(4242), true, 4000, false),
            SelfHeal::Terminate
        );
        // The pid no longer owns the port. Only a process that still looks like our own
        // orphaned CLI may be signalled; anything else is pid reuse and is left alone.
        assert_eq!(
            self_heal_action(&state(4242, 3080), Some(9999), true, 3080, true),
            SelfHeal::Terminate
        );
        assert_eq!(
            self_heal_action(&state(4242, 3080), Some(9999), true, 3080, false),
            SelfHeal::Clear
        );
        assert_eq!(
            self_heal_action(&state(4242, 3080), None, true, 3080, false),
            SelfHeal::Clear
        );
        // Already gone: nothing to signal.
        assert_eq!(
            self_heal_action(&state(4242, 3080), Some(4242), false, 3080, false),
            SelfHeal::Clear
        );

        // Keep is what makes `ours` non-None later, and that is the only way `stop_mode` can
        // pick the process-group signal for an instance this shell started.
        let ours =
            self_heal_action(&state(4242, 3080), Some(4242), true, 3080, false) == SelfHeal::Keep;
        assert_eq!(stop_mode(ours), StopMode::ProcessGroup);
    }

    #[test]
    fn only_an_unowned_dsh_web_counts_as_our_leftover() {
        let line = " 8831 /opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --patch /x --no-open --port 3080";
        // A crash hands the leftover to launchd on macOS and to `systemd --user` on Linux.
        assert!(looks_like_our_harness(line, Parent::Gone));
        assert!(looks_like_our_harness(line, Parent::Supervisor));
        // Somebody still runs that session (a terminal, or another shell of ours): never signal.
        assert!(!looks_like_our_harness(line, Parent::Live));
        // A different program took over the recorded pid.
        assert!(!looks_like_our_harness(
            " 8831 /usr/sbin/cupsd -l",
            Parent::Gone
        ));
        // A dsh that is not the web profile this shell supervises.
        assert!(!looks_like_our_harness(
            " 8831 node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --version",
            Parent::Gone
        ));
        // No such process: `ps` prints nothing.
        assert!(!looks_like_our_harness("", Parent::Gone));
        assert!(!looks_like_our_harness("\n", Parent::Gone));
    }

    #[test]
    fn ps_rows_keep_the_command_line_intact() {
        // `ps -o ppid=,command=` pads the numeric column; the command keeps its spaces.
        assert_eq!(
            parse_ps_identity(
                "   45 /Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop --flag\n"
            ),
            Some((
                45,
                "/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop --flag".to_string(),
            ))
        );
        assert_eq!(parse_ps_identity("\n"), None);
    }

    #[test]
    fn session_supervisors_are_recognized_on_both_platforms() {
        assert!(is_session_supervisor("/sbin/launchd"));
        assert!(is_session_supervisor("/usr/lib/systemd/systemd --user"));
        assert!(is_session_supervisor("/sbin/init"));
        assert!(!is_session_supervisor("/bin/zsh -l"));
        assert!(!is_session_supervisor(
            "/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop"
        ));
        assert!(!is_session_supervisor(
            "/opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web"
        ));
        // No userspace parent left at all: treated as gone without asking `ps`.
        assert_eq!(classify_parent(0), Parent::Gone);
    }

    #[test]
    fn an_install_switches_the_supervised_cli_to_the_shadow_prefix() {
        let root = std::env::temp_dir().join("dsh-desktop-installed-cli-test");
        let _ = std::fs::remove_dir_all(&root);
        let supervised = root.join("seed/lib/node_modules/@deepseek-ai/dsh/lib/bin.js");
        std::fs::create_dir_all(supervised.parent().unwrap()).unwrap();
        std::fs::write(&supervised, "#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            root.join("seed/lib/node_modules/@deepseek-ai/dsh/package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"0.1.5-rc.1"}"#,
        )
        .unwrap();

        // Nothing was installed into the prefix: stay on the supervised tree.
        let (path, version) = installed_cli(Some(&root.join("empty")), &supervised, "0.1.5-rc.2");
        assert_eq!(path, supervised);
        assert_eq!(version, "0.1.5-rc.1");

        // A shadow install adds a new tree next to the seed: path *and* version must switch, or
        // the launch keeps the old core and logs "check npm global prefix" (review P0-2).
        let prefix = root.join("prefix");
        let shadow = prefix.join("lib/node_modules/@deepseek-ai/dsh");
        std::fs::create_dir_all(shadow.join("lib")).unwrap();
        std::fs::write(shadow.join("lib/bin.js"), "#!/usr/bin/env node\n").unwrap();
        std::fs::write(
            shadow.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"0.1.5-rc.2"}"#,
        )
        .unwrap();
        let (path, version) = installed_cli(Some(&prefix), &supervised, "0.1.5-rc.2");
        assert_eq!(path, shadow.join("lib/bin.js"));
        assert_eq!(version, "0.1.5-rc.2");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_seed_copy_is_all_or_nothing() {
        let root = std::env::temp_dir().join("dsh-desktop-copy-tree-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("profiles/web");
        let staging = root.join("profiles/web.tmp");

        // A source that cannot be read leaves neither the target nor the staging directory:
        // a half profile would never be re-seeded nor repaired (review P1-6).
        assert!(copy_tree(&root.join("missing"), &target).is_err());
        assert!(!target.exists());
        assert!(!staging.exists());

        // A complete copy lands in one step and cleans up after itself.
        let from = root.join("template");
        std::fs::create_dir_all(from.join("node_modules/dshmarket")).unwrap();
        std::fs::write(from.join("package.json"), "{}").unwrap();
        std::fs::write(from.join("node_modules/dshmarket/package.json"), "{}").unwrap();
        copy_tree(&from, &target).unwrap();
        assert!(target.join("node_modules/dshmarket/package.json").is_file());
        assert!(!staging.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_instances_are_never_stopped_by_process_group() {
        assert_eq!(stop_mode(true), StopMode::ProcessGroup);
        assert_eq!(stop_mode(false), StopMode::PidOnly);
    }

    #[test]
    fn a_missing_home_never_becomes_the_filesystem_root() {
        assert_eq!(
            home_workspace(Some("/Users/me".into())),
            PathBuf::from("/Users/me")
        );
        assert_eq!(home_workspace(Some("   ".into())), std::env::temp_dir());
        assert_eq!(home_workspace(None), std::env::temp_dir());
        assert_ne!(home_workspace(None), PathBuf::from("/"));
    }

    #[test]
    fn a_bad_workspace_is_repaired_in_memory_only() {
        let dir = std::env::temp_dir().join("dsh-desktop-repair-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let raw = "{\"workspace\": \"/definitely/not/there\"}";
        std::fs::write(dir.join("config.json"), raw).unwrap();
        assert_eq!(Config::load(&dir).workspace, default_workspace());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json")).unwrap(),
            raw,
            "repair must not rewrite the user's file"
        );

        // An existing absolute directory is kept as it is.
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            dir.join("config.json"),
            format!("{{\"workspace\": {:?}}}", workspace.to_string_lossy()),
        )
        .unwrap();
        assert_eq!(Config::load(&dir).workspace, workspace);

        // A relative path cannot serve as the child's working directory either.
        std::fs::write(dir.join("config.json"), "{\"workspace\": \"relative/dir\"}").unwrap();
        assert_eq!(Config::load(&dir).workspace, default_workspace());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_dsh_path_is_ignored_in_memory_only() {
        let dir = std::env::temp_dir().join("dsh-desktop-dsh-path-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let raw = "{\"dsh_path\": \"/definitely/not/there/dsh\"}";
        std::fs::write(dir.join("config.json"), raw).unwrap();
        assert_eq!(Config::load(&dir).dsh_path, None);
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json")).unwrap(),
            raw
        );

        // An existing absolute file is remembered for the locator.
        let launcher = dir.join("dsh");
        std::fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            dir.join("config.json"),
            format!("{{\"dsh_path\": {:?}}}", launcher.to_string_lossy()),
        )
        .unwrap();
        assert_eq!(Config::load(&dir).dsh_path, Some(launcher));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_system_gate_keeps_a_lone_dsh_and_drops_a_rejected_install() {
        let node = || Some(PathBuf::from("/usr/bin/node"));
        let dsh = || {
            Some(PathBuf::from(
                "/usr/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
            ))
        };
        // No system node to judge: the dsh alone is still usable on the bundled node — this is
        // the §2.4 cell that resolving the pair with one `locate()` made unreachable (P1-4).
        assert_eq!(keep_system_install(None, dsh(), None), (None, dsh(), None));
        // Accepted, or accepted with a warning: both halves stay.
        assert_eq!(
            keep_system_install(node(), dsh(), Some(SystemGate::Accept)),
            (node(), dsh(), None)
        );
        assert_eq!(
            keep_system_install(node(), dsh(), Some(SystemGate::Warn("old".into()))),
            (node(), dsh(), None)
        );
        // Rejected: the installation as a whole goes, and the reason reaches the error page.
        assert_eq!(
            keep_system_install(node(), dsh(), Some(SystemGate::Reject("too old".into()))),
            (None, None, Some("too old".to_string()))
        );
    }

    #[test]
    fn update_only_stops_an_instance_it_is_allowed_to_stop() {
        use harness::Probe;
        // Nothing running: update freely.
        assert!(may_stop_before_update(&Probe::Closed, false, false).is_ok());
        // Someone else owns the port: never install over it.
        assert!(may_stop_before_update(&Probe::Other, false, true).is_err());
        // Our own leftover instance may always be stopped.
        assert!(may_stop_before_update(&Probe::HarnessNoSession, true, false).is_ok());
        // A foreign Harness only with explicit permission.
        assert!(may_stop_before_update(&Probe::HarnessNoSession, false, true).is_ok());
        assert!(may_stop_before_update(&Probe::HarnessNoSession, false, false).is_err());
    }

    #[test]
    fn finds_a_seed_under_the_resource_directory() {
        let dir = std::env::temp_dir().join("dsh-desktop-seed-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("runtime/node/bin")).unwrap();
        std::fs::write(dir.join("runtime/node/bin/node"), "").unwrap();

        let found = seed_root_for(Some(&dir)).expect("the resource dir holds a runtime");
        // The resource directory wins over the `tauri dev` copies next to the test binary.
        assert_eq!(found, dir.join("runtime"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forcing_bundled_resolves_into_the_seed_tree() {
        let root = std::env::temp_dir().join("dsh-desktop-resolve-test");
        let seed = root.join("runtime");
        let dsh_dir = seed.join("dsh-prefix/lib/node_modules/@deepseek-ai/dsh");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(dsh_dir.join("lib")).unwrap();
        std::fs::create_dir_all(seed.join("node/bin")).unwrap();
        std::fs::write(seed.join("node/bin/node"), "").unwrap();
        std::fs::write(dsh_dir.join("lib/bin.js"), "").unwrap();
        std::fs::write(dsh_dir.join("package.json"), "{\"version\": \"9.9.9\"}").unwrap();

        let data_dir = root.join("app-data");
        let config = Config {
            runtime: runtime::Preference::Bundled,
            ..Config::load(&data_dir)
        };
        let resolved = resolve_runtime(Some(&root), &data_dir, &config).expect("seed resolves");
        assert_eq!(resolved.node, seed.join("node/bin/node"));
        assert_eq!(resolved.dsh_js, dsh_dir.join("lib/bin.js"));
        assert_eq!(resolved.version, "9.9.9");
        // The bundled half is ours to update; a system install never is.
        assert_eq!(resolved.updates, runtime::Updates::Shadow);
        assert!(resolved.bundled());
        assert_eq!(
            resolved.update_prefix(&data_dir),
            Some(data_dir.join("runtime/prefix"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verbatim_windows_paths_lose_their_prefix() {
        #[cfg(windows)]
        {
            // Taken from a real failure: node's `fs.realpathSync` walked this path and died
            // on `lstat 'D:'` before the CLI ever started.
            assert_eq!(
                unverbatim(Path::new(r"\\?\D:\dsh\DSH Desktop\runtime\node\node.exe")),
                PathBuf::from(r"D:\dsh\DSH Desktop\runtime\node\node.exe")
            );
            assert_eq!(
                unverbatim(Path::new(r"\\?\UNC\server\share\x")),
                PathBuf::from(r"\\server\share\x")
            );
            // Past MAX_PATH the prefix is the only way to reach the file, so it stays.
            let long = format!(r"\\?\D:\{}", "a".repeat(300));
            assert_eq!(unverbatim(Path::new(&long)), PathBuf::from(&long));
        }
        // Everywhere else the helper must not touch a path.
        assert_eq!(unverbatim(Path::new("/a/b")), PathBuf::from("/a/b"));
    }

    #[test]
    fn the_system_runtime_gate_only_refuses_what_it_must() {
        let apple = locator::NodeFacts {
            arch: "arm64".into(),
            strip_types: true,
        };
        let intel = locator::NodeFacts {
            arch: "x64".into(),
            strip_types: true,
        };
        let old = locator::NodeFacts {
            arch: "arm64".into(),
            strip_types: false,
        };

        assert_eq!(
            system_runtime_gate(&apple, "arm64", true),
            SystemGate::Accept
        );
        // An x64 node under Rosetta is only worth refusing when the bundled runtime can take
        // its place; without one it is the only way to run, so it is kept with a warning.
        assert!(matches!(
            system_runtime_gate(&intel, "arm64", true),
            SystemGate::Reject(_)
        ));
        assert!(matches!(
            system_runtime_gate(&intel, "arm64", false),
            SystemGate::Warn(_)
        ));
        // Node < 22.13 cannot run the CLI at all, so this one is fatal in both cases.
        assert!(matches!(
            system_runtime_gate(&old, "arm64", false),
            SystemGate::Reject(_)
        ));
    }

    #[test]
    fn system_updates_defaults_to_upgrading_the_user_install() {
        let dir = std::env::temp_dir().join("dsh-desktop-system-updates-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // The pre-bundled behaviour: an upgrade installs. Losing that silently would be a
        // regression for everyone already running this shell.
        assert_eq!(
            Config::load(&dir).system_updates,
            runtime::SystemUpdates::Install
        );
        std::fs::write(dir.join("config.json"), "{\"system_updates\": \"notify\"}").unwrap();
        assert_eq!(
            Config::load(&dir).system_updates,
            runtime::SystemUpdates::Notify
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_preference_parses_the_env_spelling() {
        use runtime::Preference;
        assert_eq!(Preference::parse("auto"), Some(Preference::Auto));
        assert_eq!(Preference::parse("Bundled"), Some(Preference::Bundled));
        assert_eq!(Preference::parse(" system "), Some(Preference::System));
        assert_eq!(Preference::parse("nonsense"), None);
    }

    #[test]
    fn partial_configs_parse_and_broken_ones_are_not_overwritten() {
        let dir = std::env::temp_dir().join("dsh-desktop-config-test");
        let _ = std::fs::remove_dir_all(&dir);

        // Missing file: defaults are seeded once.
        let seeded = Config::load(&dir);
        assert_eq!(seeded.port, default_port());
        assert!(dir.join("config.json").is_file());

        // A partial edit keeps its value and takes defaults for the rest.
        std::fs::write(dir.join("config.json"), "{\"port\": 4321}").unwrap();
        assert_eq!(Config::load(&dir).port, 4321);

        // Unparsable: defaults apply for this run, the file survives untouched.
        let broken = "{ not json at all";
        std::fs::write(dir.join("config.json"), broken).unwrap();
        assert_eq!(Config::load(&dir).port, default_port());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json")).unwrap(),
            broken
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workspace_that_is_not_a_usable_directory_is_repaired() {
        let dir = std::env::temp_dir().join("dsh-desktop-workspace-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A drive-relative workspace (the reported `EISDIR: lstat 'D:'`) falls back to home.
        std::fs::write(dir.join("config.json"), "{\"workspace\": \"D:\"}").unwrap();
        let repaired = Config::load(&dir);
        assert_eq!(repaired.workspace, default_workspace());
        assert!(repaired.workspace.is_absolute());

        // An unusable dsh_home is dropped, so the Harness uses the default profile.
        std::fs::write(dir.join("config.json"), "{\"dsh_home\": \"\"}").unwrap();
        assert_eq!(Config::load(&dir).dsh_home, None);

        // A usable pair survives untouched.
        let workspace = dir.join("work");
        let home = dir.join("profiles");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            dir.join("config.json"),
            format!(
                "{{\"workspace\": {:?}, \"dsh_home\": {:?}}}",
                workspace.to_string_lossy(),
                home.to_string_lossy()
            ),
        )
        .unwrap();
        let kept = Config::load(&dir);
        assert_eq!(kept.workspace, workspace);
        assert_eq!(kept.dsh_home, Some(home));

        // A relative path is resolved against the app cwd: never handed to the child as-is.
        assert!(absolute(Path::new("runtime/node")).is_absolute());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PATH as a list of entries, with one separator convention for both platforms.
    fn path_entries(path: &str) -> Vec<String> {
        std::env::split_paths(path)
            .map(|entry| slashy(&entry))
            .collect()
    }

    /// Render a path the same way on Windows and elsewhere, for assertions.
    fn slashy(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// Look one variable up in an assembled child environment.
    fn value_of(env: &ChildEnv, key: &str) -> Option<String> {
        env.vars
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn a_missing_pnpm_skips_the_plugin_step_before_anything_is_stopped() {
        let pnpm = Path::new("/opt/runtime/tools/bin/pnpm");
        // Declared but not installed: installing it is a repair the user may be mid-way through.
        assert_eq!(
            plugin_skip_reason(None, Some(pnpm)),
            Some(PluginSkip::NotInstalled)
        );
        // No pnpm: `dsh plugin add` forwards to pnpm and exits 127 — after the shell already
        // stopped the Harness, so the decision has to be made here (review A1).
        assert_eq!(
            plugin_skip_reason(Some("1.0.0"), None),
            Some(PluginSkip::NoPnpm)
        );
        // Both halves present: the update may proceed.
        assert_eq!(plugin_skip_reason(Some("1.0.0"), Some(pnpm)), None);
        // The skip has to explain itself; this is the log line the user sees.
        assert!(PluginSkip::NoPnpm.reason().contains("pnpm"));
        assert!(PluginSkip::NotInstalled
            .reason()
            .contains(update::MARKET_PLUGIN));
    }

    #[test]
    fn the_child_path_carries_the_bundled_tools_that_hold_pnpm() {
        let data_dir = std::env::temp_dir().join("dsh-desktop-child-env-test");
        let _ = std::fs::remove_dir_all(&data_dir);
        let config = Config::load(&data_dir);
        // Absolute on the host platform: a leading `/` is root-relative on Windows, and
        // `merge_path` drops anything that is not absolute — so these paths are built from
        // `temp_dir` instead of being hard-coded. The Windows CI gate caught the original
        // version (a bare `/opt/...` compared as a whole-PATH string prefix).
        // Built with one `join` per component, exactly like the product does: a component such
        // as "runtime/tools" keeps its `/` on Windows while the product's `PNPM_HOME` renders
        // as `\runtime\tools\bin`, and the two strings then differ only in separators. The
        // Windows gate caught that too.
        let app = data_dir.join("app");
        let node = app.join("runtime").join("node").join("bin").join("node");
        let seed = app.join("runtime");
        let tools = data_dir.join("runtime").join("tools");

        let bundled = ChildEnv::assemble(
            &config,
            &node,
            Some(&seed),
            true,
            &data_dir,
            &BTreeMap::new(),
        );
        // Compare entries as normalised strings: Windows renders `/` as `\` on the way through
        // `join_paths`, so a whole-PATH string prefix is not portable either.
        let entries = path_entries(&bundled.path);
        let node_dir = slashy(node.parent().expect("the test node has a directory"));
        let writable = slashy(&tools.join("bin"));
        let shipped = slashy(&seed.join("tools").join("bin"));
        assert_eq!(
            entries.first().map(String::as_str),
            Some(node_dir.as_str()),
            "the node directory must lead PATH"
        );
        assert!(entries.contains(&writable), "可写 tools 前缀在 PATH 上");
        assert!(entries.contains(&shipped), "随包 tools 前缀在 PATH 上");
        // Global installs land in the writable prefix, never in the signed bundle.
        assert_eq!(
            value_of(&bundled, "PNPM_HOME"),
            Some(tools.join("bin").to_string_lossy().to_string())
        );
        assert_eq!(
            value_of(&bundled, "npm_config_prefix"),
            Some(tools.to_string_lossy().to_string())
        );
        assert_eq!(value_of(&bundled, "PATH"), Some(bundled.path.clone()));

        // A system install is the user's own tree: no toolchain of ours is pushed into it.
        let system = ChildEnv::assemble(&config, &node, None, false, &data_dir, &BTreeMap::new());
        assert!(!system.path.contains(&tools.to_string_lossy().to_string()));
        assert_eq!(value_of(&system, "PNPM_HOME"), None);

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn seeding_reports_itself_so_the_first_launch_stays_offline() {
        let root = std::env::temp_dir().join("dsh-desktop-seed-outcome-test");
        let _ = std::fs::remove_dir_all(&root);
        let seed = root.join("runtime");
        let template = seed.join("profile-template");
        std::fs::create_dir_all(template.join("node_modules/dshmarket")).unwrap();
        std::fs::write(template.join("package.json"), "{}").unwrap();
        std::fs::write(template.join("node_modules/dshmarket/package.json"), "{}").unwrap();
        let home = root.join("dsh-home");
        let config = Config {
            dsh_home: Some(home.clone()),
            ..Config::load(&root.join("app-data"))
        };

        // First launch: the template is copied and the caller is told, so the plugin check can
        // stay offline this once (review A2).
        let outcome = seed_profile_template(&config, Some(&seed));
        assert!(outcome.seeded);
        assert!(outcome.note.is_some());
        assert!(home.join("profiles/web/package.json").is_file());

        // Second launch: the profile exists, so this is not a first start any more.
        let again = seed_profile_template(&config, Some(&seed));
        assert!(!again.seeded);
        assert!(again.note.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
