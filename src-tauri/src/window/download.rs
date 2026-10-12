//! User-authorized download destinations. No page command or capability is exposed.

use std::path::{Path, PathBuf};

/// WebKit requires a new file, and must not overwrite an existing file or symlink.
#[cfg(any(target_os = "macos", test))]
fn validate_destination(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err("请选择完整的文件保存路径。".into());
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err("目标已存在，请换一个文件名，避免覆盖已有文件。".into()),
        Err(error) => Err(format!("无法访问保存位置：{error}")),
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn destination(suggested: &Path, serial: u64) -> Result<Option<PathBuf>, String> {
    let dir = super::downloads_dir().ok_or("找不到下载目录，请检查用户目录设置。")?;
    Ok(Some(super::unique_download_path(
        &dir,
        &super::file_name_of(suggested),
        serial,
    )))
}

/// Selecting a path through AppKit grants the file access WebKit's network process needs.
/// A fabricated ~/Downloads path alone does not grant that sandbox extension.
#[cfg(target_os = "macos")]
pub(super) fn destination(suggested: &Path, serial: u64) -> Result<Option<PathBuf>, String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseCancel, NSModalResponseOK, NSSavePanel};
    use objc2_foundation::{NSString, NSURL};

    let mtm = MainThreadMarker::new().ok_or("下载保存对话框必须在主线程打开。")?;
    let panel = NSSavePanel::savePanel(mtm);
    panel.setTitle(Some(&NSString::from_str("保存下载文件")));
    panel.setCanCreateDirectories(true);
    let name = super::file_name_of(suggested);
    let default = super::downloads_dir()
        .map(|dir| super::unique_download_path(&dir, &name, serial))
        .unwrap_or_else(|| PathBuf::from(&name));
    panel.setNameFieldStringValue(&NSString::from_str(
        &super::file_name_of(&default).to_string_lossy(),
    ));
    if let Some(dir) = default.parent().filter(|p| p.is_absolute()) {
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&dir.to_string_lossy()), true);
        panel.setDirectoryURL(Some(&url));
    }
    loop {
        let response = panel.runModal();
        if response == NSModalResponseCancel {
            return Ok(None);
        }
        if response != NSModalResponseOK {
            return Err("无法打开保存对话框，请重试下载。".into());
        }
        let path = panel
            .URL()
            .and_then(|url| url.path())
            .ok_or("保存对话框未返回文件路径。")?;
        let path = PathBuf::from(path.to_string());
        match validate_destination(&path) {
            Ok(()) => return Ok(Some(path)),
            Err(message) => panel.setMessage(Some(&NSString::from_str(&message))),
        }
    }
}

/// macOS does not supply a path on completion, even when the download succeeds.
pub(super) fn completion_message(success: bool, path: Option<&Path>) -> String {
    let status = if success {
        "download succeeded"
    } else {
        "download failed or cancelled"
    };
    match path {
        Some(path) => format!("{status}: {}", path.display()),
        None => format!("{status} (WebView supplied no path)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_completion_preserves_the_success_flag_without_a_path() {
        assert!(completion_message(true, None).starts_with("download succeeded"));
        assert!(completion_message(false, None).starts_with("download failed or cancelled"));
        assert!(completion_message(false, Some(Path::new("/test.png"))).contains("/test.png"));
    }

    #[test]
    fn download_destination_rejects_existing_files_and_relative_paths() {
        let dir = crate::test_dir("download-save-panel");
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("existing.png");
        std::fs::write(&existing, b"keep me").unwrap();
        assert!(validate_destination(&existing).is_err());
        assert!(validate_destination(&dir).is_err());
        assert!(validate_destination(Path::new("relative.png")).is_err());
        assert!(validate_destination(&dir.join("new.png")).is_ok());
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn download_destination_rejects_dangling_symlinks() {
        let dir = crate::test_dir("download-save-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("image.png");
        std::os::unix::fs::symlink(dir.join("missing"), &link).unwrap();
        assert!(validate_destination(&link).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
