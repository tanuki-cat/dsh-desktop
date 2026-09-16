//! What the startup path does about a Harness it did not start.
//!
//! Identifying that instance, asking the user about it, and waiting out the port
//! handovers around stopping it.

use crate::{free_port_from, Config, HANDOFF_POLL};
use crate::{harness, window};
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// What the startup path does about a Harness it did not start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ForeignAction {
    /// Stop that instance and start our own on the port.
    TakeOver { pid: u32 },
    /// Leave it running and open it in the system browser instead.
    UseBrowser,
    /// Leave it running and explain why this shell cannot use the port.
    Refuse {
        reason: String,
        /// True when the user answered the question with "cancel". The distinction matters to
        /// a retry: an explicit refusal is final for this restart, while an unproven identity
        /// or a held port is a state the user may have just fixed.
        declined: bool,
    },
    /// A Harness this shell can identify. The caller puts the question and turns the answer back
    /// into one of the other arms.
    Ask { pid: u32 },
    /// Leave that instance alone and start on another port, so neither side is disturbed.
    UseOtherPort { port: u16 },
}

/// Decide what to do about a foreign Harness, from the two identity signals alone.
///
/// `command` is the listener's command line, or `None` when the platform would not report it.
/// Both a fence-shaped answer and a `dsh web` command line are required before anything is
/// signalled: the fence proves a Harness protocol is on the port, and the command line proves the
/// process is the CLI rather than an unrelated server that happens to answer the same way.
///
/// A process that passes both is **always** asked about, whatever `config.json` says. The config
/// is the answer for a question nobody replied to, never a reason to skip asking: gating the
/// question on it made the choice invisible to everyone who had not already edited the file,
/// which is the opposite of what the review asked for (2026-09-16).
pub(crate) fn foreign_instance_action(owner: Option<u32>, command: Option<&str>) -> ForeignAction {
    let identified = command.is_some_and(harness::looks_like_dsh_web);
    match (owner, identified) {
        // Nothing may be signalled that this shell cannot identify, whoever asked.
        (_, false) => ForeignAction::Refuse {
            reason: "端口上有进程按 Harness 协议应答，但无法确认它就是 dsh web（读不到命令行，或命令行不像 dsh）。为避免误杀其它程序，本应用不会接管它。请先手动停止该进程，或在 config.json 里换一个端口。"
                .to_string(),
            declined: false,
        },
        // Identified: ask, because stopping it kills a session someone may be watching and
        // restarts the instance under a different workspace.
        (Some(pid), true) => ForeignAction::Ask { pid },
        // `identified` already proved a command line exists, so this arm is unreachable; it keeps
        // the match total without a panic in a startup path.
        (None, true) => ForeignAction::Refuse {
            reason: "端口上的 Harness 无法定位到具体进程（lsof 不可用）。请先手动停止它。"
                .to_string(),
            declined: false,
        },
    }
}

/// The ids the takeover question answers with.
const CHOICE_TAKE_OVER: &str = "take-over";
const CHOICE_BROWSER: &str = "browser";
pub(crate) const CHOICE_CANCEL: &str = "cancel";
/// Start on another port instead, leaving the instance where it is.
pub(crate) const CHOICE_PORT: &str = "port";

/// Which terminal page an outcome lands on.
///
/// Not cosmetic: [`window::show_failure`] arms the restart button *and* the Dock/Reopen entry
/// points, so a page offering a restart this shell cannot perform turns a dead end into a loop.
/// The browser fallback did exactly that — every click ran `start()` again, opened another tab,
/// and landed on the same page (2026-09-16).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminalPage {
    /// Something can still be started here: offer another attempt.
    Failure,
    /// Nothing this shell will start: report it and stop.
    Notice,
}

/// A restart only helps when the shell would plausibly start its own Harness next time.
///
/// Leaving the port to an instance this shell will not take over is the case where it cannot:
/// the instance is alive and serving, so a second attempt repeats the first exactly.
pub(crate) fn terminal_page(action: &ForeignAction) -> TerminalPage {
    match action {
        ForeignAction::UseBrowser => TerminalPage::Notice,
        // A refusal names something the user has to change — an unproven identity, a port held
        // by an unrelated program. The fix may well be followed by another attempt right here,
        // so the button is worth offering.
        _ => TerminalPage::Failure,
    }
}

