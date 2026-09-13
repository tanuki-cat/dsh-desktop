//! dsh-desktop: a Tauri shell that supervises `dsh web` and hosts it in the system WebView.

// The Windows code paths in this tree are reference material only: `process.rs` cannot tell a
// live process from a dead one there, and the Makefile refuses to build on anything but macOS
// and Linux. Refuse the build instead of producing a bundle that looks supported but
// misbehaves; the ported, machine-tested implementation lives on feat/bundled-runtime.
#[cfg(windows)]
compile_error!(
    "main 分支不构建 Windows 产物：见 README 的 Windows 免安装版说明（feat/bundled-runtime 分支）"
);

pub mod harness;
pub mod locator;
pub mod process;
pub mod shellenv;
pub mod update;
pub mod window;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, RunEvent};

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
}

fn default_port() -> u16 {
    3080
}

fn default_workspace() -> PathBuf {
    home_workspace(std::env::var("HOME").ok())
}

/// Workspace for a config that does not name one.
///
/// A missing `HOME` used to fall back to `/`, which is worse than failing: the agent runs its
/// `glob` and `grep` from the workspace root, so it would walk the whole filesystem. The
/// temporary directory is still local and private, and the log says why `$HOME` was not used.
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
    /// A workspace becomes the child's working directory, so a workspace that is missing or
    /// relative fails inside the spawn with a bare ENOENT and no mention of which config entry
    /// caused it. The value is repaired in memory rather than rewritten, the same principle as
    /// an unparsable config.json: this run adapts, the user's file stays as they left it.
    fn repair(&mut self) {
        if !(self.workspace.is_absolute() && self.workspace.is_dir()) {
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
/// shares its group with whatever terminal or script launched it, and a group signal would
/// take unrelated processes down with it (design §13.1).
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
/// flags this shell passes (`--profile web`, a `dsh` entry point), and the parent is gone — a
/// leftover of a crashed shell is reparented to init, while a session someone started in a
/// terminal keeps its shell as parent.
fn looks_like_our_orphan(pid: u32) -> bool {
    let output = match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "ppid=,command="])
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
    looks_like_our_harness(&String::from_utf8_lossy(&output.stdout))
}

