//! Resolve the three executables the shell needs: dsh launcher, its real JS entry, and node.
//!
//! `dsh` ships as `#!/usr/bin/env node`, and a GUI-launched app has no Homebrew PATH, so
//! resolving the launcher alone is not enough: node must be resolved too.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct DshLocation {
    pub launcher: PathBuf,
    pub dsh_js: PathBuf,
    pub node: PathBuf,
}

pub fn locate(
    remembered: Option<PathBuf>,
    override_env: Option<String>,
) -> Result<DshLocation, String> {
    let launcher = find_launcher(remembered, override_env)?;
    let dsh_js = real_path(&launcher);
    let node = find_node(&launcher)?;
    Ok(DshLocation {
        launcher,
        dsh_js,
        node,
    })
}

/// What a node binary can do. The bundled runtime plan §2.4 gates the system installation on
/// two facts, because a mismatch only shows up as a crash much later:
/// the architecture (an x64 node under Rosetta cannot load the bundled arm64 prebuilds) and
/// `module.stripTypeScriptTypes` (the CLI's code-runtime worker needs it; `@deepseek-ai/dsh`
/// declares no `engines.node`, so a capability probe is the only honest check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeFacts {
    pub arch: String,
    pub strip_types: bool,
}

/// Probe node once (~80 ms) for the facts above.
pub fn probe_node(node: &Path) -> Option<NodeFacts> {
    const SCRIPT: &str =
        "process.stdout.write(process.arch + \" \" + (typeof require(\"module\").stripTypeScriptTypes))";
    let output = Command::new(node).arg("-e").arg(SCRIPT).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let arch = parts.next()?.to_string();
    let strip_types = parts.next() == Some("function");
    Some(NodeFacts { arch, strip_types })
}

/// Version of a CLI tree, from the package.json that owns the entry script.
pub fn version_of(dsh_js: &Path) -> Option<String> {
    version_from_package(dsh_js)
}

/// Version of the installed CLI. Reading the owning package.json costs ~1 ms, while booting
/// node for `--version` costs ~80 ms, so the file wins and the CLI is the fallback.
pub fn version(loc: &DshLocation) -> Option<String> {
    if let Some(version) = version_from_package(&loc.dsh_js) {
        return Some(version);
    }
    let out = Command::new(&loc.node)
        .arg(&loc.dsh_js)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Walk up from the entry script to the package that owns it and read its version.
fn version_from_package(dsh_js: &Path) -> Option<String> {
    let mut dir = dsh_js.parent()?;
    for _ in 0..5 {
        if let Ok(raw) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(version) = parsed.get("version").and_then(|value| value.as_str()) {
                    let version = version.trim();
                    if !version.is_empty() {
                        return Some(version.to_string());
                    }
                }
            }
        }
        dir = dir.parent()?;
    }
    None
}

fn find_launcher(
    remembered: Option<PathBuf>,
    override_env: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(raw) = override_env {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(p) = remembered {
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(p) = path_lookup("dsh") {
        return Ok(p);
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let c = Path::new(dir).join("dsh");
        if c.is_file() {
            return Ok(c);
        }
    }
    if let Some(p) = login_shell_lookup("dsh") {
        return Ok(p);
    }
    Err("找不到 dsh。可用环境变量 DSH_DESKTOP_DSH 指定绝对路径。".into())
}

fn find_node(launcher: &Path) -> Result<PathBuf, String> {
    if let Ok(raw) = std::env::var("DSH_DESKTOP_NODE") {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(dir) = launcher.parent() {
        for name in ["node", "node.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(p) = path_lookup("node") {
        return Ok(p);
    }
    if let Some(p) = login_shell_lookup("node") {
        return Ok(p);
    }
    Err(
        "找不到 node。dsh 以 `#!/usr/bin/env node` 运行，没有 node 时启动会直接失败（exit 127）。"
            .into(),
    )
}

fn path_lookup(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    // Windows resolves `node` to `node.exe` and `dsh` to `dsh.cmd`; a bare name would never
    // match there, which made the system runtime look "not installed".
    #[cfg(windows)]
    let names: Vec<String> = ["", ".exe", ".cmd", ".bat"]
        .iter()
        .map(|extension| format!("{name}{extension}"))
        .collect();
    #[cfg(not(windows))]
    let names: Vec<String> = vec![name.to_string()];
    for dir in std::env::split_paths(&paths) {
        for candidate in &names {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn login_shell_lookup(name: &str) -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let out = Command::new(shell)
        .args(["-lc", &format!("command -v {name}")])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    let p = PathBuf::from(text);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

fn real_path(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn reads_version_from_the_owning_package() {
        let dir = std::env::temp_dir().join("dsh-desktop-version-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/bin")).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","version":"1.2.3"}"#,
        )
        .unwrap();
        let entry = dir.join("lib/bin/bin.js");
        std::fs::write(&entry, "#!/usr/bin/env node\n").unwrap();
        assert_eq!(version_from_package(&entry).as_deref(), Some("1.2.3"));
        // Nothing to read above the script: the caller must fall back to running the CLI.
        let orphan = std::env::temp_dir().join("dsh-desktop-no-package/bin.js");
        assert_eq!(version_from_package(&orphan), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolves_real_js_behind_symlink() {
        let dir = std::env::temp_dir().join("dsh-desktop-locator-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/bin")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let js = dir.join("lib/bin/bin.js");
        std::fs::write(&js, "#!/usr/bin/env node\n").unwrap();
        let link = dir.join("bin/dsh");
        std::os::unix::fs::symlink(&js, &link).unwrap();
        let loc = locate(Some(link.clone()), None).unwrap();
        assert_eq!(loc.dsh_js, std::fs::canonicalize(&js).unwrap());
        assert!(loc.node.is_file());
    }
}
