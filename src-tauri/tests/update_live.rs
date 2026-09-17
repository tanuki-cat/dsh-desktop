//! Live npm-registry check for the update module.
//!
//! Skipped unless `DSH_DESKTOP_LIVE_TESTS=1`, so the default `cargo test` stays offline.

use dsh_desktop_lib::update;
use std::path::PathBuf;

/// A per-process temporary directory, so two `cargo test` runs on one machine (another checkout,
/// or two CI jobs sharing a runner) cannot delete each other's files mid-test (review D7).
fn test_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
}

fn path_lookup(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|c| c.is_file())
}

/// Keep the live queries off the user npm cache: npm refuses to run when that cache is not
/// writable (restricted sandbox, read-only home), which has nothing to do with this shell.
/// Both tests share one directory, so the process-global setting stays race-free.
fn use_private_npm_cache() {
    let cache = test_dir("dsh-desktop-live-npm-cache");
    let _ = std::fs::create_dir_all(&cache);
    std::env::set_var("npm_config_cache", &cache);
}

#[test]
fn live_check_reports_the_registry_head() {
    if std::env::var("DSH_DESKTOP_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set DSH_DESKTOP_LIVE_TESTS=1 to query the real registry");
        return;
    }
    let node = path_lookup("node").expect("node must be on PATH for this test");
    let npm = update::npm_for(&node).expect("npm must be resolvable from node");
    use_private_npm_cache();
    let tags = vec!["latest".to_string(), "next".to_string()];

    // Any real published version outranks 0.0.1, so this must report an update.
    match update::check(&npm, update::PACKAGE, &tags, "0.0.1") {
        update::Status::UpdateAvailable { to, .. } => {
            let parsed = update::Version::parse(&to).expect("registry head must parse");
            println!("live registry head = {parsed} (dist-tags: {tags:?})");
        }
        other => panic!("expected UpdateAvailable from 0.0.1, got {other:?}"),
    }

    // And an impossible version is already "newer" than the registry.
    match update::check(&npm, update::PACKAGE, &tags, "99.0.0") {
        update::Status::UpToDate { .. } => {}
        other => panic!("expected UpToDate for 99.0.0, got {other:?}"),
    }
}
/// The whole point of the cache: the second check inside the interval must not touch the
/// network, which is what keeps the common launch ~1.9s faster.
#[test]
fn cache_short_circuits_the_second_query() {
    if std::env::var("DSH_DESKTOP_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set DSH_DESKTOP_LIVE_TESTS=1 to query the real registry");
        return;
    }
    let node = path_lookup("node").expect("node must be on PATH for this test");
    let npm = update::npm_for(&node).expect("npm must be resolvable from node");
    use_private_npm_cache();
    let tags = vec!["latest".to_string(), "next".to_string()];
    let dir = test_dir("dsh-desktop-live-cache-test");
    let _ = std::fs::remove_dir_all(&dir);

    let started = std::time::Instant::now();
    let cold = update::check_cached(&npm, update::PACKAGE, &tags, "0.0.1", &dir, 60);
    let cold_ms = started.elapsed().as_millis();

    let started = std::time::Instant::now();
    let warm = update::check_cached(&npm, update::PACKAGE, &tags, "0.0.1", &dir, 60);
    let warm_ms = started.elapsed().as_millis();

    println!("cold = {cold_ms} ms (query), warm = {warm_ms} ms (cache)");
    assert!(!cold.cached, "first call must query the registry");
    assert!(
        warm.cached,
        "second call inside the interval must use the cache"
    );
    assert_eq!(
        cold.status, warm.status,
        "cached answer must match the fresh one"
    );
    assert!(
        warm_ms * 5 < cold_ms.max(50),
        "cached path should be far cheaper: {warm_ms} vs {cold_ms} ms"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
