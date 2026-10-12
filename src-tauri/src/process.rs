//! Child-process lifetime, the on-disk state file, and stale-instance recovery.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// When the recorded process started, as the OS reports it (see [`process_start_marker`]).
    ///
    /// A pid alone is not an identity: a reboot, a crash loop or a long update window can hand
    /// the same number to an unrelated process, and every exit path here signals what the record
    /// names. Comparing the marker before signalling turns "the pid is alive" into "the pid is
    /// still the process this record describes". Absent in records written before the field
    /// existed. Missing and obsolete markers cannot authorize automatic cleanup.
    #[serde(default)]
    pub started_marker: Option<String>,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state.json")
}

fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

pub fn read_state(data_dir: &Path) -> Option<HarnessState> {
    let raw = std::fs::read_to_string(state_path(data_dir)).ok()?;
    let state: HarnessState = serde_json::from_str(&raw).ok()?;
    (valid_pid(state.pid) && state.port != 0 && Path::new(&state.cwd).is_absolute())
        .then_some(state)
}

impl HarnessState {
    pub fn identity(&self) -> Option<Identity> {
        Some(Identity {
            pid: self.pid,
            marker: self.started_marker.clone()?,
        })
    }
}

/// Create a private sibling exclusively, then replace the state in one rename.
pub fn write_state(data_dir: &Path, state: &HarnessState) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if !valid_pid(state.pid) || state.port == 0 || !Path::new(&state.cwd).is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Harness state",
        ));
    }
    std::fs::create_dir_all(data_dir)?;
    let path = state_path(data_dir);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let (temporary, mut file) = loop {
        let candidate = data_dir.join(format!(
            ".state-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match options.open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(state)?)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            restrict(&temporary);
            win::replace_file(&temporary, &path)?;
        }
        #[cfg(not(windows))]
        {
            std::fs::rename(&temporary, &path)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// OS birth identity, independent of exec and locale; unavailable identities never permit takeover.
pub fn process_start_marker(pid: u32) -> Option<String> {
    if !valid_pid(pid) {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: the output buffer is sized for the requested kernel structure.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(format!(
            "macos:{pid}:{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let start = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(format!("linux:{pid}:{}:{start}", boot.trim()))
    }
    #[cfg(windows)]
    {
        win::start_marker(pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    marker: String,
}

pub fn pin(pid: u32) -> Option<Identity> {
    Some(Identity {
        pid,
        marker: process_start_marker(pid)?,
    })
}

pub fn same_process(identity: &Identity) -> bool {
    process_start_marker(identity.pid).as_deref() == Some(identity.marker.as_str())
}

#[derive(Clone)]
struct Tracked {
    identity: Option<Identity>,
    group: bool,
}
static SPAWNED: Mutex<Option<BTreeMap<u32, Tracked>>> = Mutex::new(None);

/// Called immediately after spawn, never when reusing a previous launch.
pub fn remember_spawned(pid: u32) {
    if !valid_pid(pid) {
        return;
    }
    crate::harness::lock_or_recover(&SPAWNED)
        .get_or_insert_with(BTreeMap::new)
        .insert(
            pid,
            Tracked {
                identity: pin(pid),
                group: true,
            },
        );
}

pub fn remember_adopted(identity: Identity) {
    crate::harness::lock_or_recover(&SPAWNED)
        .get_or_insert_with(BTreeMap::new)
        .entry(identity.pid)
        .or_insert(Tracked {
            identity: Some(identity),
            group: false,
        });
}

pub fn forget_spawned(pid: u32) {
    if let Some(entries) = crate::harness::lock_or_recover(&SPAWNED).as_mut() {
        entries.remove(&pid);
    }
}

fn tracked(pid: u32) -> Option<Tracked> {
    crate::harness::lock_or_recover(&SPAWNED)
        .as_ref()?
        .get(&pid)
        .cloned()
}

pub fn spawned_here(pid: u32) -> bool {
    tracked(pid).is_some_and(|t| t.group)
}

pub fn saved_marker(pid: u32) -> Option<String> {
    tracked(pid)?.identity.map(|identity| identity.marker)
}

pub fn recorded_matches(state: &HarnessState) -> bool {
    state
        .identity()
        .is_some_and(|identity| same_process(&identity))
}

pub fn clear_state(data_dir: &Path) {
    let _ = std::fs::remove_file(state_path(data_dir));
}

pub fn is_alive(pid: u32) -> bool {
    valid_pid(pid) && kill_signal(pid)
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
    crate::shellenv::sanitize_command(command);
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
    let Some(tracked) = tracked(pid) else {
        return !is_alive(pid);
    };
    let result = if !is_alive(pid) {
        if tracked.group {
            clean_orphaned_group(pid);
        }
        true
    } else if let Some(identity) = &tracked.identity {
        terminate_identity(identity, tracked.group, grace)
    } else {
        crate::harness::app_log("无法读取进程身份，未发送终止信号");
        false
    };
    if result || !tracked.identity.as_ref().is_some_and(same_process) {
        forget_spawned(pid);
    }
    result
}

/// External identity must be captured before any user question or other wait.
pub fn terminate_pid(identity: &Identity, grace: Duration) -> bool {
    terminate_identity(identity, false, grace)
}

fn terminate_identity(identity: &Identity, group: bool, grace: Duration) -> bool {
    let pid = identity.pid;
    if !valid_pid(pid) || pid == std::process::id() {
        return false;
    }
    if !is_alive(pid) {
        return true;
    }
    if !same_process(identity) {
        return false;
    }
    if !signal_target(pid, group, TermSignal::Term) {
        return !is_alive(pid);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            if group {
                clean_orphaned_group(pid);
            }
            return true;
        }
        if !same_process(identity) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Escalation has its own check: SIGTERM may have ended the original process during the wait.
    if !same_process(identity) {
        return !is_alive(pid);
    }
    signal_target(pid, group, TermSignal::Kill);
    std::thread::sleep(Duration::from_millis(200));
    !is_alive(pid)
}

#[cfg(unix)]
fn signal_target(pid: u32, group: bool, signal: TermSignal) -> bool {
    let sig = match signal {
        TermSignal::Term => libc::SIGTERM,
        TermSignal::Kill => libc::SIGKILL,
    };
    if group {
        // Only a group leader created by this shell may receive a group signal; no PID fallback.
        if unsafe { libc::getpgid(pid as i32) } != pid as i32 {
            return false;
        }
    }
    unsafe { libc::kill(if group { -(pid as i32) } else { pid as i32 }, sig) == 0 }
}

#[cfg(windows)]
fn signal_target(pid: u32, group: bool, signal: TermSignal) -> bool {
    taskkill(pid, group, matches!(signal, TermSignal::Kill))
}

/// A dead leader's group may still contain workers. Never fall back to its now-free PID.
#[cfg(unix)]
fn clean_orphaned_group(pgid: u32) {
    if !valid_pid(pgid) {
        return;
    }
    let group = -(pgid as i32);
    if unsafe { libc::kill(group, libc::SIGTERM) } != 0 {
        return;
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn clean_orphaned_group(_pgid: u32) {}

/// Immediate cleanup of a helper spawned into its own process group.
pub fn kill_tree(pid: u32) -> bool {
    let killed = tracked(pid)
        .and_then(|t| t.identity.map(|identity| (identity, t.group)))
        .is_some_and(|(identity, group)| {
            same_process(&identity) && signal_target(pid, group, TermSignal::Kill)
        });
    forget_spawned(pid);
    killed
}

pub enum TermSignal {
    Term,
    Kill,
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
        fn GetProcessTimes(
            handle: Handle,
            created: *mut u64,
            exited: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }

    pub fn start_marker(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let (mut created, mut exited, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
            let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
            CloseHandle(handle);
            (ok != 0).then(|| format!("windows:{pid}:{created}"))
        }
    }

    pub fn replace_file(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
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
fn kill_signal(pid: u32) -> bool {
    pid_alive(pid)
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
fn kill_signal(pid: u32) -> bool {
    win::alive(pid)
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
