//! dsh-desktop: a Tauri shell that supervises `dsh web` and hosts it in the system WebView.

pub mod harness;
pub mod locator;
pub mod process;
pub mod shellenv;
pub mod update;
pub mod window;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
}

fn default_port() -> u16 {
    3080
}

fn default_workspace() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
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
                Ok(config) => return config,
                // Never rewrite a file the user owns: fall back for this run and say why.
                Err(error) => harness::app_log(&format!(
                    "config.json 无法解析，本次使用默认配置且不覆盖该文件: {error}"
                )),
            }
        }
        let config = Config {
            port: default_port(),
            workspace: default_workspace(),
            dsh_home: None,
            take_over_existing: true,
            auto_update: true,
            update_tags: default_update_tags(),
            update_check_interval_minutes: default_update_interval(),
            import_shell_env: true,
            env: BTreeMap::new(),
        };
        let _ = std::fs::create_dir_all(data_dir);
        if !path.exists() {
            let _ = std::fs::write(
                &path,
                serde_json::to_vec_pretty(&config).unwrap_or_default(),
            );
        }
        config
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

    // 1) Self-heal: an instance left behind by a crashed shell is terminated first.
    if let Some(state) = process::read_state(data_dir) {
        if process::is_alive(state.pid) {
            window::set_status(
                app,
                "正在清理上次残留的 Harness…",
                &format!("pid {}", state.pid),
            );
            process::terminate(state.pid, TERMINATE_GRACE);
        }
        process::clear_state(data_dir);
    }

    let config = Config::load(data_dir);
    let port = config.port;

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
    // Search order: DSH_DESKTOP_DSH → PATH → common prefixes → login shell.
    let location = locator::locate(None, std::env::var("DSH_DESKTOP_DSH").ok())?;
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
                match checked.status {
                    update::Status::UpdateAvailable { from, to } => {
                        window::set_status(
                            app,
                            &format!("发现新版本 v{to}，正在更新…"),
                            &format!("v{from} -> v{to}"),
                        );
                        harness::app_log(&format!("update available: {from} -> {to}, installing"));
                        let prefix = update::install_prefix(&location.dsh_js);
                        match update::install(&npm, update::PACKAGE, &to, prefix.as_deref()) {
                            Ok(()) => {
                                just_updated = true;
                                version = locator::version(&location).unwrap_or_else(|| to.clone());
                                harness::app_log(&format!("dsh updated: {from} -> {to}"));
                            }
                            Err(reason) => harness::app_log(&format!(
                                "update failed, keeping v{from}: {reason}"
                            )),
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

    // 3c) Detection. Runs after the update so a freshly installed CLI is what we boot. The startup URL carries a per-process token that no other process can
    //     recover, so a foreign instance can never hand us a session.
    match harness::probe(port) {
        harness::Probe::HarnessWithSession | harness::Probe::HarnessNoSession => {
            let owner = harness::listener_pid(port);
            let ours = process::read_state(data_dir)
                .map(|state| state.pid)
                .filter(|pid| Some(*pid) == owner && process::is_alive(*pid));

            if let Some(pid) = ours.filter(|_| !just_updated) {
                // Our own instance from an earlier run: its cookie is still valid for this
                // authority (verified across restarts), so reuse it as-is. A fresh update
                // instead restarts it, otherwise the old binary would keep serving.
                let url = url::Url::parse(&format!("http://127.0.0.1:{port}/"))
                    .map_err(|e| e.to_string())?;
                window::set_status(app, "复用本应用上次启动的 Harness…", &format!("pid {pid}"));
                // Adopted, but still ours: quitting must stop it rather than leave an orphan.
                adopt(pid, data_dir);
                return window::create_harness(app, &url, port).map_err(|e| e.to_string());
            }

            if !config.take_over_existing {
                window::open_external(&format!("http://127.0.0.1:{port}/"));
                return Err(format!(
                    "127.0.0.1:{port} 已被另一个 Harness 占用（不是本应用启动的），已改用系统浏览器打开。                     如要让本应用接管，请保持 take_over_existing=true 或先停止该实例。"
                ));
            }

            // Foreign instance: stop it (the auth fence already proved it is a Harness),
            // then start our own so the window receives a fresh authenticated URL.
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
        &format!("dsh {version} · 端口 {port}"),
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

    let url = spawned.wait_for_url(timeout).map_err(|reason| {
        let tail = spawned.ring.tail();
        format!("{reason}\n\n最近输出:\n{tail}")
    })?;

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

    window::create_harness(app, &url, actual_port).map_err(|e| e.to_string())
}

fn fail(app: &AppHandle, status: &str, detail: &str) {
    window::set_status(app, status, detail);
}

#[cfg(test)]
mod tests {
    use super::*;

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
