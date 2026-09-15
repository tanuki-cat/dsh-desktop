//! Splash window, Harness window, and navigation policy.

use crate::harness;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::webview::{DownloadEvent, NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

pub const SPLASH: &str = "splash";
pub const HARNESS: &str = "harness";

/// The event the splash page uses to report what this WebView can actually run.
pub const PROBE_EVENT: &str = "dsh-desktop:webview-probe";

/// The event the status page uses to ask for another attempt at starting the Harness.
pub const RESTART_EVENT: &str = "dsh-desktop:restart-harness";

/// APIs whose absence kills the dsh front end while it loads.
///
/// `Iterator` is the one that actually happened (2026-09-14, an Intel Mac): the bundled
/// document-preview plugin evaluates `Iterator.prototype.join` without checking that the global
/// exists, so a WebView whose JavaScriptCore predates Safari 18.4 throws during `import` and the
/// window ends up showing the harness's opaque "Failed to load plugins" page. The `Iterator`
/// global arrived in Safari 18.4 — macOS 15.4, or the Safari 18.4 update for macOS 13/14 — which
/// is far newer than the macOS versions this bundle still allows (11.0).
const REQUIRED_APIS: &[&str] = &["Iterator"];

/// APIs whose absence only costs a feature. Reported, never fatal: the bundled PDF writer uses
/// `Math.sumPrecise`, which no Safari release ships yet.
const OPTIONAL_APIS: &[&str] = &[
    "Promise.withResolvers",
    "Math.sumPrecise",
    "structuredClone",
];

/// What the splash page found missing in this WebView.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct WebviewReport {
    /// Required APIs that are absent: the dsh front end cannot load.
    #[serde(default)]
    pub missing: Vec<String>,
    /// Optional APIs that are absent: some feature will misbehave.
    #[serde(default)]
    pub degraded: Vec<String>,
    /// `navigator.userAgent`, for the log and the failure page.
    #[serde(default)]
    pub agent: String,
}

impl WebviewReport {
    /// Parse what the page sent. A payload this shell cannot read is not a reason to refuse to
    /// start — it only costs the diagnostics.
    pub fn parse(payload: &str) -> Option<WebviewReport> {
        serde_json::from_str(payload).ok()
    }

    /// False when the dsh front end cannot load in this WebView.
    pub fn supported(&self) -> bool {
        self.missing.is_empty()
    }

    /// The text shown when the UI has to move to the system browser: this WebView cannot run
    /// it, but the browser can.
    ///
    /// That is the `dsh web` path these machines have always used: the shell still supervises
    /// the Harness (updates, stopping it on exit), the browser only renders the UI — with an
    /// engine that does keep getting updates.
    pub fn browser_fallback_detail(&self, dsh_version: &str, url: &str) -> String {
        format!(
            "{}\n\n界面已在默认浏览器中打开：\n{url}\n\n\
             这个窗口是 harness 的管理窗口（更新与退出清理都在这里）：在浏览器里操作时请不要关闭它，\
             关闭它会停止 harness。",
            self.describe(dsh_version)
        )
    }

    /// The text of the failure page: what is missing, what this dsh version needs, what to do.
    pub fn describe(&self, dsh_version: &str) -> String {
        let mut text = format!(
            "系统 WebView 缺少 dsh {dsh_version} 前端必需的 JavaScript 能力：{}。\n\
             这个版本的界面需要 Safari 18.4（macOS 15.4）或更新的 WebKit，升级系统或安装 Safari 更新后重试。",
            self.missing.join("、")
        );
        if !self.degraded.is_empty() {
            text.push_str(&format!(
                "\n另外缺少（只影响部分功能）：{}。",
                self.degraded.join("、")
            ));
        }
        if !self.agent.is_empty() {
            text.push_str(&format!("\n当前 WebView：{}", self.agent));
        }
        text
    }
}

/// What the splash page reported, once.
///
/// `None` means "never reported" (the probe could not run, or the page never loaded). That is
/// treated as supported: refusing to start because a *diagnostic* is missing would be worse than
/// the bug it detects.
static REPORT: Mutex<Option<WebviewReport>> = Mutex::new(None);

/// How long to wait for that report. The splash page loads long before the Harness is up, so
/// only the fast path — reusing a live instance — can arrive here first.
const PROBE_WAIT: Duration = Duration::from_millis(500);

