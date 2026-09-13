//! Which Node and which dsh tree does the shell supervise?
//!
//! Sources (bundled-runtime plan §2.3/§2.4):
//!
//! 1. `DSH_DESKTOP_NODE` / `DSH_DESKTOP_DSH` — an explicit choice, always honoured first
//! 2. the user's own installation ("system"), used when it is present *and* passes the gates
//! 3. the bundled seed in the app bundle, plus the writable shadow prefix that core updates
//!    install into (whichever holds the higher dsh version wins)
//!
//! Everything here is pure: the caller reads the filesystem, probes node and decides whether a
//! system installation passes the architecture/capability gates, then hands the results in.
//! Keeping the policy separate is what makes the four-cell matrix of §2.4 testable offline.

use crate::update::Version;
use std::path::{Path, PathBuf};

/// Where a runtime half came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Chosen through the environment: a debugging switch, never overridden.
    Env,
    /// Read-only seed shipped inside the app bundle.
    Seed,
    /// Writable copy under app-data that core updates install into.
    Shadow,
    /// The user's own installation.
    System,
}

impl Origin {
    /// Label for the log and the status page.
    pub fn label(self) -> &'static str {
        match self {
            Origin::Env => "env",
            Origin::Seed => "bundled",
            Origin::Shadow => "bundled(updated)",
            Origin::System => "system",
        }
    }
}

/// `runtime` in config.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Preference {
    /// Use the installed runtime when it is complete and passes the gates, otherwise mix in the
    /// bundled halves (§2.4).
    #[default]
    Auto,
    /// Ignore the system installation entirely: the shipped runtime is the tested one.
    Bundled,
    /// Development switch: behave like the shell did before the runtime existed.
    System,
}

impl Preference {
    /// `DSH_DESKTOP_RUNTIME_PREFERENCE` overrides `config.json`: the same escape hatch as
    /// `DSH_DESKTOP_DSH`, and the only way to try the bundled runtime on a machine that has a
    /// perfectly good installed one.
    pub fn parse(value: &str) -> Option<Preference> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Preference::Auto),
            "bundled" => Some(Preference::Bundled),
            "system" => Some(Preference::System),
            _ => None,
        }
    }
}

/// One half of the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub path: PathBuf,
    pub origin: Origin,
}

/// What a core update may do with the chosen CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Updates {
    /// Install into `app-data/runtime/prefix`, never touching the seed or the user's install.
    Shadow,
    /// Leave it alone and only report that a newer core exists (§2.4: the shell never writes a
    /// user-managed prefix).
    Notify,
}

/// Inputs, already gated by the caller (a system path that fails the arch/capability gates must
/// be passed as `None`).
pub struct Inputs<'a> {
    pub preference: Preference,
    pub env_node: Option<PathBuf>,
    pub env_dsh: Option<PathBuf>,
    pub system_node: Option<PathBuf>,
    pub system_dsh: Option<PathBuf>,
    pub seed_node: Option<&'a Path>,
    pub seed_dsh: Option<&'a Path>,
    pub shadow_dsh: Option<&'a Path>,
    pub seed_version: Option<&'a str>,
    pub shadow_version: Option<&'a str>,
}

/// The decision, plus what updates are allowed to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub node: Pick,
    pub dsh: Pick,
    pub updates: Updates,
}

impl Decision {
    /// One line for the log: which runtime the shell is about to supervise.
    pub fn describe(&self) -> String {
        format!(
            "runtime: node {} ({}) + dsh {} ({})",
            self.node.path.display(),
            self.node.origin.label(),
            self.dsh.path.display(),
            self.dsh.origin.label()
        )
    }
}

/// Pick the node and the dsh tree to supervise.
///
/// `None` means there was no candidate at all — a build without a bundled runtime on a machine
/// with no installation either; the caller turns that into the "install node + dsh" error page.
pub fn decide(input: &Inputs<'_>) -> Option<Decision> {
    let node = pick_node(input)?;
    let dsh = pick_dsh(input)?;
    let updates = match dsh.origin {
        Origin::System => Updates::Notify,
        _ => Updates::Shadow,
    };
    Some(Decision { node, dsh, updates })
}

fn pick_node(input: &Inputs<'_>) -> Option<Pick> {
    if let Some(path) = &input.env_node {
        return Some(Pick {
            path: path.clone(),
            origin: Origin::Env,
        });
    }
    if matches!(input.preference, Preference::Auto | Preference::System) {
        if let Some(path) = &input.system_node {
            return Some(Pick {
                path: path.clone(),
                origin: Origin::System,
            });
        }
    }
    bundled_node(input).or_else(|| {
        // `bundled` was asked for but this build ships no seed: falling back beats refusing to
        // start, and the caller logs the mismatch.
        input.system_node.clone().map(|path| Pick {
            path,
            origin: Origin::System,
        })
    })
}

fn bundled_node(input: &Inputs<'_>) -> Option<Pick> {
    input.seed_node.map(|path| Pick {
        path: path.to_path_buf(),
        origin: Origin::Seed,
    })
}

fn pick_dsh(input: &Inputs<'_>) -> Option<Pick> {
    if let Some(path) = &input.env_dsh {
        return Some(Pick {
            path: path.clone(),
            origin: Origin::Env,
        });
    }
    if matches!(input.preference, Preference::Auto | Preference::System) {
        if let Some(path) = &input.system_dsh {
            return Some(Pick {
                path: path.clone(),
                origin: Origin::System,
            });
        }
    }
    pick_bundled(input).or_else(|| {
        input.system_dsh.clone().map(|path| Pick {
            path,
            origin: Origin::System,
        })
    })
}

/// The bundled half: shadow prefix and seed, higher version wins (§2.3, so a fresh `.app` with a
/// newer seed is not shadowed by an older copy under app-data).
fn pick_bundled(input: &Inputs<'_>) -> Option<Pick> {
    let seed = input.seed_dsh.map(|path| Pick {
        path: path.to_path_buf(),
        origin: Origin::Seed,
    });
    let Some(shadow_path) = input.shadow_dsh else {
        return seed;
    };
    let shadow = Pick {
        path: shadow_path.to_path_buf(),
        origin: Origin::Shadow,
    };
    // Only the shadow copy exists.
    let Some(seed) = seed else {
        return Some(shadow);
    };
    match (input.seed_version, input.shadow_version) {
        (Some(seed_version), Some(shadow_version)) => {
            match (Version::parse(seed_version), Version::parse(shadow_version)) {
                (Some(seed_version), Some(shadow_version)) if shadow_version > seed_version => {
                    Some(shadow)
                }
                (Some(_), Some(_)) => Some(seed),
                // Unparsable versions: prefer the copy that exists rather than guessing.
                _ => Some(shadow),
            }
        }
        // Without versions we cannot tell them apart; the shadow copy only exists because an
        // update put it there, so it is the one to run.
        _ => Some(shadow),
    }
}

#[cfg(test)]
mod tests {
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
}
