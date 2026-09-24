//! Unit tests for the update_flow module.
//!
//! A child module of the code under test, so `use super::*` reaches private
//! items.

use super::*;

/// A staged update is verified and swapped, and a launch that never confirmed it is undone by
/// the next one. This is the whole P2-10 contract, minus the npm call.
#[test]
fn a_staged_update_swaps_the_tree_and_an_unconfirmed_one_is_rolled_back() {
    let root = crate::test_dir("dsh-desktop-core-swap-test");
    let _ = std::fs::remove_dir_all(&root);
    let runtime = root.join("runtime");
    let paths = UpdatePaths::new(&runtime);
    std::fs::create_dir_all(&paths.staging).unwrap();
    std::fs::create_dir_all(&paths.rollback).unwrap();

    // The live tree, in the shadow prefix layout the shell updates.
    let prefix = runtime.join("prefix");
    let live = prefix.join("lib/node_modules/@deepseek-ai/dsh");
    std::fs::create_dir_all(live.join("lib")).unwrap();
    std::fs::write(live.join("lib/bin.js"), "old\n").unwrap();
    std::fs::write(live.join("package.json"), r#"{"name":"@deepseek-ai/dsh"}"#).unwrap();

    // What `stage_core_update` leaves behind once npm and the verifier are done: a complete
    // tree inside the staging prefix, verified by name and version.
    let staging = transaction::Staging::create(&paths.staging, "0.1.6").unwrap();
    let staged_dir = staging.prefix().join("lib/node_modules/@deepseek-ai/dsh");
    std::fs::create_dir_all(staged_dir.join("lib")).unwrap();
    std::fs::write(staged_dir.join("lib/bin.js"), "new\n").unwrap();
    std::fs::write(
        staged_dir.join("package.json"),
        r#"{"name":"@deepseek-ai/dsh","version":"0.1.6"}"#,
    )
    .unwrap();
    let verified =
        transaction::verify_install(&staging.prefix(), "@deepseek-ai/dsh", "0.1.6").unwrap();
    assert_eq!(verified.version, "0.1.6");
    let staged = StagedUpdate {
        _staging: staging,
        dir: verified.dir,
        version: verified.version,
        target: live.clone(),
        backup: paths.rollback.join("0.1.6"),
    };

    let (entry, version) = commit_core_update(staged, &paths).unwrap();
    assert_eq!(version, "0.1.6");
    // The entry script moved with its package directory: the spawn uses this path.
    assert!(entry.is_file());
    assert!(entry.starts_with(&live), "{}", entry.display());
    assert_eq!(
        std::fs::read_to_string(live.join("lib/bin.js")).unwrap(),
        "new\n"
    );
    // The staging shell is gone: the tree it held is the live one now.
    assert!(!paths.staging.join("staging-0.1.6").exists());
    // The swap is recorded but not confirmed: a launch that dies here must be recoverable.
    let record = transaction::read_swap(&paths.swap).expect("the swap is recorded");
    assert_eq!(record.version, "0.1.6");
    assert_eq!(record.target, live.to_string_lossy());

    // The cache still names the new version as the newest, as it does after a real check.
    update::write_cache(
        &root,
        &update::Cache {
            checked_at: update::now_secs(),
            installed: "0.1.5".into(),
            latest: Some("0.1.6".into()),
            // The tags the answer came from: this test only cares that the entry parses and is
            // carried across the recovery, so any real set will do.
            tags: update::default_tags(),
            attempted: None,
            failed: None,
            failed_at: 0,
            failures: 0,
        },
    )
    .unwrap();

    // The next launch finds the record and puts the previous tree back.
    recover_pending_swap(&paths, &root);
    // …and marks the version, so that launch does not stage and swap the same tree again.
    let cache = update::read_cache(&root).expect("the cache is still there");
    assert_eq!(cache.failed.as_deref(), Some("0.1.6"));
    assert_eq!(cache.failures, 1);
    assert_eq!(
        std::fs::read_to_string(live.join("lib/bin.js")).unwrap(),
        "old\n"
    );
    assert!(transaction::read_swap(&paths.swap).is_none());
    // The tree that failed is kept as evidence rather than deleted.
    assert_eq!(
        std::fs::read_to_string(transaction::failed_path(&live).join("lib/bin.js")).unwrap(),
        "new\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A staged tree a killed process left behind is ~290 MB of debris nothing refers to, so the
/// next launch removes it. The directory itself stays: this launch stages into it.
#[test]
fn a_stale_staging_tree_is_cleared_without_taking_the_directory_with_it() {
    let root = crate::test_dir("dsh-desktop-stale-staging-test");
    let _ = std::fs::remove_dir_all(&root);
    let paths = UpdatePaths::new(&root.join("runtime"));
    std::fs::create_dir_all(paths.staging.join("staging-0.1.5")).unwrap();
    std::fs::create_dir_all(paths.staging.join("staging-0.1.6/prefix")).unwrap();
    std::fs::write(
        paths.staging.join("staging-0.1.6/prefix/junk"),
        "half-written",
    )
    .unwrap();

    clear_stale_staging(&paths);
    assert!(!paths.staging.join("staging-0.1.5").exists());
    assert!(!paths.staging.join("staging-0.1.6").exists());
    assert!(
        paths.staging.is_dir(),
        "the staging root itself must survive"
    );

    // Nothing to clean is not an error, and neither is a staging root that never existed.
    clear_stale_staging(&paths);
    clear_stale_staging(&UpdatePaths::new(&root.join("elsewhere/runtime")));
    let _ = std::fs::remove_dir_all(&root);
}

/// A profile snapshot is taken before pnpm may rewrite the profile, and a failed install is
/// undone from it without touching the state the running Harness keeps writing.
#[test]
fn a_profile_snapshot_makes_a_failed_plugin_install_reversible() {
    let root = crate::test_dir("dsh-desktop-profile-snapshot-test");
    let _ = std::fs::remove_dir_all(&root);
    let paths = UpdatePaths::new(&root.join("runtime"));
    std::fs::create_dir_all(&paths.profiles).unwrap();

    let profile = root.join("profiles/web");
    std::fs::create_dir_all(profile.join("node_modules/dshmarket")).unwrap();
    std::fs::create_dir_all(profile.join("data")).unwrap();
    std::fs::write(
        profile.join("package.json"),
        r#"{"name":"dsh-profile-web"}"#,
    )
    .unwrap();
    std::fs::write(profile.join("node_modules/dshmarket/version"), "1.0.0").unwrap();
    std::fs::write(profile.join("data/usage.json"), "before").unwrap();

    let snapshot = snapshot_profile(&paths, &profile, "2.0.0").unwrap();
    assert!(snapshot.join("node_modules/dshmarket/version").is_file());
    // Credentials and session state are not part of the snapshot.
    assert!(!snapshot.join("data").exists());

    // pnpm got half-way and died: the tree is neither version.
    std::fs::write(profile.join("node_modules/dshmarket/version"), "2.0.0").unwrap();
    std::fs::write(profile.join("node_modules/dshmarket/half"), "junk").unwrap();
    // The Harness kept writing live state the whole time.
    std::fs::write(profile.join("data/usage.json"), "after").unwrap();

    restore_profile(&snapshot, &profile).unwrap();
    assert_eq!(
        std::fs::read_to_string(profile.join("node_modules/dshmarket/version")).unwrap(),
        "1.0.0"
    );
    assert!(!profile.join("node_modules/dshmarket/half").exists());
    assert_eq!(
        std::fs::read_to_string(profile.join("data/usage.json")).unwrap(),
        "after",
        "restoring the plugin tree must not roll back live session state"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A confirmed boot drops the record, and the rollback generation is what the next update
/// would have to undo.
#[test]
fn a_confirmed_boot_keeps_the_new_tree_and_clears_the_record() {
    let root = crate::test_dir("dsh-desktop-core-confirm-test");
    let _ = std::fs::remove_dir_all(&root);
    let paths = UpdatePaths::new(&root.join("runtime"));
    std::fs::create_dir_all(&paths.rollback).unwrap();
    transaction::write_swap(
        &paths.swap,
        &transaction::SwapRecord {
            target: "/tmp/target".to_string(),
            backup: "/tmp/backup".to_string(),
            version: "0.1.6".to_string(),
            at: 0,
            had_previous: Some(true),
        },
    )
    .unwrap();

    confirm_core_update(&paths);
    assert!(transaction::read_swap(&paths.swap).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn external_instances_are_never_stopped_by_process_group() {
    assert_eq!(stop_mode(true), StopMode::ProcessGroup);
    assert_eq!(stop_mode(false), StopMode::PidOnly);
}

#[test]
fn update_only_stops_an_instance_it_is_allowed_to_stop() {
    use harness::Probe;
    // Nothing running: update freely.
    assert!(may_stop_before_update(&Probe::Closed).is_ok());
    // Someone else owns the port: never install over it.
    assert!(may_stop_before_update(&Probe::Other).is_err());
    // A Harness — ours or somebody else's — may be stopped only after the question is put,
    // which is `stop_instance_before_update`'s job. The config is not consulted here: doing
    // so skipped the question for everyone who had not edited `config.json` (2026-09-16).
    assert!(may_stop_before_update(&Probe::Harness).is_ok());
}