/// Store what the page reported (called from the event listener in `run`).
pub fn record_report(payload: &str) {
    let Some(report) = WebviewReport::parse(payload) else {
        harness::app_log(&format!("无法解析 WebView 能力探测的上报：{payload}"));
        return;
    };
    *REPORT.lock().unwrap() = Some(report);
}

/// The reason this WebView cannot host the dsh front end, when the page reported one.
///
/// `None` means "no reason known" — either the report says the WebView is fine or the probe
/// never arrived (see [`REPORT`]).
pub fn unsupported_webview() -> Option<WebviewReport> {
    let deadline = Instant::now() + PROBE_WAIT;
    loop {
        if let Some(report) = REPORT.lock().unwrap().as_ref() {
            return (!report.supported()).then(|| report.clone());
        }
        if Instant::now() >= deadline {
            harness::app_log("WebView 能力探测没有上报，按支持处理");
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The script that asks the page what this WebView can run.
///
/// It is injected into the splash window, which is our own page and the only one holding core
/// capabilities (`capabilities/splash.json`); the Harness window deliberately gets none.
fn probe_script() -> String {
    let mut checks = String::new();
    for (list, names) in [("missing", REQUIRED_APIS), ("degraded", OPTIONAL_APIS)] {
        for name in names {
            checks.push_str(&format!(
                "  try {{ if (typeof {name} === \"undefined\") {list}.push(\"{name}\"); }} catch (error) {{ {list}.push(\"{name}\"); }}\n"
            ));
        }
    }
    PROBE_TEMPLATE
        .replace("%CHECKS%", &checks)
        .replace("%EVENT%", PROBE_EVENT)
}

/// The probe's JavaScript. ES5 on purpose: it has to run on the very engines this exists to
/// diagnose. `window.__TAURI_INTERNALS__` may not exist yet (Tauri installs its IPC bridge in
/// its own initialization script), so the report retries briefly and then gives up quietly.
const PROBE_TEMPLATE: &str = r#"
(function () {
  var missing = [];
  var degraded = [];
%CHECKS%
  var payload = { missing: missing, degraded: degraded, agent: navigator.userAgent };
  function report(attempt) {
    try {
      if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
        window.__TAURI_INTERNALS__.invoke("plugin:event|emit", { event: "%EVENT%", payload: payload });
        return;
      }
    } catch (error) {
      // An old WebView can throw here; fall through to the retry, then give up quietly.
    }
    if (attempt < 40) setTimeout(function () { report(attempt + 1); }, 25);
  }
  report(0);
})();
"#;

pub fn create_splash(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, SPLASH, WebviewUrl::App("index.html".into()))
        .title("DeepSeek Harness")
        .inner_size(460.0, 300.0)
        .resizable(false)
        // Ask this WebView what it can run before anything is spawned: an engine older than
        // Safari 18.4 cannot load the current dsh front end at all (see `WebviewReport`).
        .initialization_script(probe_script())
        .build()?;
    // Tauri keeps the app alive when its last window closes, so a status window the user
    // dismisses (typically after a failed start) would leave a windowless app behind. Only
    // then may closing it end the process: the app removes this window itself once the
    // Harness window is up (via destroy), and that must not look like the user quitting.
    let handle = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            if handle.get_webview_window(HARNESS).is_none() {
                handle.exit(0);
            }
        }
    });
    Ok(())
}

/// Set by the first terminal page. A second one (the load watcher giving up after the watchdog
/// already reported the exit) must not replace the explanation the user is reading.
///
/// Cleared by [`allow_next_failure`] whenever the shell starts another attempt, so a retry that
/// fails again can replace the page that asked for it.
static FAILURE_SHOWN: AtomicBool = AtomicBool::new(false);

/// True while the page on screen is the failure page — the one that offers a restart.
///
/// Distinct from [`FAILURE_SHOWN`] on purpose: the browser fallback ([show_notice]) is terminal
/// for the same reason (nothing may overwrite its explanation) but must not be answered with a
/// restart, since its Harness is alive and supervised.
static RETRY_OFFERED: AtomicBool = AtomicBool::new(false);

/// True when "start the Harness again" is what the user is asking for by reopening the app.
pub fn retry_offered() -> bool {
    RETRY_OFFERED.load(Ordering::SeqCst)
}

