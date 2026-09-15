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
    if !backup.exists() {
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
        let target = to.join(&name);
        let result = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_tree(&entry.path(), &target)
        } else {
            std::fs::copy(entry.path(), &target).map(|_| ())
        };
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
        let removed = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(|error| format!("无法移除 {}: {error}", path.display()))?;
    }
    for name in snapshot {
        let source = from.join(&name);
        let target = to.join(&name);
        let result = if source.is_dir() {
            copy_tree(&source, &target)
        } else {
            std::fs::copy(&source, &target).map(|_| ())
        };
        result.map_err(|error| format!("无法恢复 {}: {error}", target.display()))?;
    }
    Ok(())
}

/// Recursive copy into a path that does not exist yet.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A private directory per test: these write real trees, and the process-global temp dir is
    /// shared with every other test in the binary.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-desktop-txn-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake npm prefix holding `package` at `version`, with the entry script npm creates.
    fn fake_prefix(prefix: &Path, package: &str, version: &str) {
        let dir = prefix.join("lib").join("node_modules").join(package);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!("{{\"name\":\"{package}\",\"version\":\"{version}\"}}"),
        )
        .unwrap();
        std::fs::write(dir.join("lib").join("bin.js"), "// cli\n").unwrap();
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_staged_tree_is_verified_by_name_and_version_before_it_is_used() {
        let root = scratch("verify");
        let prefix = root.join("prefix");
        fake_prefix(&prefix, "@deepseek-ai/dsh", "0.1.6");

        let verified = verify_install(&prefix, "@deepseek-ai/dsh", "0.1.6").unwrap();
        assert_eq!(verified.version, "0.1.6");
        assert!(verified.entry.is_file());

        // The registry answering with another version must not become a silent downgrade.
        let mismatch = verify_install(&prefix, "@deepseek-ai/dsh", "0.1.7").unwrap_err();
        assert!(mismatch.contains("0.1.6"), "{mismatch}");

        // A prefix holding a different package is not this CLI, whatever version it carries.
        let other = root.join("other");
        fake_prefix(&other, "left-pad", "0.1.6");
        assert!(verify_install(&other, "@deepseek-ai/dsh", "0.1.6").is_err());

        // Nothing installed at all.
        assert!(verify_install(&root.join("empty"), "@deepseek-ai/dsh", "0.1.6").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tree_without_its_entry_script_is_not_a_usable_install() {
        let root = scratch("entry");
        let prefix = root.join("prefix");
        fake_prefix(&prefix, "@deepseek-ai/dsh", "0.1.6");
        std::fs::remove_file(prefix.join("lib/node_modules/@deepseek-ai/dsh/lib/bin.js")).unwrap();
        assert!(verify_install(&prefix, "@deepseek-ai/dsh", "0.1.6").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_commit_replaces_the_live_tree_and_keeps_the_previous_one() {
        let root = scratch("commit");
        let target = root.join("live");
        let staged = root.join("staged");
        let backup = root.join("backup");
        write(&target.join("marker"), "old");
        write(&staged.join("marker"), "new");

        commit(&target, &staged, &backup).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("marker")).unwrap(),
            "old"
        );
        assert!(!staged.exists(), "the staged tree was moved, not copied");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_commit_that_cannot_move_the_staged_tree_leaves_the_old_one_in_place() {
        let root = scratch("commit-fail");
        let target = root.join("live");
        let backup = root.join("backup");
        write(&target.join("marker"), "old");

        // The staged tree does not exist: the first rename succeeds (the live tree moves aside)
        // and the second must fail, which is exactly when the old tree has to come back.
        let error = commit(&target, &root.join("missing"), &backup).unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "old"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rollback_puts_the_previous_tree_back_and_keeps_the_failed_one() {
        let root = scratch("rollback");
        let target = root.join("live");
        let backup = root.join("backup");
        write(&target.join("marker"), "broken");
        write(&backup.join("marker"), "good");

        rollback(&target, &backup).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "good"
        );
        // The tree that failed is kept as evidence rather than deleted.
        assert_eq!(
            std::fs::read_to_string(failed_path(&target).join("marker")).unwrap(),
            "broken"
        );
        // Rolling back without a backup is an error, not a silent no-op.
        assert!(rollback(&target, &root.join("gone")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pruning_keeps_the_newest_generations() {
        let root = scratch("prune");
        for name in ["0.1.4", "0.1.5", "0.1.6"] {
            write(&root.join(name).join("marker"), name);
        }
        prune(&root, 2);
        assert!(!root.join("0.1.4").exists());
        assert!(root.join("0.1.5").exists());
        assert!(root.join("0.1.6").exists());
        // Asking to keep more than exists is not an error.
        prune(&root, 9);
        assert!(root.join("0.1.5").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The generation a rollback would need is the newest one, and a plain string sort gets that
    /// wrong the moment a version crosses a digit boundary.
    #[test]
    fn pruning_orders_generations_by_version_not_by_name() {
        let root = scratch("prune-order");
        for name in ["0.9.0", "0.10.0", "0.11.0-rc.1"] {
            write(&root.join(name).join("marker"), name);
        }
        prune(&root, 2);
        // Lexicographically "0.10.0" < "0.9.0"; by version it is newer and must survive.
        assert!(!root.join("0.9.0").exists(), "the oldest generation goes");
        assert!(root.join("0.10.0").exists());
        assert!(root.join("0.11.0-rc.1").exists());

        // A directory that is not a version at all is dropped before a real generation.
        write(&root.join("staging-leftover").join("marker"), "junk");
        prune(&root, 2);
        assert!(!root.join("staging-leftover").exists());
        assert!(root.join("0.10.0").exists());
        assert!(root.join("0.11.0-rc.1").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_profile_snapshot_skips_what_the_running_harness_keeps_writing() {
        let root = scratch("snapshot");
        let profile = root.join("profile");
        write(&profile.join("package.json"), "{\"name\":\"web\"}");
        write(&profile.join("node_modules/market/package.json"), "{}");
        write(&profile.join("data/usage.json"), "live");
        write(&profile.join(".dsh-market/log.ndjson"), "live");

        let snapshot = root.join("snapshot");
        snapshot_tree(&profile, &snapshot, PROFILE_LIVE_ENTRIES).unwrap();
        assert!(snapshot.join("package.json").is_file());
        assert!(snapshot.join("node_modules/market/package.json").is_file());
        assert!(!snapshot.join("data").exists());
        assert!(!snapshot.join(".dsh-market").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restoring_a_profile_snapshot_puts_the_old_tree_back_and_leaves_live_state_alone() {
        let root = scratch("restore");
        let profile = root.join("profile");
        write(&profile.join("package.json"), "{\"name\":\"web\"}");
        write(&profile.join("node_modules/market/version"), "1.0.0");
        write(&profile.join("data/usage.json"), "before");

        let snapshot = root.join("snapshot");
        snapshot_tree(&profile, &snapshot, PROFILE_LIVE_ENTRIES).unwrap();

        // The install rewrote the tree and the Harness wrote to `data/` while it ran.
        write(&profile.join("node_modules/market/version"), "2.0.0");
        write(&profile.join("node_modules/market/junk"), "half-written");
        write(&profile.join("data/usage.json"), "after");

        restore_tree(&snapshot, &profile, PROFILE_LIVE_ENTRIES).unwrap();
        assert_eq!(
            std::fs::read_to_string(profile.join("node_modules/market/version")).unwrap(),
            "1.0.0"
        );
        assert!(!profile.join("node_modules/market/junk").exists());
        // Live state is not rolled back: it belongs to the session, not to the install.
        assert_eq!(
            std::fs::read_to_string(profile.join("data/usage.json")).unwrap(),
            "after"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_staging_directory_is_cleared_before_use_and_removed_afterwards() {
        let root = scratch("staging");
        let path = {
            let staging = Staging::create(&root, "0.1.6").unwrap();
            let path = staging.path().to_path_buf();
            assert!(path.is_dir());
            // A previous attempt's leftovers must not be verified as this one's.
            write(&path.join("prefix/leftover"), "stale");
            drop(staging);
            path
        };
        assert!(!path.exists(), "the staging directory is removed on drop");

        let staging = Staging::create(&root, "0.1.6").unwrap();
        assert!(!staging.path().join("prefix/leftover").exists());
        assert!(staging.prefix().starts_with(staging.path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_swap_record_survives_a_round_trip_and_a_missing_file_is_not_a_record() {
        let root = scratch("swap");
        let path = root.join("update-swap.json");
        assert!(read_swap(&path).is_none());

        let record = SwapRecord {
            target: "/data/runtime/prefix/lib/node_modules/@deepseek-ai/dsh".to_string(),
            backup: "/data/runtime/rollback/0.1.5".to_string(),
            version: "0.1.6".to_string(),
            at: 1_700_000_000,
        };
        write_swap(&path, &record).unwrap();
        assert_eq!(read_swap(&path).unwrap(), record);

        clear_swap(&path);
        assert!(read_swap(&path).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
