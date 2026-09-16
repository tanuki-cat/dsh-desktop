//! Orchestrating a core or plugin update so a failure leaves the live tree untouched.
//!
//! `transaction` owns the tree operations; this owns the order they happen in and what
//! is recorded between them, so a launch that dies mid-update can put the previous tree back.

use crate::harness;
use crate::takeover::confirm_takeover;
use crate::transaction;
use crate::update;
use crate::{process, window, Config, ResolvedRuntime, DSH_PACKAGE_NAME, TERMINATE_GRACE};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::AppHandle;

/// Where the update transaction keeps its working copies.
///
/// All of them live under the app data directory, which is the one tree the shell owns: staging a
/// new CLI tree must never need space inside the prefix being replaced, or a full disk would
/// take the live tree down with it.
pub(crate) struct UpdatePaths {
    pub(crate) staging: PathBuf,
    pub(crate) rollback: PathBuf,
    pub(crate) swap: PathBuf,
    /// Where a profile is copied before pnpm rewrites it.
    pub(crate) profiles: PathBuf,
}

impl UpdatePaths {
    pub(crate) fn new(runtime_root: &Path) -> UpdatePaths {
        UpdatePaths {
            staging: runtime_root.join("staging"),
            rollback: runtime_root.join("rollback"),
            swap: runtime_root.join("update-swap.json"),
            profiles: runtime_root.join("profile-backup"),
        }
    }
}

/// Install a newer plugin market into the profile, with a snapshot to fall back on.
///
/// Returns whether the profile now loads a different version, which is what forces a restart.
/// pnpm owns this install — `dsh plugin add` is a thin wrapper around it — so unlike a core
/// update there is no staged tree to verify first. What makes it reversible is the snapshot
/// taken before pnpm is allowed to touch the profile: a half-written plugin tree is not
/// something the next launch can repair on its own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_market_plugin(
    app: &AppHandle,
    data_dir: &Path,
    paths: &UpdatePaths,
    profile_dir: &Path,
    resolved: &ResolvedRuntime,
    child_path: &OsStr,
    config: &Config,
    port: u16,
    from: &str,
    to: &str,
) -> bool {
    // Same hazard as a core update: pnpm rewrites the profile node_modules in place and a
    // running harness would break on its next lazy require().
    if let Err(reason) = stop_instance_before_update(app, data_dir, port, config) {
        harness::app_log(&format!(
            "plugin update deferred, keeping v{from}: {reason}"
        ));
        return false;
    }
    let snapshot = match snapshot_profile(paths, profile_dir, to) {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            // Without a snapshot the install would be one-way, and the plugin market is not
            // worth an unrecoverable profile.
            harness::app_log(&format!(
                "plugin update deferred, keeping v{from}: 无法快照 profile: {reason}"
            ));
            return false;
        }
    };
    let installed = update::install_plugin(
        &resolved.node,
        &resolved.dsh_js,
        "web",
        update::MARKET_PLUGIN,
        to,
        config.dsh_home.as_deref(),
        child_path,
    );
    match installed {
        Ok(()) => {
            let after =
                update::installed_plugin(profile_dir, update::MARKET_PLUGIN).unwrap_or_default();
            if after == from {
                // pnpm wrote the package somewhere the profile does not load it from: report
                // and remember the attempt instead of retrying on every launch.
                update::mark_plugin_attempt_ineffective(data_dir, to);
                harness::app_log(&format!(
                    "plugin installed but the profile still loads {} {from}",
                    update::MARKET_PLUGIN
                ));
                return false;
            }
            harness::app_log(&format!(
                "plugin updated: {} {from} -> {after}",
                update::MARKET_PLUGIN
            ));
            true
        }
        Err(reason) => {
            // pnpm may have rewritten the profile before it gave up: put the snapshot back so
            // the profile is what it was, rather than something neither version can load.
            let restored = restore_profile(&snapshot, profile_dir);
            // A failed install is usually transient, so it only suppresses the next attempt for
            // a few minutes — long enough not to stop the Harness again right away, short
            // enough to recover on its own (review A1/A5).
            update::mark_plugin_attempt_failed(data_dir, to);
            harness::app_log(&format!(
                "plugin update failed, keeping v{from}: {reason}{}",
                match restored {
                    Ok(()) => "（已从快照恢复 profile）".to_string(),
                    Err(error) => format!("（profile 恢复失败: {error}）"),
                }
            ));
            false
        }
    }
}

