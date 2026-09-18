//! Unit tests for the crate root.
//!
//! A child module of the code under test, so `use super::*` reaches private
//! items.

use super::*;
use crate::update_flow::{stop_mode, StopMode};

/// The menu item and the handler that acts on it are bound by a string: a rename on either side
/// silently drops every click, and the only recovery the user has from a page that stopped
/// drawing would stop working with no error anywhere.
#[test]
fn the_reload_menu_item_is_the_one_the_handler_watches_for() {
    // Read as source: building a real menu needs a running app and a display server, and the
    // property under test is which id gets registered against which comparison.
    let source = include_str!("lib.rs");
    // Registered under the shell constant rather than a literal, and matched against the same
    // one: two literals that happen to agree today would drift apart at the next rename.
    assert_eq!(
        source.matches("window::RELOAD_MENU_ID").count(),
        2,
        "菜单项注册与事件匹配都要用壳常量：{source}"
    );
    assert!(
        source.contains("CmdOrCtrl+R"),
        "重新加载要有快捷键，否则用户仍然只能退出应用"
    );
    // The id has to stay comparable to `MenuEvent::id()`, which `muda` implements for `&str`.
    assert!(!window::RELOAD_MENU_ID.is_empty());
    assert!(window::RELOAD_MENU_ID.starts_with("reload"));
}
#[test]
fn self_heal_keeps_a_record_it_can_reuse_and_clears_a_stale_one() {
    let state = |pid: u32, port: u16| process::HarnessState {
        pid,
        port,
        cwd: "/tmp".into(),
        started_at: 1,
    };
    // Ours, still serving the port this run uses: hand it to the reuse branch.
    assert_eq!(
        self_heal_action(&state(4242, 3080), Some(4242), true, 3080, false),
        SelfHeal::Keep
    );
    // Ours, but this run uses another port: stop it and drop the record.
    assert_eq!(
        self_heal_action(&state(4242, 3080), Some(4242), true, 4000, false),
        SelfHeal::Terminate
    );
    // The pid no longer owns the port. Only a process that still looks like our own
    // orphaned CLI may be signalled; anything else is pid reuse and is left alone.
    assert_eq!(
        self_heal_action(&state(4242, 3080), Some(9999), true, 3080, true),
        SelfHeal::Terminate
    );
    assert_eq!(
        self_heal_action(&state(4242, 3080), Some(9999), true, 3080, false),
        SelfHeal::Clear
    );
    assert_eq!(
        self_heal_action(&state(4242, 3080), None, true, 3080, false),
        SelfHeal::Clear
    );
    // Already gone: nothing to signal.
    assert_eq!(
        self_heal_action(&state(4242, 3080), Some(4242), false, 3080, false),
        SelfHeal::Clear
    );

    // Keep is what makes `ours` non-None later, and that is the only way `stop_mode` can
    // pick the process-group signal for an instance this shell started.
    let ours =
        self_heal_action(&state(4242, 3080), Some(4242), true, 3080, false) == SelfHeal::Keep;
    assert_eq!(stop_mode(ours), StopMode::ProcessGroup);
}

