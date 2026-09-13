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

/// Put a status page in front of the user for something they must see, such as a Harness that
/// died after a successful start. Rebuilds the splash when it was already closed.
pub fn show_failure(app: &AppHandle, status: &str, detail: &str) {
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

/// The Harness page is remote content: it gets no capability, and navigation is fenced
/// to the current loopback authority. Everything else opens in the system browser.
pub fn create_harness(app: &AppHandle, url: &Url, port: u16) -> tauri::Result<()> {
    // `destroy` rather than `close`: close fires the window listeners (and the Harness window
    // ends the app on a user close), which must stay a user-only signal.
    if let Some(existing) = app.get_webview_window(HARNESS) {
        let _ = existing.destroy();
    }
    // Set once the page reports "finished". A failed navigation reports nothing at all, which is
    // what the retry thread below watches for.
    let loaded = Arc::new(AtomicBool::new(false));
    let loaded_flag = loaded.clone();
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
        // The only signal that the first navigation actually rendered. A failed one produces
        // no event at all, which is what the retry thread below waits to find out.
        .on_page_load(move |_window, payload| {
            if matches!(payload.event(), PageLoadEvent::Finished) {
                loaded_flag.store(true, Ordering::SeqCst);
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
    std::thread::spawn(move || watch_first_load(watcher_app, watcher, target, port, loaded));

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
    /// Stop watching: the window is gone, or the port still serves a Harness and only the
    /// load event failed to arrive. Retrying a healthy page would restart a load in progress.
    Settled,
    /// Navigate to the same URL again.
    Retry,
    /// Out of attempts with nothing serving the port: tell the user.
    Report,
}

/// Pure retry policy, kept out of the thread so the matrix is testable.
fn load_outcome(
    attempt: u32,
    attempts: u32,
    window_alive: bool,
    port_serving: bool,
) -> LoadOutcome {
    if !window_alive || port_serving {
        return LoadOutcome::Settled;
    }
    if attempt >= attempts {
        LoadOutcome::Report
    } else {
        LoadOutcome::Retry
    }
}

/// Wait for the first successful render, re-navigating the startup URL when it never comes.
///
/// Design §4 steps 8/9: a failed load may retry the same token URL. Only the "nothing is serving
/// the port" case is retried — a live port whose load event is merely late must not have its load
/// restarted, and a `dsh` that never answers is the transient race this exists for.
fn watch_first_load(
    app: AppHandle,
    window: WebviewWindow,
    url: Url,
    port: u16,
    loaded: Arc<AtomicBool>,
) {
    for attempt in 1..=LOAD_ATTEMPTS {
        if wait_for_load(&loaded, LOAD_TIMEOUT) {
            return;
        }
        let window_alive = app.get_webview_window(HARNESS).is_some();
        let serving = port_serving(port);
        match load_outcome(attempt, LOAD_ATTEMPTS, window_alive, serving) {
            LoadOutcome::Settled => {
                if window_alive {
                    harness::app_log(&format!(
                        "page load event did not arrive, but port {port} still serves a Harness; leaving the window alone"
                    ));
                }
                return;
            }
            LoadOutcome::Retry => {
                harness::app_log(&format!(
                    "page did not finish loading, retrying {url} (attempt {}/{LOAD_ATTEMPTS})",
                    attempt + 1
                ));
                if let Err(error) = window.navigate(url.clone()) {
                    harness::app_log(&format!("retry navigation failed: {error}"));
                }
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
            }
            LoadOutcome::Report => {
                harness::app_log(&format!(
                    "page never finished loading after {LOAD_ATTEMPTS} attempts: {url}"
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

/// Poll the flag instead of blocking on a channel: the watcher also has to give up when no event
/// ever arrives, which a blocking receive cannot express.
fn wait_for_load(loaded: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if loaded.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    loaded.load(Ordering::SeqCst)
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
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
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
    fn load_retry_stops_on_a_live_port_or_a_closed_window() {
        // Nothing serving the port and attempts left: navigate again.
        assert_eq!(load_outcome(1, 3, true, false), LoadOutcome::Retry);
        assert_eq!(load_outcome(2, 3, true, false), LoadOutcome::Retry);
        // Out of attempts with the port still dead: report instead of looping forever.
        assert_eq!(load_outcome(3, 3, true, false), LoadOutcome::Report);
        // A late load event on a serving port must not restart the load in progress.
        assert_eq!(load_outcome(1, 3, true, true), LoadOutcome::Settled);
        assert_eq!(load_outcome(3, 3, true, true), LoadOutcome::Settled);
        // The user closed the window: stop watching it.
        assert_eq!(load_outcome(1, 3, false, false), LoadOutcome::Settled);
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
