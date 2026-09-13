//! dsh-desktop: a Tauri shell that supervises `dsh web` and hosts it in the system WebView.

pub mod harness;
pub mod locator;
pub mod process;
pub mod runtime;
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
}

fn default_port() -> u16 {
    3080
}

fn default_workspace() -> PathBuf {
    home_dir().unwrap_or_else(std::env::temp_dir)
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
            require_tested_dsh: false,
            runtime: runtime::Preference::Auto,
        };
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
    /// A workspace becomes the child's working directory, so a drive-relative or POSIX-style
    /// path (`D:`, `/c/Users/me` written into config.json by an earlier build) makes node resolve
    /// its own main module against the wrong root and fail with `EISDIR: lstat 'D:'`. We repair
    /// the value in memory instead of rewriting the user's file, and log what we ignored.
    fn repair(&mut self) {
        if usable_directory(&self.workspace) {
            self.workspace = absolute(&self.workspace);
        } else {
            let fallback = default_workspace();
            harness::app_log(&format!(
                "config.json 中的 workspace 不可用，已改用 {}: {}",
                fallback.display(),
                self.workspace.display()
            ));
            self.workspace = fallback;
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
    window::set_status(
        app,
        "更新前先停止正在运行的 Harness…",
        &format!("pid {pid}"),
    );
    process::terminate(pid, TERMINATE_GRACE);
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
        "stopped Harness pid {pid} before updating the CLI"
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
}

impl ResolvedRuntime {
    fn bundled(&self) -> bool {
        !matches!(self.updates, runtime::Updates::Notify)
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

    // The user's own installation, gated: an x64 node under Rosetta cannot load the bundled
    // arm64 prebuilds, and a node without stripTypeScriptTypes cannot run the CLI (§2.4).
    let system = locator::locate(None, None).ok();
    let facts = system
        .as_ref()
        .and_then(|loc| locator::probe_node(&loc.node));
    let host_arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    let accepted = facts
        .as_ref()
        .filter(|facts| facts.arch == host_arch && facts.strip_types);
    if let (Some(facts), None) = (facts.as_ref(), accepted) {
        harness::app_log(&format!(
            "system node rejected (arch {}, stripTypeScriptTypes {}); using the bundled runtime",
            facts.arch, facts.strip_types
        ));
    }
    let system_node = accepted.and(system.as_ref().map(|loc| loc.node.clone()));
    let system_dsh = accepted.and(system.as_ref().map(|loc| loc.dsh_js.clone()));

    let decision = runtime::decide(&runtime::Inputs {
        preference: std::env::var("DSH_DESKTOP_RUNTIME_PREFERENCE")
            .ok()
            .and_then(|value| runtime::Preference::parse(&value))
            .unwrap_or(config.runtime),
        env_node: std::env::var("DSH_DESKTOP_NODE")
            .ok()
            .map(|value| absolute(Path::new(&value)))
            .filter(|path| path.is_file()),
        env_dsh: std::env::var("DSH_DESKTOP_DSH")
            .ok()
            .map(|value| absolute(Path::new(&value)))
            .filter(|path| path.is_file()),
        system_node,
        system_dsh,
        seed_node: seed_node.as_deref(),
        seed_dsh: seed_dsh.as_deref(),
        shadow_dsh: shadow_dsh.as_deref(),
        seed_version: seed_version.as_deref(),
        shadow_version: shadow_version.as_deref(),
    })
    .ok_or_else(|| {
        "找不到 dsh，也没有自带运行时。请安装 node 与 @deepseek-ai/dsh，或用 DSH_DESKTOP_DSH / DSH_DESKTOP_NODE 指定路径。"
            .to_string()
    })?;

    let describe = format!("{} | updates: {:?}", decision.describe(), decision.updates);
    harness::app_log(&describe);
    let version = locator::version_of(&decision.dsh.path).unwrap_or_else(|| "未知".into());
    Ok(ResolvedRuntime {
        node: absolute(&decision.node.path),
        dsh_js: absolute(&decision.dsh.path),
        version,
        updates: decision.updates,
        seed,
    })
}

/// The login-shell capture: the shell that was run, and the variables it printed.
type EnvCapture = std::thread::JoinHandle<(String, Option<(String, BTreeMap<String, String>)>)>;

/// First launch of a bundled build: install the profile template (which carries the plugin
/// market) into the user's DSH_HOME, unless a profile is already there (plan §2.5).
fn seed_profile_template(config: &Config, seed: Option<&Path>) -> Option<String> {
    let seed = seed?;
    let template = seed.join("profile-template");
    if !template.join("package.json").is_file() {
        return None;
    }
    let home = config
        .dsh_home
        .clone()
        .or_else(|| home_dir().map(|home| home.join(".dsh")))?;
    let profile = home.join("profiles").join("web");
    if profile.exists() {
        return None;
    }
    match copy_tree(&template, &profile) {
        Ok(()) => Some(format!(
            "seeded profile template into {}",
            profile.display()
        )),
        Err(error) => Some(format!("could not seed the profile template: {error}")),
    }
}

/// Recursive copy — the template is ~30 files, so no need for a dependency.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
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
    // The two paths every startup failure is traced back to; cheap to log, and the only way
    // to diagnose a machine we cannot run on.
    harness::app_log(&format!(
        "app data dir = {} | workspace = {}",
        data_dir.display(),
        config.workspace.display()
    ));

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
    let resolved = resolve_runtime(app.path().resource_dir().ok().as_deref(), data_dir, &config)?;
    let mut version = resolved.version.clone();

    // First launch of a bundled build: put the profile template (plugin market included) in
    // place before the CLI starts, so the marketplace exists without any download (§2.5).
    if let Some(note) = seed_profile_template(&config, resolved.seed.as_deref()) {
        harness::app_log(&note);
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
                match checked.status {
                    update::Status::UpdateAvailable { from, to } => {
                        window::set_status(
                            app,
                            &format!("发现新版本 v{to}，正在更新…"),
                            &format!("v{from} -> v{to}"),
                        );
                        // The CLI belongs to the user when we resolved their installation: the shell
                        // never writes a prefix it does not own (§2.4), it only reports.
                        if matches!(resolved.updates, runtime::Updates::Notify) {
                            harness::app_log(&format!(
                                "update available: {from} -> {to}; the CLI is the user install, not touching it"
                            ));
                            window::set_status(
                                app,
                                &format!("有新版本 v{to}（当前使用系统安装，未自动更新）"),
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
                                    let prefix = resolved.update_prefix(data_dir);
                                    match update::install(
                                        &npm,
                                        update::PACKAGE,
                                        &to,
                                        prefix.as_deref(),
                                    ) {
                                        Ok(()) => {
                                            version = locator::version_of(&resolved.dsh_js)
                                                .unwrap_or_else(|| to.clone());
                                            if version == from {
                                                // npm installed the package somewhere other than where
                                                // this CLI lives (custom prefix, pnpm/yarn/volta layout),
                                                // so the supervised binary is unchanged. Say so instead of
                                                // claiming an update and restarting for nothing.
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
    if let Some(dir) = resolved.node.parent() {
        prefix.push(dir.to_string_lossy().to_string());
    }
    // Bundled runtime: the shipped pnpm (and the writable tools prefix taking precedence) go in
    // front so plugins, MCP servers and agent commands use the same toolchain as the shell (§5).
    if resolved.bundled() {
        // npm puts shims in `bin/` on Unix and directly in the prefix on Windows; PATH entries
        // that do not exist are harmless, so both are offered.
        let tools = data_dir.join("runtime").join("tools");
        prefix.push(tools.join("bin").to_string_lossy().to_string());
        prefix.push(tools.to_string_lossy().to_string());
        if let Some(seed) = &resolved.seed {
            prefix.push(seed.join("tools").join("bin").to_string_lossy().to_string());
            prefix.push(seed.join("tools").to_string_lossy().to_string());
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
    let merged = shellenv::merge_path(
        &prefix,
        shell_path.as_deref(),
        std::env::var("PATH").ok().as_deref(),
    );
    child_env.retain(|(key, _)| key != "PATH");
    child_env.push(("PATH".to_string(), merged));
    // Recentred global installs: with the read-only seed in front of PATH, `npm i -g` from the
    // harness would target the app bundle. Point it (and pnpm) at the writable tools prefix.
    if resolved.bundled() {
        let tools = data_dir.join("runtime").join("tools");
        child_env.retain(|(key, _)| key != "npm_config_prefix" && key != "PNPM_HOME");
        child_env.push((
            "npm_config_prefix".to_string(),
            tools.to_string_lossy().to_string(),
        ));
        child_env.push((
            "PNPM_HOME".to_string(),
            tools.join("bin").to_string_lossy().to_string(),
        ));
    }
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
    let home = config
        .dsh_home
        .clone()
        .or_else(|| home_dir().map(|home| home.join(".dsh")))
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
}
