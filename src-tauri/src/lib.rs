//! dsh-desktop: a Tauri shell that supervises `dsh web` and hosts it in the system WebView.

// Windows is a supported target since `feat/bundled-runtime` merged in (2026-09-13):
// `process.rs` probes liveness with `OpenProcess` + `GetExitCodeProcess`, and
// `.github/workflows/windows-portable.yml` stages a bundled runtime on a Windows runner.

pub mod harness;
pub mod locator;
pub mod process;
pub mod runtime;
pub mod shellenv;
pub mod transaction;
pub mod update;
pub mod window;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Listener, Manager, RunEvent};

/// First run may initialise a profile; later runs are fast (measured ~4s on macOS).
const STARTUP_TIMEOUT_FIRST: Duration = Duration::from_secs(90);
const STARTUP_TIMEOUT_NEXT: Duration = Duration::from_secs(30);
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// How long the shell waits for someone else to bring the port back before it starts its own
/// Harness.
///
/// A self-restart hands off: the plugin market SIGTERMs the host, which shuts down cleanly, and
/// a detached helper boots a replacement a few seconds later (measured: a warm `dsh web` binds
/// ~4s after spawn). A clean exit earns the wait; a late replacement is still handled, because
/// the restart takes the port back and retries when our own launch loses it.
const HANDOFF_GRACE: Duration = Duration::from_secs(8);
/// The same wait for an exit that looks like a crash. Nothing is expected to come back, so the
/// full grace would only delay the restart the user is waiting for.
const HANDOFF_GRACE_QUICK: Duration = Duration::from_secs(2);
/// Poll interval inside both waits.
const HANDOFF_POLL: Duration = Duration::from_millis(400);
/// A run that lasted this long is not a crash loop: the next exit starts the restart count over.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// Consecutive short runs the shell restarts on its own before it stops and reports.
const MAX_AUTO_RESTARTS: u32 = 3;

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
    /// When another Harness owns the port, stop it and take over so this shell ends up with a
    /// valid session. Off by default: that instance may be a terminal session or an agent run
    /// the user still wants, and killing it is not a decision a startup path should take
    /// silently. With it off the shell opens the running instance in the system browser.
    #[serde(default = "default_take_over")]
    pub take_over_existing: bool,
    /// Check the npm registry before every start and install a newer CLI when there is one.
    #[serde(default = "default_auto_update")]
    pub auto_update: bool,
    /// Dist-tags consulted in auto mode; the highest version among them wins. Only `latest` by
    /// default: a prerelease channel is not something to move a desktop user onto unasked.
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
    /// running it and failing in a confusing way. On by default: the shell drives the CLI
    /// through flags it parses back out of stdout, so a version outside the tested window can
    /// fail in ways that look like a broken install. Set false to warn and continue anyway.
    #[serde(default = "default_require_tested_dsh")]
    pub require_tested_dsh: bool,
    /// Which runtime to supervise: `auto` (use an installed one when it passes the gates,
    /// otherwise the bundled halves), `bundled` (always the shipped runtime) or `system`
    /// (pre-bundled behaviour, for development).
    #[serde(default)]
    pub runtime: runtime::Preference,
    /// What to do when the supervised CLI is the user's own installation and a newer version
    /// exists: `notify` (default — report it and leave the tree alone, §2.4: never rewrite a
    /// prefix we do not own) or `install` (upgrade it in place). Only affects system
    /// installations; a bundled runtime always updates its own shadow prefix.
    #[serde(default = "default_system_updates")]
    pub system_updates: runtime::SystemUpdates,
    /// Keep the plugin market (`dshmarket`) in the profile current, the same way the CLI itself
    /// is kept current: check the dist-tags, stop the instance using the profile, install, and
    /// restart so the new plugin is what loads. It rewrites files in the profile
    /// (`package.json` + lockfile), so it is off by default: a profile is user data, and the
    /// plugin tree is the part of it most likely to be mid-edit. Set true to keep it current.
    #[serde(default)]
    pub auto_update_plugins: bool,
    /// Install the legacy-WebKit compat layer when the probe finds APIs the dsh front end needs
    /// and this engine lacks (see `window::compat_script`). On by default: without it those
    /// machines get the system browser instead of the native window. Off restores exactly that
    /// older behaviour, which is the switch to reach for when a compat shim itself misbehaves.
    #[serde(default = "default_webkit_compat")]
    pub webkit_compat: bool,
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
    false
}

fn default_auto_update() -> bool {
    true
}

fn default_webkit_compat() -> bool {
    true
}

fn default_require_tested_dsh() -> bool {
    true
}

fn default_system_updates() -> runtime::SystemUpdates {
    runtime::SystemUpdates::Notify
}

fn default_update_interval() -> u64 {
    60
}

