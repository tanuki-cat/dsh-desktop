//! Unit tests for the takeover module.
//!
//! A child module of the code under test, so `use super::*` reaches private
//! items.

use super::*;

/// Taking the port from another Harness kills a session someone may be watching, so an
/// identified instance is always asked about — never signalled, and never silently resolved
/// to a browser either. Gating the question on `take_over_existing` hid it from every user
/// who had not already edited `config.json` (2026-09-16).
#[test]
fn an_identified_foreign_instance_is_always_asked_about() {
    const CMD: &str = "/opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --port 3080";

    // Identified: the question is put, and only the answer may signal anything. Both values
    // of the config reach this same arm — that is the fix.
    assert_eq!(
        foreign_instance_action(Some(4242), Some(CMD)),
        ForeignAction::Ask { pid: 4242 }
    );
    // The port answers like a Harness, but nothing proves which program it is. This is the
    // case the old code killed: an unrelated server on the configured port.
    assert!(matches!(
        foreign_instance_action(Some(4242), Some("/usr/bin/python3 -m http.server")),
        ForeignAction::Refuse { .. }
    ));
    // A command line the platform would not report is not proof either.
    assert!(matches!(
        foreign_instance_action(Some(4242), None),
        ForeignAction::Refuse { .. }
    ));
    assert!(matches!(
        foreign_instance_action(None, Some(CMD)),
        ForeignAction::Refuse { .. }
    ));
}

/// The page a startup outcome lands on decides whether the user gets a button that can work.
/// The browser fallback has nothing to restart, so offering one looped: click, re-run the same
/// doomed start, open another tab, land here again (observed in the field, 2026-09-16).
#[test]
fn leaving_the_instance_alone_does_not_offer_a_restart() {
    assert_eq!(
        terminal_page(&ForeignAction::UseBrowser),
        TerminalPage::Notice
    );
    // A refusal is something the user can act on and retry in place, so the button stays.
    assert_eq!(
        terminal_page(&ForeignAction::Refuse {
            reason: "port held".to_string()
        }),
        TerminalPage::Failure
    );
    assert_eq!(
        terminal_page(&ForeignAction::TakeOver { pid: 1 }),
        TerminalPage::Failure
    );
}

/// A real command line is mostly path, and the box it goes in scrolls — so it is shortened
/// around a middle the user does not need, keeping the interpreter and the flags.
#[test]
fn a_long_command_line_is_elided_around_its_middle() {
    // Short enough to show as-is: no ellipsis, no truncation.
    let short = "/usr/bin/node /srv/dsh/lib/bin.js --profile web";
    assert_eq!(elide_command(short), short);

    let long = format!(
        "/Users/me/Library/Application Support/JetBrains/Toolbox/apps/node/versions/22/bin/node {}/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --no-open --port 3080",
        "/Users/me/WorkSpace/RustRoverProjects/a-deeply-nested-directory".repeat(3)
    );
    let shown = elide_command(&long);
    assert!(
        shown.chars().count() <= COMMAND_DISPLAY_LIMIT + 1,
        "{shown}"
    );
    // Both ends survive: the interpreter that was used, and the flags that say what it does.
    assert!(
        shown.starts_with("/Users/me/Library/Application Support"),
        "{shown}"
    );
    assert!(
        shown.ends_with("--profile web --no-open --port 3080"),
        "{shown}"
    );
    assert!(shown.contains('…'), "{shown}");
    // The line still identifies the CLI, which is the whole reason it is shown.
    assert!(harness::looks_like_dsh_web(&long));
}

/// What answering yes would do comes before the evidence for it: the bottom of the box is the
/// part a small window scrolls out of sight.
#[test]
fn the_question_leads_with_what_takeover_would_do() {
    let (_, detail) = takeover_question(3080, 7, None, Path::new("/Users/me/project"));
    let effect = detail
        .find("接管会先终止该进程")
        .expect("the effect is stated");
    let evidence = detail.find("要接管的进程").expect("the process is named");
    assert!(effect < evidence, "{detail}");
    // The workspace is part of the effect — a takeover does not continue the other session —
    // so it has to be above the command line rather than after it.
    let workspace = detail
        .find("/Users/me/project")
        .expect("the workspace is named");
    assert!(workspace < evidence, "{detail}");
}

/// An unanswered question runs the config, and a question that could not be put at all has to
/// land in the same place: otherwise a headless launch and an ignored one would disagree.
#[test]
fn an_unanswered_question_follows_the_config() {
    assert_eq!(
        unanswered_choice(true, 4242),
        ForeignAction::TakeOver { pid: 4242 }
    );
    assert_eq!(unanswered_choice(false, 4242), ForeignAction::UseBrowser);
}

/// The question has to name what the user is about to end: a pid alone does not tell them
/// which terminal session or workspace they are looking at.
#[test]
fn the_takeover_question_names_the_process_and_this_shells_workspace() {
    const CMD: &str = "/usr/bin/node /srv/dsh/lib/bin.js --profile web --port 3080";
    let (status, detail) = takeover_question(3080, 4242, Some(CMD), Path::new("/Users/me/project"));
    assert_eq!(status, "检测到其它 Harness");
    assert!(detail.contains("127.0.0.1:3080"), "{detail}");
    assert!(detail.contains("pid 4242"), "{detail}");
    assert!(detail.contains(CMD), "{detail}");
    // The workspace this shell would restart it with, which is the part that surprises
    // people: a takeover does not continue the other instance session.
    assert!(detail.contains("/Users/me/project"), "{detail}");
    // A command line the platform would not report is still named as unknown rather than
    // left out, so the user knows what the shell does not know.
    let (_, blind) = takeover_question(3080, 7, None, Path::new("/tmp"));
    assert!(blind.contains("读不到命令行"), "{blind}");
}

/// The question has to answer the user's next question about their open browser tab, and it
/// has to answer it correctly: the old cookie survives a restart on the same authority when
/// `DSH_HOME` is unchanged (measured, see the doc comment), and is refused when it is not.
#[test]
fn the_question_says_what_happens_to_an_open_browser_tab() {
    let (_, detail) = takeover_question(3080, 4242, None, Path::new("/tmp"));
    assert!(detail.contains("旧标签页不用手动关"), "{detail}");
    assert!(detail.contains("刷新就会连到重启后的实例"), "{detail}");
    // The exception, which is the case where the advice above would be wrong.
    assert!(detail.contains("authentication required"), "{detail}");
    assert!(detail.contains("dsh_home"), "{detail}");
}

/// An unanswered question falls back to exactly what the config asked for, so a headless or
/// ignored launch behaves the way the shell did before it could ask.
#[test]
fn an_unanswered_takeover_question_follows_the_config_default() {
    // The wait itself: a value that never arrives is a timeout, not a panic or a hang.
    assert_eq!(
        window::wait_for_choice(1, Duration::from_millis(30), || None),
        None
    );
    // An answer to a different question (a click that crossed a retry) is not this answer.
    let stale = || {
        Some(window::Choice {
            question: 1,
            id: CHOICE_TAKE_OVER.to_string(),
        })
    };
    assert_eq!(
        window::wait_for_choice(2, Duration::from_millis(30), stale),
        None
    );
    let fresh = || {
        Some(window::Choice {
            question: 2,
            id: CHOICE_BROWSER.to_string(),
        })
    };
    assert_eq!(
        window::wait_for_choice(2, Duration::from_secs(5), fresh).map(|choice| choice.id),
        Some(CHOICE_BROWSER.to_string())
    );
}