/// Let the next terminal page replace whatever is on screen. Called when the shell starts a
/// recovery, or a restart the user asked for.
pub fn allow_next_failure() {
    FAILURE_SHOWN.store(false, Ordering::SeqCst);
}

/// Put a status page in front of the user for something they must see, such as a Harness that
/// died after a successful start. Rebuilds the splash when it was already closed, and offers the
/// restart button: the shell can start another Harness in this very process.
pub fn show_failure(app: &AppHandle, status: &str, detail: &str) {
    if FAILURE_SHOWN.swap(true, Ordering::SeqCst) {
        harness::app_log(&format!(
            "failure page already shown; keeping it instead of: {status}"
        ));
        return;
    }
    if present_status_page(app) {
        RETRY_OFFERED.store(true, Ordering::SeqCst);
        eval_status(app, "__setFailure", status, detail);
    }
}

/// The same page without the restart offer.
///
/// Used where another Harness must not be started: the old-WebView fallback keeps supervising
/// the running instance and hands only the rendering to the browser, so a restart button there
/// would kill a working Harness and open a second browser tab.
pub fn show_notice(app: &AppHandle, status: &str, detail: &str) {
    if FAILURE_SHOWN.swap(true, Ordering::SeqCst) {
        harness::app_log(&format!(
            "failure page already shown; keeping it instead of: {status}"
        ));
        return;
    }
    if present_status_page(app) {
        RETRY_OFFERED.store(false, Ordering::SeqCst);
        eval_status(app, "__setStatus", status, detail);
    }
}

/// Bring the status page back while the shell itself is still working on the Harness.
///
/// Not latched, and without the restart button: this is the "an attempt is running" face of the
/// same page. A success removes it again through `create_harness`; a failure replaces it through
/// [`show_failure`].
pub fn show_progress(app: &AppHandle, status: &str, detail: &str) {
    if present_status_page(app) {
        RETRY_OFFERED.store(false, Ordering::SeqCst);
        eval_status(app, "__setStatus", status, detail);
    }
}

/// Rebuild the splash when it was already closed, with no Harness window left in front of it.
///
/// Every caller reaches this with a Harness window that is dead or unusable, and the status page
/// tells the user that closing it quits the app. Destroy (not close: that would look like a user
/// quit) the Harness window so that sentence is true — the splash only exits the app while no
/// Harness window is left.
fn present_status_page(app: &AppHandle) -> bool {
    if let Some(harness_window) = app.get_webview_window(HARNESS) {
        let _ = harness_window.destroy();
    }
    if app.get_webview_window(SPLASH).is_none() {
        if let Err(error) = create_splash(app) {
            harness::app_log(&format!("could not reopen the status window: {error}"));
            return false;
        }
    }
    true
}

/// Update the splash without granting the page any Tauri permission (Rust-side eval).
pub fn set_status(app: &AppHandle, status: &str, detail: &str) {
    eval_status(app, "__setStatus", status, detail);
}

/// Call one of the page's two status entry points: `__setStatus` for progress, `__setFailure`
/// for the terminal page that carries the restart button.
///
/// A page this call just created may not have parsed its `<head>` yet, and that is where both
/// entry points are defined — an eval landing before them would simply be lost. The retry costs
/// nothing when the page is ready (the first call succeeds) and is the difference between the
/// user seeing the page and it staying on "正在启动…".
fn eval_status(app: &AppHandle, function: &str, status: &str, detail: &str) {
    if let Some(window) = app.get_webview_window(SPLASH) {
        let _ = window.eval(status_script(function, status, detail));
    }
}

/// The script `eval_status` sends, kept out of the window call so the escaping is testable.
fn status_script(function: &str, status: &str, detail: &str) -> String {
    let status = json!(status);
    let detail = json!(detail);
    format!(
        "(function () {{\n\
         var apply = function () {{\n\
         if (!window.{function}) return false;\n\
         window.{function}({status}, {detail});\n\
         return true;\n\
         }};\n\
         if (apply()) return;\n\
         var attempts = 0;\n\
         var timer = setInterval(function () {{\n\
         if (apply() || ++attempts > 40) clearInterval(timer);\n\
         }}, 25);\n\
         }})()"
    )
}

/// Wait this long for the first load to report "finished". The port already answered a probe
/// before the window was built, so a healthy load completes well inside it.
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// Navigations of the same startup URL, including the first one.
const LOAD_ATTEMPTS: u32 = 3;

