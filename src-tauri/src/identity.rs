//! Whether a leftover process is the `dsh web` this shell started.
//!
//! A crashed shell leaves its Harness behind. Before anything is signalled, the record has
//! to be matched against a real process: a reboot can hand the same low pid to an unrelated
//! program, and a process-group signal would take that program's group with it.

use crate::harness;
use crate::process::HarnessState;

/// What step 1 does with the record a crashed shell may have left behind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SelfHeal {
    /// The record is stale (the pid is gone, or it belongs to something else now): drop it.
    Clear,
    /// The record is ours and still owns its port, but this run will use another one: stop it.
    Terminate,
    /// Ours, still serving the port this run will use: keep the file so the detection step can
    /// reuse the instance instead of restarting it.
    Keep,
}

/// Pure decision for step 1, so the three branches are testable without a running process.
///
/// `listener` is the pid that owns `state.port`; `looks_ours` is the secondary identity check
/// for a record whose pid no longer owns its port (see [`looks_like_our_orphan`]).
pub(crate) fn self_heal_action(
    state: &HarnessState,
    listener: Option<u32>,
    alive: bool,
    port: u16,
    looks_ours: bool,
) -> SelfHeal {
    if !alive {
        return SelfHeal::Clear;
    }
    if listener != Some(state.pid) {
        // The pid was reused, or the instance lost its port: only a process that still looks
        // like our own orphaned CLI may be signalled.
        return if looks_ours {
            SelfHeal::Terminate
        } else {
            SelfHeal::Clear
        };
    }
    if state.port == port {
        SelfHeal::Keep
    } else {
        SelfHeal::Terminate
    }
}

/// Does this pid still look like the `dsh web` this shell started?
///
/// Only consulted for a record whose pid no longer owns its port, where the other possible
/// reading is pid reuse. Two signals make that misread unlikely: the command line carries the
/// flags this shell passes (`--profile web`, a `dsh` entry point), and nobody owns the process
/// any more — a leftover of a crashed shell is handed to the session supervisor, while a
/// session someone still runs (a terminal, or another shell of ours) keeps its parent.
pub(crate) fn looks_like_our_orphan(pid: u32) -> bool {
    // `-ww` matters: the flags that identify our spawn sit behind a long node path, and some
    // `ps` builds truncate the command column to the terminal width without it.
    let output = match std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "ppid=,command="])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            harness::app_log(&format!(
                "ps 不可用，无法确认残留进程 {pid} 的身份，按无关进程处理: {error}"
            ));
            return false;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((ppid, _)) = parse_ps_identity(&text) else {
        return false;
    };
    looks_like_our_harness(&text, classify_parent(ppid))
}

/// `ps -p <pid> -o ppid=,command=` output -> (parent pid, command line).
fn parse_ps_identity(output: &str) -> Option<(u32, String)> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    let mut fields = line.split_whitespace();
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, fields.collect::<Vec<_>>().join(" ")))
}

/// Who owns the candidate process now?
#[derive(Debug, PartialEq, Eq)]
enum Parent {
    /// The parent is gone: the ordinary outcome of a shell crash on macOS and Linux alike.
    Gone,
    /// A session supervisor adopted it: launchd (pid 1) on macOS, `systemd --user` on most Linux
    /// desktops. The latter is a child subreaper, so the orphan lands on a pid far from 1 —
    /// assuming pid 1 here would leave the Linux leftovers uncleaned (review R1).
    Supervisor,
    /// A live process that is neither: a shell or another `dsh-desktop`, so somebody still runs
    /// this session. Never signal it.
    Live,
}

/// Classify the candidate's parent.
///
/// Anything that cannot be established counts as [`Parent::Live`]: when the answer is unknown,
/// keeping the process is the safe side.
fn classify_parent(ppid: u32) -> Parent {
    if ppid == 0 {
        // The kernel: no userspace parent is left to own it.
        return Parent::Gone;
    }
    let output = match std::process::Command::new("ps")
        .args(["-ww", "-p", &ppid.to_string(), "-o", "command="])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Parent::Live,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(command) = text.lines().find(|line| !line.trim().is_empty()) else {
        return Parent::Gone;
    };
    if is_session_supervisor(command) {
        Parent::Supervisor
    } else {
        Parent::Live
    }
}

/// Names an orphan is handed to on the platforms this shell builds for.
fn is_session_supervisor(command: &str) -> bool {
    command.contains("launchd") || command.contains("systemd") || command.contains("init")
}

/// Is this candidate the orphaned `dsh web` this shell started?
fn looks_like_our_harness(output: &str, parent: Parent) -> bool {
    let Some((_, command)) = parse_ps_identity(output) else {
        return false;
    };
    harness::looks_like_dsh_web(&command) && parent != Parent::Live
}

#[cfg(test)]
mod tests;
