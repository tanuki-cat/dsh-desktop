//! Child-process lifetime, the on-disk state file, and stale-instance recovery.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
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

/// How long to keep waiting for a finished child's output before giving up on it.
///
/// The child has already exited, so the reader normally reaches end-of-file at once; this wait only
/// runs out when a grandchild inherited the pipe and keeps it open, or when the reader thread has
/// not been scheduled yet on a loaded machine. The second case is why it is not shorter: output cut
/// here is incomplete, and the caller then drops the partial last line rather than use it.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Bytes kept from a helper's stdout. Every caller wants one line; this is the backstop for a
/// helper that ignores its arguments and prints a whole document.
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// Run a short-lived helper and return the first non-empty line of its stdout.
///
/// `None` means the command **could not be run to completion** — it failed to spawn, it ran past
/// `timeout` and was killed, or the only line it printed could not be kept whole. `Some("")` means
/// it ran and printed nothing, which callers must be able to tell apart: "no such process" is an
/// answer, while "`ps` never ran" is not.
///
/// `Command::output()` waits for ever. Several callers here run a **login shell**
/// (`$SHELL -lc …`), which sources arbitrary user rc files: one that waits on a network mount, a
/// password prompt, or stdin parks whichever thread asked — the startup path in every case — with
/// no way out for the user. The others run `npm`, `lsof` or `ps`, which stall on exactly the
/// proxies, locks and unresponsive filesystems their own budgets exist for.
///
/// Only stdout is collected, and only its first non-empty line: every caller is looking for one
/// path or one version on one line, so stderr and the rest of the stream are dropped rather than
/// buffered. The exit status is deliberately not folded in — callers validate the content they
/// need (`is_file`, a version parse), and one of them treats empty output as a real answer.
pub fn stdout_within(command: &mut std::process::Command, timeout: Duration) -> Option<String> {
    let output = output_within(command, timeout, OUTPUT_LIMIT)?;
    let line = first_line(&output.text);
    // Empty-but-truncated is not "printed nothing": the only line there was got cut, and reading
    // that as an answer would turn "`ps` said something we could not keep" into "no such process".
    if line.is_empty() && output.truncated {
        return None;
    }
    Some(line)
}

/// What a bounded helper run produced.
///
/// Distinct from `update::Captured`, which is a bounded *pipe* buffer for a long-running install:
/// this one is the decoded answer to a short helper call.
pub struct HelperOutput {
    /// The child's stdout, decoded lossily and capped at the caller's limit.
    pub text: String,
    /// True when `text` is not the whole story: the child wrote more than the cap, or the pipe was
    /// still open when the drain gave up. A partial last line is dropped in both cases, so `text`
    /// only ever holds complete lines then — a value cut mid-way (an API key in an `env` dump)
    /// must not be mistaken for the real one.
    pub truncated: bool,
    /// The exit status, when the child was reaped before the budget ran out.
    pub status: Option<std::process::ExitStatus>,
}