/// What the webview reported about the navigations this shell started.
///
/// The two events answer different questions, which is why both are tracked: no event at all
/// means the navigation never began, while a start without a finish means a load is genuinely
/// running and must not be restarted.
#[derive(Default)]
struct LoadSignals {
    started: AtomicBool,
    finished: AtomicBool,
}

/// The Harness page is remote content: it gets no capability, and navigation is fenced
/// to the current loopback authority. Everything else opens in the system browser.
pub fn create_harness(app: &AppHandle, url: &Url, port: u16) -> tauri::Result<()> {
    // `destroy` rather than `close`: close fires the window listeners (and the Harness window
    // ends the app on a user close), which must stay a user-only signal.
    if let Some(existing) = app.get_webview_window(HARNESS) {
        let _ = existing.destroy();
    }
    // Set as the page reports its progress. A navigation the webview drops reports nothing at
    // all, which is what the retry thread below watches for.
    let signals = Arc::new(LoadSignals::default());
    let load_signals = signals.clone();
    let window = WebviewWindowBuilder::new(app, HARNESS, WebviewUrl::External(url.clone()))
        .title("DeepSeek Harness")
        .inner_size(1440.0, 960.0)
        .min_inner_size(900.0, 600.0)
        .on_navigation(move |target| {
            let same_origin = target.scheme() == "http"
                && target.host_str() == Some("127.0.0.1")
                && target.port() == Some(port);
            if !same_origin {
                harness::app_log(&format!("navigation blocked, opening externally: {target}"));
                open_external(target.as_str());
            }
            same_origin
        })
        // window.open / target=_blank: never spawn a Tauri window for remote content.
        .on_new_window(move |target, _features| {
            harness::app_log(&format!("new window request, opening externally: {target}"));
            open_external(target.as_str());
            NewWindowResponse::Deny
        })
        // The only evidence of what the first navigation did. `PageLoadEvent` is
        // non-exhaustive, so each variant is matched on its own.
        .on_page_load(move |_window, payload| {
            let event = payload.event();
            if matches!(event, PageLoadEvent::Started) {
                load_signals.started.store(true, Ordering::SeqCst);
            }
            if matches!(event, PageLoadEvent::Finished) {
                load_signals.finished.store(true, Ordering::SeqCst);
            }
        })
        // Downloads land in the user's Downloads folder instead of vanishing.
        .on_download(move |_webview, event| {
            match event {
                DownloadEvent::Requested { url, destination } => {
                    *destination =
                        unique_download_path(&downloads_dir(), &file_name_of(destination));
                    harness::app_log(&format!(
                        "download started: {url} -> {}",
                        destination.display()
                    ));
                }
                DownloadEvent::Finished { url, path, .. } => {
                    let where_to = path
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unknown".into());
                    harness::app_log(&format!("download finished: {url} -> {where_to}"));
                }
                // The enum is #[non_exhaustive]: future variants must not break the build.
                _ => {}
            }
            true
        })
        .build()?;

    // A first navigation that never renders used to leave a blank window with nothing in the
    // log. The token URL is safe to revisit (measured: it is not single-use), so retry it.
    let watcher = window.clone();
    let watcher_app = window.app_handle().clone();
    let target = url.clone();
    std::thread::spawn(move || watch_first_load(watcher_app, watcher, target, port, signals));

    // Closing the Harness window quits the app, which stops the supervised process.
    let handle = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            handle.exit(0);
        }
    });

    if let Some(splash) = app.get_webview_window(SPLASH) {
        // Same reason as above: removing the status window is not a user close.
        let _ = splash.destroy();
    }
    Ok(())
}

/// What to do after the page missed its load deadline.
#[derive(Debug, PartialEq, Eq)]
enum LoadOutcome {
    /// Stop watching: the window is gone, or a load genuinely began (its `Started` arrived), so
    /// restarting it would interrupt work in progress.
    Settled,
    /// The navigation never began: send it again.
    Retry,
    /// Attempts ran out with nothing serving the port: tell the user.
    Report,
}

