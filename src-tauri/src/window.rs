//! Splash window, Harness window, and navigation policy.

use crate::harness;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::webview::{DownloadEvent, NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

pub const SPLASH: &str = "splash";
pub const HARNESS: &str = "harness";

pub fn create_splash(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, SPLASH, WebviewUrl::App("index.html".into()))
        .title("DeepSeek Harness")
        .inner_size(460.0, 300.0)
        .resizable(false)
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

/// Set by the first terminal failure page. A second one (the load watcher giving up after the
/// watchdog already reported the exit) must not replace the explanation the user is reading.
static FAILURE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Put a status page in front of the user for something they must see, such as a Harness that
/// died after a successful start. Rebuilds the splash when it was already closed.
pub fn show_failure(app: &AppHandle, status: &str, detail: &str) {
    if FAILURE_SHOWN.swap(true, Ordering::SeqCst) {
        harness::app_log(&format!(
            "failure page already shown; keeping it instead of: {status}"
        ));
        return;
    }
    // Every caller reaches this with a Harness window that is dead or unusable, and the status
    // page tells the user that closing it quits the app. Destroy (not close: that would look
    // like a user quit) the Harness window so that sentence is true — the splash only exits the
    // app while no Harness window is left.
    if let Some(harness_window) = app.get_webview_window(HARNESS) {
        let _ = harness_window.destroy();
    }
    if app.get_webview_window(SPLASH).is_none() {
        if let Err(error) = create_splash(app) {
            harness::app_log(&format!("could not reopen the status window: {error}"));
            return;
        }
    }
    set_status(app, status, detail);
}

/// Update the splash without granting the page any Tauri permission (Rust-side eval).
pub fn set_status(app: &AppHandle, status: &str, detail: &str) {
    if let Some(window) = app.get_webview_window(SPLASH) {
        let script = format!(
            "window.__setStatus && window.__setStatus({}, {})",
            json!(status),
            json!(detail)
        );
        let _ = window.eval(&script);
    }
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