/// `ps -p <pid> -o ppid=,command=` output -> is this a reparented `dsh web`?
fn looks_like_our_harness(output: &str) -> bool {
    let Some(line) = output.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let Some((ppid, command)) = line.trim().split_once(char::is_whitespace) else {
        return false;
    };
    ppid.trim() == "1" && command.contains("--profile web") && command.contains("dsh")
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
    let env_capture = config.import_shell_env.then(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        std::thread::spawn(move || {
            let imported = shellenv::import(Path::new(&shell));
            (shell, imported)
        })
    });

    // 3) Locate dsh + node (a GUI-launched app has no Homebrew PATH).
    window::set_status(app, "正在定位 dsh 与 node…", "");
    // Search order: DSH_DESKTOP_DSH → dsh_path (config.json) → PATH → common prefixes → login shell.
    let location = locator::locate(
        config.dsh_path.clone(),
        std::env::var("DSH_DESKTOP_DSH").ok(),
    )?;
    let mut version = locator::version(&location).unwrap_or_else(|| "未知".into());

    // 3b) Update the supervised CLI before booting it, so "core upgrade" needs no terminal.
    let mut just_updated = false;
    if config.auto_update && !version.is_empty() {
        match update::npm_for(&location.node) {
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
                // again inside the window: npm wrote the package somewhere this shell does not
                // run it from, so every launch would stop the Harness and rebuild the tree.
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
                        harness::app_log(&format!("update available: {from} -> {to}, installing"));
                        // npm rewrites the CLI tree in place. Anything serving from it must be
                        // stopped first, or the live session breaks on its next lazy require().
                        // A deferred update only skips the install: startup continues and the
                        // instance keeps running (its tree was never touched).
                        match stop_instance_before_update(app, data_dir, port, &config) {
                            Err(reason) => harness::app_log(&format!(
                                "update deferred, keeping v{from}: {reason}"
                            )),
                            Ok(()) => {
                                let prefix = update::install_prefix(&location.dsh_js);
                                match update::install(&npm, update::PACKAGE, &to, prefix.as_deref())
                                {
                                    Ok(()) => {
                                        version = locator::version(&location)
                                            .unwrap_or_else(|| to.clone());
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
                    let url = url::Url::parse(&format!("http://127.0.0.1:{port}/"))
                        .map_err(|e| e.to_string())?;
                    window::set_status(app, "复用本应用上次启动的 Harness…", &format!("pid {pid}"));
                    // Adopted, but still ours: quitting must stop it rather than leave an orphan.
                    adopt(pid, data_dir);
                    return window::create_harness(app, &url, port).map_err(|e| e.to_string());
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

    // 3d) A GUI-launched app inherits launchd's environment, not the login shell's, so the
    //     supervised CLI would miss DEEPSEEK_API_KEY and friends. Import them here.
    let mut child_env: Vec<(String, String)> = Vec::new();
    let mut shell_path: Option<String> = None;
    if let Some(handle) = env_capture {
        let captured = handle.join().ok();
        let shell = captured
            .as_ref()
            .map(|(shell, _)| shell.clone())
            .unwrap_or_else(|| "登录 shell".to_string());
        match captured.and_then(|(_, imported)| imported) {
            Some((flag, imported)) => {
                let names: Vec<String> = imported.keys().cloned().collect();
                // Names only: values may be credentials.
                harness::app_log(&format!(
                    "imported {} env vars via {shell} {flag}: {}",
                    names.len(),
                    names.join(", ")
                ));
                shell_path = imported.get("PATH").cloned();
                child_env.extend(imported.into_iter().filter(|(key, _)| key != "PATH"));
            }
            None => harness::app_log(&format!(
                "login shell env import failed ({shell}); using the app environment"
            )),
        }
    }

    let mut prefix: Vec<String> = Vec::new();
    if let Some(dir) = location.node.parent() {
        prefix.push(dir.to_string_lossy().to_string());
    }
    prefix.push("/opt/homebrew/bin".to_string());
    prefix.push("/usr/local/bin".to_string());
    let merged = shellenv::merge_path(
        &prefix,
        shell_path.as_deref(),
        std::env::var("PATH").ok().as_deref(),
    );
    child_env.retain(|(key, _)| key != "PATH");
    child_env.push(("PATH".to_string(), merged));
    for (key, value) in &config.env {
        child_env.retain(|(existing, _)| existing != key);
        child_env.push((key.clone(), value.clone()));
    }
    if let Some((_, path)) = child_env.iter().find(|(key, _)| key == "PATH") {
        harness::app_log(&format!("child PATH = {path}"));
    }
    let options = harness::SpawnOptions {
        workspace: &config.workspace,
        overlay: &overlay,
        dsh_home: config.dsh_home.as_deref(),
        port,
        log_path: &log_path,
        env: &child_env,
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
    let spawned = harness::spawn(&location, &options).map_err(|e| format!("启动进程失败: {e}"))?;
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
    let home = config
        .dsh_home
        .clone()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".dsh"))
        })
        .unwrap_or_else(|| PathBuf::from(".dsh"));
    let timeout = if home.join("profiles").join("web").exists() {
        STARTUP_TIMEOUT_NEXT
    } else {
        STARTUP_TIMEOUT_FIRST
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
    fn only_a_reparented_dsh_web_looks_like_our_orphan() {
        // A leftover of a crashed shell: reparented to init and running the flags we pass.
        assert!(looks_like_our_harness(
            "    1 /opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --patch /x --no-open --port 3080"
        ));
        // The same command with a live parent is somebody else's session.
        assert!(!looks_like_our_harness(
            " 8831 /opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web"
        ));
        // Reparented, but a different program: the recorded pid was reused.
        assert!(!looks_like_our_harness("    1 /usr/sbin/cupsd -l"));
        // A dsh that is not the web profile this shell supervises.
        assert!(!looks_like_our_harness(
            "    1 node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --version"
        ));
        // No such process: `ps` prints nothing.
        assert!(!looks_like_our_harness(""));
        assert!(!looks_like_our_harness("\n"));
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
}