/// Pure retry policy, kept out of the thread so the matrix is testable.
///
/// The discriminator is the `Started` event, not the port: a blank window whose webview never
/// received the navigation looks exactly like a healthy port from the outside, and that is the
/// case this watch exists for. A port that still answers is never turned into a failure page —
/// if an environment delivers no load events at all, the retry must not escalate into an error
/// over a working app.
fn load_outcome(
    attempt: u32,
    attempts: u32,
    window_alive: bool,
    started: bool,
    port_serving: bool,
) -> LoadOutcome {
    if !window_alive || started {
        return LoadOutcome::Settled;
    }
    if attempt < attempts {
        return LoadOutcome::Retry;
    }
    if port_serving {
        LoadOutcome::Settled
    } else {
        LoadOutcome::Report
    }
}

/// Wait for the first successful render, re-navigating the startup URL when it never comes.
///
/// Design §4 steps 8/9: a failed load may retry the same token URL. The retry covers the case
/// where the webview never got as far as starting the navigation; a load that started is left
/// alone, and a page that never renders while the port is healthy is reported in the log only.
fn watch_first_load(
    app: AppHandle,
    window: WebviewWindow,
    url: Url,
    port: u16,
    signals: Arc<LoadSignals>,
) {
    for attempt in 1..=LOAD_ATTEMPTS {
        if wait_for_load(&signals, LOAD_TIMEOUT) {
            return;
        }
        let window_alive = app.get_webview_window(HARNESS).is_some();
        let started = signals.started.load(Ordering::SeqCst);
        let serving = port_serving(port);
        match load_outcome(attempt, LOAD_ATTEMPTS, window_alive, started, serving) {
            LoadOutcome::Settled => {
                if window_alive {
                    harness::app_log(&format!(
                        "page load did not finish within {LOAD_TIMEOUT:?} (started={started}, port {port} serving={serving}); leaving the window alone"
                    ));
                }
                return;
            }
            LoadOutcome::Retry => {
                harness::app_log(&format!(
                    "navigation never started, retrying {url} (attempt {}/{LOAD_ATTEMPTS})",
                    attempt + 1
                ));
                if let Err(error) = window.navigate(url.clone()) {
                    harness::app_log(&format!("retry navigation failed: {error}"));
                }
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
            }
            LoadOutcome::Report => {
                harness::app_log(&format!(
                    "navigation never started and port {port} stopped serving after {LOAD_ATTEMPTS} attempts: {url}"
                ));
                show_failure(
                    &app,
                    "Harness 页面加载失败",
                    &format!(
                        "窗口连续 {LOAD_ATTEMPTS} 次都没有加载出 {url}，端口 {port} 也没有在提供服务。\n\n关闭本窗口即退出应用；重新启动会重新拉起 Harness 并换一个新的启动 URL。"
                    ),
                );
                return;
            }
        }
    }
}

/// Poll the finished flag instead of blocking on a channel: the watcher also has to give up when
/// no event ever arrives, which a blocking receive cannot express.
fn wait_for_load(signals: &LoadSignals, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if signals.finished.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    signals.finished.load(Ordering::SeqCst)
}

/// True while the port answers as a Harness: the launch URL is only worth revisiting while the
/// process behind it is still there.
fn port_serving(port: u16) -> bool {
    matches!(
        harness::probe(port),
        harness::Probe::HarnessWithSession | harness::Probe::HarnessNoSession
    )
}

