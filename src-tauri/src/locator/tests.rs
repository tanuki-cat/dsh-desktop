//! Unit tests for the locator module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;
#[cfg(unix)]
use std::time::Instant;

/// The probe runs on whatever node this machine has; on a machine without node it is
/// simply skipped (`None` is a valid answer the caller must handle).
#[test]
fn probes_a_real_node_for_arch_and_capability() {
    let Some(node) = path_lookup("node") else {
        eprintln!("skipped: no node on PATH");
        return;
    };
    let facts = probe_node(&node).expect("a working node must answer the probe");
    assert!(!facts.arch.is_empty());
    // The CLI needs stripTypeScriptTypes; if this machine's node is too old the test tells
    // us rather than silently asserting the opposite.
    assert!(
        facts.strip_types,
        "node {} lacks stripTypeScriptTypes",
        facts.arch
    );
}

/// A node that never answers must not hold up startup: the probe runs before the
/// supervised process and before the URL wait, so it carries its own deadline.
#[test]
#[cfg(unix)]
fn gives_up_on_a_node_that_never_answers() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join("dsh-desktop-hanging-node-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("node");
    std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let started = Instant::now();
    assert!(probe_node_within(&fake, Duration::from_millis(300)).is_none());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the probe must not wait for the child"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tree with the given manifest content and an entry script inside it.
fn tree(name: &str, manifest: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib/bin")).unwrap();
    std::fs::write(dir.join("package.json"), manifest).unwrap();
    let entry = dir.join("lib/bin/bin.js");
    std::fs::write(&entry, "#!/usr/bin/env node\n").unwrap();
    (dir, entry)
}

#[test]
fn reads_version_from_the_owning_package() {
    let (dir, entry) = tree(
        "dsh-desktop-version-test",
        r#"{"name":"@deepseek-ai/dsh","version":"1.2.3"}"#,
    );
    assert_eq!(version_of(&entry).as_deref(), Some("1.2.3"));
    assert_eq!(describe_version(&entry), "1.2.3");
    // Nothing to read above the script: the caller must fall back to running the CLI.
    let orphan = std::env::temp_dir().join("dsh-desktop-no-package/bin.js");
    assert_eq!(version_of(&orphan), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The field report that started this: something called `dsh` on PATH that is not this CLI
/// must not have its version printed as the dsh version, and must not be supervised either.
#[test]
fn a_foreign_dsh_is_never_mistaken_for_this_cli() {
    // Homebrew ships a distributed shell under this very name.
    let (dir, launcher) = tree(
        "dsh-desktop-foreign-dsh-test",
        r#"{"name":"dsh","version":"0.25.10"}"#,
    );
    assert_eq!(version_of(&launcher), None);
    let described = describe_version(&launcher);
    assert!(described.contains("未知"), "{described}");
    assert!(
        !described.contains("0.25.10"),
        "别人的版本号不能出现在这里：{described}"
    );

    // Even when it answers `--version`, the manifest named a different package: a foreign
    // binary that prints a version is not evidence that it owns this shell's CLI.
    match judge(&launcher, |_| Some("0.25.10".to_string())) {
        Candidate::Foreign(reason) => {
            assert!(
                reason.contains("属于 dsh，不是 @deepseek-ai/dsh"),
                "{reason}"
            )
        }
        other => panic!("a foreign dsh must not be adopted: {other:?}"),
    }

    // A compiled launcher from another project, sitting in a tree with no manifest at all:
    // it cannot be identified, and it is not a node script that could answer `--version`.
    let bare = std::env::temp_dir().join("dsh-desktop-foreign-binary-test");
    let _ = std::fs::remove_dir_all(&bare);
    std::fs::create_dir_all(&bare).unwrap();
    let binary = bare.join("dsh");
    std::fs::write(&binary, [0x7fu8, b'E', b'L', b'F', 2, 1, 1, 0]).unwrap();
    match judge(&binary, |_| None) {
        Candidate::Foreign(reason) => {
            assert!(reason.contains("不是 node 启动脚本"), "{reason}")
        }
        other => panic!("a native binary must not be adopted: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&bare);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A layout this walk cannot read (a shim outside its package) is still the user's install:
/// it is used, but only because it answered `--version` with a version.
#[test]
fn an_unidentified_launcher_is_used_only_when_it_answers_with_a_version() {
    let dir = std::env::temp_dir().join("dsh-desktop-unidentified-dsh-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("dsh");
    std::fs::write(&shim, "#!/usr/bin/env node\n").unwrap();

    assert_eq!(
        judge(&shim, |_| Some("0.1.5-rc.2".to_string())),
        Candidate::Unidentified("0.1.5-rc.2".to_string())
    );
    // Anything that is not a version is not an answer.
    assert!(matches!(
        judge(&shim, |_| Some("Dancer shell, version 0.25.10".to_string())),
        Candidate::Foreign(_)
    ));
    assert!(matches!(judge(&shim, |_| None), Candidate::Foreign(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// End to end on this machine, when it has a CLI at all: whatever the search returns must be
/// a tree that names this package — that is the invariant the field report broke.
#[test]
fn whatever_the_search_returns_is_really_this_cli() {
    let Some(entry) = system_dsh(None, None) else {
        eprintln!("skipped: no dsh resolvable on this machine");
        return;
    };
    match dsh_package(&entry) {
        Ok(Some(manifest)) => {
            assert_eq!(manifest.name, DSH_PACKAGE);
            assert!(
                version_of(&entry).is_some(),
                "{} 有清单却没有版本",
                entry.display()
            );
            println!(
                "resolved {} -> {}",
                entry.display(),
                describe_version(&entry)
            );
        }
        // Nothing identified: the search only returns such a candidate after `--version`
        // answered with a version, so re-judging it now has to land in the same category.
        Ok(None) => {
            let node = find_node_without_env(&entry, None);
            let probed = probe_version_line(node.as_deref(), &entry);
            match judge(&entry, |_| probed.clone()) {
                Candidate::Unidentified(version) => {
                    println!("resolved {} (unidentified, {version})", entry.display())
                }
                other => panic!("返回了无法识别的候选 {other:?}: {}", entry.display()),
            }
        }
        Err(error) => panic!("清单读取失败: {error}"),
    }
}

#[test]
fn only_a_version_line_counts_as_an_answer() {
    assert_eq!(
        parse_version_line("0.1.5-rc.2\n"),
        Some("0.1.5-rc.2".to_string())
    );
    assert_eq!(parse_version_line("  1.2.3  \n"), Some("1.2.3".to_string()));
    // Chatter, a usage line, or another tool's banner must not become "the version".
    assert_eq!(parse_version_line("Dancer shell, version 0.25.10"), None);
    assert_eq!(parse_version_line("Usage: dsh [options]"), None);
    assert_eq!(parse_version_line(""), None);
    assert_eq!(parse_version_line("   \n"), None);
}

#[cfg(unix)]
#[test]
fn the_environment_override_wins_over_the_remembered_path() {
    let dir = std::env::temp_dir().join("dsh-desktop-locator-order-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    // Both launchers live inside a package that really is this CLI: the search now refuses a
    // candidate whose owning manifest names something else, so an anonymous script would be
    // rejected before the order under test could matter.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"@deepseek-ai/dsh","version":"0.1.5-rc.2"}"#,
    )
    .unwrap();
    let remembered = dir.join("bin/dsh");
    let from_env = dir.join("bin/dsh-env");
    for file in [&remembered, &from_env] {
        std::fs::write(file, "#!/usr/bin/env node\n").unwrap();
    }

    // DSH_DESKTOP_DSH comes first in the documented order (design §4 step 3).
    let location = locate(
        Some(remembered.clone()),
        Some(from_env.to_string_lossy().to_string()),
    )
    .unwrap();
    assert_eq!(location.launcher, from_env);

    // Without it, the remembered path is used ahead of PATH and the common prefixes.
    let location = locate(Some(remembered.clone()), None).unwrap();
    assert_eq!(location.launcher, remembered);

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn resolves_real_js_behind_symlink() {
    let dir = std::env::temp_dir().join("dsh-desktop-locator-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib/bin")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    // The launcher has to belong to a package that really is this CLI: the search now
    // rejects a candidate whose owning manifest names something else.
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"@deepseek-ai/dsh","version":"0.1.5-rc.2"}"#,
    )
    .unwrap();
    let js = dir.join("lib/bin/bin.js");
    std::fs::write(&js, "#!/usr/bin/env node\n").unwrap();
    let link = dir.join("bin/dsh");
    std::os::unix::fs::symlink(&js, &link).unwrap();
    let loc = locate(Some(link.clone()), None).unwrap();
    assert_eq!(loc.dsh_js, std::fs::canonicalize(&js).unwrap());
    assert!(loc.node.is_file());
}

/// The login shell's PATH is searched even when the app's own PATH has nothing: that is how a
/// Finder-launched app finds an nvm/fnm node, which only `.zshrc` puts on PATH.
#[cfg(unix)]
#[test]
fn the_imported_login_path_is_searched() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join("dsh-desktop-imported-path-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let tool = dir.join("bin").join("dsh-desktop-only-here");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    let shell_path = format!("relative/bin:{}", dir.join("bin").display());
    assert_eq!(
        search("dsh-desktop-only-here", Some(&shell_path)),
        Some(tool.clone())
    );
    assert_eq!(
        path_lookup_in("dsh-desktop-only-here", OsStr::new("relative/bin")),
        None,
        "a relative PATH entry is not searched"
    );
    // With an imported PATH in hand, a miss is a miss: no login shell is started for it.
    let started = std::time::Instant::now();
    assert_eq!(
        search("dsh-desktop-no-such-tool", Some("/nonexistent")),
        None
    );
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&dir);
}