/// Copy a profile aside before pnpm rewrites it, so a failed plugin install can be undone.
///
/// Unlike the core update there is no staged tree to build first: `dsh plugin add` owns the
/// profile layout and pnpm is what installs, so the only way to make the install reversible is
/// to keep what was there. The entries the running Harness keeps writing are skipped (see
/// [`transaction::PROFILE_LIVE_ENTRIES`]): restoring stale credentials over live ones would be
/// a second, worse failure.
fn snapshot_profile(
    paths: &UpdatePaths,
    profile_dir: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let snapshot = paths.profiles.join(version_label(label));
    transaction::snapshot_tree(profile_dir, &snapshot, transaction::PROFILE_LIVE_ENTRIES)?;
    // Keep one generation per plugin version; the directory is a full copy of node_modules.
    transaction::prune(&paths.profiles, PROFILE_SNAPSHOTS);
    Ok(snapshot)
}

/// How many profile snapshots to keep. Each is a copy of the profile dependency tree, so this
/// is a size decision as much as a history one.
pub(crate) const PROFILE_SNAPSHOTS: usize = 2;

/// Put a profile snapshot back after a failed plugin install.
fn restore_profile(snapshot: &Path, profile_dir: &Path) -> Result<(), String> {
    transaction::restore_tree(snapshot, profile_dir, transaction::PROFILE_LIVE_ENTRIES)
}

/// A core update that has been staged and verified, and may now be committed.
///
/// Holding the staging guard is what makes deferring the commit safe: a launch that fails
/// before it ever boots the new tree — a busy port, a foreign Harness, a rejected version —
/// drops this value and takes the staged tree with it, leaving the live one untouched.
pub(crate) struct StagedUpdate {
    _staging: transaction::Staging,
    /// The staged package directory, about to become the live one.
    dir: PathBuf,
    version: String,
    /// The live package directory this will replace.
    target: PathBuf,
    /// Where the live tree goes while the new one proves it boots.
    backup: PathBuf,
}

/// The package directory a CLI would occupy inside `prefix`, whether or not it is there yet.
///
/// npm uses `<prefix>/lib/node_modules` on Unix and `<prefix>/node_modules` on Windows. The
/// directory is named rather than searched for because the interesting case is a prefix that has
/// no CLI yet: a bundled build installs into the shadow prefix for the first time this way.
pub(crate) fn package_dir_in(prefix: &Path) -> PathBuf {
    // The same two layouts `DSH_JS_SUFFIXES` names, minus the entry script. npm puts global
    // packages under `lib/` on Unix and directly under the prefix on Windows, and a swap has
    // to name the directory npm will actually write.
    #[cfg(windows)]
    {
        prefix.join("node_modules").join(DSH_PACKAGE_NAME)
    }
    #[cfg(not(windows))]
    {
        prefix
            .join("lib")
            .join("node_modules")
            .join(DSH_PACKAGE_NAME)
    }
}

/// Build the new CLI tree somewhere else and check it before anything is replaced.
///
/// This is the whole point of the transaction: npm writes into a directory nobody is running
/// from, and a registry that answered with the wrong version, a truncated download or a prefix
/// npm ignored is a report here instead of a broken install there.
///
/// `target_prefix` is where the tree will end up, which is not always where the running CLI
/// lives: a bundled build runs the read-only seed inside the app bundle and updates the writable
/// shadow prefix under app-data (review P0-2).
pub(crate) fn stage_core_update(
    npm: &Path,
    to: &str,
    target_prefix: &Path,
    paths: &UpdatePaths,
    cache: &Path,
) -> Result<StagedUpdate, String> {
    let staging = transaction::Staging::create(&paths.staging, to)?;
    let prefix = staging.prefix();
    update::install(npm, update::PACKAGE, to, Some(&prefix), Some(cache))?;
    let verified = transaction::verify_install(&prefix, update::PACKAGE, to)?;
    let target = package_dir_in(target_prefix);
    // A staged tree that resolves to the live tree would make the swap a no-op that still
    // reports success; saying so is better than moving a directory onto itself.
    if verified.dir == target {
        return Err(format!(
            "暂存目录与正在使用的 CLI 是同一个: {}",
            target.display()
        ));
    }
    let backup = paths.rollback.join(version_label(to));
    Ok(StagedUpdate {
        _staging: staging,
        dir: verified.dir,
        version: verified.version,
        target,
        backup,
    })
}

