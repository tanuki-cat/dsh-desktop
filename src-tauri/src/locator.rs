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

pub fn locate(remembered: Option<PathBuf>, override_env: Option<String>) -> Result<DshLocation, String> {
    let launcher = find_launcher(remembered, override_env)?;
    let dsh_js = real_path(&launcher);
    let node = find_node(&launcher)?;
    Ok(DshLocation { launcher, dsh_js, node })
}

pub fn version(loc: &DshLocation) -> Option<String> {
    let out = Command::new(&loc.node).arg(&loc.dsh_js).arg("--version").output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

fn find_launcher(remembered: Option<PathBuf>, override_env: Option<String>) -> Result<PathBuf, String> {
    if let Some(raw) = override_env {
        let p = PathBuf::from(raw);
        if p.is_file() { return Ok(p); }
    }
    if let Some(p) = remembered {
        if p.is_file() { return Ok(p); }
    }
    if let Some(p) = path_lookup("dsh") { return Ok(p); }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let c = Path::new(dir).join("dsh");
        if c.is_file() { return Ok(c); }
    }
    if let Some(p) = login_shell_lookup("dsh") { return Ok(p); }
    Err("找不到 dsh。可用环境变量 DSH_DESKTOP_DSH 指定绝对路径。".into())
}

fn find_node(launcher: &Path) -> Result<PathBuf, String> {
    if let Ok(raw) = std::env::var("DSH_DESKTOP_NODE") {
        let p = PathBuf::from(raw);
        if p.is_file() { return Ok(p); }
    }
    if let Some(dir) = launcher.parent() {
        let c = dir.join("node");
        if c.is_file() { return Ok(c); }
    }
    if let Some(p) = path_lookup("node") { return Ok(p); }
    if let Some(p) = login_shell_lookup("node") { return Ok(p); }
    Err("找不到 node。dsh 以 `#!/usr/bin/env node` 运行，没有 node 时启动会直接失败（exit 127）。".into())
}

fn path_lookup(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|d| d.join(name)).find(|c| c.is_file())
}

fn login_shell_lookup(name: &str) -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let out = Command::new(shell).args(["-lc", &format!("command -v {name}")]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() { return None; }
    let p = PathBuf::from(text);
    if p.is_file() { Some(p) } else { None }
}

fn real_path(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

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
