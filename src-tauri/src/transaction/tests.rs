//! Unit tests for the transaction module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

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
