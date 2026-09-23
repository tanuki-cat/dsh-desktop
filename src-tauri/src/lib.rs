//! dsh-desktop: a Tauri shell that supervises `dsh web` and hosts it in the system WebView.

// Windows is a supported target since `feat/bundled-runtime` merged in (2026-09-13):
// `process.rs` probes liveness with `OpenProcess` + `GetExitCodeProcess`, and
// `.github/workflows/windows-portable.yml` stages a bundled runtime on a Windows runner.

pub mod harness;
pub mod identity;
pub mod locator;
pub mod process;
pub mod runtime;
pub mod shellenv;
pub mod takeover;
pub mod transaction;
pub mod update;
pub mod update_flow;
pub mod window;

use identity::{looks_like_our_orphan, self_heal_action, SelfHeal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use takeover::{
    confirm_takeover, foreign_instance_action, harness_listener, identified_dsh_web,
    resolve_foreign_action, terminal_page, wait_for_handoff, wait_for_port_free, ForeignAction,
    Retry, TerminalPage,
};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::{AppHandle, Listener, Manager, RunEvent};
use update_flow::{
    clear_stale_staging, commit_core_update, confirm_core_update, recover_pending_swap,
    roll_back_core_update, stage_core_update, update_market_plugin, StagedUpdate, UpdatePaths,
};

/// First run may initialise a profile; later runs are fast (measured ~4s on macOS).
const STARTUP_TIMEOUT_FIRST: Duration = Duration::from_secs(90);
const STARTUP_TIMEOUT_NEXT: Duration = Duration::from_secs(30);
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// The dsh profile this shell supervises. Named here rather than inline because the plugin skip
/// message has to quote the same name it acts on.
const PROFILE_NAME: &str = "web";

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
/// How often an adopted instance is checked for liveness. It has no `Child` to wait on, so
/// this is the only signal; a few seconds of delay before recovery is imperceptible next to
/// the restart that follows.
const REUSED_POLL: Duration = Duration::from_secs(3);

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
    /// Dist-tags consulted in auto mode; the highest version among them wins.
    ///
    /// Defaults to `latest` plus `alpha` — upstream publishes ahead of `latest`, so following
    /// only the release tag means never seeing the newest build. A tag the registry does not
    /// publish is ignored while another one matches, which is why this list is also usable for the
    /// plugin market (`dshmarket` publishes no `alpha`). Narrow it to `["latest"]` to stay on
    /// release versions only.
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
    /// Stand in for animation frames while WebKit is not painting the window (see
    /// `window::RENDER_FALLBACK_SCRIPT`). On by default: without it a window that is merely
    /// occluded stops showing streamed output until the user uncovers it. Off restores the older
    /// behaviour, for comparing against it or for a page the wrapper disagrees with.
    #[serde(default = "default_render_fallback")]
    pub render_fallback: bool,
    /// Carry what the Harness page reports about itself — uncaught errors, rejected promises and
    /// the two console levels that carry failures — back into this shell's log (see
    /// `window::PAGE_DIAGNOSTICS_SCRIPT`). On by default: the window has no console, and every
    /// WebView-only fault in this repo's history has otherwise cost a round trip asking the user
    /// to reproduce it under an attached inspector. It only observes; off removes the script.
    #[serde(default = "default_page_diagnostics")]
    pub page_diagnostics: bool,
    /// Hold the focus still while a menu popup is clicked, which WebKit does not do on its own
    /// (see `window::MENU_FOCUS_GUARD_SCRIPT`). On by default: without it the dsh model and
    /// reasoning pickers close without selecting anything, because the popup unmounts before the
    /// `click` reaches it. This is the one shim here that changes page *behaviour* rather than
    /// filling in a missing API, so it has its own switch — off restores the engine's own.
    #[serde(default = "default_keep_menu_focus")]
    pub keep_menu_focus: bool,
    /// Stand in for the scroll anchoring WebKit never implemented (see
    /// `window::SCROLL_ANCHOR_SCRIPT`). On by default: without it a conversation whose content
    /// above the reader changes height — expanding a reasoning block in dsh 0.1.7 does exactly
    /// that — throws the viewport by that height on WebKit, which is the reported flicker, while
    /// Chromium and Gecko compensate natively and never show it. Off restores the engine's own
    /// behaviour, for comparing against it or for a page the shim disagrees with.
    #[serde(default = "default_scroll_anchor")]
    pub scroll_anchor: bool,
}

