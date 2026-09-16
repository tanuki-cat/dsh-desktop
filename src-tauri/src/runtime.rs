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

    /// The same label for a caller that has the value in hand.
    pub fn label_of(origin: Origin) -> &'static str {
        origin.label()
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

/// `system_updates` in config.json: what a newer core means for a CLI we do not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemUpdates {
    /// Upgrade the user's installation in place — the behaviour from before the bundled
    /// runtime existed. The default: silently losing auto-upgrade would be a functional
    /// regression for everyone already running this shell.
    #[default]
    Install,
    /// Report the new version and leave the tree alone (§2.4: never rewrite a prefix the
    /// shell does not own).
    Notify,
}

impl SystemUpdates {
    /// Label for the log line, so one lookup tells which policy ran.
    pub fn label(self) -> &'static str {
        match self {
            SystemUpdates::Install => "install",
            SystemUpdates::Notify => "notify",
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
    // Only a tree this shell owns may be rewritten by a core update, and `Shadow` installs into
    // the writable prefix. `Env` points at a tree that is not ours: classifying it as `Shadow`
    // both installed updates where they are never read and flipped the shell into "bundled"
    // mode, which rewrites PATH / npm_config_prefix / PNPM_HOME for the child (review P1-5).
    let updates = match dsh.origin {
        Origin::Seed | Origin::Shadow => Updates::Shadow,
        Origin::System | Origin::Env => Updates::Notify,
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
mod tests;