/// Run a command under a whole-process budget and collect a bounded slice of its stdout.
///
/// The pipe is always read to the end on its own thread — a reader that stopped early would leave
/// the child blocked on a full pipe for the rest of its budget — but only `limit` bytes are kept.
///
/// The reader reports through a channel rather than a `JoinHandle`, and the caller waits on it with
/// a **bounded** `recv_timeout` instead of joining. That matters because the pipe stays open while
/// *any* process holds the write end: a login shell exits immediately, but an agent or daemon its
/// rc file started in the background inherited stdout, so `join` would block until that process
/// exits — turning a one-second budget into an unbounded wait on the startup thread.
pub fn output_within(
    command: &mut std::process::Command,
    timeout: Duration,
    limit: usize,
) -> Option<HelperOutput> {
    command.stdin(std::process::Stdio::null());
    // Central, so no caller has to remember it: the shipped exe is `windows_subsystem =
    // "windows"`, and a console program (`powershell`, `netstat`, `lsof`, a login shell) spawned
    // without this flashes a window on screen for every call.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Published as it is read rather than sent once at the end: the pipe can outlive the child,
    // so waiting for the reader to finish before looking at what it read would throw away output
    // that has already arrived.
    let shared = Arc::new(Mutex::new(Drained::default()));
    let (tx, rx) = mpsc::channel();
    let sink = Arc::clone(&shared);
    std::thread::spawn(move || {
        let mut pipe = stdout;
        let mut chunk = [0u8; 8 * 1024];
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut sink = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let room = limit.saturating_sub(sink.bytes.len());
            let take = room.min(read);
            sink.bytes.extend_from_slice(&chunk[..take]);
            if take < read {
                sink.truncated = true;
            }
        }
        let _ = tx.send(());
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            // Any exit status: a non-zero `ps` that printed nothing is still an answer.
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // The budget ran out and the child was killed: the command did not run to completion, which
    // is the one thing `None` means to every caller.
    let status = status?;

    // Bounded even after the child exited: the pipe may outlive it (see the doc above). What was
    // already read stays available either way, because the reader published it.
    let complete = rx.recv_timeout(DRAIN_GRACE).is_ok();
    let drained = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let truncated = drained.truncated || !complete;
    let kept = if truncated {
        complete_lines(&drained.bytes)
    } else {
        &drained.bytes[..]
    };
    // Lossy: a path with a non-UTF-8 byte must not turn into "no output at all".
    Some(HelperOutput {
        text: String::from_utf8_lossy(kept).into_owned(),
        truncated,
        status: Some(status),
    })
}

/// The bytes up to and including the last newline: the lines that are known to be whole.
fn complete_lines(bytes: &[u8]) -> &[u8] {
    match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(at) => &bytes[..=at],
        None => &[],
    }
}

/// What the reader thread has collected so far, shared with the waiting caller.
#[derive(Default)]
struct Drained {
    bytes: Vec<u8>,
    truncated: bool,
}

/// First non-empty line, trimmed — the shape every [`stdout_within`] caller parses.
///
/// Empty when the command printed nothing at all, which callers rely on (see the doc above).
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_default()
}

/// SIGTERM the process group, wait for the grace period, then SIGKILL.
///
/// The group is signalled even when its leader is already gone: node spawns workers and MCP
/// children into the same group, and those outlive the leader. Returning early on a dead leader
/// left them running — holding the port, the memory, and any lock the tree came with — while the
/// shell believed it had cleaned up.
pub fn terminate(pid: u32, grace: Duration) -> bool {
    if !is_alive(pid) {
        // The leader is gone, but its group may not be: signal the group, and only the group.
        // The pid on its own now names nothing — or, once the OS reuses it, an unrelated process.
        clean_orphaned_group(pid);
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

/// Stop what is left of a process group whose leader has already exited.
///
/// Unix only, and by group id only. `kill(-pgid)` reaches the leader's surviving children and
/// fails harmlessly once the group is empty; it never falls back to the bare pid, which after the
/// leader's exit is free for the OS to hand to an unrelated process.
///
/// Windows has no process groups to address this way: `taskkill /T` walks the tree from a *live*
/// process, so a dead leader's pid reaches nothing of its own — and Windows reuses pids quickly,
/// so signalling it could reach somebody else's tree. There is nothing safe to do there.
#[cfg(unix)]
fn clean_orphaned_group(pgid: u32) {
    let group = -(pgid as i32);
    // SAFETY: `kill` has no memory-safety preconditions; a negative argument addresses a group.
    if unsafe { libc::kill(group, libc::SIGTERM) } != 0 {
        return;
    }
    // A short fixed wait: with the leader reaped there is no single process to poll, and the
    // children need a moment to act on SIGTERM before the group is killed outright.
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: as above.
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn clean_orphaned_group(_pgid: u32) {}

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

/// Keeps a console program spawned by this GUI app from flashing a window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new("taskkill");
    // The shipped exe is `windows_subsystem = "windows"`, so a console program spawned without
    // this flag flashes a console window on every call — and this one runs on the quit path.
    command.creation_flags(CREATE_NO_WINDOW);
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