fn default_scroll_anchor() -> bool {
    true
}

fn default_render_fallback() -> bool {
    true
}

fn default_page_diagnostics() -> bool {
    true
}

fn default_keep_menu_focus() -> bool {
    true
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

/// The tags the config falls back to. Defined in [`update`] beside the code that consumes them,
/// so the shipped default and the live registry test cannot drift apart.
fn default_update_tags() -> Vec<String> {
    update::default_tags()
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
            render_fallback: default_render_fallback(),
            page_diagnostics: default_page_diagnostics(),
            keep_menu_focus: default_keep_menu_focus(),
            scroll_anchor: default_scroll_anchor(),
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

/// The app menu: the default one plus the reload the shell cannot offer any other way.
///
/// Tauri builds the standard macOS menu (App / File / Edit / View / Window / Help), and its View
/// menu holds only fullscreen — there is no reload item, and the page cannot offer one either:
/// it is remote content with no capability. Without this the only recovery from a page that
/// stopped drawing is quitting the app and waiting for the Harness to come back, which is exactly
/// the report this fixes (2026-09-18). Reloading keeps the session: it is the same navigation the
/// drawing watchdog performs, and the conversation lives in the Harness process.
fn window_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::default(app)?;
    let reload = MenuItem::with_id(
        app,
        window::RELOAD_MENU_ID,
        "重新加载界面",
        true,
        Some("CmdOrCtrl+R"),
    )?;
    // Into the View menu rather than a new one: that is where a reload belongs on macOS, and the
    // submenu is found by its text because the id of the default View submenu is not exported.
    let target = menu
        .items()?
        .into_iter()
        .filter_map(|item| item.as_submenu().cloned())
        .find(|submenu| submenu.text().map(|text| text == "View").unwrap_or(false));
    match target {
        Some(view) => {
            view.prepend(&reload)?;
            view.prepend(&PredefinedMenuItem::separator(app)?)?;
        }
        // A menu this shell cannot find is a reason to still offer the reload, not to drop it:
        // a top-level item is worse placement than the View menu and far better than nothing.
        None => {
            harness::app_log("默认菜单里没有 View 子菜单，重新加载项放到顶层");
            menu.append(&reload)?;
        }
    }
    Ok(menu)
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
        .menu(window_menu)
        .on_menu_event(|app, event| {
            if event.id() == window::RELOAD_MENU_ID {
                window::reload_harness(app);
            }
        })
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
///
/// `shell_path` is the login shell's `PATH`, when it was imported: the user's own installation is
/// looked up there as well as on the app's PATH (see `locator::system_node`).
fn resolve_runtime(
    resources: Option<&Path>,
    data_dir: &Path,
    config: &Config,
    shell_path: Option<&str>,
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
    let system_node = probe_system
        .then(|| locator::system_node(shell_path))
        .flatten();
    let system_dsh = probe_system
        .then(|| locator::system_dsh(config.dsh_path.clone(), shell_path))
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
    let profile = home
        .as_ref()
        .map(|home| home.join("profiles").join(PROFILE_NAME));
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

/// Seed a profile template into place, preserving its symlinks.
///
/// The template is a pnpm tree, so it carries the same links a profile does (`.bin/*` into
/// `node_modules/`, and every dependency under `.dsh-module-fallback/`). `is_dir` is false for a
/// link, which sent directory links into `fs::copy` and failed the whole seed.
fn copy_tree_into(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        transaction::copy_entry(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

/// Whether the instance currently owning `port` is the one recorded in our state file.
///
/// The same test `stop_instance_before_update` uses to tell "our own Harness" from "somebody
/// else's": only our own is safe to swap a tree under without asking.
fn ours_on_port(data_dir: &Path, port: u16) -> bool {
    let owner = harness::listener_pid(port);
    process::read_state(data_dir)
        .map(|state| state.pid)
        .is_some_and(|pid| Some(pid) == owner && process::is_alive(pid))
}

/// Whether an instance left running may be serving from the tree a swap would replace.
///
/// Pure, so the matrix is testable. `owned` is whether `target` is this shell's own shadow prefix;
/// `command` is the running instance's command line, when it could be read.
///
/// - No tree at `target` yet (a bundled build's first update): nothing can be running from it.
/// - The user's own prefix: an instance they kept is most likely their terminal `dsh`, started
///   through a launcher symlink whose path does not name the tree — so it is assumed to use it.
/// - The shadow prefix: only a plugin-market handoff replays this shell's own command line, which
///   names the tree's real path; an unreadable command line is assumed to.
fn swap_conflicts(target: &Path, target_exists: bool, owned: bool, command: Option<&str>) -> bool {
    if !target_exists {
        return false;
    }
    if !owned {
        return true;
    }
    match command {
        Some(command) => command.contains(&*target.to_string_lossy()),
        None => true,
    }
}

/// Whether the verified tree must stay staged because somebody is still serving from it.
///
/// Pure, so the matrix is testable. Three independent reasons, any one of which is enough:
///
/// - `kept_conflict` — the user asked to keep the instance on the original port and it may be
///   running this very tree (see [`swap_conflicts`]);
/// - `raced` — a Harness appeared on the *spawn* port after detection, so the new tree cannot
///   boot there anyway;
/// - `raced_original` — a Harness appeared on the *original* port while this launch was moved
///   elsewhere. Nothing else covers this one: the instance is not in `state.json` (the
///   watchdog restarted it while an update was staging, so the reuse branch does not adopt it
///   and the `Some(pid)` branch does not stop it), yet it serves from the tree the swap would
///   rename away. Committing anyway leaves the app rolling a broken tree back and restarting
///   onto it — a failure page it cannot leave (see the v0.4.4 review, item A1).
fn swap_blocked(kept_conflict: bool, raced: bool, raced_original: bool) -> bool {
    kept_conflict || raced || raced_original
}

/// Forget a supervised pid without signalling it (used when the process is already gone).
fn disown(pid: u32) {
    let mut guard = LIVE.lock().unwrap();
    if guard.as_ref().is_some_and(|live| live.pid == pid) {
        *guard = None;
    }
}

/// Why a launch attempt did not end with a Harness window.
#[derive(Debug)]
struct StartError {
    /// What the user reads on the failure page.
    reason: String,
    /// The user answered this attempt's takeover question with "no". A retry must report that,
    /// not put the same question again — the answer is already given.
    declined: bool,
}

impl From<String> for StartError {
    fn from(reason: String) -> Self {
        StartError {
            reason,
            declined: false,
        }
    }
}

/// Whether a failed restart may still ask about the instance on `port`.
///
/// Not when the attempt already asked and the answer was "no" (`declined`), and not when the
/// answer was "keep it, use another port" — which shows as the launch port having moved away
/// from `port` (`port_now`). Either answer is final for this restart.
fn may_ask_after_failure(error: &StartError, port: u16, port_now: u16) -> bool {
    !error.declined && port_now == port
}

/// A launch that failed after spawning must not leave a half-started Harness behind: it
/// would keep the port and make the failure page a lie.
fn abort_start(pid: u32, reason: String) -> Result<(), StartError> {
    process::terminate(pid, TERMINATE_GRACE);
    disown(pid);
    Err(reason.into())
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
        // The user already answered for the instance on this port during the attempt — "no", or
        // "keep it and use another port". Asking again is the same question twice for one restart.
        Err(error) if !may_ask_after_failure(&error, port, runtime_port(config.port)) => {
            harness::app_log(&format!(
                "本次启动失败（{}），用户已选择保留端口 {port} 上的实例：不再追问",
                error.reason
            ));
            Err(error.reason)
        }
        // A handoff slower than the grace can still take the port while our own launch boots.
        // Reporting a failure there would be wrong — the port is serving a Harness — so ask about
        // that instance and make the one remaining attempt if the answer allows it. The guard is
        // the identity, not the config: whether to signal is the user's answer, and the config
        // only decides what an unanswered question means.
        Err(StartError { reason, .. }) => match harness_listener(port) {
            // Unidentified: report the original failure rather than killing a process this
            // shell cannot prove is the CLI.
            Some(pid) if !identified_dsh_web(pid) => {
                harness::app_log(&format!(
                    "端口 {port} 由 pid {pid} 服务，但无法确认它是 dsh web：不接管"
                ));
                Err(reason)
            }
            // A retry is still this shell deciding to end somebody else instance, so it is
            // the same question. It is only reached when this attempt did not ask it already
            // (the arms above), so the user sees it at most once per restart.
            Some(pid) => match confirm_takeover(app, &config, port, pid) {
                Retry::TakeOver => {
                    harness::app_log(&format!(
                        "本次启动失败（{reason}），但端口 {port} 已由 pid {pid} 服务：接管后重试一次"
                    ));
                    window::set_status(app, "正在接管重新启动的 Harness…", &format!("pid {pid}"));
                    process::terminate_pid(pid, TERMINATE_GRACE);
                    let _ = wait_for_port_free(port, TERMINATE_GRACE);
                    start(app, data_dir).map_err(|error| error.reason)
                }
                Retry::Declined => {
                    harness::app_log(&format!(
                        "本次启动失败（{reason}），端口 {port} 上的 pid {pid} 已按用户选择保留"
                    ));
                    Err(reason)
                }
                Retry::Refused => {
                    harness::app_log(&format!(
                        "本次启动失败（{reason}），端口 {port} 上的 pid {pid} 未被接管：保留该实例"
                    ));
                    Err(reason)
                }
            },
            // Nothing owns the port any more: report the original failure.
            None => Err(reason),
        },
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
    let clean = status.as_ref().is_ok_and(|status| status.code() == Some(0));
    let reason = status
        .as_ref()
        .map(exit_reason)
        .unwrap_or_else(|error| format!("无法等待进程结束: {error}"));
    recover_exited(app, data_dir, pid, ring, started, reason, clean);
}

/// Watch a Harness this shell adopted rather than spawned, by polling its pid.
///
/// The reuse branch hands the window straight to an instance an earlier launch left running, and
/// there is no `Child` to wait on — so without this the instance could die unnoticed: the page
/// keeps drawing its last frame, the liveness watchdog only checks that frames move, and the
/// window sits on a page that can never connect again. That is the failure the automatic
/// recovery exists to prevent, and it applied to every reused instance.
fn watch_reused(app: AppHandle, data_dir: PathBuf, pid: u32, ring: harness::Ring) {
    let started = Instant::now();
    loop {
        std::thread::sleep(REUSED_POLL);
        if EXITING.load(Ordering::SeqCst) {
            return;
        }
        // Gone, or replaced on the port by a different process: either way this launch is no
        // longer being served by what it adopted.
        if process::is_alive(pid) {
            continue;
        }
        // No exit status is available for a process that is not our child, so the exit cannot be
        // called clean: an unknown outcome must not earn the longer handoff wait.
        recover_exited(
            app,
            data_dir,
            pid,
            ring,
            started,
            "进程已不在（复用启动，拿不到退出码）".to_string(),
            false,
        );
        return;
    }
}

/// What to do about a Harness that is no longer running, however it was being watched.
fn recover_exited(
    app: AppHandle,
    data_dir: PathBuf,
    pid: u32,
    ring: harness::Ring,
    started: Instant,
    reason: String,
    clean: bool,
) {
    // Quitting stops the Harness on purpose, and the watchdog wakes up for that exit like any
    // other. It must not be read as a crash: that logged "exited unexpectedly" on every quit and
    // raced the exit with a restart and a freshly opened status window.
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    harness::app_log(&format!("Harness pid {pid} exited unexpectedly ({reason})"));
    disown(pid);
    process::clear_state(&data_dir);
    let output = ring.tail();

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
                "profile {} 里没有声明 {}：界面不会出现插件市场，也不会自动安装（用 `dsh plugin --profile {} add {}` 装上，或改用自带运行时版让首启播种模板）",
                PROFILE_NAME,
                update::MARKET_PLUGIN,
                PROFILE_NAME,
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
    if let Err(error) = start(&app, &data_dir) {
        fail(&app, "DeepSeek Harness 启动失败", &error.reason);
    }
}

/// Say on the Harness window what the splash could only say for a few seconds.
///
/// The update check runs before that window exists, so the sentence it draws lands on a splash
/// that is overwritten and destroyed moments later — a notice nobody can read. Called wherever the
/// Harness window becomes ready: the reuse branch returns early, and reusing an instance says
/// nothing about whether a newer version exists.
fn announce_pending_update(app: &AppHandle, pending: Option<&str>, core_swapped: bool) {
    match pending {
        Some(to) => window::announce_update(app, to),
        // A core swap installed the version a previous notice would have named. `just_updated` is
        // not the test for that: the plugin-market step sets it too, for a version that has
        // nothing to do with the CLI.
        None if core_swapped => window::clear_update_notice(app),
        None => {}
    }
}

fn start(app: &AppHandle, data_dir: &Path) -> Result<(), StartError> {
    harness::init_app_log(&data_dir.join("logs").join("harness.log"));

    // The config comes first: step 1 needs the port this run will use to tell a leftover
    // instance it may hand to the reuse branch from one it must stop.
    let config = Config::load(data_dir);
    // The port this run uses, which is the configured one unless an earlier takeover question
    // moved it (see [`PORT_OVERRIDE`]). Mutable because the question below can move it again.
    let mut port = runtime_port(config.port);
    // The port this launch was configured for, kept unchanged while `port` follows an answer
    // that moves the launch elsewhere. The swap in 3d needs it: a stale instance left on the
    // original port is exactly the one nothing else notices (see `swap_blocked`).
    let configured_port = port;
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
    // alongside the swap recovery below and is joined just before the runtime is resolved: the
    // PATH it carries is where that resolution looks for the user's own node and dsh.
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
    recover_pending_swap(&paths, data_dir);
    clear_stale_staging(&paths);

    // 3) Resolve the runtime: bundled seed / shadow prefix / the user's install (§2.3/§2.4).
    window::set_status(app, "正在解析运行时…", "");
    // The login shell's PATH is the one the user's terminal sees. Resolving with it is what finds
    // an nvm/fnm install: those tools initialise in `.zshrc`, which the `-lic` capture reads and a
    // bare `-lc` lookup does not — so the lookup used to miss a node the child PATH then had.
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
    let mut resolved = resolve_runtime(
        app.path().resource_dir().ok().as_deref(),
        data_dir,
        &config,
        imported.get("PATH").map(String::as_str),
    )?;
    let mut version = resolved.version.clone();

    // The profile directory decides the startup timeout, so look at it *before* seeding: a
    // freshly seeded profile made the very first launch take the 30 s "warm" timeout instead of
    // the 90 s budget meant for a first start (review P1-6).
    let home = config
        .dsh_home
        .clone()
        .or_else(|| home_dir().map(|home| home.join(".dsh")))
        .unwrap_or_else(|| PathBuf::from(".dsh"));
    let first_launch = !home.join("profiles").join(PROFILE_NAME).exists();

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

    // 3b2) The shell drives the CLI through `--profile web --patch … --no-open --port N` and reads
    //      its startup line. Versions outside the range we test against may change either, so say so
    //      now instead of failing later with a timeout that hides the real reason.
    //
    //      Placed before the update check on purpose: a version this shell refuses to boot must not
    //      be staged, and must not have the running instance stopped for it either. The staging
    //      step below builds a ~290 MB tree and (on the plugin path) stops a live Harness, all of
    //      which is wasted when the launch then refuses to start — and the stop is worse than
    //      wasted, because the refusal returns without ever starting a replacement (review C1).
    let compatibility = update::compatibility(&version);
    let untested = !matches!(compatibility, update::Compatibility::Tested);
    if untested {
        harness::app_log(&format!("warning: {}", compatibility.describe()));
        if config.require_tested_dsh {
            return Err(format!(
                "{}\n\n如需强行使用，请在 config.json 里设置 \"require_tested_dsh\": false。",
                compatibility.describe()
            )
            .into());
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
    // A version worth telling the user about that this launch will not install. Announced on the
    // Harness window once it exists: the splash that shows it now does not outlive startup.
    //
    // An auto-restart runs this function again in the same process, so the notice starts empty
    // each time and is dropped with the window the previous run announced it on.
    let mut pending_update_notice: Option<String> = None;
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
                    // The same suppression the plugin market gets: a version whose tree was
                    // swapped in and never booted must not be re-downloaded and re-swapped on
                    // every single launch.
                    update::Status::UpdateAvailable { to, .. } if checked.failed_recently => {
                        harness::app_log(&format!(
                            "update {to} failed to start last time; not retrying yet (the wait grows with each failure)"
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
                            // The splash above is destroyed moments from now, so the notice is
                            // carried on the Harness window instead (see PENDING_UPDATE).
                            pending_update_notice = Some(to.clone());
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
                            pending_update_notice = Some(to.clone());
                        } else {
                            harness::app_log(&format!(
                                "update available: {from} -> {to}, installing"
                            ));
                            // Staging happens BEFORE anything is stopped. It builds the new
                            // tree in `runtime/staging/…` and never touches the one in use, so
                            // the running session can keep serving while a 300-second npm
                            // install runs — and a registry that fails half-way costs the user
                            // nothing. The stop happens in step 3c, once a verified tree exists
                            // and the launch is actually committed to swapping it in.
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
                                Err(reason) => {
                                    // The startup thread waits on this install (up to its full
                                    // budget when a proxy stalls), so a failure is remembered
                                    // like a failed boot: the next launch does not wait again.
                                    update::mark_core_attempt_failed(data_dir, &to);
                                    harness::app_log(&format!(
                                        "update staged but not committed, keeping v{from}: {reason}"
                                    ))
                                }
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
    //      spawns share one toolchain. The login-shell variables were collected before the
    //      runtime was resolved (see step 3).
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
        let profile_dir = home.join("profiles").join(PROFILE_NAME);
        let declared = update::declares_plugin(&profile_dir, update::MARKET_PLUGIN);
        let installed = update::installed_plugin(&profile_dir, update::MARKET_PLUGIN);
        // Resolve pnpm before anything is stopped: it is what actually installs a plugin,
        // and without it the CLI exits 127 after the instance is already gone (review A1).
        let pnpm = update::find_pnpm(Some(OsStr::new(&child.path)));
        // Asked unconditionally, not only when the market is declared: "the profile does not
        // declare it" is exactly the case whose reason never reached the log, and the comment
        // on `plugin_skip_reason` promises that it does.
        // Reaching the `else` means every precondition held, so `installed` is present.
        if let Some(skip) = plugin_skip_reason(declared, installed.as_deref(), pnpm.as_deref()) {
            harness::app_log(&skip.reason());
        } else {
            if let Some(current) = installed {
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
                                    "plugin {} {to} failed to install last time; not retrying yet (the wait grows with each failure)",
                                    update::MARKET_PLUGIN
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

    // Whether the detection step signalled something and must therefore wait for the port to
    // come free before spawning.
    let mut took_over = false;
    // An instance the user chose to keep running (the "use another port" answer), with its pid
    // when known. The swap in 3d must not pull the tree out from under it.
    let mut kept_instance: Option<Option<u32>> = None;
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
                        let scripts = harness_scripts(&config);
                        let scripts: Vec<&str> = scripts.iter().map(String::as_str).collect();
                        window::create_harness(app, &url, port, &scripts)
                            .map_err(|e| e.to_string())?;
                        // This branch returns without reaching the announcement below.
                        announce_pending_update(
                            app,
                            pending_update_notice.as_deref(),
                            core_swapped,
                        );
                        // There is no `Child` for a reused instance, so liveness is polled:
                        // without it the window would stay on a page that can never connect
                        // again, which is exactly what the automatic recovery exists to avoid.
                        let handle = app.clone();
                        let watched_dir = data_dir.to_path_buf();
                        // Nothing was spawned here, so there is no output of our own to show;
                        // the failure page says what is known instead of quoting an empty log.
                        let ring = harness::Ring::new();
                        std::thread::spawn(move || watch_reused(handle, watched_dir, pid, ring));
                        return Ok(());
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
                            kept_instance = Some(owner);
                            // The spawn below reads `port`, so the rest of this attempt runs
                            // against the new one.
                            port = free;
                        }
                        ForeignAction::Refuse { reason, declined } => {
                            return Err(StartError { reason, declined })
                        }
                        // The startup path answers the question through
                        // `resolve_foreign_action`, so reaching this arm means a caller
                        // forgot to; refuse rather than signal silently.
                        ForeignAction::Ask { pid } => {
                            return Err(format!(
                                "端口 {port} 上的 Harness（pid {pid}）没有得到处理，已放弃本次启动。"
                            )
                            .into());
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
                        return Err(format!("接管失败：端口 {port} 仍被占用。").into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        harness::Probe::Other => {
            return Err(
                format!("端口 {port} 被其它程序占用，请在 config.json 里换一个端口。").into(),
            );
        }
        harness::Probe::Closed => {}
    }

    // 3d) The launch is now committed to booting a Harness: put the verified tree in place.
    //     Everything above could still have returned without starting one, and a swap made for
    //     a launch that never happens would leave a rollback record for the next launch to
    //     undo. From here on, a failure to boot is exactly what the record is for.
    if let Some(staged) = staged_update.take() {
        // Two ways a Harness this shell does not control can still be running when the swap is
        // due, and in both the staged tree is dropped while the launch itself carries on.
        //
        // An instance the user chose to keep ("use another port") may serve from exactly the tree
        // the swap would replace: renaming it out from under a live process fails outright on
        // Windows and breaks it on its next lazy `require()` on Unix. Its port is no longer
        // `port` — the answer moved this launch elsewhere — so it is judged by what it runs,
        // not by probing.
        let kept_conflict = kept_instance.is_some_and(|pid| {
            swap_conflicts(
                staged.target(),
                staged.target().exists(),
                resolved.updates == runtime::Updates::Shadow,
                pid.and_then(harness::process_command).as_deref(),
            )
        });
        // A Harness that appeared on the spawn port after detection (a late handoff): the launch
        // cannot boot the new tree on that port anyway, and the spawn below reports it.
        let raced = !took_over
            && harness::probe(port) == harness::Probe::Harness
            && !ours_on_port(data_dir, port);
        // The port this launch detected the instance on, before any answer moved it. A stale
        // instance there is the one case detection cannot see: it is not in state.json (the
        // watchdog restarted it while an update was staging), so the reuse branch does not adopt
        // it and the `Some(pid)` branch does not stop it either — while it keeps serving from the
        // very tree this swap is about to rename out from under it.
        let raced_original = port != configured_port
            && harness::probe(configured_port) == harness::Probe::Harness
            && !ours_on_port(data_dir, configured_port);
        if swap_blocked(kept_conflict, raced, raced_original) {
            harness::app_log(&format!(
                "{}：放弃本次切换（已暂存的 v{} 作废，下次启动再更新）",
                if kept_conflict {
                    "被保留的外部实例可能正在使用待替换的 CLI 树"
                } else if raced {
                    "检测之后端口上又出现了外部 Harness"
                } else {
                    "原端口上仍有实例在服务，它可能正在使用待替换的 CLI 树"
                },
                staged.version
            ));
            drop(staged);
        } else {
            let from = version.clone();
            let staged_version = staged.version.clone();
            match commit_core_update(staged, &paths) {
                Ok((installed_path, installed_version)) => {
                    version = installed_version;
                    resolved.dsh_js = installed_path;
                    core_swapped = true;
                    harness::app_log(&format!("dsh updated: {from} -> {to}", to = version));
                }
                Err(reason) => {
                    update::mark_core_attempt_failed(data_dir, &staged_version);
                    harness::app_log(&format!("update failed, keeping v{from}: {reason}"))
                }
            }
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
    // A tree that was just swapped in is booting for the first time too: a new CLI version may
    // migrate the profile or rebuild caches on its first start, and timing that out on the warm
    // budget rolls back an update that was only slow.
    let timeout = if first_launch || core_swapped {
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
            // The cache still names this version as the newest, so without a marker every
            // later launch would stage and swap the same broken tree again.
            // `version` was updated to the swapped one at the commit above, so it names the
            // tree that failed rather than the one that was running before.
            if core_swapped {
                update::mark_core_attempt_failed(data_dir, &version);
            }
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
    let scripts = harness_scripts(&config);
    // Whether the compat half actually made it in. The render fallback is always there when it is
    // enabled, so it cannot answer this question.
    let compat = config
        .webkit_compat
        .then(window::needed_compat_script)
        .flatten()
        .is_some();
    if let Some(report) = window::report().filter(|report| report.needs_compat()) {
        // The reason has to name the real one. `compat` is decided before the spawn, and the
        // report arrives while the CLI boots, so a missing script can also mean the probe was
        // not in yet — blaming the config for that sent a reader looking at a setting that was
        // never the problem (found by running the built app, 2026-09-15).
        harness::app_log(&format!(
            "WebView 缺少 {}：{}",
            report.missing.join("、"),
            if compat {
                "已注入兼容层"
            } else if config.webkit_compat {
                "探测未在上报窗口内到达，未注入兼容层"
            } else {
                "webkit_compat=false，未注入兼容层"
            }
        ));
    }
    let scripts: Vec<&str> = scripts.iter().map(String::as_str).collect();
    if let Err(error) = window::create_harness(app, &url, actual_port, &scripts) {
        return abort_start(pid, error.to_string());
    }
    announce_pending_update(app, pending_update_notice.as_deref(), core_swapped);

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

/// Every script the Harness window carries, in injection order.
///
/// The compat layer comes first: it supplies engine capabilities, and the render fallback wraps
/// `requestAnimationFrame`, so it has to see whatever that layer installed. Both are shell-owned
/// scripts injected before the document is parsed, which is why the remote page gains no
/// capability from either.
fn harness_scripts(config: &Config) -> Vec<String> {
    let mut scripts = Vec::new();
    if config.webkit_compat {
        if let Some(compat) = window::needed_compat_script() {
            scripts.push(compat);
        }
    }
    if config.render_fallback {
        scripts.push(window::render_fallback_script());
    }
    if config.keep_menu_focus {
        scripts.push(window::menu_focus_guard_script());
    }
    // The anchor shim observes layout in the same conversation the guard's presses land in, and it
    // reads only geometry; it is independent of the two above and of the diagnostics below.
    if config.scroll_anchor {
        scripts.push(window::scroll_anchor_script());
    }
    // Last, so it observes a page the others have already finished setting up — and so a fault
    // in the diagnostics never precedes the layer it would be reporting about.
    if config.page_diagnostics {
        scripts.push(window::page_diagnostics_script());
    }
    scripts
}

/// A per-process temporary directory for tests.
///
/// The name carries the process id: a second `cargo test` on the same machine — another checkout,
/// or two CI jobs sharing a runner — would otherwise reuse `temp_dir()/dsh-desktop-<name>` and
/// delete the other run files mid-test (review D7). Within one binary the names stay distinct,
/// which is what keeps parallel `#[test]`s apart.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
}

#[cfg(test)]
mod tests;