fn downloads_dir() -> PathBuf {
    let base = crate::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Downloads");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Never overwrite an existing download: `report.pdf` becomes `report-1.pdf` when taken.
fn unique_download_path(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let name = Path::new(name);
    let stem = name
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "dsh-download".to_string());
    let extension = name
        .extension()
        .map(|ext| ext.to_string_lossy().to_string());
    for index in 1..1000 {
        let file = match &extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = dir.join(file);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

fn file_name_of(suggested: &std::path::Path) -> std::ffi::OsString {
    suggested
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("dsh-download"))
}

/// Schemes the shell is willing to hand to the operating system.
///
/// The Harness page renders whatever the agent produced — model-authored links, fetched pages —
/// so every URL arriving here is attacker-influenced. `open` / `xdg-open` act on far more than
/// web pages: `file:` launches local applications on macOS, and any registered custom scheme
/// reaches its app handler. Only web navigation leaves the shell (design §7).
pub fn may_open(target: &Url) -> bool {
    matches!(target.scheme(), "http" | "https")
}

/// Hand a link to the system browser, or drop it with a log line.
pub fn open_external(target: &str) {
    let url = match Url::parse(target) {
        Ok(url) => url,
        Err(error) => {
            harness::app_log(&format!("external link dropped ({error}): {target}"));
            return;
        }
    };
    if !may_open(&url) {
        harness::app_log(&format!(
            "external scheme blocked: {} ({target})",
            url.scheme()
        ));
        return;
    }

    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "cmd";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    command.args(["/C", "start", ""]);
    let _ = command.arg(url.as_str()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_schemes_leave_the_shell() {
        let allow = |raw: &str| may_open(&Url::parse(raw).expect("test URL must parse"));
        assert!(allow("https://example.com/docs"));
        assert!(allow("http://127.0.0.1:3080/?token=abc"));
        // `file:` would launch a local application through `open` on macOS.
        assert!(!allow("file:///System/Applications/Calculator.app"));
        assert!(!allow("smb://host/share"));
        // Custom schemes reach whichever app registered them.
        assert!(!allow("dsh-desktop://open"));
        assert!(!allow("javascript:alert(1)"));
        assert!(!allow("data:text/html,<script>alert(1)</script>"));
        assert!(!allow("mailto:someone@example.com"));
    }

    #[test]
    fn a_missing_required_api_makes_the_webview_unsupported() {
        let report = WebviewReport::parse(
            r#"{"missing":["Iterator"],"degraded":["Math.sumPrecise"],"agent":"Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15"}"#,
        )
        .expect("the page's report must parse");

        assert!(!report.supported());
        let text = report.describe("0.1.5-rc.2");
        assert!(text.contains("Iterator"), "缺什么要写清楚：{text}");
        assert!(text.contains("0.1.5-rc.2"), "是哪个 dsh 版本要写清楚");
        // The user has to be told what to do about it — that is what the opaque plugin error
        // this replaces never did.
        assert!(text.contains("Safari 18.4"), "补救方向要写清楚：{text}");
        assert!(text.contains("Math.sumPrecise"), "降级项也要列出来");
        assert!(text.contains("AppleWebKit"), "报上当前 WebView 便于排查");
    }

    #[test]
    fn the_browser_fallback_tells_the_user_what_to_keep_open() {
        let report =
            WebviewReport::parse(r#"{"missing":["Iterator"],"degraded":[],"agent":"old"}"#)
                .expect("the page's report must parse");

        let text = report.browser_fallback_detail("0.1.5-rc.2", "http://127.0.0.1:3080/?token=t");
        assert!(
            text.contains("http://127.0.0.1:3080/?token=t"),
            "给出地址：{text}"
        );
        assert!(text.contains("Iterator"), "说明原因");
        assert!(
            text.contains("不要关闭"),
            "关掉这个窗口会停 harness，必须写明：{text}"
        );
    }

    #[test]
    fn a_degraded_but_complete_webview_still_starts() {
        let report =
            WebviewReport::parse(r#"{"missing":[],"degraded":["Math.sumPrecise"],"agent":"x"}"#)
                .expect("the page's report must parse");
        assert!(
            report.supported(),
            "a missing optional API must never block the GUI"
        );
    }

    #[test]
    fn an_unreadable_report_is_not_a_reason_to_refuse_to_start() {
        assert_eq!(WebviewReport::parse("not json"), None);
        // An empty report is a WebView that found nothing missing: supported.
        assert_eq!(
            WebviewReport::parse("{}").map(|report| report.supported()),
            Some(true)
        );
    }

    /// One test owns the process-wide report slot on purpose: `record_report` overwrites it,
    /// and every assertion here depends on what was reported last.
    #[test]
    fn the_gate_reads_what_the_page_reported() {
        record_report(r#"{"missing":["Iterator"],"degraded":[],"agent":"old"}"#);
        let refused = unsupported_webview().expect("a missing Iterator must be refused");
        assert_eq!(refused.missing, ["Iterator"]);

        // A payload this shell cannot read keeps the previous answer (logged, not fatal).
        record_report("not json");
        assert!(
            unsupported_webview().is_some(),
            "a broken report must not clear a refusal"
        );

        // A complete WebView starts, however many optional APIs are missing.
        record_report(r#"{"missing":[],"degraded":["Math.sumPrecise"],"agent":"new"}"#);
        assert!(
            unsupported_webview().is_none(),
            "a complete WebView must start"
        );
    }

    #[test]
    fn the_probe_asks_about_the_api_that_broke_the_ui() {
        let script = probe_script();
        // The one that actually failed in the field (see REQUIRED_APIS).
        assert!(script.contains("typeof Iterator === \"undefined\""));
        assert!(script.contains("Math.sumPrecise"));
        // It reports through the splash window's core capability.
        assert!(script.contains(PROBE_EVENT));
        assert!(script.contains("plugin:event|emit"));
        // It runs on exactly the engines it exists to diagnose: keep it ES5.
        assert!(!script.contains("=>"), "the probe must stay ES5");
        assert!(!script.contains('`'), "the probe must stay ES5");
        assert!(!script.contains("??"), "the probe must stay ES5");
    }

    #[test]
    fn the_status_script_applies_now_or_waits_for_the_page() {
        let script = status_script("__setFailure", "Harness 已退出", "退出码 0\n第二行");
        // The page defines its entry points in <head>; a freshly created window may not have
        // parsed them yet, so the script must retry rather than drop the message.
        assert!(script.contains("window.__setFailure"), "{script}");
        assert!(script.contains("setInterval"), "{script}");
        // The text is embedded as a JS string literal, newlines and all.
        assert!(script.contains(r#""退出码 0\n第二行""#), "{script}");
        // Same builder for the progress face: the difference is which entry point it calls.
        let progress = status_script("__setStatus", "正在启动…", "");
        assert!(progress.contains("window.__setStatus"), "{progress}");
        assert!(!progress.contains("__setFailure"));
    }

    /// The page names the restart event as a string literal; the shell listens for the
    /// constant. Renaming one without the other would silently drop every click.
    #[test]
    fn the_status_page_reports_the_restart_event_the_shell_listens_for() {
        let page = include_str!("../../src/index.html");
        assert!(page.contains(RESTART_EVENT), "按钮必须发壳监听的那个事件名");
        assert!(
            page.contains("__setFailure"),
            "页面要有终态入口（带重启按钮的那一面）"
        );
        assert!(page.contains("__setStatus"), "页面要有进行中入口");
        assert!(
            page.contains("plugin:event|emit"),
            "只走 splash 窗口已有的 core 事件能力，不新增 capability"
        );
    }

    #[test]
    fn load_retry_follows_the_started_event_and_never_fails_a_live_port() {
        // A navigation that never began is sent again while attempts remain, whether or not the
        // port answers: the blank-window case usually comes with a healthy port.
        assert_eq!(load_outcome(1, 3, true, false, true), LoadOutcome::Retry);
        assert_eq!(load_outcome(2, 3, true, false, false), LoadOutcome::Retry);
        // A load that started is not interrupted, however long it takes.
        assert_eq!(load_outcome(1, 3, true, true, true), LoadOutcome::Settled);
        assert_eq!(load_outcome(3, 3, true, true, false), LoadOutcome::Settled);
        // Out of attempts: a failure page only when nothing serves the port either. A healthy
        // port means the load events themselves are missing, which a retry cannot fix.
        assert_eq!(load_outcome(3, 3, true, false, false), LoadOutcome::Report);
        assert_eq!(load_outcome(3, 3, true, false, true), LoadOutcome::Settled);
        // The user closed the window: stop watching it.
        assert_eq!(
            load_outcome(1, 3, false, false, false),
            LoadOutcome::Settled
        );
    }

    #[test]
    fn download_names_never_collide() {
        let dir = std::env::temp_dir().join("dsh-desktop-download-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Free name is used as-is, taken names get a counter before the extension.
        assert_eq!(
            unique_download_path(&dir, std::ffi::OsStr::new("a.pdf")),
            dir.join("a.pdf")
        );
        std::fs::write(dir.join("a.pdf"), "x").unwrap();
        assert_eq!(
            unique_download_path(&dir, std::ffi::OsStr::new("a.pdf")),
            dir.join("a-1.pdf")
        );
        std::fs::write(dir.join("a-1.pdf"), "x").unwrap();
        assert_eq!(
            unique_download_path(&dir, std::ffi::OsStr::new("a.pdf")),
            dir.join("a-2.pdf")
        );
        // Extension-less names keep working.
        std::fs::write(dir.join("b"), "x").unwrap();
        assert_eq!(
            unique_download_path(&dir, std::ffi::OsStr::new("b")),
            dir.join("b-1")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
