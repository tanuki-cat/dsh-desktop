//! Staged installs, integrity checks, atomic swaps and rollback.
//!
//! Two dependency trees get rewritten while the shell runs: the CLI it supervises, and — when
//! `auto_update_plugins` is on — the profile's plugin tree. npm and pnpm both rewrite in place,
//! so an install that fails half-way used to leave a tree that is neither the old version nor
//! the new one, with the Harness already stopped and nothing to fall back on (review P2-10).
//!
//! Every install here goes the same way: build a complete tree somewhere else, verify it really
//! is this package at the requested version, move the live tree aside, then move the staged one
//! in. Anything that fails before the last step leaves the live tree untouched, and that last
//! step is a rename within one directory tree.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One staging directory, removed when the attempt ends however it ends.
///
/// A successful commit *moves* the staged tree into place, so what the drop removes is only the
/// leftover prefix shell (npm's `bin/`, a half-written cache) — never the tree now in use.
pub struct Staging {
    root: PathBuf,
}

impl Staging {
    /// A fresh, empty staging directory for one install attempt.
    ///
    /// The name carries the version so a leftover from a killed process is identifiable, and the
    /// directory is removed first: a previous attempt's half-written tree must never be verified
    /// and committed as if it were this one.
    pub fn create(root: &Path, label: &str) -> Result<Staging, String> {
        let safe: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let dir = root.join(format!("staging-{safe}"));
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("无法清理上次的暂存目录 {}: {error}", dir.display()));
            }
        }
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("无法创建暂存目录 {}: {error}", dir.display()))?;
        Ok(Staging { root: dir })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The npm prefix to install into: the staged tree's own prefix, never the live one.
    pub fn prefix(&self) -> PathBuf {
        self.root.join("prefix")
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Where npm puts one global package inside a prefix, on either platform.
///
/// Unix uses `<prefix>/lib/node_modules`, Windows `<prefix>/node_modules`; a staged tree is
/// read back on the platform that wrote it, so only the native layout has to be found.
pub fn package_dir(prefix: &Path, package: &str) -> Option<PathBuf> {
    ["lib/node_modules", "node_modules"]
        .iter()
        .map(|root| prefix.join(root).join(package))
        .find(|dir| dir.is_dir())
}

/// The version of the package installed at `dir`, and only when the manifest names `package`.
///
/// The name check is what makes this an identity check rather than a version read: npm can be
/// pointed at a prefix holding something else entirely, and committing that over the supervised
/// CLI is the failure this guards against.
pub fn version_of_package(dir: &Path, package: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if manifest.get("name")?.as_str()? != package {
        return None;
    }
    let version = manifest.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// What a verified staged install turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The staged package directory, ready to be moved into the live prefix.
    pub dir: PathBuf,
    /// The CLI entry script inside it, as the shell would spawn it.
    pub entry: PathBuf,
    pub version: String,
}

/// Check that a staged prefix really holds `package` at `expected`, and name what is missing.
///
/// Runs before anything live is touched, so a registry that answered with a different version, a
/// truncated download or a prefix npm ignored is a report instead of a broken install.
pub fn verify_install(prefix: &Path, package: &str, expected: &str) -> Result<Verified, String> {
    let dir = package_dir(prefix, package)
        .ok_or_else(|| format!("暂存目录里没有 {package}（{}）", prefix.display()))?;
    let version = version_of_package(&dir, package)
        .ok_or_else(|| format!("暂存目录的 {package} 没有可读的 package.json 或名字不符"))?;
    let entry = dir.join("lib").join("bin.js");
    if !entry.is_file() {
        return Err(format!(
            "暂存目录的 {package} 缺入口脚本 {}",
            entry.display()
        ));
    }
    if version != expected {
        return Err(format!("暂存目录装的是 {version}，不是请求的 {expected}"));
    }
    Ok(Verified {
        dir,
        entry,
        version,
    })
}

/// Move a path, falling back to copy-then-remove when the two ends are on different filesystems.
///
/// `rename` is what makes the swap atomic, so it is always tried first. The fallback is not
/// atomic and is only reached when the rollback directory and the live prefix sit on different
/// volumes — a configuration the user chose by pointing `dsh_path` at another disk.
pub fn move_path(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            copy_tree(from, to).map_err(|error| {
                format!(
                    "无法移动 {} 到 {}（rename: {rename_error}；copy: {error}）",
                    from.display(),
                    to.display()
                )
            })?;
            std::fs::remove_dir_all(from)
                .map_err(|error| format!("复制完成但无法删除 {}: {error}", from.display()))
        }
    }
}

