//! Live regression test for the GUI launch failure mode.
//!
//! npm is a `#!/usr/bin/env node` script, so an app that inherits launchd PATH (no node)
//! could never run it: every update check died with `env: node: No such file or directory`.
//! The module now repairs PATH for its npm children; this test reproduces a Finder launch to prove it.
//!
//! Its own test binary, because it rewrites PATH for the whole process. Skipped unless
//! `DSH_DESKTOP_LIVE_TESTS=1`.

use dsh_desktop_lib::update;
use std::path::PathBuf;
use std::process::Command;

/// PATH with no node in it, standing in for a Finder/Dock launch.
const NO_NODE_PATH: &str = "/nonexistent-bin";

/// A per-process temporary directory, so two `cargo test` runs on one machine (another checkout,
/// or two CI jobs sharing a runner) cannot delete each other's files mid-test (review D7).
fn test_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
}

/// `name` on this process's PATH, with the extensions Windows appends to a bare name.
///
/// A bare join finds nothing there: the toolchain installs `node.exe`, and `is_file()` does not
/// consult `PATHEXT`, so every case needing node reported "node must be on PATH" instead of
/// running (the library's own lookup learned this first).
fn path_lookup(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        candidate_names(name)
            .into_iter()
            .map(|candidate| dir.join(candidate))
            .find(|path| path.is_file())
    })
}

/// The file names a bare program name may stand for, command extensions first.
#[cfg(windows)]
fn candidate_names(name: &str) -> Vec<String> {
    [".exe", ".cmd", ".bat", ""]
        .iter()
        .map(|extension| format!("{name}{extension}"))
        .collect()
}

#[cfg(not(windows))]
fn candidate_names(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

#[test]
fn npm_still_runs_when_the_app_path_has_no_node() {
    if std::env::var("DSH_DESKTOP_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set DSH_DESKTOP_LIVE_TESTS=1 to query the real registry");
        return;
    }
    let node = path_lookup("node").expect("node must be on PATH for this test");
    let npm = update::npm_for(&node).expect("npm must be resolvable from node");

    // Everything above is resolved from the real PATH; from here on the process looks
    // like a GUI launch. The private cache keeps the test off the user cache.
    std::env::set_var("PATH", NO_NODE_PATH);
    let cache = test_dir("dsh-desktop-live-npm-cache-env");
    let _ = std::fs::remove_dir_all(&cache);
    std::env::set_var("npm_config_cache", &cache);

    // Premise: the plain invocation is what the app used to do, and why it always failed.
    let plain = Command::new(&npm)
        .arg("--version")
        .output()
        .expect("npm must be spawnable");
    assert!(
        !plain.status.success(),
        "premise broken: npm ran without node on PATH"
    );
    eprintln!(
        "premise confirmed: plain npm exits {:?} without node on PATH",
        plain.status.code()
    );

    // The repaired PATH must carry the real check all the way to the registry.
    let tags = vec!["latest".to_string(), "next".to_string()];
    match update::check(&npm, update::PACKAGE, &tags, "0.0.1") {
        update::Status::UpdateAvailable { to, .. } => {
            eprintln!("live registry head = {to} with PATH={NO_NODE_PATH}");
        }
        other => panic!("update check must survive a PATH without node, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&cache);
}
