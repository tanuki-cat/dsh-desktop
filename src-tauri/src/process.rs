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

/// Minimal Win32 bindings: no extra dependency, one handle probe.
#[cfg(windows)]
mod win {
    pub type Handle = *mut core::ffi::c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: u32 = 5;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    /// True while the pid is a running process.
    ///
    /// A failed `OpenProcess` means "gone" — except for access denied, which means the
    /// process exists but belongs to someone else. Reporting that as gone would make the
    /// shell skip cleaning up an instance it cannot inspect.
    pub fn alive(pid: u32) -> bool {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE
        }
    }
}

/// Windows has no SIGTERM: `taskkill` without `/F` asks the process to close (the closest
/// thing to a graceful stop), `/F` is the force-kill counterpart of SIGKILL.
#[cfg(windows)]
fn signal_pid(pid: u32, signal: TermSignal) -> bool {
    taskkill(pid, false, matches!(signal, TermSignal::Kill))
}

#[cfg(windows)]
fn taskkill(pid: u32, tree: bool, force: bool) -> bool {
    let mut command = std::process::Command::new("taskkill");
    command.args(["/PID", &pid.to_string()]);
    if tree {
        command.arg("/T");
    }
    if force {
        command.arg("/F");
    }
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_signal(pid: u32, signal: Option<TermSignal>) -> bool {
    let sig = match signal {
        None => return pid_alive(pid),
        Some(TermSignal::Term) => libc::SIGTERM,
        Some(TermSignal::Kill) => libc::SIGKILL,
    };
    // Negative pid targets the process group created by `process_group(0)`.
    unsafe { libc::kill(-(pid as i32), sig) == 0 || libc::kill(pid as i32, sig) == 0 }
}

/// True while the pid is a running process.
///
/// The plain `kill(pid, 0)` probe is not enough: it also succeeds for a zombie, and a child of
/// ours stays one until it is reaped. The supervised Harness is our child, so probing with the
/// signal alone would report "still running" for the whole grace period and turn every quit
/// into a fixed multi-second wait. `waitpid` settles that case; anything that is not our child
/// (leftovers from an earlier run, foreign instances) falls back to the signal probe.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    if reaped == pid as i32 {
        false
    } else if reaped == 0 {
        true
    } else {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

/// Windows: liveness through `OpenProcess` + `GetExitCodeProcess` (a plain "assume alive"
/// made `terminate` sit out the whole grace period and then report failure), termination
/// through `taskkill` — graceful first, `/F` only after the grace period.
#[cfg(windows)]
fn kill_signal(pid: u32, signal: Option<TermSignal>) -> bool {
    match signal {
        None => win::alive(pid),
        Some(TermSignal::Term) => taskkill(pid, true, false),
        Some(TermSignal::Kill) => taskkill(pid, true, true),
    }
}

/// Keep a file only this user can read. Used for the state file and for config.json, whose
/// `env` map may hold credentials.
#[cfg(unix)]
pub fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn restrict(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn terminate_stops_a_spawned_process_group() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        // Same shape as the supervised Harness: own process group, silent stdio.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(is_alive(pid));

        let started = Instant::now();
        assert!(
            terminate(pid, Duration::from_secs(3)),
            "terminate must report success"
        );
        assert!(!is_alive(pid), "pid {pid} must be gone after terminate");
        // A child that exited but was never reaped used to look alive for the whole grace
        // period, so quit always took the full timeout. SIGTERM must settle this fast.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "terminate waited for a zombie: {:?}",
            started.elapsed()
        );
        let _ = child.wait();
    }

    #[test]
    fn state_round_trip() {
        let dir = std::env::temp_dir().join("dsh-desktop-state-test");
        let _ = std::fs::remove_dir_all(&dir);
        let state = HarnessState {
            pid: 4242,
            port: 3080,
            cwd: "/tmp".into(),
            started_at: 7,
        };
        write_state(&dir, &state).unwrap();
        let back = read_state(&dir).unwrap();
        assert_eq!(back.pid, 4242);
        assert_eq!(back.port, 3080);
        clear_state(&dir);
        assert!(read_state(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