#[test]
fn a_seed_copy_is_all_or_nothing() {
    let root = crate::test_dir("dsh-desktop-copy-tree-test");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("profiles/web");
    let staging = root.join("profiles/web.tmp");

    // A source that cannot be read leaves neither the target nor the staging directory:
    // a half profile would never be re-seeded nor repaired (review P1-6).
    assert!(copy_tree(&root.join("missing"), &target).is_err());
    assert!(!target.exists());
    assert!(!staging.exists());

    // A complete copy lands in one step and cleans up after itself.
    let from = root.join("template");
    std::fs::create_dir_all(from.join("node_modules/dshmarket")).unwrap();
    std::fs::write(from.join("package.json"), "{}").unwrap();
    std::fs::write(from.join("node_modules/dshmarket/package.json"), "{}").unwrap();
    copy_tree(&from, &target).unwrap();
    assert!(target.join("node_modules/dshmarket/package.json").is_file());
    assert!(!staging.exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_home_never_becomes_the_filesystem_root() {
    assert_eq!(
        home_workspace(Some("/Users/me".into())),
        PathBuf::from("/Users/me")
    );
    assert_eq!(home_workspace(Some("   ".into())), std::env::temp_dir());
    assert_eq!(home_workspace(None), std::env::temp_dir());
    assert_ne!(home_workspace(None), PathBuf::from("/"));
}

#[test]
fn a_bad_workspace_is_repaired_in_memory_only() {
    let dir = crate::test_dir("dsh-desktop-repair-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let raw = "{\"workspace\": \"/definitely/not/there\"}";
    std::fs::write(dir.join("config.json"), raw).unwrap();
    assert_eq!(Config::load(&dir).workspace, default_workspace());
    assert_eq!(
        std::fs::read_to_string(dir.join("config.json")).unwrap(),
        raw,
        "repair must not rewrite the user's file"
    );

    // An existing absolute directory is kept as it is.
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!("{{\"workspace\": {:?}}}", workspace.to_string_lossy()),
    )
    .unwrap();
    assert_eq!(Config::load(&dir).workspace, workspace);

    // A relative path cannot serve as the child's working directory either.
    std::fs::write(dir.join("config.json"), "{\"workspace\": \"relative/dir\"}").unwrap();
    assert_eq!(Config::load(&dir).workspace, default_workspace());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bad_dsh_path_is_ignored_in_memory_only() {
    let dir = crate::test_dir("dsh-desktop-dsh-path-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let raw = "{\"dsh_path\": \"/definitely/not/there/dsh\"}";
    std::fs::write(dir.join("config.json"), raw).unwrap();
    assert_eq!(Config::load(&dir).dsh_path, None);
    assert_eq!(
        std::fs::read_to_string(dir.join("config.json")).unwrap(),
        raw
    );

    // An existing absolute file is remembered for the locator.
    let launcher = dir.join("dsh");
    std::fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!("{{\"dsh_path\": {:?}}}", launcher.to_string_lossy()),
    )
    .unwrap();
    assert_eq!(Config::load(&dir).dsh_path, Some(launcher));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_system_gate_keeps_a_lone_dsh_and_drops_a_rejected_install() {
    let node = || Some(PathBuf::from("/usr/bin/node"));
    let dsh = || {
        Some(PathBuf::from(
            "/usr/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        ))
    };
    // No system node to judge: the dsh alone is still usable on the bundled node — this is
    // the §2.4 cell that resolving the pair with one `locate()` made unreachable (P1-4).
    assert_eq!(keep_system_install(None, dsh(), None), (None, dsh(), None));
    // Accepted, or accepted with a warning: both halves stay.
    assert_eq!(
        keep_system_install(node(), dsh(), Some(SystemGate::Accept)),
        (node(), dsh(), None)
    );
    assert_eq!(
        keep_system_install(node(), dsh(), Some(SystemGate::Warn("old".into()))),
        (node(), dsh(), None)
    );
    // Rejected: the installation as a whole goes, and the reason reaches the error page.
    assert_eq!(
        keep_system_install(node(), dsh(), Some(SystemGate::Reject("too old".into()))),
        (None, None, Some("too old".to_string()))
    );
}

/// A free port is what makes the third option real. The configured port is not free by
/// definition — that is why the question is being asked — so the search starts above it.
#[test]
fn another_port_is_offered_only_when_one_is_actually_free() {
    // Hold a port, then ask for a free one starting there: the answer must skip it.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port must bind");
    let taken = held.local_addr().unwrap().port();
    let found = free_port_from(taken).expect("the range must hold a free port");
    assert!(found > taken, "{found} must be above {taken}");
    // It really is free, and it is inside the bounded range.
    assert!(std::net::TcpListener::bind(("127.0.0.1", found)).is_ok());
    assert!(found <= taken.saturating_add(PORT_SEARCH_RANGE));
}

/// The override is per launch: a configured port that something else owns stays configured, so
/// the next start asks again instead of migrating a port the session cookie is bound to.
#[test]
fn a_port_override_lasts_for_one_launch_only() {
    // Nothing stored: the configured port wins.
    assert_eq!(runtime_port(3080), 3080);
    PORT_OVERRIDE.store(3091, Ordering::SeqCst);
    assert_eq!(runtime_port(3080), 3091);
    PORT_OVERRIDE.store(0, Ordering::SeqCst);
    assert_eq!(runtime_port(3080), 3080);
}

#[test]
fn finds_a_seed_under_the_resource_directory() {
    let dir = crate::test_dir("dsh-desktop-seed-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("runtime/node/bin")).unwrap();
    std::fs::write(dir.join("runtime/node/bin/node"), "").unwrap();

    let found = seed_root_for(Some(&dir)).expect("the resource dir holds a runtime");
    // The resource directory wins over the `tauri dev` copies next to the test binary.
    assert_eq!(found, dir.join("runtime"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forcing_bundled_resolves_into_the_seed_tree() {
    let root = crate::test_dir("dsh-desktop-resolve-test");
    let seed = root.join("runtime");
    let dsh_dir = seed.join("dsh-prefix/lib/node_modules/@deepseek-ai/dsh");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(dsh_dir.join("lib")).unwrap();
    std::fs::create_dir_all(seed.join("node/bin")).unwrap();
    std::fs::write(seed.join("node/bin/node"), "").unwrap();
    std::fs::write(dsh_dir.join("lib/bin.js"), "").unwrap();
    // The manifest has to name the package: a version is only read from a tree that
    // identifies itself as this CLI.
    std::fs::write(
        dsh_dir.join("package.json"),
        "{\"name\": \"@deepseek-ai/dsh\", \"version\": \"9.9.9\"}",
    )
    .unwrap();

    let data_dir = root.join("app-data");
    let config = Config {
        runtime: runtime::Preference::Bundled,
        ..Config::load(&data_dir)
    };
    let resolved = resolve_runtime(Some(&root), &data_dir, &config, None).expect("seed resolves");
    assert_eq!(resolved.node, seed.join("node/bin/node"));
    assert_eq!(resolved.dsh_js, dsh_dir.join("lib/bin.js"));
    assert_eq!(resolved.version, "9.9.9");
    // The bundled half is ours to update; a system install never is.
    assert_eq!(resolved.updates, runtime::Updates::Shadow);
    assert!(resolved.bundled());
    assert_eq!(
        resolved.update_prefix(&data_dir),
        Some(data_dir.join("runtime/prefix"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn verbatim_windows_paths_lose_their_prefix() {
    #[cfg(windows)]
    {
        // Taken from a real failure: node's `fs.realpathSync` walked this path and died
        // on `lstat 'D:'` before the CLI ever started.
        assert_eq!(
            unverbatim(Path::new(r"\\?\D:\dsh\DSH Desktop\runtime\node\node.exe")),
            PathBuf::from(r"D:\dsh\DSH Desktop\runtime\node\node.exe")
        );
        assert_eq!(
            unverbatim(Path::new(r"\\?\UNC\server\share\x")),
            PathBuf::from(r"\\server\share\x")
        );
        // Past MAX_PATH the prefix is the only way to reach the file, so it stays.
        let long = format!(r"\\?\D:\{}", "a".repeat(300));
        assert_eq!(unverbatim(Path::new(&long)), PathBuf::from(&long));
    }
    // Everywhere else the helper must not touch a path.
    assert_eq!(unverbatim(Path::new("/a/b")), PathBuf::from("/a/b"));
}

#[test]
fn the_system_runtime_gate_only_refuses_what_it_must() {
    let apple = locator::NodeFacts {
        arch: "arm64".into(),
        strip_types: true,
    };
    let intel = locator::NodeFacts {
        arch: "x64".into(),
        strip_types: true,
    };
    let old = locator::NodeFacts {
        arch: "arm64".into(),
        strip_types: false,
    };

    assert_eq!(
        system_runtime_gate(&apple, "arm64", true),
        SystemGate::Accept
    );
    // An x64 node under Rosetta is only worth refusing when the bundled runtime can take
    // its place; without one it is the only way to run, so it is kept with a warning.
    assert!(matches!(
        system_runtime_gate(&intel, "arm64", true),
        SystemGate::Reject(_)
    ));
    assert!(matches!(
        system_runtime_gate(&intel, "arm64", false),
        SystemGate::Warn(_)
    ));
    // Node < 22.13 cannot run the CLI at all, so this one is fatal in both cases.
    assert!(matches!(
        system_runtime_gate(&old, "arm64", false),
        SystemGate::Reject(_)
    ));
}

#[test]
fn system_updates_defaults_to_leaving_the_user_install_alone() {
    let dir = crate::test_dir("dsh-desktop-system-updates-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A prefix this shell does not own is not rewritten unless the user asks for it: an
    // automatic `npm install -g` can change a CLI other tools on the machine share.
    assert_eq!(
        Config::load(&dir).system_updates,
        runtime::SystemUpdates::Notify
    );
    std::fs::write(dir.join("config.json"), "{\"system_updates\": \"install\"}").unwrap();
    assert_eq!(
        Config::load(&dir).system_updates,
        runtime::SystemUpdates::Install
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The defaults a user gets without editing anything. Every one of these decides whether the
/// shell may touch something it does not own, so each is asserted rather than assumed.
#[test]
fn defaults_do_not_take_over_foreign_state() {
    let dir = crate::test_dir("dsh-desktop-config-defaults-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = Config::load(&dir);

    // Killing whatever holds the port is the user's call, not a startup path's.
    assert!(!config.take_over_existing);
    // A prerelease channel is not a default target for a desktop user.
    assert_eq!(config.update_tags, vec!["latest".to_string()]);
    // A profile is user data; keeping its plugin tree current is opt-in.
    assert!(!config.auto_update_plugins);
    // The shell parses the CLI's startup line, so an untested version is refused by default.
    assert!(config.require_tested_dsh);
    assert_eq!(config.system_updates, runtime::SystemUpdates::Notify);
    // Still on: the bundled/shadow runtime is this shell's own tree to update.
    assert!(config.auto_update);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn runtime_preference_parses_the_env_spelling() {
    use runtime::Preference;
    assert_eq!(Preference::parse("auto"), Some(Preference::Auto));
    assert_eq!(Preference::parse("Bundled"), Some(Preference::Bundled));
    assert_eq!(Preference::parse(" system "), Some(Preference::System));
    assert_eq!(Preference::parse("nonsense"), None);
}

#[test]
fn partial_configs_parse_and_broken_ones_are_not_overwritten() {
    let dir = crate::test_dir("dsh-desktop-config-test");
    let _ = std::fs::remove_dir_all(&dir);

    // Missing file: defaults are seeded once.
    let seeded = Config::load(&dir);
    assert_eq!(seeded.port, default_port());
    assert!(dir.join("config.json").is_file());

    // A partial edit keeps its value and takes defaults for the rest.
    std::fs::write(dir.join("config.json"), "{\"port\": 4321}").unwrap();
    assert_eq!(Config::load(&dir).port, 4321);

    // Unparsable: defaults apply for this run, the file survives untouched.
    let broken = "{ not json at all";
    std::fs::write(dir.join("config.json"), broken).unwrap();
    assert_eq!(Config::load(&dir).port, default_port());
    assert_eq!(
        std::fs::read_to_string(dir.join("config.json")).unwrap(),
        broken
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_workspace_that_is_not_a_usable_directory_is_repaired() {
    let dir = crate::test_dir("dsh-desktop-workspace-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A drive-relative workspace (the reported `EISDIR: lstat 'D:'`) falls back to home.
    std::fs::write(dir.join("config.json"), "{\"workspace\": \"D:\"}").unwrap();
    let repaired = Config::load(&dir);
    assert_eq!(repaired.workspace, default_workspace());
    assert!(repaired.workspace.is_absolute());

    // An unusable dsh_home is dropped, so the Harness uses the default profile.
    std::fs::write(dir.join("config.json"), "{\"dsh_home\": \"\"}").unwrap();
    assert_eq!(Config::load(&dir).dsh_home, None);

    // A usable pair survives untouched.
    let workspace = dir.join("work");
    let home = dir.join("profiles");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!(
            "{{\"workspace\": {:?}, \"dsh_home\": {:?}}}",
            workspace.to_string_lossy(),
            home.to_string_lossy()
        ),
    )
    .unwrap();
    let kept = Config::load(&dir);
    assert_eq!(kept.workspace, workspace);
    assert_eq!(kept.dsh_home, Some(home));

    // A relative path is resolved against the app cwd: never handed to the child as-is.
    assert!(absolute(Path::new("runtime/node")).is_absolute());

    let _ = std::fs::remove_dir_all(&dir);
}

/// PATH as a list of entries, with one separator convention for both platforms.
fn path_entries(path: &str) -> Vec<String> {
    std::env::split_paths(path)
        .map(|entry| slashy(&entry))
        .collect()
}

/// Render a path the same way on Windows and elsewhere, for assertions.
fn slashy(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Look one variable up in an assembled child environment.
fn value_of(env: &ChildEnv, key: &str) -> Option<String> {
    env.vars
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

#[test]
fn a_missing_pnpm_skips_the_plugin_step_before_anything_is_stopped() {
    let pnpm = Path::new("/opt/runtime/tools/bin/pnpm");
    // Declared but not installed: installing it is a repair the user may be mid-way through.
    assert_eq!(
        plugin_skip_reason(true, None, Some(pnpm)),
        Some(PluginSkip::NotInstalled)
    );
    // No pnpm: `dsh plugin add` forwards to pnpm and exits 127 — after the shell already
    // stopped the Harness, so the decision has to be made here (review A1).
    assert_eq!(
        plugin_skip_reason(true, Some("1.0.0"), None),
        Some(PluginSkip::NoPnpm)
    );
    // Both halves present: the update may proceed.
    assert_eq!(plugin_skip_reason(true, Some("1.0.0"), Some(pnpm)), None);
    // The skip has to explain itself; this is the log line the user sees.
    assert!(PluginSkip::NoPnpm.reason().contains("pnpm"));
    assert!(PluginSkip::NotInstalled
        .reason()
        .contains(update::MARKET_PLUGIN));
}

/// Core updates are the same hazard as the identity check: `npm i -g @deepseek-ai/dsh`
/// against a foreign or unreadable tree installs a second CLI and restarts onto it.
#[test]
fn the_core_update_only_touches_a_tree_it_can_identify() {
    // The shell's own writable prefix: always its business.
    assert!(may_update_core(runtime::Updates::Shadow, "0.1.5-rc.2"));
    assert!(may_update_core(runtime::Updates::Shadow, "未知"));
    // A user's install whose version this package really publishes: updatable in place.
    assert!(may_update_core(runtime::Updates::Notify, "0.1.5-rc.2"));
    assert!(may_update_core(runtime::Updates::Notify, "0.1.6-alpha.1"));
    // A user's install with no version to compare against: leave it alone.
    assert!(!may_update_core(runtime::Updates::Notify, "未知"));
    assert!(!may_update_core(runtime::Updates::Notify, ""));
    assert!(!may_update_core(runtime::Updates::Notify, "Dancer's shell"));
}

/// A profile without the market used to be skipped in silence, which is how "the UI has no
/// plugin market" became impossible to explain from the log.
#[test]
fn a_profile_without_the_market_says_so_instead_of_going_quiet() {
    let pnpm = Path::new("/opt/runtime/tools/bin/pnpm");
    assert_eq!(
        plugin_skip_reason(false, None, Some(pnpm)),
        Some(PluginSkip::NotDeclared)
    );
    // Not declared wins over the other reasons: there is nothing to install or update.
    assert_eq!(
        plugin_skip_reason(false, Some("1.0.0"), None),
        Some(PluginSkip::NotDeclared)
    );
    let reason = PluginSkip::NotDeclared.reason();
    assert!(reason.contains(update::MARKET_PLUGIN), "{reason}");
    assert!(reason.contains("profile"), "{reason}");
    // The repair path is named, because the missing menu is not something a user can guess.
    assert!(reason.contains("dsh plugin"), "{reason}");
}

#[test]
fn the_child_path_carries_the_bundled_tools_that_hold_pnpm() {
    let data_dir = crate::test_dir("dsh-desktop-child-env-test");
    let _ = std::fs::remove_dir_all(&data_dir);
    let config = Config::load(&data_dir);
    // Absolute on the host platform: a leading `/` is root-relative on Windows, and
    // `merge_path` drops anything that is not absolute — so these paths are built from
    // `temp_dir` instead of being hard-coded. The Windows CI gate caught the original
    // version (a bare `/opt/...` compared as a whole-PATH string prefix).
    // Built with one `join` per component, exactly like the product does: a component such
    // as "runtime/tools" keeps its `/` on Windows while the product's `PNPM_HOME` renders
    // as `\runtime\tools\bin`, and the two strings then differ only in separators. The
    // Windows gate caught that too.
    let app = data_dir.join("app");
    let node = app.join("runtime").join("node").join("bin").join("node");
    let seed = app.join("runtime");
    let tools = data_dir.join("runtime").join("tools");

    let bundled = ChildEnv::assemble(
        &config,
        &node,
        Some(&seed),
        true,
        &data_dir,
        &BTreeMap::new(),
    );
    // Compare entries as normalised strings: Windows renders `/` as `\` on the way through
    // `join_paths`, so a whole-PATH string prefix is not portable either.
    let entries = path_entries(&bundled.path);
    let node_dir = slashy(node.parent().expect("the test node has a directory"));
    let writable = slashy(&tools.join("bin"));
    let shipped = slashy(&seed.join("tools").join("bin"));
    assert_eq!(
        entries.first().map(String::as_str),
        Some(node_dir.as_str()),
        "the node directory must lead PATH"
    );
    assert!(entries.contains(&writable), "可写 tools 前缀在 PATH 上");
    assert!(entries.contains(&shipped), "随包 tools 前缀在 PATH 上");
    // Global installs land in the writable prefix, never in the signed bundle.
    assert_eq!(
        value_of(&bundled, "PNPM_HOME"),
        Some(tools.join("bin").to_string_lossy().to_string())
    );
    assert_eq!(
        value_of(&bundled, "npm_config_prefix"),
        Some(tools.to_string_lossy().to_string())
    );
    assert_eq!(value_of(&bundled, "PATH"), Some(bundled.path.clone()));

    // A system install is the user's own tree: no toolchain of ours is pushed into it.
    let system = ChildEnv::assemble(&config, &node, None, false, &data_dir, &BTreeMap::new());
    assert!(!system.path.contains(&tools.to_string_lossy().to_string()));
    assert_eq!(value_of(&system, "PNPM_HOME"), None);

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn seeding_reports_itself_so_the_first_launch_stays_offline() {
    let root = crate::test_dir("dsh-desktop-seed-outcome-test");
    let _ = std::fs::remove_dir_all(&root);
    let seed = root.join("runtime");
    let template = seed.join("profile-template");
    std::fs::create_dir_all(template.join("node_modules/dshmarket")).unwrap();
    std::fs::write(template.join("package.json"), "{}").unwrap();
    std::fs::write(template.join("node_modules/dshmarket/package.json"), "{}").unwrap();
    let home = root.join("dsh-home");
    let config = Config {
        dsh_home: Some(home.clone()),
        ..Config::load(&root.join("app-data"))
    };

    // First launch: the template is copied and the caller is told, so the plugin check can
    // stay offline this once (review A2).
    let outcome = seed_profile_template(&config, true, Some(&seed));
    assert!(outcome.seeded);
    assert!(outcome.note.is_some());
    assert!(home.join("profiles/web/package.json").is_file());

    // Second launch: the profile exists, so this is not a first start any more — and saying
    // so is what makes "my UI has no plugin market" answerable from the log.
    let again = seed_profile_template(&config, true, Some(&seed));
    assert!(!again.seeded);
    assert_eq!(
        again
            .note
            .as_deref()
            .map(|note| note.contains("profile 已存在")),
        Some(true),
        "{:?}",
        again.note
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The decision matrix, without a filesystem: every skip has to be sayable.
#[test]
fn seeding_decides_on_four_plain_facts() {
    assert_eq!(seed_decision(true, true, true, false), Ok(()));
    assert_eq!(
        seed_decision(false, true, true, false),
        Err(SeedSkip::NotBundled)
    );
    assert_eq!(
        seed_decision(true, false, true, false),
        Err(SeedSkip::NoTemplate)
    );
    assert_eq!(
        seed_decision(true, true, false, false),
        Err(SeedSkip::NoHome)
    );
    assert_eq!(
        seed_decision(true, true, true, true),
        Err(SeedSkip::ProfileExists)
    );
    // A non-bundled build names itself, not whatever else is missing.
    assert_eq!(
        seed_decision(false, false, false, true),
        Err(SeedSkip::NotBundled)
    );
    // Each reason has to name the consequence, because the missing menu cannot be guessed.
    for skip in [
        SeedSkip::NotBundled,
        SeedSkip::NoTemplate,
        SeedSkip::NoHome,
        SeedSkip::ProfileExists,
    ] {
        let reason = skip.reason();
        assert!(!reason.is_empty(), "{skip:?} must explain itself");
    }
    assert!(SeedSkip::ProfileExists.reason().contains("插件市场"));
}

#[test]
fn a_self_restart_gets_the_long_handoff_wait_and_a_crash_does_not() {
    // The plugin market's restart: the host is SIGTERMed, shuts down with code 0, and a
    // detached helper boots the replacement a few seconds later — waiting is the point.
    assert_eq!(
        exit_action(Duration::from_secs(5), 0, true),
        ExitAction::Recover {
            attempt: 1,
            grace: HANDOFF_GRACE
        }
    );
    // A crash or a kill has nothing behind it: the restart must not be delayed by the full
    // grace (this is the case the old code answered instantly with a failure page).
    assert_eq!(
        exit_action(Duration::from_secs(5), 0, false),
        ExitAction::Recover {
            attempt: 1,
            grace: HANDOFF_GRACE_QUICK
        }
    );
    assert!(HANDOFF_GRACE_QUICK < HANDOFF_GRACE);
}

#[test]
fn automatic_restarts_are_budgeted_and_reset_by_a_healthy_run() {
    let quick = |attempt| ExitAction::Recover {
        attempt,
        grace: HANDOFF_GRACE_QUICK,
    };
    // A crash loop counts up to the budget, then stops instead of looping for ever.
    assert_eq!(exit_action(Duration::from_secs(1), 0, false), quick(1));
    assert_eq!(exit_action(Duration::from_secs(1), 1, false), quick(2));
    assert_eq!(exit_action(Duration::from_secs(1), 2, false), quick(3));
    assert_eq!(
        exit_action(Duration::from_secs(1), 3, false),
        ExitAction::Report
    );
    // One run that lasted is proof the loop is over: the count starts again from one, so a
    // Harness that works for hours and then dies is never refused a restart.
    assert_eq!(exit_action(HEALTHY_RUN, 3, false), quick(1));
    assert_eq!(
        exit_action(Duration::from_secs(3600), MAX_AUTO_RESTARTS, true),
        ExitAction::Recover {
            attempt: 1,
            grace: HANDOFF_GRACE
        }
    );
}

/// The page used to print Rust's Option debug form ("Some(0)", "None") at the user.
#[cfg(unix)]
#[test]
fn the_exit_reason_reaches_the_user_as_words() {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        exit_reason(&std::process::ExitStatus::from_raw(0)),
        "退出码 0"
    );
    assert_eq!(
        exit_reason(&std::process::ExitStatus::from_raw(3 << 8)),
        "退出码 3"
    );
    // Killed by a signal: no exit code at all, which is how a "kill -9" reads here.
    assert_eq!(
        exit_reason(&std::process::ExitStatus::from_raw(9)),
        "被信号 9 终止"
    );
}

/// A restart asks about the instance on the port at most once: an answer given during the attempt
/// ("no", or "keep it and use another port") is final for that restart.
#[test]
fn a_failed_restart_does_not_ask_the_same_question_twice() {
    let failed = StartError::from("boot timed out".to_string());
    assert!(may_ask_after_failure(&failed, 3080, 3080));

    let declined = StartError {
        reason: "kept".to_string(),
        declined: true,
    };
    assert!(!may_ask_after_failure(&declined, 3080, 3080));

    // The user kept the instance on 3080 and this attempt moved to 3081.
    assert!(!may_ask_after_failure(&failed, 3080, 3081));
}

/// The 3d swap must not pull a tree out from under an instance the user chose to keep, and must
/// not refuse an update for a tree nothing can be using.
#[test]
fn a_kept_instance_blocks_only_a_swap_of_the_tree_it_may_use() {
    let target = Path::new("/data/runtime/prefix/lib/node_modules/@deepseek-ai/dsh");
    let ours =
        "/app/node /data/runtime/prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web";
    let other = "/usr/local/bin/node /usr/local/bin/dsh web";

    // First update of a bundled build: the shadow tree does not exist yet.
    assert!(!swap_conflicts(target, false, true, None));
    // The user's own prefix: a kept terminal instance is assumed to use it, whatever it prints.
    assert!(swap_conflicts(target, true, false, Some(other)));
    // The shadow prefix: decided by the command line, and an unreadable one is a conflict.
    assert!(swap_conflicts(target, true, true, Some(ours)));
    assert!(!swap_conflicts(target, true, true, Some(other)));
    assert!(swap_conflicts(target, true, true, None));
}

/// The 3d swap is refused when any of the three "somebody may be using this tree" signals
/// fires, and allowed only when none of them does.
#[test]
fn only_a_clear_field_lets_the_staged_tree_be_committed() {
    assert!(!swap_blocked(false, false, false));
    // The user kept the instance that may be running this tree.
    assert!(swap_blocked(true, false, false));
    // A Harness took the port this launch is about to spawn on.
    assert!(swap_blocked(false, true, false));
    // A Harness is still on the port detection started from: it is not in state.json, so
    // neither the reuse branch nor the `Some(pid)` branch would have stopped it.
    assert!(swap_blocked(false, false, true));
    assert!(swap_blocked(true, true, true));
}
