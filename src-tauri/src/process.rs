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

/// Stop a process this shell spawned for a short-lived job (npm, pnpm, the CLI's plugin command)
/// together with whatever it spawned.
///
/// Unlike [`terminate`] this does not wait for a graceful exit: the caller has already decided
/// the job ran past its budget, and a package manager that ignored SIGTERM once will ignore it
/// again. It is never used on a Harness the user may be watching — only on the update helpers,
/// whose whole purpose is to finish and exit.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // The child was spawned with `process_group(0)`, so the negative pid is its own group.
        // Falling back to the pid covers a caller that spawned it without one.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // `/T` reaches the tree, `/F` skips the grace period the caller already spent.
        let _ = taskkill(pid, true, true);
    }
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

/// Windows: replace the file's ACL with one entry for this user only.
///
/// The user profile directory is not a boundary here: a file created under it inherits whatever
/// the parent grants, which on a shared or domain-joined machine can include other accounts and
/// the local `Users` group. `config.json` may hold an API key and `state.json` names the
/// supervised process, so both get an explicit owner-only ACL rather than an inherited one.
///
/// `icacls` is part of every supported Windows install, which is why this does not pull in a
/// Win32 security dependency: the shell already reaches for `taskkill` and `netstat` the same way.
#[cfg(windows)]
pub fn restrict(path: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Without an account to grant there is nothing safe to do: `/inheritance:r` on its own
    // would strip every entry and could leave the file unreachable, so the ACL stays as it is.
    let Some(account) = current_account() else {
        return;
    };
    // `/inheritance:r` drops everything inherited from the parent, and `/grant:r` replaces this
    // account's entries with full control, so the result is exactly one principal.
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{account}:F"))
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    // Best effort, like the Unix branch: a failure here must not stop the app from starting.
    let _ = status;
}

/// The account an ACL grant should name, as `DOMAIN\user` or a bare user name.
#[cfg(windows)]
fn current_account() -> Option<String> {
    // `USERDOMAIN\USERNAME` is what `icacls` accepts for a local or domain account; a bare
    // `USERNAME` is the fallback when the domain is not in the environment.
    match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) if !domain.is_empty() && !user.is_empty() => {
            Some(format!("{domain}\\{user}"))
        }
        (_, Ok(user)) if !user.is_empty() => Some(user),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
