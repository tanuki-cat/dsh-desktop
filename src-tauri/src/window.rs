//! Splash window, Harness window, and navigation policy.

use crate::harness;
use serde_json::json;
use std::path::PathBuf;
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use url::Url;

pub const SPLASH: &str = "splash";
pub const HARNESS: &str = "harness";

pub fn create_splash(app: &AppHandle) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, SPLASH, WebviewUrl::App("index.html".into()))
        .title("DeepSeek Harness")
        .inner_size(460.0, 300.0)
        .resizable(false)
        .build()?;
    Ok(())
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

/// The Harness page is remote content: it gets no capability, and navigation is fenced
/// to the current loopback authority. Everything else opens in the system browser.
pub fn create_harness(app: &AppHandle, url: &Url, port: u16) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(HARNESS) {
        let _ = existing.close();
    }
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
        // Downloads land in the user's Downloads folder instead of vanishing.
        .on_download(move |_webview, event| {
            match event {
                DownloadEvent::Requested { url, destination } => {
                    *destination = downloads_dir().join(file_name_of(destination));
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

    // Closing the Harness window quits the app, which stops the supervised process.
    let handle = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            handle.exit(0);
        }
    });

    if let Some(splash) = app.get_webview_window(SPLASH) {
        let _ = splash.close();
    }
    Ok(())
}

fn downloads_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Downloads");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn file_name_of(suggested: &std::path::Path) -> std::ffi::OsString {
    suggested
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("dsh-download"))
}

pub fn open_external(target: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "cmd";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    command.args(["/C", "start", ""]);
    let _ = command.arg(target).spawn();
}