/// Put `staged` in place of `target`, keeping the previous tree at `backup`.
///
/// The old tree is moved out of the way first rather than deleted: between the two renames
/// `target` does not exist, which is a much smaller window than an install, and `backup` is
/// what a rollback needs if the new tree turns out not to boot.
pub fn commit(target: &Path, staged: &Path, backup: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
    }
    let _ = std::fs::remove_dir_all(backup);
    let had_previous = target.exists();
    if had_previous {
        move_path(target, backup)?;
    }
    match move_path(staged, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Put the old tree back rather than leaving the target missing: a failed commit must
            // not be worse than not having tried.
            if had_previous {
                if let Err(restore) = move_path(backup, target) {
                    return Err(format!(
                        "{error}\n而且无法把原目录放回 {}: {restore}",
                        target.display()
                    ));
                }
            }
            Err(error)
        }
    }
}

/// Put `backup` back as `target`, replacing whatever is there.
///
/// Used when the swapped-in tree never proved it can boot. The tree that failed is kept beside
/// the restored one instead of being deleted: it is the only evidence of what went wrong, and
/// the next update attempt overwrites it anyway.
pub fn rollback(target: &Path, backup: &Path) -> Result<(), String> {
    rollback_with(target, backup, None)
}

/// [`rollback`] with the swap record's knowledge of whether a backup was ever written.
///
/// `had_previous == Some(false)` is the bundled first update: the tree being replaced had no
/// predecessor, so there is nothing to restore and the failed tree is simply moved aside. The
/// seed inside the app takes over again, which is the correct "previous version".
pub fn rollback_with(
    target: &Path,
    backup: &Path,
    had_previous: Option<bool>,
) -> Result<(), String> {
    if !backup.exists() {
        if had_previous == Some(false) {
            // Nothing to put back: discard the tree that failed to boot so the seed is used
            // again. Kept as `.failed` like any other rollback, for the same diagnostic reason.
            if target.exists() {
                let failed = failed_path(target);
                let _ = std::fs::remove_dir_all(&failed);
                move_path(target, &failed)?;
            }
            return Ok(());
        }
        return Err(format!("回滚目录 {} 不存在", backup.display()));
    }
    let failed = failed_path(target);
    let _ = std::fs::remove_dir_all(&failed);
    if target.exists() {
        move_path(target, &failed)?;
    }
    match move_path(backup, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            if failed.exists() {
                let _ = move_path(&failed, target);
            }
            Err(error)
        }
    }
}

/// Where the tree that failed to boot is kept while the last-known-good one is restored.
pub fn failed_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".failed");
    target.with_file_name(name)
}

/// Delete all but the newest `keep` generations of a rollback directory.
///
/// Entries are named after the version they hold, and the ordering is **semantic**, not
/// lexicographic: a plain string sort puts `0.10.0` below `0.9.0` and would delete the newest
/// generation — the one thing this directory exists to keep. Names that are not versions sort
/// last, so a directory someone created by hand goes before a real generation does.
pub fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut generations: Vec<(Option<crate::update::Version>, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            (crate::update::Version::parse(&label), path)
        })
        .collect();
    if generations.len() <= keep {
        return;
    }
    // Newest first; a name that is not a version goes to the end of the list.
    generations.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(a), Some(b)) => b.cmp(a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    for (_, path) in &generations[keep..] {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// A swap that has happened but whose tree has not yet proved it can boot.
///
/// Written before the live tree is replaced and removed once the CLI prints its startup URL. A
/// launch that finds this file is a launch whose predecessor swapped a tree and never got that
/// far — the process was killed, or the boot timed out — so the last-known-good tree goes back
/// before anything else runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwapRecord {
    /// The live package directory that was replaced.
    pub target: String,
    /// Where the previous tree was moved to.
    pub backup: String,
    pub version: String,
    pub at: u64,
    /// Whether there was a previous tree to move to `backup` at all.
    ///
    /// False on the first update of a bundled install: the CLI then runs from the read-only seed
    /// inside the app and the shadow prefix is still empty, so `commit` has nothing to move and
    /// `backup` never comes into existence. A rollback must delete the failed tree rather than
    /// look for a backup that was never written — otherwise the bad tree stays in place, the
    /// record is kept "for the next launch", and every launch from then on fails the same way.
    ///
    /// `Option` so records written before this field existed still parse; `None` means "assume
    /// there was one", which keeps the old behaviour of refusing to delete a user's install.
    #[serde(default)]
    pub had_previous: Option<bool>,
}

pub fn read_swap(path: &Path) -> Option<SwapRecord> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_swap(path: &Path, record: &SwapRecord) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建 {}: {error}", parent.display()))?;
    }
    let raw = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    std::fs::write(path, raw).map_err(|error| format!("无法写入 {}: {error}", path.display()))
}

