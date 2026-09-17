//! Unit tests for the transaction module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

/// A private directory per test: these write real trees, and the process-global temp dir is
/// shared with every other test in the binary.
fn scratch(name: &str) -> PathBuf {
    let dir = crate::test_dir(&format!("dsh-desktop-txn-{name}"));
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
        had_previous: Some(true),
    };
    write_swap(&path, &record).unwrap();
    assert_eq!(read_swap(&path).unwrap(), record);

    clear_swap(&path);
    assert!(read_swap(&path).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// A real profile is full of symlinks — pnpm puts every dependency under `.dsh-module-fallback/`
/// as a link into `node_modules/`, and `.bin/*` are links to files. `fs::copy` follows them, so a
/// link to a directory failed the whole snapshot and a link to a file was materialised.
#[cfg(unix)]
#[test]
fn a_snapshot_preserves_symlinks() {
    let root = scratch("snapshot-symlinks");
    let live = root.join("live");
    let snap = root.join("snap");
    std::fs::create_dir_all(live.join("node_modules/pkg")).unwrap();
    std::fs::write(live.join("node_modules/pkg/index.js"), "x").unwrap();
    std::fs::create_dir_all(live.join("node_modules/.bin")).unwrap();
    std::fs::write(live.join("node_modules/.bin/tool.js"), "y").unwrap();
    // The two shapes a pnpm tree has: a link to a directory, and a link to a file.
    std::os::unix::fs::symlink("node_modules/pkg", live.join("fallback-pkg")).unwrap();
    std::os::unix::fs::symlink("../tool.js", live.join("node_modules/.bin/tool")).unwrap();

    snapshot_tree(&live, &snap, &[]).expect("a profile with symlinks must snapshot");

    for name in ["fallback-pkg", "node_modules/.bin/tool"] {
        let meta = std::fs::symlink_metadata(snap.join(name)).unwrap();
        assert!(meta.file_type().is_symlink(), "{name} was materialised");
    }
    assert_eq!(
        std::fs::read_link(snap.join("fallback-pkg")).unwrap(),
        Path::new("node_modules/pkg"),
        "the link target must be preserved verbatim"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The reproduction from the review: a real pnpm profile has links under `.dsh-module-fallback/`
/// (to directories) and in `node_modules/.bin/` (to files). Snapshotted against the live profile
/// when one exists, so the shapes are the real ones rather than a fixture guess.
#[cfg(unix)]
#[test]
fn a_real_profile_snapshots_when_one_is_present() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let profile = PathBuf::from(home).join(".dsh/profiles/web");
    if !profile.is_dir() {
        // A machine without a profile is not a failure; the fixture test above covers the shapes.
        return;
    }
    let root = scratch("real-profile-snapshot");
    let dest = root.join("snap");
    snapshot_tree(&profile, &dest, PROFILE_LIVE_ENTRIES)
        .expect("snapshotting a real profile must not fail");
    assert!(dest.join("node_modules").is_dir() || dest.join("package.json").is_file());
    let _ = std::fs::remove_dir_all(&root);
}

/// Restoring must put the links back, and removing a link to a directory must not follow it.
#[cfg(unix)]
#[test]
fn a_restore_puts_symlinks_back_without_following_them() {
    let root = scratch("restore-symlinks");
    let live = root.join("live");
    let snap = root.join("snap");
    std::fs::create_dir_all(live.join("real")).unwrap();
    std::fs::write(live.join("real/keep.txt"), "keep").unwrap();
    std::fs::create_dir_all(&snap).unwrap();
    std::os::unix::fs::symlink("real", snap.join("dirlink")).unwrap();
    std::os::unix::fs::symlink("real", live.join("dirlink")).unwrap();

    restore_tree(&snap, &live, &[]).expect("restore must handle a directory link");

    assert!(std::fs::symlink_metadata(live.join("dirlink"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(
        live.join("real/keep.txt").is_file(),
        "removing the old link must not delete what it pointed at"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The bundled first update: the shadow prefix is empty, so `commit` had no previous tree to move
/// and `backup` was never written. A rollback then used to fail, leaving the tree that would not
/// boot in place and the record on disk — so every later launch repeated the same failure.
#[test]
fn a_first_update_with_no_backup_discards_the_failed_tree() {
    let root = scratch("rollback-first-update");
    let target = root.join("prefix");
    let staged = root.join("staging/0.1.6");
    let backup = root.join("rollback/0.1.5");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("bin.js"), "broken").unwrap();
    assert!(!target.exists(), "the shadow prefix starts empty");

    commit(&target, &staged, &backup).unwrap();
    assert!(
        target.exists() && !backup.exists(),
        "nothing was moved to backup"
    );

    // The tree never booted, so this is the rollback the launch performs.
    rollback_with(&target, &backup, Some(false)).expect("a first update must still roll back");
    assert!(
        !target.exists(),
        "the failed tree must be gone so the seed is used again"
    );
    assert!(failed_path(&target).is_dir(), "it is kept as evidence");
    let _ = std::fs::remove_dir_all(&root);
}

/// A record written before `had_previous` existed must keep the old, safe behaviour: never delete
/// a tree just because a backup is missing.
#[test]
fn a_legacy_record_still_refuses_to_delete_the_install() {
    let root = scratch("rollback-legacy");
    let target = root.join("prefix");
    let backup = root.join("rollback/0.1.5");
    std::fs::create_dir_all(&target).unwrap();

    assert!(rollback_with(&target, &backup, None).is_err());
    assert!(
        target.exists(),
        "an unknown history must not delete anything"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The normal case still restores the previous tree rather than discarding the new one.
#[test]
fn a_rollback_with_a_backup_restores_it() {
    let root = scratch("rollback-restore");
    let target = root.join("prefix");
    let staged = root.join("staging/0.1.6");
    let backup = root.join("rollback/0.1.5");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("old.js"), "old").unwrap();
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("new.js"), "new").unwrap();

    commit(&target, &staged, &backup).unwrap();
    assert!(backup.exists(), "a previous tree was moved aside");
    rollback_with(&target, &backup, Some(true)).unwrap();
    assert!(target.join("old.js").is_file(), "the old tree is back");
    let _ = std::fs::remove_dir_all(&root);
}