/// A version string that is safe as a directory name.
pub(crate) fn version_label(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Put the staged tree in place, recording the swap before the live tree is touched.
///
/// The record is what makes a launch that dies between here and the first successful boot
/// recoverable: it names the tree to put back, and [`recover_pending_swap`] reads it before
/// anything else runs.
pub(crate) fn commit_core_update(
    staged: StagedUpdate,
    paths: &UpdatePaths,
) -> Result<(PathBuf, String), String> {
    transaction::write_swap(
        &paths.swap,
        &transaction::SwapRecord {
            target: staged.target.to_string_lossy().to_string(),
            backup: staged.backup.to_string_lossy().to_string(),
            version: staged.version.clone(),
            at: update::now_secs(),
        },
    )?;
    if let Err(error) = transaction::commit(&staged.target, &staged.dir, &staged.backup) {
        // Nothing was replaced, so the record would only mislead the next launch into rolling
        // back a tree that is still the old one.
        transaction::clear_swap(&paths.swap);
        return Err(error);
    }
    // The entry script moved with its package directory, so it is named from the new location
    // rather than from the staging one it was verified in.
    let entry = staged.target.join("lib").join("bin.js");
    Ok((entry, staged.version))
}

/// Undo a swap this launch made, because the new tree never printed its startup URL.
///
/// Returns whether the previous tree is back in place. The swap record is the only source of
/// truth here: without it there is nothing to put back, and the caller says so instead of
/// claiming a rollback that did not happen.
pub(crate) fn roll_back_core_update(paths: &UpdatePaths) -> bool {
    let Some(record) = transaction::read_swap(&paths.swap) else {
        return false;
    };
    match transaction::rollback(Path::new(&record.target), Path::new(&record.backup)) {
        Ok(()) => {
            harness::app_log(&format!(
                "v{} 未能启动，已回滚到上一棵树（{}）",
                record.version, record.backup
            ));
            transaction::clear_swap(&paths.swap);
            transaction::prune(&paths.rollback, ROLLBACK_GENERATIONS);
            true
        }
        Err(error) => {
            // Keep the record: the next launch retries the rollback rather than booting a tree
            // that has already failed once.
            harness::app_log(&format!("回滚失败，保留记录以便下次重试: {error}"));
            false
        }
    }
}

/// The new tree booted: the swap is real, so the record and the older generations can go.
pub(crate) fn confirm_core_update(paths: &UpdatePaths) {
    transaction::clear_swap(&paths.swap);
    transaction::prune(&paths.rollback, ROLLBACK_GENERATIONS);
}

/// How many previous CLI trees to keep. One is enough to undo one bad update; more only costs
/// the ~290 MB each of them takes.
pub(crate) const ROLLBACK_GENERATIONS: usize = 2;

/// Remove staged trees a killed process left behind.
///
/// A staged tree only has a reason to exist inside the launch that built it: the commit moves it
/// into place or the guard drops it. One left on disk is therefore always debris from a process
/// that died mid-update, and it is ~290 MB of it. The whole directory goes rather than the
/// generations being counted, because nothing in it is referenced by anything.
pub(crate) fn clear_stale_staging(paths: &UpdatePaths) {
    let Ok(entries) = std::fs::read_dir(&paths.staging) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => harness::app_log(&format!("清理上次遗留的暂存目录 {}", path.display())),
            Err(error) => harness::app_log(&format!("无法清理 {}: {error}", path.display())),
        }
    }
}