/// Only the release channel: `next` is a prerelease tag, and following it by default would move
/// a desktop user onto an untested build without them asking.
fn default_update_tags() -> Vec<String> {
    vec!["latest".to_string()]
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
            take_over_existing: default_take_over(),
            auto_update: default_auto_update(),
            update_tags: default_update_tags(),
            update_check_interval_minutes: default_update_interval(),
            import_shell_env: default_import_shell_env(),
            env: BTreeMap::new(),
            require_tested_dsh: default_require_tested_dsh(),
            runtime: runtime::Preference::Auto,
            system_updates: default_system_updates(),
            auto_update_plugins: false,
            webkit_compat: default_webkit_compat(),
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

/// Automatic restarts spent on the current crash streak. A run that lasts proves the streak is
/// over, so the next exit starts the count again (see [`exit_action`]).
static AUTO_RESTARTS: AtomicU32 = AtomicU32::new(0);

/// One attempt at a time: the watchdog and the status page's button both lead here, and two
/// Harnesses racing for one port is exactly the failure this guards against.
static RESTARTING: AtomicBool = AtomicBool::new(false);

/// A port chosen for this launch because the user asked to leave the configured one alone.
///
/// Set when the takeover question is answered with "use another port". Deliberately not written
/// back to `config.json`: this run adapts and the user's file stays as they left it, the same
/// principle a repaired workspace follows. It lasts for this launch, so the next start asks again
/// instead of silently migrating a fixed port — which the session cookie is bound to.
static PORT_OVERRIDE: AtomicU16 = AtomicU16::new(0);

/// How far above the configured port to look for a free one.
///
/// A bounded scan keeps the choice predictable: the same machine state picks the same port, so a
/// second launch that meets the same external instance lands in the same place and the session
/// cookie still matches.
const PORT_SEARCH_RANGE: u16 = 20;

/// The port this launch uses: the configured one unless the user moved this run elsewhere.
fn runtime_port(configured: u16) -> u16 {
    match PORT_OVERRIDE.load(Ordering::SeqCst) {
        0 => configured,
        port => port,
    }
}

/// A free loopback port above `from`, when there is one within [`PORT_SEARCH_RANGE`].
///
/// Binding is the test: a port something already listens on cannot be bound, which covers both
/// another Harness and an unrelated server.
fn free_port_from(from: u16) -> Option<u16> {
    let last = from.saturating_add(PORT_SEARCH_RANGE);
    (from.saturating_add(1)..=last)
        .find(|candidate| std::net::TcpListener::bind(("127.0.0.1", *candidate)).is_ok())
}

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
    let builder = tauri::Builder::default();
    // WebKit kills the WebContent process under memory pressure, and the window it leaves behind
    // looks alive while answering nothing — not even the UI's own reconnection logic, which lived
    // in that process. A shell that only watched its child process would sit there until the user
    // restarted the app (field report 2026-09-15). macOS-only: the other platforms never call it.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let builder = builder.on_web_content_process_terminate(window::recover_terminated_webview);
    builder
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            for label in [window::HARNESS, window::SPLASH] {
                if let Some(existing) = app.get_webview_window(label) {
                    let _ = existing.show();
                    let _ = existing.set_focus();
                    // A second launch while the status page reports a failure is the user
                    // asking for the Harness to be started: the app is alive but has no window
                    // onto one, so only re-focusing the page would look like being ignored.
                    if label == window::SPLASH && window::retry_offered() {
                        request_restart(app);
                    }
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
            // The status page's restart button reports through the same window's core
            // capability, for the same reason: no other IPC surface is granted.
            let restart_handle = app.handle().clone();
            app.listen(window::RESTART_EVENT, move |_event| {
                request_restart(&restart_handle);
            });
            // The takeover question is answered through the same window core capability as
            // the restart button: no other IPC surface is granted.
            app.listen(window::CHOICE_EVENT, |event| {
                window::record_choice(event.payload());
            });
            let handle = app.handle().clone();
            window::create_splash(&handle)?;
            std::thread::spawn(move || startup(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build dsh-desktop")
        // The parameter is underscore-prefixed because only the macOS Reopen arm reads it: using
        // it there is fine, and every other target stays warning-free.
        .run(|_app, event| match event {
            RunEvent::ExitRequested { .. } | RunEvent::Exit => shutdown(),
            // macOS activates a running app instead of starting a second one, so the Dock icon
            // (or Finder) is how "open it again" reaches a shell parked on a failure page. It
            // means the same thing there as the restart button does.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. }
                if window::retry_offered()
                    && _app.get_webview_window(window::HARNESS).is_none() =>
            {
                request_restart(_app);
            }
            _ => {}
        });
}

/// May the instance currently owning the port be stopped so an update can rewrite the CLI tree
/// it serves from? Node loads modules lazily, so updating a live tree breaks the running
/// Harness on its next `require()` — the tree must not be touched while it is in use.
///
/// `is_ours` is the state-file match and nothing else. A foreign Harness is never refused here:
/// whether it may be stopped is the user's answer to the question [`stop_instance_before_update`]
/// puts, and the config only decides what an unanswered question means. Refusing on the config
/// would skip the question for the same reason it was skipped on the startup path (2026-09-16).
fn may_stop_before_update(probe: &harness::Probe) -> Result<(), String> {
    match probe {
        harness::Probe::Closed | harness::Probe::Harness => Ok(()),
        harness::Probe::Other => Err("端口被其它程序占用，跳过本次更新".to_string()),
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
    may_stop_before_update(&probe)?;

    let Some(pid) = owner else {
        return Err(format!(
            "端口 {port} 上已有 Harness，但无法确定它的进程（lsof 不可用），跳过本次更新"
        ));
    };
    // A foreign instance is somebody else session, and an update stops it for reasons that
    // have nothing to do with what they were doing: ask before touching it. Declining turns
    // this into a deferred update, which is what the config-only version used to do.
    if ours.is_none() && !confirm_takeover(app, config, port, pid) {
        return Err(format!(
            "端口 {port} 上的外部 Harness（pid {pid}）没有被接管，跳过本次更新"
        ));
    }
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

/// Where the update transaction keeps its working copies.
///
/// All of them live under the app data directory, which is the one tree the shell owns: staging a
/// new CLI tree must never need space inside the prefix being replaced, or a full disk would
/// take the live tree down with it.
struct UpdatePaths {
    staging: PathBuf,
    rollback: PathBuf,
    swap: PathBuf,
    /// Where a profile is copied before pnpm rewrites it.
    profiles: PathBuf,
}

impl UpdatePaths {
    fn new(runtime_root: &Path) -> UpdatePaths {
        UpdatePaths {
            staging: runtime_root.join("staging"),
            rollback: runtime_root.join("rollback"),
            swap: runtime_root.join("update-swap.json"),
            profiles: runtime_root.join("profile-backup"),
        }
    }
}

/// Install a newer plugin market into the profile, with a snapshot to fall back on.
///
/// Returns whether the profile now loads a different version, which is what forces a restart.
/// pnpm owns this install — `dsh plugin add` is a thin wrapper around it — so unlike a core
/// update there is no staged tree to verify first. What makes it reversible is the snapshot
/// taken before pnpm is allowed to touch the profile: a half-written plugin tree is not
/// something the next launch can repair on its own.
#[allow(clippy::too_many_arguments)]
fn update_market_plugin(
    app: &AppHandle,
    data_dir: &Path,
    paths: &UpdatePaths,
    profile_dir: &Path,
    resolved: &ResolvedRuntime,
    child_path: &OsStr,
    config: &Config,
    port: u16,
    from: &str,
    to: &str,
) -> bool {
    // Same hazard as a core update: pnpm rewrites the profile node_modules in place and a
    // running harness would break on its next lazy require().
    if let Err(reason) = stop_instance_before_update(app, data_dir, port, config) {
        harness::app_log(&format!(
            "plugin update deferred, keeping v{from}: {reason}"
        ));
        return false;
    }
    let snapshot = match snapshot_profile(paths, profile_dir, to) {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            // Without a snapshot the install would be one-way, and the plugin market is not
            // worth an unrecoverable profile.
            harness::app_log(&format!(
                "plugin update deferred, keeping v{from}: 无法快照 profile: {reason}"
            ));
            return false;
        }
    };
    let installed = update::install_plugin(
        &resolved.node,
        &resolved.dsh_js,
        "web",
        update::MARKET_PLUGIN,
        to,
        config.dsh_home.as_deref(),
        child_path,
    );
    match installed {
        Ok(()) => {
            let after =
                update::installed_plugin(profile_dir, update::MARKET_PLUGIN).unwrap_or_default();
            if after == from {
                // pnpm wrote the package somewhere the profile does not load it from: report
                // and remember the attempt instead of retrying on every launch.
                update::mark_plugin_attempt_ineffective(data_dir, to);
                harness::app_log(&format!(
                    "plugin installed but the profile still loads {} {from}",
                    update::MARKET_PLUGIN
                ));
                return false;
            }
            harness::app_log(&format!(
                "plugin updated: {} {from} -> {after}",
                update::MARKET_PLUGIN
            ));
            true
        }
        Err(reason) => {
            // pnpm may have rewritten the profile before it gave up: put the snapshot back so
            // the profile is what it was, rather than something neither version can load.
            let restored = restore_profile(&snapshot, profile_dir);
            // A failed install is usually transient, so it only suppresses the next attempt for
            // a few minutes — long enough not to stop the Harness again right away, short
            // enough to recover on its own (review A1/A5).
            update::mark_plugin_attempt_failed(data_dir, to);
            harness::app_log(&format!(
                "plugin update failed, keeping v{from}: {reason}{}",
                match restored {
                    Ok(()) => "（已从快照恢复 profile）".to_string(),
                    Err(error) => format!("（profile 恢复失败: {error}）"),
                }
            ));
            false
        }
    }
}

/// Copy a profile aside before pnpm rewrites it, so a failed plugin install can be undone.
///
/// Unlike the core update there is no staged tree to build first: `dsh plugin add` owns the
/// profile layout and pnpm is what installs, so the only way to make the install reversible is
/// to keep what was there. The entries the running Harness keeps writing are skipped (see
/// [`transaction::PROFILE_LIVE_ENTRIES`]): restoring stale credentials over live ones would be
/// a second, worse failure.
fn snapshot_profile(
    paths: &UpdatePaths,
    profile_dir: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let snapshot = paths.profiles.join(version_label(label));
    transaction::snapshot_tree(profile_dir, &snapshot, transaction::PROFILE_LIVE_ENTRIES)?;
    // Keep one generation per plugin version; the directory is a full copy of node_modules.
    transaction::prune(&paths.profiles, PROFILE_SNAPSHOTS);
    Ok(snapshot)
}

/// How many profile snapshots to keep. Each is a copy of the profile dependency tree, so this
/// is a size decision as much as a history one.
const PROFILE_SNAPSHOTS: usize = 2;

/// Put a profile snapshot back after a failed plugin install.
fn restore_profile(snapshot: &Path, profile_dir: &Path) -> Result<(), String> {
    transaction::restore_tree(snapshot, profile_dir, transaction::PROFILE_LIVE_ENTRIES)
}

/// A core update that has been staged and verified, and may now be committed.
///
/// Holding the staging guard is what makes deferring the commit safe: a launch that fails
/// before it ever boots the new tree — a busy port, a foreign Harness, a rejected version —
/// drops this value and takes the staged tree with it, leaving the live one untouched.
struct StagedUpdate {
    _staging: transaction::Staging,
    /// The staged package directory, about to become the live one.
    dir: PathBuf,
    version: String,
    /// The live package directory this will replace.
    target: PathBuf,
    /// Where the live tree goes while the new one proves it boots.
    backup: PathBuf,
}

/// The package directory a CLI would occupy inside `prefix`, whether or not it is there yet.
///
/// npm uses `<prefix>/lib/node_modules` on Unix and `<prefix>/node_modules` on Windows. The
/// directory is named rather than searched for because the interesting case is a prefix that has
/// no CLI yet: a bundled build installs into the shadow prefix for the first time this way.
fn package_dir_in(prefix: &Path) -> PathBuf {
    // The same two layouts `DSH_JS_SUFFIXES` names, minus the entry script. npm puts global
    // packages under `lib/` on Unix and directly under the prefix on Windows, and a swap has
    // to name the directory npm will actually write.
    #[cfg(windows)]
    {
        prefix.join("node_modules").join(DSH_PACKAGE_NAME)
    }
    #[cfg(not(windows))]
    {
        prefix
            .join("lib")
            .join("node_modules")
            .join(DSH_PACKAGE_NAME)
    }
}

/// Build the new CLI tree somewhere else and check it before anything is replaced.
///
/// This is the whole point of the transaction: npm writes into a directory nobody is running
/// from, and a registry that answered with the wrong version, a truncated download or a prefix
/// npm ignored is a report here instead of a broken install there.
///
/// `target_prefix` is where the tree will end up, which is not always where the running CLI
/// lives: a bundled build runs the read-only seed inside the app bundle and updates the writable
/// shadow prefix under app-data (review P0-2).
fn stage_core_update(
    npm: &Path,
    to: &str,
    target_prefix: &Path,
    paths: &UpdatePaths,
    cache: &Path,
) -> Result<StagedUpdate, String> {
    let staging = transaction::Staging::create(&paths.staging, to)?;
    let prefix = staging.prefix();
    update::install(npm, update::PACKAGE, to, Some(&prefix), Some(cache))?;
    let verified = transaction::verify_install(&prefix, update::PACKAGE, to)?;
    let target = package_dir_in(target_prefix);
    // A staged tree that resolves to the live tree would make the swap a no-op that still
    // reports success; saying so is better than moving a directory onto itself.
    if verified.dir == target {
        return Err(format!(
            "暂存目录与正在使用的 CLI 是同一个: {}",
            target.display()
        ));
    }
    let backup = paths.rollback.join(version_label(to));
    Ok(StagedUpdate {
        _staging: staging,
        dir: verified.dir,
        version: verified.version,
        target,
        backup,
    })
}

/// A version string that is safe as a directory name.
fn version_label(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Put the staged tree in place, recording the swap before the live tree is touched.
///
/// The record is what makes a launch that dies between here and the first successful boot
/// recoverable: it names the tree to put back, and [`recover_pending_swap`] reads it before
/// anything else runs.
fn commit_core_update(
    staged: StagedUpdate,
    paths: &UpdatePaths,
) -> Result<(PathBuf, String), String> {
    transaction::write_swap(
        &paths.swap,
        &transaction::SwapRecord {
            target: staged.target.to_string_lossy().to_string(),
            backup: staged.backup.to_string_lossy().to_string(),
            version: staged.version.clone(),
            at: update::now_secs(),
        },
    )?;
    if let Err(error) = transaction::commit(&staged.target, &staged.dir, &staged.backup) {
        // Nothing was replaced, so the record would only mislead the next launch into rolling
        // back a tree that is still the old one.
        transaction::clear_swap(&paths.swap);
        return Err(error);
    }
    // The entry script moved with its package directory, so it is named from the new location
    // rather than from the staging one it was verified in.
    let entry = staged.target.join("lib").join("bin.js");
    Ok((entry, staged.version))
}

/// Undo a swap this launch made, because the new tree never printed its startup URL.
///
/// Returns whether the previous tree is back in place. The swap record is the only source of
/// truth here: without it there is nothing to put back, and the caller says so instead of
/// claiming a rollback that did not happen.
fn roll_back_core_update(paths: &UpdatePaths) -> bool {
    let Some(record) = transaction::read_swap(&paths.swap) else {
        return false;
    };
    match transaction::rollback(Path::new(&record.target), Path::new(&record.backup)) {
        Ok(()) => {
            harness::app_log(&format!(
                "v{} 未能启动，已回滚到上一棵树（{}）",
                record.version, record.backup
            ));
            transaction::clear_swap(&paths.swap);
            transaction::prune(&paths.rollback, ROLLBACK_GENERATIONS);
            true
        }
        Err(error) => {
            // Keep the record: the next launch retries the rollback rather than booting a tree
            // that has already failed once.
            harness::app_log(&format!("回滚失败，保留记录以便下次重试: {error}"));
            false
        }
    }
}

/// The new tree booted: the swap is real, so the record and the older generations can go.
fn confirm_core_update(paths: &UpdatePaths) {
    transaction::clear_swap(&paths.swap);
    transaction::prune(&paths.rollback, ROLLBACK_GENERATIONS);
}

/// How many previous CLI trees to keep. One is enough to undo one bad update; more only costs
/// the ~290 MB each of them takes.
const ROLLBACK_GENERATIONS: usize = 2;

/// Remove staged trees a killed process left behind.
///
/// A staged tree only has a reason to exist inside the launch that built it: the commit moves it
/// into place or the guard drops it. One left on disk is therefore always debris from a process
/// that died mid-update, and it is ~290 MB of it. The whole directory goes rather than the
/// generations being counted, because nothing in it is referenced by anything.
fn clear_stale_staging(paths: &UpdatePaths) {
    let Ok(entries) = std::fs::read_dir(&paths.staging) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => harness::app_log(&format!("清理上次遗留的暂存目录 {}", path.display())),
            Err(error) => harness::app_log(&format!("无法清理 {}: {error}", path.display())),
        }
    }
}

/// Undo a swap that a previous launch made and never confirmed.
///
/// Reached at the very start of a launch, before the runtime is resolved: the tree the record
/// names is the one the previous launch replaced, and a launch that finds this file is by
/// definition one where the new tree never printed its startup URL — the process was killed, or
/// the boot timed out.
fn recover_pending_swap(paths: &UpdatePaths) {
    let Some(record) = transaction::read_swap(&paths.swap) else {
        return;
    };
    let target = PathBuf::from(&record.target);
    let backup = PathBuf::from(&record.backup);
    harness::app_log(&format!(
        "上次更新到 v{} 的切换没有完成（CLI 未启动成功），正在回滚到 {} 中的上一棵树",
        record.version,
        backup.display()
    ));
    match transaction::rollback(&target, &backup) {
        Ok(()) => {
            harness::app_log(&format!("已回滚 {}", target.display()));
            transaction::clear_swap(&paths.swap);
            transaction::prune(&paths.rollback, ROLLBACK_GENERATIONS);
        }
        Err(error) => {
            // Keep the record: the next launch should try again rather than leave a tree
            // nobody can boot and no record of what to put back.
            harness::app_log(&format!("回滚失败，保留记录以便下次重试: {error}"));
        }
    }
}

/// The supervised runtime, resolved once at startup.
struct ResolvedRuntime {
    node: PathBuf,
    dsh_js: PathBuf,
    version: String,
    /// Where the supervised dsh came from: the seed, the shadow prefix, the environment, or the
    /// user's own installation. Shown on the status page, because "which tree is this" is the
    /// difference between a working setup and a machine running an unrelated `dsh`.
    origin: runtime::Origin,
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
    // Reported with the reason when it cannot be read: the status page and the log used to show a
    // bare "未知", which hid *why* the tree could not be identified (2026-09-15).
    let version = locator::describe_version(&decision.dsh.path);
    Ok(ResolvedRuntime {
        node: absolute(&decision.node.path),
        dsh_js: absolute(&decision.dsh.path),
        version,
        origin: decision.dsh.origin,
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

/// Why a bundled build did not put its profile template in place.
///
/// Every branch reports: a user whose UI has no plugin market needs the log to say which of
/// these happened, and the template is the only thing that installs one without a download.
#[derive(Debug, PartialEq, Eq)]
enum SeedSkip {
    /// This build has no bundled runtime: a machine that installed its own dsh gets no template
    /// (the template pins a market version, and this tree is not ours — review P1-7).
    NotBundled,
    /// The bundle has no template to copy.
    NoTemplate,
    /// Neither `dsh_home` nor a home directory could be resolved.
    NoHome,
    /// A profile is already there: seeding never overwrites one (plan §2.5).
    ProfileExists,
}

impl SeedSkip {
    fn reason(&self) -> String {
        match self {
            SeedSkip::NotBundled => {
                "本构建没有自带运行时：不播种 profile 模板（插件市场由用户的安装自行决定）"
                    .to_string()
            }
            SeedSkip::NoTemplate => "随包资源里没有 profile-template，跳过播种".to_string(),
            SeedSkip::NoHome => "无法确定 DSH_HOME，跳过 profile 模板播种".to_string(),
            SeedSkip::ProfileExists => {
                "profile 已存在，跳过模板播种：插件市场不会因此被安装（缺失就自己 add 一次）"
                    .to_string()
            }
        }
    }
}

/// Which half of the seeding decision this run lands on, without touching the filesystem.
///
/// Pure, so the matrix is testable: `bundled` is whether this build's own tree is being
/// supervised, `template` whether the bundle carries one, and `profile_exists` whether the user
/// already has a web profile.
fn seed_decision(
    bundled: bool,
    template: bool,
    home: bool,
    profile_exists: bool,
) -> Result<(), SeedSkip> {
    if !bundled {
        return Err(SeedSkip::NotBundled);
    }
    if !template {
        return Err(SeedSkip::NoTemplate);
    }
    if !home {
        return Err(SeedSkip::NoHome);
    }
    if profile_exists {
        return Err(SeedSkip::ProfileExists);
    }
    Ok(())
}

/// First launch of a bundled build: install the profile template (which carries the plugin
/// market) into the user's DSH_HOME, unless a profile is already there (plan §2.5).
///
/// Every skip is reported through `note`: the template is how a bundled build gets its plugin
/// market, so a silent skip is exactly the state a user cannot diagnose from the UI.
fn seed_profile_template(config: &Config, bundled: bool, seed: Option<&Path>) -> SeedOutcome {
    let mut outcome = SeedOutcome::default();
    let template = seed.map(|seed| seed.join("profile-template"));
    let home = config
        .dsh_home
        .clone()
        .or_else(|| home_dir().map(|home| home.join(".dsh")));
    let profile = home.as_ref().map(|home| home.join("profiles").join("web"));
    let decision = seed_decision(
        bundled,
        template
            .as_ref()
            .is_some_and(|template| template.join("package.json").is_file()),
        profile.is_some(),
        profile.as_ref().is_some_and(|profile| profile.exists()),
    );
    let (template, profile) = match (decision, template, profile) {
        (Ok(()), Some(template), Some(profile)) => (template, profile),
        // `seed_decision` already rejected every state that cannot reach the copy, so this arm is
        // unreachable; it reports instead of panicking, because a later edit to either side must
        // not be able to crash startup.
        (decision, _, _) => {
            outcome.note = Some(match decision {
                Err(skip) => skip.reason(),
                Ok(()) => "无法确定 profile 模板或 DSH_HOME，跳过播种".to_string(),
            });
            return outcome;
        }
    };
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

/// What the watchdog does about one unexpected exit.
#[derive(Debug, PartialEq, Eq)]
enum ExitAction {
    /// Start another Harness, after waiting `grace` in case something else is booting one.
    Recover { attempt: u32, grace: Duration },
    /// The automatic budget is spent: report it and let the user decide.
    Report,
}

/// Pure watchdog policy: is this exit answered with another attempt, and how long may a handoff
/// take first?
///
/// `uptime` is how long the dead Harness had been running, `previous` the consecutive automatic
/// restarts behind it, and `clean` whether it exited with code 0 — the shape of a deliberate
/// handoff, because the plugin market SIGTERMs the host and it shuts down with that code. A run
/// that lasted [`HEALTHY_RUN`] is not a crash loop, so the count starts over rather than
/// eventually refusing to start at all.
fn exit_action(uptime: Duration, previous: u32, clean: bool) -> ExitAction {
    let attempt = if uptime >= HEALTHY_RUN {
        1
    } else {
        previous.saturating_add(1)
    };
    if attempt > MAX_AUTO_RESTARTS {
        return ExitAction::Report;
    }
    ExitAction::Recover {
        attempt,
        grace: if clean {
            HANDOFF_GRACE
        } else {
            HANDOFF_GRACE_QUICK
        },
    }
}

/// How a finished child reads to a person: an exit code, or the signal that killed it.
fn exit_reason(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("被信号 {signal} 终止");
        }
    }
    match status.code() {
        Some(0) => "退出码 0".to_string(),
        Some(code) => format!("退出码 {code}"),
        None => "结束状态未知".to_string(),
    }
}

/// The user asked for another attempt: the status page's button, or a second launch while that
/// page was in front. Runs off the caller's thread, because starting a Harness blocks.
fn request_restart(app: &AppHandle) {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    if RESTARTING.load(Ordering::SeqCst) {
        harness::app_log("已有一次恢复/重启在进行中，忽略重复的重启请求");
        return;
    }
    let Ok(data_dir) = app.path().app_data_dir() else {
        harness::app_log("无法获取应用数据目录，忽略重启请求");
        return;
    };
    // A person's explicit request outranks the crash-loop budget.
    AUTO_RESTARTS.store(0, Ordering::SeqCst);
    harness::app_log("restart requested from the status page");
    window::show_progress(app, "正在重新启动 Harness…", "");
    let handle = app.clone();
    std::thread::spawn(
        move || match restart_harness(&handle, &data_dir, HANDOFF_GRACE_QUICK) {
            Ok(()) => harness::app_log("Harness restarted on request"),
            Err(reason) => window::show_failure(
                &handle,
                "重新启动 Harness 失败",
                &format!("{reason}\n\n关闭本窗口即退出应用，再次打开会重新尝试。"),
            ),
        },
    );
}

/// Bring the Harness back with the runtime, workspace and credentials this shell owns.
///
/// `Ok` means a Harness is up, including when another attempt was already running; `Err` carries
/// the reason the user has to see.
fn restart_harness(app: &AppHandle, data_dir: &Path, grace: Duration) -> Result<(), String> {
    if RESTARTING.swap(true, Ordering::SeqCst) {
        harness::app_log("已有一次恢复/重启在进行中，忽略本次触发");
        return Ok(());
    }
    // The page that is about to be replaced may be the failure page; a retry that fails again
    // must be able to say so.
    window::allow_next_failure();
    let outcome = take_over_handoff_and_start(app, data_dir, grace);
    RESTARTING.store(false, Ordering::SeqCst);
    outcome
}

/// Wait out a handoff, take the port back, and start this shell's own Harness.
///
/// A replacement booted by the plugin market must not be adopted: it replayed the CLI's own
/// argv, so its working directory — and therefore the workspace dsh uses — is the CLI directory
/// instead of the one configured here.
fn take_over_handoff_and_start(
    app: &AppHandle,
    data_dir: &Path,
    grace: Duration,
) -> Result<(), String> {
    let config = Config::load(data_dir);
    let port = runtime_port(config.port);
    if let Some(pid) = wait_for_handoff(port, grace) {
        // A replacement is on the port, and it is not ours to adopt for the reason above. What to
        // do about it is not decided here: the startup path below owns that question, asks it once
        // for an identified instance, and knows every answer (take over, browser, another port,
        // cancel). Asking here as well would put the same question twice for one restart.
        harness::app_log(&format!(
            "端口 {port} 在交接窗内已由 pid {pid} 服务：交由启动流程处理（不采用它重放的 cwd）"
        ));
    }
    match start(app, data_dir) {
        Ok(()) => Ok(()),
        // A handoff slower than the grace can still take the port while our own launch boots.
        // Reporting a failure there would be wrong — the port is serving a Harness — so ask about
        // that instance and make the one remaining attempt if the answer allows it. The guard is
        // the identity, not the config: whether to signal is the user's answer, and the config
        // only decides what an unanswered question means.
        Err(reason) => match harness_listener(port) {
            // Unidentified: report the original failure rather than killing a process this
            // shell cannot prove is the CLI.
            Some(pid) if !identified_dsh_web(pid) => {
                harness::app_log(&format!(
                    "端口 {port} 由 pid {pid} 服务，但无法确认它是 dsh web：不接管"
                ));
                Err(reason)
            }
            // A retry is still this shell deciding to end somebody else instance, so it is
            // the same question.
            Some(pid) if !confirm_takeover(app, &config, port, pid) => {
                harness::app_log(&format!(
                    "本次启动失败（{reason}），端口 {port} 上的 pid {pid} 未被接管：保留该实例"
                ));
                Err(reason)
            }
            Some(pid) => {
                harness::app_log(&format!(
                    "本次启动失败（{reason}），但端口 {port} 已由 pid {pid} 服务：接管后重试一次"
                ));
                window::set_status(app, "正在接管重新启动的 Harness…", &format!("pid {pid}"));
                process::terminate_pid(pid, TERMINATE_GRACE);
                let _ = wait_for_port_free(port, TERMINATE_GRACE);
                start(app, data_dir)
            }
            // Nothing owns the port any more: report the original failure.
            None => Err(reason),
        },
    }
}

/// What the startup path does about a Harness it did not start.
#[derive(Debug, PartialEq, Eq)]
enum ForeignAction {
    /// Stop that instance and start our own on the port.
    TakeOver { pid: u32 },
    /// Leave it running and open it in the system browser instead.
    UseBrowser,
    /// Leave it running and explain why this shell cannot use the port.
    Refuse { reason: String },
    /// A Harness this shell can identify. The caller puts the question and turns the answer back
    /// into one of the other arms.
    Ask { pid: u32 },
    /// Leave that instance alone and start on another port, so neither side is disturbed.
    UseOtherPort { port: u16 },
}

/// Decide what to do about a foreign Harness, from the two identity signals alone.
///
/// `command` is the listener's command line, or `None` when the platform would not report it.
/// Both a fence-shaped answer and a `dsh web` command line are required before anything is
/// signalled: the fence proves a Harness protocol is on the port, and the command line proves the
/// process is the CLI rather than an unrelated server that happens to answer the same way.
///
/// A process that passes both is **always** asked about, whatever `config.json` says. The config
/// is the answer for a question nobody replied to, never a reason to skip asking: gating the
/// question on it made the choice invisible to everyone who had not already edited the file,
/// which is the opposite of what the review asked for (2026-09-16).
fn foreign_instance_action(owner: Option<u32>, command: Option<&str>) -> ForeignAction {
    let identified = command.is_some_and(harness::looks_like_dsh_web);
    match (owner, identified) {
        // Nothing may be signalled that this shell cannot identify, whoever asked.
        (_, false) => ForeignAction::Refuse {
            reason: "端口上有进程按 Harness 协议应答，但无法确认它就是 dsh web（读不到命令行，或命令行不像 dsh）。为避免误杀其它程序，本应用不会接管它。请先手动停止该进程，或在 config.json 里换一个端口。"
                .to_string(),
        },
        // Identified: ask, because stopping it kills a session someone may be watching and
        // restarts the instance under a different workspace.
        (Some(pid), true) => ForeignAction::Ask { pid },
        // `identified` already proved a command line exists, so this arm is unreachable; it keeps
        // the match total without a panic in a startup path.
        (None, true) => ForeignAction::Refuse {
            reason: "端口上的 Harness 无法定位到具体进程（lsof 不可用）。请先手动停止它。"
                .to_string(),
        },
    }
}

/// The ids the takeover question answers with.
const CHOICE_TAKE_OVER: &str = "take-over";
const CHOICE_BROWSER: &str = "browser";
const CHOICE_CANCEL: &str = "cancel";
/// Start on another port instead, leaving the instance where it is.
const CHOICE_PORT: &str = "port";

/// Which terminal page an outcome lands on.
///
/// Not cosmetic: [`window::show_failure`] arms the restart button *and* the Dock/Reopen entry
/// points, so a page offering a restart this shell cannot perform turns a dead end into a loop.
/// The browser fallback did exactly that — every click ran `start()` again, opened another tab,
/// and landed on the same page (2026-09-16).
#[derive(Debug, PartialEq, Eq)]
enum TerminalPage {
    /// Something can still be started here: offer another attempt.
    Failure,
    /// Nothing this shell will start: report it and stop.
    Notice,
}

/// A restart only helps when the shell would plausibly start its own Harness next time.
///
/// Leaving the port to an instance this shell will not take over is the case where it cannot:
/// the instance is alive and serving, so a second attempt repeats the first exactly.
fn terminal_page(action: &ForeignAction) -> TerminalPage {
    match action {
        ForeignAction::UseBrowser => TerminalPage::Notice,
        // A refusal names something the user has to change — an unproven identity, a port held
        // by an unrelated program. The fix may well be followed by another attempt right here,
        // so the button is worth offering.
        _ => TerminalPage::Failure,
    }
}

/// The question put to the user when a Harness this shell did not start is in the way.
///
/// The detail is the whole point of asking: a pid alone does not tell the user which terminal
/// session, agent run or workspace they are about to end, so the command line and this shell's
/// own workspace are both spelled out.
fn takeover_question(
    port: u16,
    pid: u32,
    command: Option<&str>,
    workspace: &Path,
) -> (String, String) {
    let detail = format!(
        "端口 127.0.0.1:{port} 上已有一个不是本应用启动的 Harness。\n\n\
         进程: pid {pid}\n\
         命令行: {}\n\n\
         接管会先终止该进程（它当前的会话、正在执行的 agent 任务与浏览器里已打开的页面都会断开），\
         然后用本应用的 workspace 重新启动：\n{}\n\n\
         不接管则用系统浏览器打开那个实例，本应用退出。",
        command.unwrap_or("<读不到命令行>"),
        workspace.display()
    );
    ("检测到其它 Harness".to_string(), detail)
}

/// Put the takeover question and turn the answer into an action.
///
/// Never answering is not the same as choosing the browser: `config.json` says what an unanswered
/// question means, and the hint under the buttons repeats it, so the timeout runs the config's
/// answer rather than a hardcoded one.
///
/// A question that cannot be put at all — no status window, a page that never loaded — is
/// *unanswered* in the same sense, which keeps one rule for both.
fn resolve_foreign_action(
    app: &AppHandle,
    config: &Config,
    port: u16,
    action: ForeignAction,
) -> ForeignAction {
    let ForeignAction::Ask { pid } = action else {
        return action;
    };
    let command = harness::process_command(pid);
    let (status, detail) = takeover_question(port, pid, command.as_deref(), &config.workspace);
    // Offered only when a free port exists: a dead button is worse than one option fewer, and the
    // search is what the arm below would need anyway.
    let other_port = free_port_from(port);
    let mut options = vec![
        window::ChoiceOption {
            id: CHOICE_TAKE_OVER.to_string(),
            label: format!("终止 pid {pid} 并接管端口 {port}"),
        },
        window::ChoiceOption {
            id: CHOICE_BROWSER.to_string(),
            label: "保留它，用系统浏览器打开".to_string(),
        },
    ];
    if let Some(free) = other_port {
        options.push(window::ChoiceOption {
            id: CHOICE_PORT.to_string(),
            label: format!("保留它，本应用改用端口 {free}"),
        });
    }
    options.push(window::ChoiceOption {
        id: CHOICE_CANCEL.to_string(),
        label: "什么都不做，退出本应用".to_string(),
    });
    let hint = format!(
        "{} 秒内没有选择将按 config.json 的 take_over_existing={} 处理。",
        window::CHOICE_TIMEOUT.as_secs(),
        config.take_over_existing
    );
    window::set_status(app, &status, &detail);
    match window::ask_choice(app, &status, &detail, &options, &hint).as_deref() {
        Some(CHOICE_TAKE_OVER) => ForeignAction::TakeOver { pid },
        Some(CHOICE_BROWSER) => ForeignAction::UseBrowser,
        Some(CHOICE_PORT) if other_port.is_some() => ForeignAction::UseOtherPort {
            port: other_port.unwrap_or(port),
        },
        Some(CHOICE_CANCEL) => ForeignAction::Refuse {
            reason: format!(
                "已按你的选择保留端口 {port} 上的 Harness（pid {pid}），本应用没有接管它。"
            ),
        },
        // Unanswered, or an id this build no longer offers: the config decides.
        _ => unanswered_choice(config.take_over_existing, pid),
    }
}

/// What an unanswered question means, which is what `config.json` asked for.
///
/// Split out so the rule is testable without a window: a timeout and a question that could not be
/// put have to land in exactly the same place, or a headless launch would behave differently from
/// an ignored one.
fn unanswered_choice(allow: bool, pid: u32) -> ForeignAction {
    if allow {
        ForeignAction::TakeOver { pid }
    } else {
        ForeignAction::UseBrowser
    }
}

/// The same question for a Harness that appeared during a handoff or a restart.
///
/// Returns whether the caller may signal it. An unanswered question follows `config.json`, and
/// "use another port" is *not* permission to signal: that instance is exactly what the user asked
/// the shell to keep.
fn confirm_takeover(app: &AppHandle, config: &Config, port: u16, pid: u32) -> bool {
    matches!(
        resolve_foreign_action(app, config, port, ForeignAction::Ask { pid }),
        ForeignAction::TakeOver { .. }
    )
}

/// The pid serving `port` as a Harness, when one is.
fn harness_listener(port: u16) -> Option<u32> {
    if matches!(harness::probe(port), harness::Probe::Harness) {
        harness::listener_pid(port)
    } else {
        None
    }
}

/// Is this pid proven to be the `dsh web` CLI, and so safe to signal?
///
/// The same two-signal rule the startup path applies through [`foreign_instance_action`]: the auth
/// fence put a Harness on the port, and the command line has to name the CLI. A pid whose command
/// line cannot be read is not proven, and every kill path consults this rather than assuming the
/// fence was enough.
fn identified_dsh_web(pid: u32) -> bool {
    harness::process_command(pid).is_some_and(|command| harness::looks_like_dsh_web(&command))
}

/// Wait out a handoff: another process may be booting a replacement on our port.
fn wait_for_handoff(port: u16, grace: Duration) -> Option<u32> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(pid) = harness_listener(port) {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(HANDOFF_POLL);
    }
}

/// Wait for `port` to stop answering, so the next launch can bind it.
fn wait_for_port_free(port: u16, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(harness::probe(port), harness::Probe::Closed) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait for the Harness to end, and put it back when it does.
///
/// An unexpected exit used to leave the window on a page that could never connect again. Two
/// shapes have to be told apart instead: a deliberate self-restart (the plugin market stops the
/// host and a detached helper boots a replacement on the same port) and a crash or a kill. Both
/// end with the shell starting its own Harness again — the handoff wait only decides whether
/// someone else is about to serve the port, so the shell can take it over instead of racing it.
fn watch_harness(
    app: AppHandle,
    data_dir: PathBuf,
    pid: u32,
    ring: harness::Ring,
    mut child: Child,
) {
    let started = Instant::now();
    let status = child.wait();
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    let reason = status
        .as_ref()
        .map(exit_reason)
        .unwrap_or_else(|error| format!("无法等待进程结束: {error}"));
    harness::app_log(&format!("Harness pid {pid} exited unexpectedly ({reason})"));
    disown(pid);
    process::clear_state(&data_dir);
    let output = ring.tail();
    let clean = status.as_ref().is_ok_and(|status| status.code() == Some(0));

    match exit_action(started.elapsed(), AUTO_RESTARTS.load(Ordering::SeqCst), clean) {
        ExitAction::Report => window::show_failure(
            &app,
            "Harness 已退出",
            &format!(
                "dsh web 进程已结束（{reason}），连续 {MAX_AUTO_RESTARTS} 次自动重启都没有稳定下来，已停止自动重试。\n\n最近输出:\n{output}"
            ),
        ),
        ExitAction::Recover { attempt, grace } => {
            AUTO_RESTARTS.store(attempt, Ordering::SeqCst);
            window::show_progress(
                &app,
                "Harness 已退出，正在重新启动…",
                &format!("pid {pid} 已结束（{reason}）；第 {attempt}/{MAX_AUTO_RESTARTS} 次自动恢复"),
            );
            match restart_harness(&app, &data_dir, grace) {
                Ok(()) => harness::app_log(&format!(
                    "Harness pid {pid} exited and was recovered (automatic restart {attempt})"
                )),
                Err(failure) => window::show_failure(
                    &app,
                    "Harness 已退出",
                    &format!("dsh web 进程已结束（{reason}），自动重启失败：{failure}\n\n最近输出:\n{output}"),
                ),
            }
        }
    }
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
    harness::looks_like_dsh_web(&command) && parent != Parent::Live
}

/// The npm package this shell supervises, for the messages that have to name it.
const DSH_PACKAGE_NAME: &str = "@deepseek-ai/dsh";

/// May the core update path touch the supervised tree?
///
/// Two facts have to hold. The tree must be one this shell owns (`Shadow`, the writable prefix
/// updates install into) or a version that `@deepseek-ai/dsh` really publishes: handing a foreign
/// `dsh` (or an unreadable version) to `npm install -g @deepseek-ai/dsh@latest` would install a
/// second CLI beside it and restart the shell onto a tree the user never chose (2026-09-15).
fn may_update_core(updates: runtime::Updates, version: &str) -> bool {
    matches!(updates, runtime::Updates::Shadow) || crate::update::Version::parse(version).is_some()
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
    /// The profile does not mention the market at all, so there is nothing to keep current.
    /// Reported rather than silently skipped: "the UI has no plugin market" is otherwise a fact
    /// the user has to work out from the absence of a menu.
    NotDeclared,
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
            PluginSkip::NotDeclared => format!(
                "profile {} 里没有声明 {}：界面不会出现插件市场，也不会自动安装（                 用 `dsh plugin --profile web add {}` 装上，或改用自带运行时版让首启播种模板）",
                update::MARKET_PLUGIN,
                update::MARKET_PLUGIN,
                update::MARKET_PLUGIN
            ),
        }
    }
}

/// Why the plugin market step cannot run this launch.
///
/// `declared` is the profile's own `package.json`: a market the user removed must not come back
/// (design §2.5), but that decision is worth a log line — the shell used to skip the whole step
/// without a word, which is how "the plugin shop is gone" became unexplainable from the log.
fn plugin_skip_reason(
    declared: bool,
    installed: Option<&str>,
    pnpm: Option<&Path>,
) -> Option<PluginSkip> {
    if !declared {
        return Some(PluginSkip::NotDeclared);
    }
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
    // The port this run uses, which is the configured one unless an earlier takeover question
    // moved it (see [`PORT_OVERRIDE`]). Mutable because the question below can move it again.
    let mut port = runtime_port(config.port);
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

    // 2b) Finish what a previous launch started. A swap record that is still here means that
    //     launch replaced the CLI tree and never saw the new one print its startup URL, so the
    //     tree it replaced goes back before anything resolves a runtime: otherwise this launch
    //     would pick the same unproven tree again and fail the same way.
    let runtime_root = data_dir.join("runtime");
    let paths = UpdatePaths::new(&runtime_root);
    recover_pending_swap(&paths);
    clear_stale_staging(&paths);

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
    // Reported either way: a bundled build that skipped the template has no plugin market, and
    // that is the one fact a user cannot see from the UI.
    let seed_outcome = seed_profile_template(&config, resolved.bundled(), resolved.seed.as_deref());
    let seeded_this_run = seed_outcome.seeded;
    if let Some(note) = seed_outcome.note {
        harness::app_log(&note);
    }

    // The update path installs into `runtime/prefix` and keeps npm's cache there: create both
    // up front (idempotent), like the plan's §3 step 2 asks (review P2-11). The staging and
    // rollback directories belong to the update transaction and are created for the same
    // reason: a missing directory discovered mid-update is a failed update.
    for dir in [
        runtime_root.join("prefix"),
        runtime_root.join("tools"),
        runtime_root.join("npm-cache"),
        paths.staging.clone(),
        paths.rollback.clone(),
        paths.profiles.clone(),
    ] {
        if let Err(error) = std::fs::create_dir_all(&dir) {
            harness::app_log(&format!("无法创建 {}: {error}", dir.display()));
        }
    }

    // 3b) Update the supervised CLI before booting it, so "core upgrade" needs no terminal.
    let may_update = may_update_core(resolved.updates, &version);
    if !may_update {
        harness::app_log(&format!(
            "跳过核心更新检查：监督的 dsh {version} 不是 {DSH_PACKAGE_NAME} 的已发布版本之一"
        ));
    }
    let mut just_updated = false;
    // Held until the moment the new tree is actually booted: everything between here and the
    // spawn can still decide not to start a Harness at all (a busy port, a foreign instance,
    // a rejected version), and a swap made for a launch that never happens is a rollback the
    // next launch has to clean up for no reason.
    let mut staged_update: Option<StagedUpdate> = None;
    // Separate from `just_updated`: the plugin step sets that one too, and only a core swap
    // has a rollback record to confirm or undo.
    let mut core_swapped = false;
    if config.auto_update && may_update {
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
                        } else if !update::may_install(&to, config.require_tested_dsh) {
                            // Installing a version the startup check below would refuse to boot
                            // leaves the user with a CLI this shell just wrote and will not run,
                            // and the working version is already overwritten. Report it instead.
                            harness::app_log(&format!(
                                "update available: {from} -> {to}, but it is outside the tested range ({}); not installing",
                                update::compatibility(&to).describe()
                            ));
                            window::set_status(
                                app,
                                &format!("有新版本 v{to}（超出已测试区间，未自动安装）"),
                                &format!("v{from} -> v{to}；如需使用请在 config.json 里设置 \"require_tested_dsh\": false"),
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
                                    let cache = runtime_root.join("npm-cache");
                                    // Which tree the new version belongs in: this shell own
                                    // shadow prefix, or the user prefix when they asked for
                                    // in-place upgrades of their own installation.
                                    let target_prefix = resolved
                                        .update_prefix(data_dir)
                                        .or_else(|| update::install_prefix(&resolved.dsh_js));
                                    // Build and verify the new tree next to the live one. Nothing
                                    // is replaced until that succeeded, so a registry that fails
                                    // half-way leaves the CLI the user has exactly as it was.
                                    let staged = match target_prefix.as_deref() {
                                        // No prefix to speak of: a CLI whose layout names no
                                        // npm prefix at all (a hand-built tree). There is
                                        // nothing to swap, so say that instead of guessing.
                                        None => Err(format!(
                                            "无法确定 {} 所属的 npm 前缀，不做原地更新",
                                            resolved.dsh_js.display()
                                        )),
                                        Some(prefix) => {
                                            stage_core_update(&npm, &to, prefix, &paths, &cache)
                                        }
                                    };
                                    match staged {
                                        Err(reason) => harness::app_log(&format!(
                                            "update staged but not committed, keeping v{from}: {reason}"
                                        )),
                                        // Verified, not yet in place: the swap waits until the
                                        // launch is committed to booting it.
                                        Ok(staged) => {
                                            harness::app_log(&format!(
                                                "update staged: {from} -> {to}（校验通过，待切换）"
                                            ));
                                            staged_update = Some(staged);
                                        }
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
        let declared = update::declares_plugin(&profile_dir, update::MARKET_PLUGIN);
        if declared {
            let installed = update::installed_plugin(&profile_dir, update::MARKET_PLUGIN);
            // Resolve pnpm before anything is stopped: it is what actually installs a plugin,
            // and without it the CLI exits 127 after the instance is already gone (review A1).
            let pnpm = update::find_pnpm(Some(OsStr::new(&child.path)));
            if let Some(skip) = plugin_skip_reason(declared, installed.as_deref(), pnpm.as_deref())
            {
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
                                if update_market_plugin(
                                    app,
                                    data_dir,
                                    &paths,
                                    &profile_dir,
                                    &resolved,
                                    OsStr::new(&child.path),
                                    &config,
                                    port,
                                    &from,
                                    &to,
                                ) {
                                    just_updated = true;
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

    // Whether the detection step signalled something and must therefore wait for the port to
    // come free before spawning.
    let mut took_over = false;
    // 3c) Detection. Runs after the update so a freshly installed CLI is what we boot. The startup URL carries a per-process token that no other process can
    //     recover, so a foreign instance can never hand us a session.
    match harness::probe(port) {
        harness::Probe::Harness => {
            let owner = harness::listener_pid(port);
            let ours = process::read_state(data_dir)
                .map(|state| state.pid)
                .filter(|pid| Some(*pid) == owner && process::is_alive(*pid));

            match ours {
                // Our own instance from an earlier run, and no update is about to touch its CLI
                // tree: its cookie is still valid for this authority (verified across
                // restarts), so reuse it as-is instead of restarting it.
                //
                // A staged core update counts here even though it has not been committed yet:
                // it is committed just before the spawn, so reusing this instance would leave
                // the new tree staged and never booted.
                Some(pid) if !just_updated && staged_update.is_none() => {
                    if window::unsupported_webview(config.webkit_compat).is_some() {
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
                        let compat = harness_compat(&config);
                        return window::create_harness(app, &url, port, compat.as_deref())
                            .map_err(|e| e.to_string());
                    }
                }
                // Our own instance that a fresh update (or one about to be committed) just
                // made obsolete: it is our child, so its whole process group goes down
                // together, exactly as at exit.
                Some(pid) => {
                    window::set_status(app, "更新完成，正在重启 Harness…", &format!("pid {pid}"));
                    process::terminate(pid, TERMINATE_GRACE);
                }
                None => {
                    // Two independent signals have to agree before a process this shell did not
                    // start is signalled: the auth fence named a Harness, and the command line
                    // names the CLI. Either one alone is a guess.
                    //
                    // Set only when this shell actually signalled something: the wait below is
                    // for a port that was just vacated, and would be meaningless (and
                    // mislabelled) after a choice that leaves the other instance running.
                    took_over = false;
                    let command = owner.and_then(harness::process_command);
                    let action = foreign_instance_action(owner, command.as_deref());
                    // An identified instance is always a question: it may be a terminal session
                    // or an agent run the user wants to keep, and only they can say.
                    let action = resolve_foreign_action(app, &config, port, action);
                    let page = terminal_page(&action);
                    match action {
                        ForeignAction::UseBrowser => {
                            window::open_external(&format!("http://127.0.0.1:{port}/"));
                            let status = format!("端口 {port} 已被另一个 Harness 占用");
                            let detail = format!(
                                "已按你的选择在系统浏览器中打开 127.0.0.1:{port}。\n\n\
                                 本应用没有接管它，因此没有可重新启动的 Harness：关闭本窗口即退出应用。\n\n\
                                 想改这个行为，就在 config.json 里设 \"take_over_existing\": true \
                                 （接管会终止该实例及其当前会话），或先自己停掉它。\n\n\
                                 浏览器需要已有该实例的登录 cookie；若看到 authentication required，\
                                 请在启动那个实例的终端里重新打开一次它打印的 URL。"
                            );
                            match page {
                                TerminalPage::Notice => window::show_notice(app, &status, &detail),
                                TerminalPage::Failure => fail(app, &status, &detail),
                            }
                            return Ok(());
                        }
                        // Leave that instance where it is and start this shell on a port of its
                        // own. Recorded for this launch only: the configured port is what the
                        // session cookie is bound to, so a silent permanent migration would be
                        // worse than asking again next time.
                        ForeignAction::UseOtherPort { port: free } => {
                            harness::app_log(&format!(
                                "端口 {port} 上的 Harness（{}) 保留不动，本应用改用端口 {free}",
                                owner
                                    .map(|pid| format!("pid {pid}"))
                                    .unwrap_or_else(|| "pid 未知".to_string())
                            ));
                            PORT_OVERRIDE.store(free, Ordering::SeqCst);
                            // The spawn below reads `port`, so the rest of this attempt runs
                            // against the new one.
                            port = free;
                        }
                        ForeignAction::Refuse { reason } => return Err(reason),
                        // The startup path answers the question through
                        // `resolve_foreign_action`, so reaching this arm means a caller
                        // forgot to; refuse rather than signal silently.
                        ForeignAction::Ask { pid } => {
                            return Err(format!(
                                "端口 {port} 上的 Harness（pid {pid}）没有得到处理，已放弃本次启动。"
                            ));
                        }
                        ForeignAction::TakeOver { pid } => {
                            // Stop it, then start our own so the window receives a fresh
                            // authenticated URL. Never a group signal: its process group belongs
                            // to whatever started it (a terminal, or an agent run).
                            window::set_status(app, "正在接管其它 Harness…", &format!("pid {pid}"));
                            process::terminate_pid(pid, TERMINATE_GRACE);
                            took_over = true;
                        }
                    }
                }
            }
            // Only a takeover needs this: it just vacated the port, and the spawn below cannot
            // bind it otherwise. Every other answer either left the port to that instance (the
            // new one is free by construction) or returned already.
            if took_over {
                let deadline = std::time::Instant::now() + TERMINATE_GRACE;
                while !matches!(harness::probe(port), harness::Probe::Closed) {
                    if std::time::Instant::now() > deadline {
                        return Err(format!("接管失败：端口 {port} 仍被占用。"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        harness::Probe::Other => {
            return Err(format!(
                "端口 {port} 被其它程序占用，请在 config.json 里换一个端口。"
            ));
        }
        harness::Probe::Closed => {}
    }

    // 3d) The launch is now committed to booting a Harness: put the verified tree in place.
    //     Everything above could still have returned without starting one, and a swap made for
    //     a launch that never happens would leave a rollback record for the next launch to
    //     undo. From here on, a failure to boot is exactly what the record is for.
    if let Some(staged) = staged_update.take() {
        let from = version.clone();
        match commit_core_update(staged, &paths) {
            Ok((installed_path, installed_version)) => {
                version = installed_version;
                resolved.dsh_js = installed_path;
                core_swapped = true;
                harness::app_log(&format!("dsh updated: {from} -> {to}", to = version));
            }
            Err(reason) => harness::app_log(&format!("update failed, keeping v{from}: {reason}")),
        }
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
    // The source is named on the status page, not just in the log: "which dsh is this" is the
    // first question when the UI behaves like a different installation (2026-09-15).
    //
    // The compat layer is deliberately not named here. Its answer comes from the splash page,
    // which probes while this thread is still resolving and updating, so a read taken now is a
    // guess — and a guess printed on the status line is worse than no hint at all.
    window::set_status(
        app,
        "正在启动 Harness…",
        &format!(
            "dsh {version}{} · {} · {} · 端口 {port}",
            if untested {
                "（未测试版本）"
            } else {
                ""
            },
            runtime::Origin::label_of(resolved.origin),
            resolved.dsh_js.display(),
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
            // A tree that was just swapped in and never printed its URL is not one to keep:
            // put the previous one back now, while the reason is still in hand, instead of
            // making the user hit the same wall on the next launch.
            let rolled_back = core_swapped && roll_back_core_update(&paths);
            return abort_start(
                pid,
                format!(
                    "{reason}{}\n\n最近输出:\n{tail}",
                    if rolled_back {
                        "\n\n新版本未能启动，已回滚到上一个版本。"
                    } else {
                        ""
                    }
                ),
            );
        }
    };

    // The new tree booted and printed its URL: the swap is real. Until this line the previous
    // tree was still the fallback a launch would roll back to.
    if core_swapped {
        confirm_core_update(&paths);
    }
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

    // This call waits for the splash page's report, so everything below reads a settled answer
    // rather than a race.
    if hand_the_gui_to_the_browser(app, &url, &version, config.webkit_compat) {
        return Ok(());
    }
    // Decided here, not before the spawn: the report only exists once the splash page has run its
    // probe, and reading the slot earlier returned `None` for a report that had simply not arrived
    // yet — which silently opened an unpatched window and blamed it on `webkit_compat`
    // (found by running the built app, 2026-09-15).
    let compat = harness_compat(&config);
    if let Some(report) = window::report().filter(|report| report.needs_compat()) {
        // The reason has to name the real one. `compat` is decided before the spawn, and the
        // report arrives while the CLI boots, so a missing script can also mean the probe was
        // not in yet — blaming the config for that sent a reader looking at a setting that was
        // never the problem (found by running the built app, 2026-09-15).
        harness::app_log(&format!(
            "WebView 缺少 {}：{}",
            report.missing.join("、"),
            if compat.is_some() {
                "已注入兼容层"
            } else if config.webkit_compat {
                "探测未在上报窗口内到达，未注入兼容层"
            } else {
                "webkit_compat=false，未注入兼容层"
            }
        ));
    }
    if let Err(error) = window::create_harness(app, &url, actual_port, compat.as_deref()) {
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

/// A startup failure is terminal for this attempt, but not for the app: the page it lands on
/// carries the restart button, and a second launch means the same thing.
fn fail(app: &AppHandle, status: &str, detail: &str) {
    window::show_failure(app, status, detail);
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
///
/// `compat` is what `config.json` allows: with it off, an engine the compat layer could patch is
/// treated as unsupported, so the UI goes to the browser instead of into a window without its shim.
fn hand_the_gui_to_the_browser(
    app: &AppHandle,
    url: &url::Url,
    version: &str,
    compat: bool,
) -> bool {
    let Some(report) = window::unsupported_webview(compat) else {
        return false;
    };
    harness::app_log(&format!(
        "WebView 缺少 dsh 前端必需的能力（{}），改用默认浏览器打开 {url}",
        report.gaps(compat).join("、")
    ));
    window::open_external(url.as_str());
    // Notice, not failure: the Harness is alive and stays supervised here, so no restart button.
    window::show_notice(
        app,
        "系统 WebView 太旧，界面已改在浏览器中打开",
        &report.browser_fallback_detail(version, url.as_str(), compat),
    );
    true
}

/// The compat script the Harness window may carry, when the shell is allowed to install it.
fn harness_compat(config: &Config) -> Option<String> {
    config
        .webkit_compat
        .then(window::needed_compat_script)
        .flatten()
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

    /// A staged update is verified and swapped, and a launch that never confirmed it is undone by
    /// the next one. This is the whole P2-10 contract, minus the npm call.
    #[test]
    fn a_staged_update_swaps_the_tree_and_an_unconfirmed_one_is_rolled_back() {
        let root = std::env::temp_dir().join("dsh-desktop-core-swap-test");
        let _ = std::fs::remove_dir_all(&root);
        let runtime = root.join("runtime");
        let paths = UpdatePaths::new(&runtime);
        std::fs::create_dir_all(&paths.staging).unwrap();
        std::fs::create_dir_all(&paths.rollback).unwrap();

        // The live tree, in the shadow prefix layout the shell updates.
        let prefix = runtime.join("prefix");
        let live = prefix.join("lib/node_modules/@deepseek-ai/dsh");
        std::fs::create_dir_all(live.join("lib")).unwrap();
        std::fs::write(live.join("lib/bin.js"), "old\n").unwrap();
        std::fs::write(live.join("package.json"), r#"{"name":"@deepseek-ai/dsh"}"#).unwrap();

        // What `stage_core_update` leaves behind once npm and the verifier are done: a complete
        // tree inside the staging prefix, verified by name and version.
        let staging = transaction::Staging::create(&paths.staging, "0.1.6").unwrap();
        let staged_dir = staging.prefix().join("lib/node_modules/@deepseek-ai/dsh");
        std::fs::create_dir_all(staged_dir.join("lib")).unwrap();
        std::fs::write(staged_dir.join("lib/bin.js"), "new\n").unwrap();
        std::fs::write(
            staged_dir.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"0.1.6"}"#,
        )
        .unwrap();
        let verified =
            transaction::verify_install(&staging.prefix(), "@deepseek-ai/dsh", "0.1.6").unwrap();
        assert_eq!(verified.version, "0.1.6");
        let staged = StagedUpdate {
            _staging: staging,
            dir: verified.dir,
            version: verified.version,
            target: live.clone(),
            backup: paths.rollback.join("0.1.6"),
        };

        let (entry, version) = commit_core_update(staged, &paths).unwrap();
        assert_eq!(version, "0.1.6");
        // The entry script moved with its package directory: the spawn uses this path.
        assert!(entry.is_file());
        assert!(entry.starts_with(&live), "{}", entry.display());
        assert_eq!(
            std::fs::read_to_string(live.join("lib/bin.js")).unwrap(),
            "new\n"
        );
        // The staging shell is gone: the tree it held is the live one now.
        assert!(!paths.staging.join("staging-0.1.6").exists());
        // The swap is recorded but not confirmed: a launch that dies here must be recoverable.
        let record = transaction::read_swap(&paths.swap).expect("the swap is recorded");
        assert_eq!(record.version, "0.1.6");
        assert_eq!(record.target, live.to_string_lossy());

        // The next launch finds the record and puts the previous tree back.
        recover_pending_swap(&paths);
        assert_eq!(
            std::fs::read_to_string(live.join("lib/bin.js")).unwrap(),
            "old\n"
        );
        assert!(transaction::read_swap(&paths.swap).is_none());
        // The tree that failed is kept as evidence rather than deleted.
        assert_eq!(
            std::fs::read_to_string(transaction::failed_path(&live).join("lib/bin.js")).unwrap(),
            "new\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A staged tree a killed process left behind is ~290 MB of debris nothing refers to, so the
    /// next launch removes it. The directory itself stays: this launch stages into it.
    #[test]
    fn a_stale_staging_tree_is_cleared_without_taking_the_directory_with_it() {
        let root = std::env::temp_dir().join("dsh-desktop-stale-staging-test");
        let _ = std::fs::remove_dir_all(&root);
        let paths = UpdatePaths::new(&root.join("runtime"));
        std::fs::create_dir_all(paths.staging.join("staging-0.1.5")).unwrap();
        std::fs::create_dir_all(paths.staging.join("staging-0.1.6/prefix")).unwrap();
        std::fs::write(
            paths.staging.join("staging-0.1.6/prefix/junk"),
            "half-written",
        )
        .unwrap();

        clear_stale_staging(&paths);
        assert!(!paths.staging.join("staging-0.1.5").exists());
        assert!(!paths.staging.join("staging-0.1.6").exists());
        assert!(
            paths.staging.is_dir(),
            "the staging root itself must survive"
        );

        // Nothing to clean is not an error, and neither is a staging root that never existed.
        clear_stale_staging(&paths);
        clear_stale_staging(&UpdatePaths::new(&root.join("elsewhere/runtime")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A profile snapshot is taken before pnpm may rewrite the profile, and a failed install is
    /// undone from it without touching the state the running Harness keeps writing.
    #[test]
    fn a_profile_snapshot_makes_a_failed_plugin_install_reversible() {
        let root = std::env::temp_dir().join("dsh-desktop-profile-snapshot-test");
        let _ = std::fs::remove_dir_all(&root);
        let paths = UpdatePaths::new(&root.join("runtime"));
        std::fs::create_dir_all(&paths.profiles).unwrap();

        let profile = root.join("profiles/web");
        std::fs::create_dir_all(profile.join("node_modules/dshmarket")).unwrap();
        std::fs::create_dir_all(profile.join("data")).unwrap();
        std::fs::write(
            profile.join("package.json"),
            r#"{"name":"dsh-profile-web"}"#,
        )
        .unwrap();
        std::fs::write(profile.join("node_modules/dshmarket/version"), "1.0.0").unwrap();
        std::fs::write(profile.join("data/usage.json"), "before").unwrap();

        let snapshot = snapshot_profile(&paths, &profile, "2.0.0").unwrap();
        assert!(snapshot.join("node_modules/dshmarket/version").is_file());
        // Credentials and session state are not part of the snapshot.
        assert!(!snapshot.join("data").exists());

        // pnpm got half-way and died: the tree is neither version.
        std::fs::write(profile.join("node_modules/dshmarket/version"), "2.0.0").unwrap();
        std::fs::write(profile.join("node_modules/dshmarket/half"), "junk").unwrap();
        // The Harness kept writing live state the whole time.
        std::fs::write(profile.join("data/usage.json"), "after").unwrap();

        restore_profile(&snapshot, &profile).unwrap();
        assert_eq!(
            std::fs::read_to_string(profile.join("node_modules/dshmarket/version")).unwrap(),
            "1.0.0"
        );
        assert!(!profile.join("node_modules/dshmarket/half").exists());
        assert_eq!(
            std::fs::read_to_string(profile.join("data/usage.json")).unwrap(),
            "after",
            "restoring the plugin tree must not roll back live session state"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A confirmed boot drops the record, and the rollback generation is what the next update
    /// would have to undo.
    #[test]
    fn a_confirmed_boot_keeps_the_new_tree_and_clears_the_record() {
        let root = std::env::temp_dir().join("dsh-desktop-core-confirm-test");
        let _ = std::fs::remove_dir_all(&root);
        let paths = UpdatePaths::new(&root.join("runtime"));
        std::fs::create_dir_all(&paths.rollback).unwrap();
        transaction::write_swap(
            &paths.swap,
            &transaction::SwapRecord {
                target: "/tmp/target".to_string(),
                backup: "/tmp/backup".to_string(),
                version: "0.1.6".to_string(),
                at: 0,
            },
        )
        .unwrap();

        confirm_core_update(&paths);
        assert!(transaction::read_swap(&paths.swap).is_none());
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
        assert!(may_stop_before_update(&Probe::Closed).is_ok());
        // Someone else owns the port: never install over it.
        assert!(may_stop_before_update(&Probe::Other).is_err());
        // A Harness — ours or somebody else's — may be stopped only after the question is put,
        // which is `stop_instance_before_update`'s job. The config is not consulted here: doing
        // so skipped the question for everyone who had not edited `config.json` (2026-09-16).
        assert!(may_stop_before_update(&Probe::Harness).is_ok());
    }

    /// Taking the port from another Harness kills a session someone may be watching, so an
    /// identified instance is always asked about — never signalled, and never silently resolved
    /// to a browser either. Gating the question on `take_over_existing` hid it from every user
    /// who had not already edited `config.json` (2026-09-16).
    #[test]
    fn an_identified_foreign_instance_is_always_asked_about() {
        const CMD: &str = "/opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --port 3080";

        // Identified: the question is put, and only the answer may signal anything. Both values
        // of the config reach this same arm — that is the fix.
        assert_eq!(
            foreign_instance_action(Some(4242), Some(CMD)),
            ForeignAction::Ask { pid: 4242 }
        );
        // The port answers like a Harness, but nothing proves which program it is. This is the
        // case the old code killed: an unrelated server on the configured port.
        assert!(matches!(
            foreign_instance_action(Some(4242), Some("/usr/bin/python3 -m http.server")),
            ForeignAction::Refuse { .. }
        ));
        // A command line the platform would not report is not proof either.
        assert!(matches!(
            foreign_instance_action(Some(4242), None),
            ForeignAction::Refuse { .. }
        ));
        assert!(matches!(
            foreign_instance_action(None, Some(CMD)),
            ForeignAction::Refuse { .. }
        ));
    }

    /// The page a startup outcome lands on decides whether the user gets a button that can work.
    /// The browser fallback has nothing to restart, so offering one looped: click, re-run the same
    /// doomed start, open another tab, land here again (observed in the field, 2026-09-16).
    #[test]
    fn leaving_the_instance_alone_does_not_offer_a_restart() {
        assert_eq!(
            terminal_page(&ForeignAction::UseBrowser),
            TerminalPage::Notice
        );
        // A refusal is something the user can act on and retry in place, so the button stays.
        assert_eq!(
            terminal_page(&ForeignAction::Refuse {
                reason: "port held".to_string()
            }),
            TerminalPage::Failure
        );
        assert_eq!(
            terminal_page(&ForeignAction::TakeOver { pid: 1 }),
            TerminalPage::Failure
        );
    }

    /// An unanswered question runs the config, and a question that could not be put at all has to
    /// land in the same place: otherwise a headless launch and an ignored one would disagree.
    #[test]
    fn an_unanswered_question_follows_the_config() {
        assert_eq!(
            unanswered_choice(true, 4242),
            ForeignAction::TakeOver { pid: 4242 }
        );
        assert_eq!(unanswered_choice(false, 4242), ForeignAction::UseBrowser);
    }

    /// A free port is what makes the third option real. The configured port is not free by
    /// definition — that is why the question is being asked — so the search starts above it.
    #[test]
    fn another_port_is_offered_only_when_one_is_actually_free() {
        // Hold a port, then ask for a free one starting there: the answer must skip it.
        let held = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port must bind");
        let taken = held.local_addr().unwrap().port();
        let found = free_port_from(taken).expect("the range must hold a free port");
        assert!(found > taken, "{found} must be above {taken}");
        // It really is free, and it is inside the bounded range.
        assert!(std::net::TcpListener::bind(("127.0.0.1", found)).is_ok());
        assert!(found <= taken.saturating_add(PORT_SEARCH_RANGE));
    }

    /// The override is per launch: a configured port that something else owns stays configured, so
    /// the next start asks again instead of migrating a port the session cookie is bound to.
    #[test]
    fn a_port_override_lasts_for_one_launch_only() {
        // Nothing stored: the configured port wins.
        assert_eq!(runtime_port(3080), 3080);
        PORT_OVERRIDE.store(3091, Ordering::SeqCst);
        assert_eq!(runtime_port(3080), 3091);
        PORT_OVERRIDE.store(0, Ordering::SeqCst);
        assert_eq!(runtime_port(3080), 3080);
    }

    /// The question has to name what the user is about to end: a pid alone does not tell them
    /// which terminal session or workspace they are looking at.
    #[test]
    fn the_takeover_question_names_the_process_and_this_shells_workspace() {
        const CMD: &str = "/usr/bin/node /srv/dsh/lib/bin.js --profile web --port 3080";
        let (status, detail) =
            takeover_question(3080, 4242, Some(CMD), Path::new("/Users/me/project"));
        assert_eq!(status, "检测到其它 Harness");
        assert!(detail.contains("127.0.0.1:3080"), "{detail}");
        assert!(detail.contains("pid 4242"), "{detail}");
        assert!(detail.contains(CMD), "{detail}");
        // The workspace this shell would restart it with, which is the part that surprises
        // people: a takeover does not continue the other instance session.
        assert!(detail.contains("/Users/me/project"), "{detail}");
        // A command line the platform would not report is still named as unknown rather than
        // left out, so the user knows what the shell does not know.
        let (_, blind) = takeover_question(3080, 7, None, Path::new("/tmp"));
        assert!(blind.contains("读不到命令行"), "{blind}");
    }

    /// An unanswered question falls back to exactly what the config asked for, so a headless or
    /// ignored launch behaves the way the shell did before it could ask.
    #[test]
    fn an_unanswered_takeover_question_follows_the_config_default() {
        // The wait itself: a value that never arrives is a timeout, not a panic or a hang.
        assert_eq!(
            window::wait_for_choice(1, Duration::from_millis(30), || None),
            None
        );
        // An answer to a different question (a click that crossed a retry) is not this answer.
        let stale = || {
            Some(window::Choice {
                question: 1,
                id: CHOICE_TAKE_OVER.to_string(),
            })
        };
        assert_eq!(
            window::wait_for_choice(2, Duration::from_millis(30), stale),
            None
        );
        let fresh = || {
            Some(window::Choice {
                question: 2,
                id: CHOICE_BROWSER.to_string(),
            })
        };
        assert_eq!(
            window::wait_for_choice(2, Duration::from_secs(5), fresh).map(|choice| choice.id),
            Some(CHOICE_BROWSER.to_string())
        );
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
        // The manifest has to name the package: a version is only read from a tree that
        // identifies itself as this CLI.
        std::fs::write(
            dsh_dir.join("package.json"),
            "{\"name\": \"@deepseek-ai/dsh\", \"version\": \"9.9.9\"}",
        )
        .unwrap();

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
    fn system_updates_defaults_to_leaving_the_user_install_alone() {
        let dir = std::env::temp_dir().join("dsh-desktop-system-updates-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A prefix this shell does not own is not rewritten unless the user asks for it: an
        // automatic `npm install -g` can change a CLI other tools on the machine share.
        assert_eq!(
            Config::load(&dir).system_updates,
            runtime::SystemUpdates::Notify
        );
        std::fs::write(dir.join("config.json"), "{\"system_updates\": \"install\"}").unwrap();
        assert_eq!(
            Config::load(&dir).system_updates,
            runtime::SystemUpdates::Install
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The defaults a user gets without editing anything. Every one of these decides whether the
    /// shell may touch something it does not own, so each is asserted rather than assumed.
    #[test]
    fn defaults_do_not_take_over_foreign_state() {
        let dir = std::env::temp_dir().join("dsh-desktop-config-defaults-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config::load(&dir);

        // Killing whatever holds the port is the user's call, not a startup path's.
        assert!(!config.take_over_existing);
        // A prerelease channel is not a default target for a desktop user.
        assert_eq!(config.update_tags, vec!["latest".to_string()]);
        // A profile is user data; keeping its plugin tree current is opt-in.
        assert!(!config.auto_update_plugins);
        // The shell parses the CLI's startup line, so an untested version is refused by default.
        assert!(config.require_tested_dsh);
        assert_eq!(config.system_updates, runtime::SystemUpdates::Notify);
        // Still on: the bundled/shadow runtime is this shell's own tree to update.
        assert!(config.auto_update);

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
            plugin_skip_reason(true, None, Some(pnpm)),
            Some(PluginSkip::NotInstalled)
        );
        // No pnpm: `dsh plugin add` forwards to pnpm and exits 127 — after the shell already
        // stopped the Harness, so the decision has to be made here (review A1).
        assert_eq!(
            plugin_skip_reason(true, Some("1.0.0"), None),
            Some(PluginSkip::NoPnpm)
        );
        // Both halves present: the update may proceed.
        assert_eq!(plugin_skip_reason(true, Some("1.0.0"), Some(pnpm)), None);
        // The skip has to explain itself; this is the log line the user sees.
        assert!(PluginSkip::NoPnpm.reason().contains("pnpm"));
        assert!(PluginSkip::NotInstalled
            .reason()
            .contains(update::MARKET_PLUGIN));
    }

    /// Core updates are the same hazard as the identity check: `npm i -g @deepseek-ai/dsh`
    /// against a foreign or unreadable tree installs a second CLI and restarts onto it.
    #[test]
    fn the_core_update_only_touches_a_tree_it_can_identify() {
        // The shell's own writable prefix: always its business.
        assert!(may_update_core(runtime::Updates::Shadow, "0.1.5-rc.2"));
        assert!(may_update_core(runtime::Updates::Shadow, "未知"));
        // A user's install whose version this package really publishes: updatable in place.
        assert!(may_update_core(runtime::Updates::Notify, "0.1.5-rc.2"));
        assert!(may_update_core(runtime::Updates::Notify, "0.1.6-alpha.1"));
        // A user's install with no version to compare against: leave it alone.
        assert!(!may_update_core(runtime::Updates::Notify, "未知"));
        assert!(!may_update_core(runtime::Updates::Notify, ""));
        assert!(!may_update_core(runtime::Updates::Notify, "Dancer's shell"));
    }

    /// A profile without the market used to be skipped in silence, which is how "the UI has no
    /// plugin market" became impossible to explain from the log.
    #[test]
    fn a_profile_without_the_market_says_so_instead_of_going_quiet() {
        let pnpm = Path::new("/opt/runtime/tools/bin/pnpm");
        assert_eq!(
            plugin_skip_reason(false, None, Some(pnpm)),
            Some(PluginSkip::NotDeclared)
        );
        // Not declared wins over the other reasons: there is nothing to install or update.
        assert_eq!(
            plugin_skip_reason(false, Some("1.0.0"), None),
            Some(PluginSkip::NotDeclared)
        );
        let reason = PluginSkip::NotDeclared.reason();
        assert!(reason.contains(update::MARKET_PLUGIN), "{reason}");
        assert!(reason.contains("profile"), "{reason}");
        // The repair path is named, because the missing menu is not something a user can guess.
        assert!(reason.contains("dsh plugin"), "{reason}");
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
        let outcome = seed_profile_template(&config, true, Some(&seed));
        assert!(outcome.seeded);
        assert!(outcome.note.is_some());
        assert!(home.join("profiles/web/package.json").is_file());

        // Second launch: the profile exists, so this is not a first start any more — and saying
        // so is what makes "my UI has no plugin market" answerable from the log.
        let again = seed_profile_template(&config, true, Some(&seed));
        assert!(!again.seeded);
        assert_eq!(
            again
                .note
                .as_deref()
                .map(|note| note.contains("profile 已存在")),
            Some(true),
            "{:?}",
            again.note
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The decision matrix, without a filesystem: every skip has to be sayable.
    #[test]
    fn seeding_decides_on_four_plain_facts() {
        assert_eq!(seed_decision(true, true, true, false), Ok(()));
        assert_eq!(
            seed_decision(false, true, true, false),
            Err(SeedSkip::NotBundled)
        );
        assert_eq!(
            seed_decision(true, false, true, false),
            Err(SeedSkip::NoTemplate)
        );
        assert_eq!(
            seed_decision(true, true, false, false),
            Err(SeedSkip::NoHome)
        );
        assert_eq!(
            seed_decision(true, true, true, true),
            Err(SeedSkip::ProfileExists)
        );
        // A non-bundled build names itself, not whatever else is missing.
        assert_eq!(
            seed_decision(false, false, false, true),
            Err(SeedSkip::NotBundled)
        );
        // Each reason has to name the consequence, because the missing menu cannot be guessed.
        for skip in [
            SeedSkip::NotBundled,
            SeedSkip::NoTemplate,
            SeedSkip::NoHome,
            SeedSkip::ProfileExists,
        ] {
            let reason = skip.reason();
            assert!(!reason.is_empty(), "{skip:?} must explain itself");
        }
        assert!(SeedSkip::ProfileExists.reason().contains("插件市场"));
    }

    #[test]
    fn a_self_restart_gets_the_long_handoff_wait_and_a_crash_does_not() {
        // The plugin market's restart: the host is SIGTERMed, shuts down with code 0, and a
        // detached helper boots the replacement a few seconds later — waiting is the point.
        assert_eq!(
            exit_action(Duration::from_secs(5), 0, true),
            ExitAction::Recover {
                attempt: 1,
                grace: HANDOFF_GRACE
            }
        );
        // A crash or a kill has nothing behind it: the restart must not be delayed by the full
        // grace (this is the case the old code answered instantly with a failure page).
        assert_eq!(
            exit_action(Duration::from_secs(5), 0, false),
            ExitAction::Recover {
                attempt: 1,
                grace: HANDOFF_GRACE_QUICK
            }
        );
        assert!(HANDOFF_GRACE_QUICK < HANDOFF_GRACE);
    }

    #[test]
    fn automatic_restarts_are_budgeted_and_reset_by_a_healthy_run() {
        let quick = |attempt| ExitAction::Recover {
            attempt,
            grace: HANDOFF_GRACE_QUICK,
        };
        // A crash loop counts up to the budget, then stops instead of looping for ever.
        assert_eq!(exit_action(Duration::from_secs(1), 0, false), quick(1));
        assert_eq!(exit_action(Duration::from_secs(1), 1, false), quick(2));
        assert_eq!(exit_action(Duration::from_secs(1), 2, false), quick(3));
        assert_eq!(
            exit_action(Duration::from_secs(1), 3, false),
            ExitAction::Report
        );
        // One run that lasted is proof the loop is over: the count starts again from one, so a
        // Harness that works for hours and then dies is never refused a restart.
        assert_eq!(exit_action(HEALTHY_RUN, 3, false), quick(1));
        assert_eq!(
            exit_action(Duration::from_secs(3600), MAX_AUTO_RESTARTS, true),
            ExitAction::Recover {
                attempt: 1,
                grace: HANDOFF_GRACE
            }
        );
    }

    /// The page used to print Rust's Option debug form ("Some(0)", "None") at the user.
    #[cfg(unix)]
    #[test]
    fn the_exit_reason_reaches_the_user_as_words() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            exit_reason(&std::process::ExitStatus::from_raw(0)),
            "退出码 0"
        );
        assert_eq!(
            exit_reason(&std::process::ExitStatus::from_raw(3 << 8)),
            "退出码 3"
        );
        // Killed by a signal: no exit code at all, which is how a "kill -9" reads here.
        assert_eq!(
            exit_reason(&std::process::ExitStatus::from_raw(9)),
            "被信号 9 终止"
        );
    }
}