/// How much of a command line the question shows.
///
/// These lines carry two deep absolute paths (the node that runs the CLI and the CLI itself), so
/// they run to several hundred characters on any real installation. The box they go in scrolls,
/// and a scroll box hides its own content: what the user reads has to be the part that identifies
/// the process.
const COMMAND_DISPLAY_LIMIT: usize = 160;

/// Shorten a command line for display, keeping both ends recognisable.
///
/// The middle is what goes. The start names the interpreter and the tree the CLI was installed
/// into; the end carries the flags (`--profile web`, `--port`) that say what it is doing. Those
/// are the two things a person checks to answer "is this mine?", and the path depth in between is
/// exactly what makes the line too long to show.
fn elide_command(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    if chars.len() <= COMMAND_DISPLAY_LIMIT {
        return command.to_string();
    }
    let head = COMMAND_DISPLAY_LIMIT / 2;
    let tail = COMMAND_DISPLAY_LIMIT - head;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// The question put to the user when a Harness this shell did not start is in the way.
///
/// The detail is the whole point of asking: a pid alone does not tell the user which terminal
/// session, agent run or workspace they are about to end. So it leads with what answering yes
/// would *do* — end that session and restart under this shell's workspace — and only then shows
/// the process as evidence. The order matters on a small window, where the bottom of this text is
/// the part that scrolls out of sight.
///
/// The line about old tabs is measured, not assumed (2026-09-16): after a kill and a restart on
/// the same port, the cookie the *previous* instance issued is still accepted, because the
/// signing key is stable per `DSH_HOME` and the authority did not change. A tampered cookie is
/// rejected, so that acceptance is real verification and not a permissive fence. Change the
/// `DSH_HOME` and the same cookie is answered `401` — hence the second half of that line, which
/// says what to do instead of promising that every tab keeps working.
fn takeover_question(
    port: u16,
    pid: u32,
    command: Option<&str>,
    workspace: &Path,
) -> (String, String) {
    let detail = format!(
        "端口 127.0.0.1:{port} 上已有一个不是本应用启动的 Harness。\n\n\
         接管会先终止该进程 —— 它当前的会话、正在执行的 agent 任务、浏览器里已打开的页面都会断开 ——\
         然后用本应用的 workspace 重新启动：\n{}\n\n\
         浏览器里指向这个端口的旧标签页不用手动关：刷新就会连到重启后的实例。\
         若显示 authentication required，说明那个实例用的是另一个 dsh_home，\
         需要在启动它的终端里重新打开它打印的 URL。\n\n\
         要接管的进程: pid {pid}\n\
         命令行: {}\n\n\
         不接管则用系统浏览器打开那个实例，本应用退出。",
        workspace.display(),
        command
            .map(elide_command)
            .as_deref()
            .unwrap_or("<读不到命令行>")
    );
    ("检测到其它 Harness".to_string(), detail)
}

/// Put the takeover question and turn the answer into an action.
///
/// Never answering is not the same as choosing the browser: `config.json` says what an unanswered
/// question means, and the hint under the buttons repeats it, so the timeout runs the config's
/// answer rather than a hardcoded one.
///
/// A question that cannot be put at all — no status window, a page that never loaded — is
/// *unanswered* in the same sense, which keeps one rule for both.
pub(crate) fn resolve_foreign_action(
    app: &AppHandle,
    config: &Config,
    port: u16,
    action: ForeignAction,
) -> ForeignAction {
    let ForeignAction::Ask { pid } = action else {
        return action;
    };
    let command = harness::process_command(pid);
    let (status, detail) = takeover_question(port, pid, command.as_deref(), &config.workspace);
    // Offered only when a free port exists: a dead button is worse than one option fewer, and the
    // search is what the arm below would need anyway.
    let other_port = free_port_from(port);
    let mut options = vec![
        window::ChoiceOption {
            id: CHOICE_TAKE_OVER.to_string(),
            label: format!("终止 pid {pid} 并接管端口 {port}"),
        },
        window::ChoiceOption {
            id: CHOICE_BROWSER.to_string(),
            label: "保留它，用系统浏览器打开".to_string(),
        },
    ];
    if let Some(free) = other_port {
        options.push(window::ChoiceOption {
            id: CHOICE_PORT.to_string(),
            label: format!("保留它，本应用改用端口 {free}"),
        });
    }
    options.push(window::ChoiceOption {
        id: CHOICE_CANCEL.to_string(),
        label: "什么都不做，退出本应用".to_string(),
    });
    let hint = format!(
        "{} 秒内没有选择将按 config.json 的 take_over_existing={} 处理。",
        window::CHOICE_TIMEOUT.as_secs(),
        config.take_over_existing
    );
    window::set_status(app, &status, &detail);
    match window::ask_choice(app, &status, &detail, &options, &hint).as_deref() {
        Some(CHOICE_TAKE_OVER) => ForeignAction::TakeOver { pid },
        Some(CHOICE_BROWSER) => ForeignAction::UseBrowser,
        Some(CHOICE_PORT) if other_port.is_some() => ForeignAction::UseOtherPort {
            port: other_port.unwrap_or(port),
        },
        Some(CHOICE_CANCEL) => ForeignAction::Refuse {
            reason: format!(
                "已按你的选择保留端口 {port} 上的 Harness（pid {pid}），本应用没有接管它。"
            ),
            declined: true,
        },
        // Unanswered, or an id this build no longer offers: the config decides.
        _ => unanswered_choice(config.take_over_existing, pid),
    }
}

/// What an unanswered question means, which is what `config.json` asked for.
///
/// Split out so the rule is testable without a window: a timeout and a question that could not be
/// put have to land in exactly the same place, or a headless launch would behave differently from
/// an ignored one.
fn unanswered_choice(allow: bool, pid: u32) -> ForeignAction {
    if allow {
        ForeignAction::TakeOver { pid }
    } else {
        ForeignAction::UseBrowser
    }
}

/// What a restart should do about the instance now holding the port.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// The user let this shell signal it: stop it and try once more.
    TakeOver,
    /// The user said no. The answer is already given — asking again during the retry would put
    /// the same question twice for one restart, which is what it looked like from the outside.
    Declined,
    /// Nobody was asked, or the answer was not permission: report the original failure.
    Refused,
}