/// Undo a swap that a previous launch made and never confirmed.
///
/// Reached at the very start of a launch, before the runtime is resolved: the tree the record
/// names is the one the previous launch replaced, and a launch that finds this file is by
/// definition one where the new tree never printed its startup URL — the process was killed, or
/// the boot timed out.
pub(crate) fn recover_pending_swap(paths: &UpdatePaths) {
    let Some(record) = transaction::read_swap(&paths.swap) else {
        return;
    };
    let target = PathBuf::from(&record.target);
    let backup = PathBuf::from(&record.backup);
    harness::app_log(&format!(
        "上次更新到 v{} 的切换没有完成（CLI 未启动成功），正在回滚到 {} 中的上一棵树",
        record.version,
        backup.display()
    ));
    match transaction::rollback(&target, &backup) {
        Ok(()) => {
            harness::app_log(&format!("已回滚 {}", target.display()));
            transaction::clear_swap(&paths.swap);
            transaction::prune(&paths.rollback, ROLLBACK_GENERATIONS);
        }
        Err(error) => {
            // Keep the record: the next launch should try again rather than leave a tree
            // nobody can boot and no record of what to put back.
            harness::app_log(&format!("回滚失败，保留记录以便下次重试: {error}"));
        }
    }
}

/// May the instance currently owning the port be stopped so an update can rewrite the CLI tree
/// it serves from? Node loads modules lazily, so updating a live tree breaks the running
/// Harness on its next `require()` — the tree must not be touched while it is in use.
///
/// `is_ours` is the state-file match and nothing else. A foreign Harness is never refused here:
/// whether it may be stopped is the user's answer to the question [`stop_instance_before_update`]
/// puts, and the config only decides what an unanswered question means. Refusing on the config
/// would skip the question for the same reason it was skipped on the startup path (2026-09-16).
fn may_stop_before_update(probe: &harness::Probe) -> Result<(), String> {
    match probe {
        harness::Probe::Closed | harness::Probe::Harness => Ok(()),
        harness::Probe::Other => Err("端口被其它程序占用，跳过本次更新".to_string()),
    }
}

/// How the instance owning the port must be stopped before an install rewrites the tree it
/// serves from.
///
/// Only a Harness this shell started lives in the process group we created; a foreign one
/// shares its group with whatever terminal or script launched it, and `process::terminate`
/// signals the whole group first (design §13.1 / review P0-3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopMode {
    ProcessGroup,
    PidOnly,
}

pub(crate) fn stop_mode(ours: bool) -> StopMode {
    if ours {
        StopMode::ProcessGroup
    } else {
        StopMode::PidOnly
    }
}

impl StopMode {
    fn describe(&self) -> &'static str {
        match self {
            StopMode::ProcessGroup => "process group, our own instance",
            StopMode::PidOnly => "pid only, external instance",
        }
    }
}

/// Stop whatever Harness owns the port, so the install cannot rewrite the tree it is serving
/// from. Returns Err with the reason when the instance must be left alone.
pub(crate) fn stop_instance_before_update(
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
    may_stop_before_update(&probe)?;

    let Some(pid) = owner else {
        return Err(format!(
            "端口 {port} 上已有 Harness，但无法确定它的进程（lsof 不可用），跳过本次更新"
        ));
    };
    // A foreign instance is somebody else session, and an update stops it for reasons that
    // have nothing to do with what they were doing: ask before touching it. Declining turns
    // this into a deferred update, which is what the config-only version used to do.
    if ours.is_none() && !confirm_takeover(app, config, port, pid) {
        return Err(format!(
            "端口 {port} 上的外部 Harness（pid {pid}）没有被接管，跳过本次更新"
        ));
    }
    let mode = stop_mode(ours.is_some());
    window::set_status(
        app,
        "更新前先停止正在运行的 Harness…",
        &format!("pid {pid}（{}）", mode.describe()),
    );
    let _ = match mode {
        StopMode::ProcessGroup => process::terminate(pid, TERMINATE_GRACE),
        StopMode::PidOnly => process::terminate_pid(pid, TERMINATE_GRACE),
    };
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
        "stopped Harness pid {pid} before updating the CLI ({})",
        mode.describe()
    ));
    Ok(())
}

#[cfg(test)]
mod tests;
