//! Unit tests for the identity module.
//!
//! A child module of the code under test, so `use super::*` reaches private
//! items.

use super::*;

#[test]
fn only_an_unowned_dsh_web_counts_as_our_leftover() {
    let line = " 8831 /opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --patch /x --no-open --port 3080";
    // A crash hands the leftover to launchd on macOS and to `systemd --user` on Linux.
    assert!(looks_like_our_harness(line, Parent::Gone));
    assert!(looks_like_our_harness(line, Parent::Supervisor));
    // Somebody still runs that session (a terminal, or another shell of ours): never signal.
    assert!(!looks_like_our_harness(line, Parent::Live));
    // A different program took over the recorded pid.
    assert!(!looks_like_our_harness(
        " 8831 /usr/sbin/cupsd -l",
        Parent::Gone
    ));
    // A dsh that is not the web profile this shell supervises.
    assert!(!looks_like_our_harness(
        " 8831 node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --version",
        Parent::Gone
    ));
    // No such process: `ps` prints nothing.
    assert!(!looks_like_our_harness("", Parent::Gone));
    assert!(!looks_like_our_harness("\n", Parent::Gone));
}

#[test]
fn ps_rows_keep_the_command_line_intact() {
    // `ps -o ppid=,command=` pads the numeric column; the command keeps its spaces.
    assert_eq!(
        parse_ps_identity(
            "   45 /Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop --flag\n"
        ),
        Some((
            45,
            "/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop --flag".to_string(),
        ))
    );
    assert_eq!(parse_ps_identity("\n"), None);
}

#[test]
fn session_supervisors_are_recognized_on_both_platforms() {
    assert!(is_session_supervisor("/sbin/launchd"));
    assert!(is_session_supervisor("/usr/lib/systemd/systemd --user"));
    assert!(is_session_supervisor("/sbin/init"));
    assert!(!is_session_supervisor("/bin/zsh -l"));
    assert!(!is_session_supervisor(
        "/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop"
    ));
    assert!(!is_session_supervisor(
        "/opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web"
    ));
    // No userspace parent left at all: treated as gone without asking `ps`.
    assert_eq!(classify_parent(0), Parent::Gone);
}

/// The three answers `ps` can give must not be collapsed into two.
#[test]
fn a_parent_is_classified_from_what_ps_answered() {
    // `ps` could not run or ran out of budget: unknown, so keep the process.
    assert_eq!(parent_from_ps(None), Parent::Live);
    // `ps` ran and printed nothing: no such process, which is how a crashed shell's
    // leftover reads. Treating this as Live would stop every orphan from being cleaned up.
    assert_eq!(parent_from_ps(Some(String::new())), Parent::Gone);
    // A real parent, in both shapes.
    assert_eq!(
        parent_from_ps(Some("/sbin/launchd".into())),
        Parent::Supervisor
    );
    assert_eq!(
        parent_from_ps(Some("/usr/lib/systemd/systemd --user".into())),
        Parent::Supervisor
    );
    assert_eq!(parent_from_ps(Some("/bin/zsh -l".into())), Parent::Live);
}