pub fn clear_swap(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Entries of a dsh profile that belong to the running Harness rather than to the plugin tree.
///
/// Credentials, session state and the market's own log are written continuously while the
/// Harness runs. Copying them into a snapshot would restore stale credentials over live ones,
/// and copying them back would undo everything the user did during the install.
pub const PROFILE_LIVE_ENTRIES: &[&str] = &["data", ".dsh-market"];

/// Copy the dependency tree of a profile aside, skipping the entries the Harness keeps writing.
pub fn snapshot_tree(from: &Path, to: &Path, skip: &[&str]) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).map_err(|error| format!("无法创建 {}: {error}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .map_err(|error| format!("无法读取 {}: {error}", from.display()))?
        .flatten()
    {
        let name = entry.file_name();
        if skip.iter().any(|skip| name == std::ffi::OsStr::new(skip)) {
            continue;
        }
        // `copy_entry`, not `is_dir` + `fs::copy`: a symlink is neither, and following one is
        // what made a real profile unsnapshottable.
        let result = copy_entry(&entry.path(), &to.join(&name));
        if let Err(error) = result {
            return Err(format!("无法快照 {}: {error}", entry.path().display()));
        }
    }
    Ok(())
}

/// Put a snapshot back, replacing every entry the install may have rewritten.
///
/// Only the snapshot's own entries are touched, so the live entries it skipped stay exactly as
/// the running Harness left them.
pub fn restore_tree(from: &Path, to: &Path, skip: &[&str]) -> Result<(), String> {
    let snapshot: Vec<std::ffi::OsString> = std::fs::read_dir(from)
        .map_err(|error| format!("无法读取快照 {}: {error}", from.display()))?
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    for entry in std::fs::read_dir(to)
        .map_err(|error| format!("无法读取 {}: {error}", to.display()))?
        .flatten()
    {
        let name = entry.file_name();
        if skip.iter().any(|skip| name == std::ffi::OsStr::new(skip)) {
            continue;
        }
        if !snapshot.contains(&name) {
            continue;
        }
        let path = entry.path();
        // A symlink to a directory is removed with `remove_file`: `remove_dir_all` would
        // follow it and delete the target's contents.
        let is_link = std::fs::symlink_metadata(&path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        let removed = if !is_link && path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(|error| format!("无法移除 {}: {error}", path.display()))?;
    }
    for name in snapshot {
        let source = from.join(&name);
        let target = to.join(&name);
        copy_entry(&source, &target)
            .map_err(|error| format!("无法恢复 {}: {error}", target.display()))?;
    }
    Ok(())
}

/// Copy one entry, preserving a symlink as a symlink.
///
/// `DirEntry::file_type` does not follow links, so a link is neither `is_dir` nor a regular file,
/// and `fs::copy` on one follows it: a link to a directory fails outright ("neither a regular
/// file nor a symlink to a regular file"), and a link to a file is materialised as a real copy.
/// A profile is full of both — pnpm puts every dependency under `.dsh-module-fallback/` as a link
/// into `node_modules/`, and `.bin/*` are links to files — so a snapshot of a real profile failed
/// on the first directory link and left the plugin install with no rollback.
pub(crate) fn copy_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        // A relative target is relative to the directory holding the link, not to this process.
        let base = from.parent().unwrap_or_else(|| Path::new(""));
        return symlink(&target, &base.join(&target), to);
    }
    if meta.is_dir() {
        return copy_tree(from, to);
    }
    std::fs::copy(from, to).map(|_| ())
}

/// Create a symlink to `target` at `to`, replacing whatever is already there.
///
/// `target` is written verbatim, so a relative link stays relative. `resolved` is the same target
/// seen from the link's own directory, for the one place the target has to be inspected.
#[cfg(unix)]
fn symlink(target: &Path, _resolved: &Path, to: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(to);
    let _ = std::fs::remove_dir_all(to);
    std::os::unix::fs::symlink(target, to)
}

/// Windows needs a privilege for a file symlink and a different call for a directory one.
///
/// Falling back to a real copy keeps the snapshot usable when the privilege is absent (a normal
/// user without Developer Mode); the tree then still restores, it just stops sharing inodes.
///
/// `resolved` is what the link points at, joined to the link's directory: canonicalising the bare
/// `target` resolved a relative link against this process's working directory instead, so the
/// fallback copied an unrelated file or failed.
#[cfg(windows)]
fn symlink(target: &Path, resolved: &Path, to: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(to);
    let _ = std::fs::remove_dir_all(to);
    let resolved = std::fs::canonicalize(resolved).unwrap_or_else(|_| resolved.to_path_buf());
    let created = if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, to)
    } else {
        std::os::windows::fs::symlink_file(target, to)
    };
    match created {
        Ok(()) => Ok(()),
        Err(_) if resolved.is_dir() => copy_tree(&resolved, to),
        Err(_) => std::fs::copy(&resolved, to).map(|_| ()),
    }
}

/// Recursive copy into a path that does not exist yet.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        copy_entry(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
