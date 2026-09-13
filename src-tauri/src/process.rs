//! Child-process lifetime, the on-disk state file, and stale-instance recovery.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessState {
    pub pid: u32,
    pub port: u16,
    pub cwd: String,
    pub started_at: u64,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state.json")
}

pub fn read_state(data_dir: &Path) -> Option<HarnessState> {
    let raw = std::fs::read_to_string(state_path(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_state(data_dir: &Path, state: &HarnessState) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = state_path(data_dir);
    std::fs::write(&path, serde_json::to_vec_pretty(state)?)?;
    restrict(&path);
    Ok(())
}

pub fn clear_state(data_dir: &Path) {
    let _ = std::fs::remove_file(state_path(data_dir));
}

pub fn is_alive(pid: u32) -> bool {
    kill_signal(pid, None)
}

/// SIGTERM the process group, wait for the grace period, then SIGKILL.
pub fn terminate(pid: u32, grace: Duration) -> bool {
    if !is_alive(pid) {
        return true;
    }
    kill_signal(pid, Some(TermSignal::Term));
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    kill_signal(pid, Some(TermSignal::Kill));
    std::thread::sleep(Duration::from_millis(200));
    !is_alive(pid)
}

/// Signal a single PID: used for instances this shell did not start, whose process
/// group belongs to the user's terminal rather than to us.
pub fn terminate_pid(pid: u32, grace: Duration) -> bool {
    if !is_alive(pid) {
        return true;
    }
    signal_pid(pid, TermSignal::Term);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    signal_pid(pid, TermSignal::Kill);
    std::thread::sleep(Duration::from_millis(200));
    !is_alive(pid)
}

pub enum TermSignal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_pid(pid: u32, signal: TermSignal) -> bool {
    let sig = match signal {
        TermSignal::Term => libc::SIGTERM,
        TermSignal::Kill => libc::SIGKILL,
    };
    unsafe { libc::kill(pid as i32, sig) == 0 }
}

#[cfg(windows)]
fn signal_pid(pid: u32, _signal: TermSignal) -> bool {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_signal(pid: u32, signal: Option<TermSignal>) -> bool {
    let sig = match signal {
        None => 0,
        Some(TermSignal::Term) => libc::SIGTERM,
        Some(TermSignal::Kill) => libc::SIGKILL,
    };
    // Negative pid targets the process group created by `process_group(0)`.
    unsafe { libc::kill(-(pid as i32), sig) == 0 || libc::kill(pid as i32, sig) == 0 }
}

#[cfg(windows)]
fn kill_signal(pid: u32, signal: Option<TermSignal>) -> bool {
    match signal {
        None => true,
        Some(_) => std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    }
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let dir = std::env::temp_dir().join("dsh-desktop-state-test");
        let _ = std::fs::remove_dir_all(&dir);
        let state = HarnessState { pid: 4242, port: 3080, cwd: "/tmp".into(), started_at: 7 };
        write_state(&dir, &state).unwrap();
        let back = read_state(&dir).unwrap();
        assert_eq!(back.pid, 4242);
        assert_eq!(back.port, 3080);
        clear_state(&dir);
        assert!(read_state(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