/// The same question for a Harness that appeared during a handoff or a restart.
///
/// An unanswered question follows `config.json`, and "use another port" is *not* permission to
/// signal: that instance is exactly what the user asked the shell to keep.
pub(crate) fn confirm_takeover(app: &AppHandle, config: &Config, port: u16, pid: u32) -> Retry {
    match resolve_foreign_action(app, config, port, ForeignAction::Ask { pid }) {
        ForeignAction::TakeOver { .. } => Retry::TakeOver,
        ForeignAction::Refuse { declined: true, .. } => Retry::Declined,
        _ => Retry::Refused,
    }
}

/// The pid serving `port` as a Harness, when one is.
pub(crate) fn harness_listener(port: u16) -> Option<u32> {
    if matches!(harness::probe(port), harness::Probe::Harness) {
        harness::listener_pid(port)
    } else {
        None
    }
}

/// Is this pid proven to be the `dsh web` CLI, and so safe to signal?
///
/// The same two-signal rule the startup path applies through [`foreign_instance_action`]: the auth
/// fence put a Harness on the port, and the command line has to name the CLI. A pid whose command
/// line cannot be read is not proven, and every kill path consults this rather than assuming the
/// fence was enough.
pub(crate) fn identified_dsh_web(pid: u32) -> bool {
    harness::process_command(pid).is_some_and(|command| harness::looks_like_dsh_web(&command))
}

/// Wait out a handoff: another process may be booting a replacement on our port.
pub(crate) fn wait_for_handoff(port: u16, grace: Duration) -> Option<u32> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(pid) = harness_listener(port) {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(HANDOFF_POLL);
    }
}

/// Wait for `port` to stop answering, so the next launch can bind it.
pub(crate) fn wait_for_port_free(port: u16, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(harness::probe(port), harness::Probe::Closed) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests;
