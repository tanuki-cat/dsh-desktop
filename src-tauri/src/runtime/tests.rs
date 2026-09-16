//! Unit tests for the runtime module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

fn base_inputs(preference: Preference) -> Inputs<'static> {
    Inputs {
        preference,
        env_node: None,
        env_dsh: None,
        system_node: None,
        system_dsh: None,
        seed_node: Some(Path::new("/app/Contents/Resources/runtime/node/bin/node")),
        seed_dsh: Some(Path::new(
            "/app/Contents/Resources/runtime/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        shadow_dsh: None,
        seed_version: Some("0.1.5-rc.2"),
        shadow_version: None,
    }
}

#[test]
fn nothing_installed_uses_both_bundled_halves() {
    let decision = decide(&base_inputs(Preference::Auto)).expect("a candidate exists");
    assert_eq!(decision.node.origin, Origin::Seed);
    assert_eq!(decision.dsh.origin, Origin::Seed);
    assert_eq!(decision.updates, Updates::Shadow);
}

#[test]
fn a_complete_install_is_preferred_and_never_updated() {
    let input = Inputs {
        system_node: Some(PathBuf::from("/opt/homebrew/bin/node")),
        system_dsh: Some(PathBuf::from(
            "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        ..base_inputs(Preference::Auto)
    };
    let decision = decide(&input).expect("a candidate exists");
    assert_eq!(decision.node.origin, Origin::System);
    assert_eq!(decision.dsh.origin, Origin::System);
    // The shell must not rewrite a prefix the user manages.
    assert_eq!(decision.updates, Updates::Notify);
}

#[test]
fn half_an_install_is_combined_with_the_bundled_half() {
    let node_only = Inputs {
        system_node: Some(PathBuf::from("/usr/bin/node")),
        ..base_inputs(Preference::Auto)
    };
    let decision = decide(&node_only).expect("a candidate exists");
    assert_eq!(decision.node.origin, Origin::System);
    assert_eq!(decision.dsh.origin, Origin::Seed);

    let dsh_only = Inputs {
        system_dsh: Some(PathBuf::from(
            "/usr/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        ..base_inputs(Preference::Auto)
    };
    let decision = decide(&dsh_only).expect("a candidate exists");
    assert_eq!(decision.node.origin, Origin::Seed);
    assert_eq!(decision.dsh.origin, Origin::System);
}

#[test]
fn forcing_bundled_ignores_the_system_install() {
    let input = Inputs {
        system_node: Some(PathBuf::from("/opt/homebrew/bin/node")),
        system_dsh: Some(PathBuf::from(
            "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        ..base_inputs(Preference::Bundled)
    };
    let decision = decide(&input).expect("a candidate exists");
    assert_eq!(decision.node.origin, Origin::Seed);
    assert_eq!(decision.dsh.origin, Origin::Seed);
    assert_eq!(decision.updates, Updates::Shadow);
}

#[test]
fn environment_overrides_win_over_everything() {
    let input = Inputs {
        env_node: Some(PathBuf::from("/custom/node")),
        env_dsh: Some(PathBuf::from("/custom/dsh/lib/bin.js")),
        system_node: Some(PathBuf::from("/opt/homebrew/bin/node")),
        system_dsh: Some(PathBuf::from(
            "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        ..base_inputs(Preference::Auto)
    };
    let decision = decide(&input).expect("a candidate exists");
    assert_eq!(decision.node.path, PathBuf::from("/custom/node"));
    assert_eq!(decision.dsh.path, PathBuf::from("/custom/dsh/lib/bin.js"));
    assert_eq!(decision.node.origin, Origin::Env);
    assert_eq!(decision.dsh.origin, Origin::Env);
    // An override points at a tree this shell does not own: updates only notify, and the
    // "bundled" side effects (PATH, npm_config_prefix, PNPM_HOME) stay off (review P1-5).
    assert_eq!(decision.updates, Updates::Notify);
}

#[test]
fn no_candidate_at_all_is_reported_as_none() {
    // A build without a bundled runtime on a machine with no installation: the caller turns
    // this into the "install node + dsh" page instead of running something that is not there.
    let input = Inputs {
        seed_node: None,
        seed_dsh: None,
        ..base_inputs(Preference::Auto)
    };
    assert!(decide(&input).is_none());
}

#[test]
fn forcing_bundled_without_a_seed_falls_back_to_the_install() {
    let input = Inputs {
        seed_node: None,
        seed_dsh: None,
        system_node: Some(PathBuf::from("/opt/homebrew/bin/node")),
        system_dsh: Some(PathBuf::from(
            "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        ..base_inputs(Preference::Bundled)
    };
    let decision = decide(&input).expect("the installed runtime is the only candidate");
    assert_eq!(decision.node.origin, Origin::System);
    assert_eq!(decision.dsh.origin, Origin::System);
}

#[test]
fn the_higher_bundled_version_wins() {
    let input = Inputs {
        shadow_dsh: Some(Path::new(
            "/data/runtime/prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        shadow_version: Some("0.1.6"),
        ..base_inputs(Preference::Auto)
    };
    assert_eq!(
        decide(&input).expect("a candidate exists").dsh.origin,
        Origin::Shadow
    );

    // A newer seed must not be shadowed by an older copy under app-data.
    let stale_shadow = Inputs {
        shadow_dsh: Some(Path::new(
            "/data/runtime/prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
        )),
        shadow_version: Some("0.1.4"),
        ..base_inputs(Preference::Auto)
    };
    assert_eq!(
        decide(&stale_shadow)
            .expect("a candidate exists")
            .dsh
            .origin,
        Origin::Seed
    );
}
