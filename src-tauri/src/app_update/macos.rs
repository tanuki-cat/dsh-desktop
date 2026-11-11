//! The macOS half of an in-place update: unpack, verify, swap, relaunch.
//!
//! Only this platform can do it. The artifact is an `.app` bundle, and a bundle can be renamed on
//! disk while the process that came from the old one keeps running out of the copy the kernel has
//! already mapped. A Linux `.deb` needs root, and a Windows executable cannot be overwritten while
//! it is executing, so both stop after a verified download (see `apply`).

use crate::harness;
use std::path::{Path, PathBuf};

/// Inside a bundle: `Contents/MacOS/<executable>`.
pub fn bundle_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| candidate.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

/// Read a `<key>` / `<string>` pair out of an XML plist.
///
/// A plist parser would be a dependency for four keys. Every bundle in this pipeline is written
/// by Tauri as XML, and this reads exactly that shape. A binary plist (which this pipeline never
/// produces) yields `None`, and the caller then refuses the package rather than installing one it
/// cannot identify.
pub fn plist_string(plist: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let rest = plist.split(&needle).nth(1)?;
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(rest[start..end].trim().to_string())
}

/// The `.app` this process runs from, when it runs from one.
///
/// `None` for a `cargo run` binary: there is nothing to replace in that case, and saying so is
/// more useful than guessing at `target/release`.
pub fn current_bundle() -> Option<PathBuf> {
    bundle_root(&std::env::current_exe().ok()?)
}

/// Unpack `package` and return the single `.app` inside it.
///
/// `ditto` rather than `unzip`: it preserves extended attributes and symlinks, so the result is
/// the bundle the pipeline produced rather than a near copy whose framework links are broken.
pub fn unpack(package: &Path, destination: &Path) -> Result<PathBuf, String> {
    let _ = std::fs::remove_dir_all(destination);
    std::fs::create_dir_all(destination).map_err(|error| format!("无法创建解包目录: {error}"))?;
    let output = std::process::Command::new("ditto")
        .arg("-x")
        .arg("-k")
        .arg(package)
        .arg(destination)
        .output()
        .map_err(|error| format!("无法执行 ditto: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "解包失败（ditto 退出码 {:?}）: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(destination)
        .map_err(|error| format!("无法读取解包目录: {error}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
        .collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err("解包结果里没有 .app".to_string()),
        _ => Err("解包结果里有多个 .app，无法判断该安装哪一个".to_string()),
    }
}

/// Check that `bundle` is this application at `version`.
pub fn verify_bundle(bundle: &Path, version: &str) -> Result<(), String> {
    let plist_path = bundle.join("Contents").join("Info.plist");
    let plist = std::fs::read_to_string(&plist_path)
        .map_err(|error| format!("无法读取 {}: {error}", plist_path.display()))?;
    let identifier = plist_string(&plist, "CFBundleIdentifier")
        .ok_or_else(|| "Info.plist 没有 CFBundleIdentifier".to_string())?;
    if identifier != super::BUNDLE_ID {
        return Err(format!(
            "更新包的 CFBundleIdentifier 是 {identifier}，不是 {}",
            super::BUNDLE_ID
        ));
    }
    let found = plist_string(&plist, "CFBundleShortVersionString")
        .ok_or_else(|| "Info.plist 没有 CFBundleShortVersionString".to_string())?;
    if found != version {
        return Err(format!("更新包自报版本 {found}，期望 {version}"));
    }
    Ok(())
}

/// Put `package` in place of the running bundle and clear the quarantine flag.
///
/// The current bundle is moved aside first, so a failure between the two renames leaves the
/// previous installation intact under `backup_root` and the caller can put it back. The order is
/// what makes a half-finished update impossible: at every instant one of the two directories is
/// the whole application.
pub fn install(package: &Path, backup_root: &Path, version: &str) -> Result<PathBuf, String> {
    let current = current_bundle().ok_or_else(|| {
        "当前进程不是从 .app 里启动的（可能是 cargo run），无法就地安装".to_string()
    })?;
    std::fs::create_dir_all(backup_root).map_err(|error| format!("无法创建备份目录: {error}"))?;
    let staging = backup_root.join(".staging");
    // The staging tree is removed whatever the outcome: a failed attempt must not leave a
    // half-unpacked bundle behind for the next one to trip over.
    let outcome = swap_in(package, &current, backup_root, &staging, version);
    let _ = std::fs::remove_dir_all(&staging);
    outcome
}

/// Unpack, check, and swap; separated so [`install`] can clean up on every path.
fn swap_in(
    package: &Path,
    current: &Path,
    backup_root: &Path,
    staging: &Path,
    version: &str,
) -> Result<PathBuf, String> {
    let unpacked = unpack(package, staging)?;
    verify_bundle(&unpacked, version)?;

    let backup = backup_path(backup_root, current, version);
    let _ = std::fs::remove_dir_all(&backup);
    if let Err(error) = std::fs::rename(current, &backup) {
        return Err(format!("无法把当前安装挪到备份: {error}"));
    }
    if let Err(error) = std::fs::rename(&unpacked, current) {
        // Nothing else may remove the user's application: put it back before reporting.
        let restored = std::fs::rename(&backup, current);
        return Err(format!(
            "无法把新版放进安装位置: {error}{}",
            match restored {
                Ok(()) => "（已恢复原安装）".to_string(),
                Err(restore) => {
                    format!("（原安装还在 {}，恢复失败: {restore}）", backup.display())
                }
            }
        ));
    }
    // Neither the old nor the new build is signed, so a downloaded bundle carries the quarantine
    // flag and Gatekeeper would refuse to launch exactly the version the user just updated to.
    let _ = std::process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(current)
        .status();
    prune_backups(backup_root, &backup);
    harness::app_log(&format!(
        "桌面应用已安装到 {}，上一版保留在 {}",
        current.display(),
        backup.display()
    ));
    Ok(current.to_path_buf())
}

/// Where to keep the installation this update replaces.
///
/// Named after the version being **replaced**, not the one being installed: the directory holds a
/// complete copy of the old application, and a rollback has to find the version it would go back
/// to. The new version's own name would describe the wrong thing.
pub fn backup_path(backup_root: &Path, current: &Path, new_version: &str) -> PathBuf {
    let replaced = std::fs::read_to_string(current.join("Contents").join("Info.plist"))
        .ok()
        .and_then(|plist| plist_string(&plist, "CFBundleShortVersionString"))
        .filter(|value| !value.is_empty());
    let name = replaced.unwrap_or_else(|| new_version.to_string());
    backup_root.join(format!("{name}.app"))
}

/// Keep only the backup that was just made.
///
/// A bundled installation is most of a gigabyte, and only the version replaced by the newest
/// update is a rollback anything can reach: an older backup is disk space no decision uses.
fn prune_backups(backup_root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(backup_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only bundles are candidates: the staging directory lives here too.
        if path == *keep || !path.extension().is_some_and(|ext| ext == "app") {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Start the freshly installed bundle and let this process exit.
///
/// `open -n` starts a new instance even though the old one is still running; the caller then
/// exits, which reuses the ordinary shutdown path and reaps the Harness child normally.
pub fn relaunch(bundle: &Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-n")
        .arg(bundle)
        .status()
        .map(|_| ())
        .map_err(|error| format!("无法启动新版本: {error}"))
}
