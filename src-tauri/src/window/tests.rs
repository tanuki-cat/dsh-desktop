//! Unit tests for the window module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

#[test]
fn only_web_schemes_leave_the_shell() {
    let allow = |raw: &str| may_open(&Url::parse(raw).expect("test URL must parse"));
    assert!(allow("https://example.com/docs"));
    assert!(allow("http://127.0.0.1:3080/?token=abc"));
    // `file:` would launch a local application through `open` on macOS.
    assert!(!allow("file:///System/Applications/Calculator.app"));
    assert!(!allow("smb://host/share"));
    // Custom schemes reach whichever app registered them.
    assert!(!allow("dsh-desktop://open"));
    assert!(!allow("javascript:alert(1)"));
    assert!(!allow("data:text/html,<script>alert(1)</script>"));
    assert!(!allow("mailto:someone@example.com"));
}

#[test]
fn a_shimmable_gap_is_covered_instead_of_forced_to_the_browser() {
    let report = WebviewReport::parse(
        r#"{"missing":["Iterator"],"degraded":["structuredClone"],"syntax":[],"agent":"Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15"}"#,
    )
    .expect("the page's report must parse");

    // macOS 15.0.1 (Safari 18.0): the one API the pdf.js guard needs, nothing else.
    assert!(
        report.supported(true),
        "a shimmable API must not block the native window"
    );
    assert!(report.needs_compat());
    assert!(report.uncovered().is_empty());

    // The same engine with the compat layer switched off is the old shell: browser fallback.
    assert!(
        !report.supported(false),
        "webkit_compat=false restores the old gate"
    );
    assert_eq!(report.gaps(false), ["Iterator"]);

    let text = report.describe("0.1.5-rc.2", false);
    assert!(text.contains("Iterator"), "缺什么要写清楚：{text}");
    assert!(text.contains("0.1.5-rc.2"), "是哪个 dsh 版本要写清楚");
    // The floor dropped from Safari 18.4 to the syntax the bundles need.
    assert!(text.contains("Safari 16.4"), "补救方向要写清楚：{text}");
    assert!(
        text.contains("webkit_compat"),
        "是开关关掉的，要说清楚：{text}"
    );
    assert!(text.contains("structuredClone"), "降级项也要列出来");
    assert!(text.contains("AppleWebKit"), "报上当前 WebView 便于排查");
}

#[test]
fn syntax_no_polyfill_can_add_still_sends_the_user_to_the_browser() {
    // macOS ≤ 12: the bundles use class static blocks, and no script makes that parse.
    let report = WebviewReport::parse(
        r#"{"missing":["Iterator"],"syntax":["class static block"],"agent":"old"}"#,
    )
    .expect("the page's report must parse");

    assert!(!report.supported(true), "语法缺口补不了，只能回退浏览器");
    assert!(report.needs_compat(), "缺的 API 仍然可补");
    assert_eq!(report.gaps(true), ["class static block"]);
    assert!(report
        .describe("0.1.5-rc.2", true)
        .contains("class static block"));
}

#[test]
fn the_browser_fallback_tells_the_user_what_to_keep_open() {
    let report = WebviewReport::parse(
        r#"{"missing":["Iterator"],"syntax":["class static block"],"agent":"old"}"#,
    )
    .expect("the page's report must parse");

    let text = report.browser_fallback_detail("0.1.5-rc.2", "http://127.0.0.1:3080/?token=t", true);
    assert!(
        text.contains("http://127.0.0.1:3080/?token=t"),
        "给出地址：{text}"
    );
    // The reason is the syntax gap, not Iterator: with the compat layer on, Iterator is the
    // one thing this page did *not* fail on.
    assert!(text.contains("class static block"), "说明原因：{text}");
    assert!(
        text.contains("不要关闭"),
        "关掉这个窗口会停 harness，必须写明：{text}"
    );
}

#[test]
fn a_degraded_but_complete_webview_still_starts() {
    let report =
        WebviewReport::parse(r#"{"missing":[],"degraded":["structuredClone"],"agent":"x"}"#)
            .expect("the page's report must parse");
    assert!(
        report.supported(true),
        "a missing optional API must never block the GUI"
    );
    assert!(report.supported(false));
    assert!(
        !report.needs_compat(),
        "nothing to install, no window script"
    );
}

#[test]
fn an_unreadable_report_is_not_a_reason_to_refuse_to_start() {
    assert_eq!(WebviewReport::parse("not json"), None);
    // An empty report is a WebView that found nothing missing: supported.
    assert_eq!(
        WebviewReport::parse("{}").map(|report| report.supported(true)),
        Some(true)
    );
}

/// One test owns the process-wide report slot on purpose: `record_report` overwrites it, and
/// every assertion here depends on what was reported last.
#[test]
fn the_gate_reads_what_the_page_reported() {
    record_report(r#"{"missing":["Iterator"],"degraded":[],"syntax":[],"agent":"old"}"#);
    assert!(
        unsupported_webview(true).is_none(),
        "a shimmable gap keeps the native window"
    );
    let refused = unsupported_webview(false).expect("with compat off it must be refused");
    assert_eq!(refused.missing, ["Iterator"]);
    let script = needed_compat_script().expect("Iterator needs a shim");
    assert!(script.contains("globalThis.Iterator"), "{script}");

    // A payload this shell cannot read keeps the previous answer (logged, not fatal).
    record_report("not json");
    assert!(
        unsupported_webview(false).is_some(),
        "a broken report must not clear a refusal"
    );

    // A complete WebView starts, however many optional APIs are missing.
    record_report(r#"{"missing":[],"degraded":["structuredClone"],"agent":"new"}"#);
    assert!(
        unsupported_webview(false).is_none(),
        "a complete WebView must start"
    );
    assert!(
        needed_compat_script().is_none(),
        "nothing missing, nothing injected"
    );
}

/// The splash page probes while the CLI is still booting, so a reader that does not wait turns
/// that race into a silently unpatched window: `needed_compat_script` used to return `None` for
/// a report that was merely late, and the caller then logged `webkit_compat=false` for an
/// engine whose probe had said the opposite (found by running the built app, 2026-09-15).
///
/// One test owns the process-wide report slot (see [`the_gate_reads_what_the_page_reported`]),
/// so this asserts the wait without competing for it: the slot is emptied, a writer is started,
/// and the reader must pick the answer up rather than give up on the empty slot.
#[test]
fn a_value_that_arrives_late_is_still_read() {
    let reads = std::cell::Cell::new(0);
    let started = Instant::now();
    // Answers on the fifth poll, i.e. after an empty slot for ~80 ms.
    let value = wait_for(|| {
        reads.set(reads.get() + 1);
        (reads.get() >= 5).then_some("late")
    });

    assert_eq!(value, Some("late"), "a late value must still be read");
    assert!(
        started.elapsed() >= Duration::from_millis(80),
        "the reader must have waited for the answer"
    );
}

/// A probe that never reports is not a verdict: the wait ends and the caller sees nothing,
/// which both readers turn into "no reason known" rather than into a refusal or a patch.
#[test]
fn a_slot_that_stays_empty_ends_the_wait() {
    let started = Instant::now();
    let value: Option<u8> = wait_for(|| None);

    assert_eq!(value, None);
    assert!(
        started.elapsed() >= PROBE_WAIT,
        "it must have waited the full budget before giving up"
    );
}

#[test]
fn the_probe_asks_about_every_shimmed_api_and_the_syntax_floor() {
    let script = probe_script();
    // The one that actually failed in the field (see SHIMMED_APIS).
    assert!(script.contains("typeof Iterator === \"undefined\""));
    // Prototype methods are not globals: the check names the expression it asks about.
    assert!(script.contains("typeof [].findLast === \"undefined\""));
    assert!(script.contains("Math.sumPrecise"));
    // The syntax verdict comes from the page, not from an eval the page's CSP would block.
    assert!(script.contains("__dshSyntaxMissing"));
    assert!(
        !script.contains("new Function"),
        "the probe must not eval: script-src forbids it, and a blocked eval is\n             indistinguishable from syntax this engine cannot parse"
    );
    // It reports through the splash window's core capability.
    assert!(script.contains(PROBE_EVENT));
    assert!(script.contains("plugin:event|emit"));
    // It runs on exactly the engines it exists to diagnose: keep it ES5.
    assert!(!script.contains("=>"), "the probe must stay ES5");
    assert!(!script.contains('`'), "the probe must stay ES5");
    assert!(!script.contains("??"), "the probe must stay ES5");
}

/// Run a compat block through a real JS engine, as `tests/webkit_compat_shim.rs` does for the
/// shipped bundle.
///
/// `node` has no substitute here: returning early when it is missing made the cases below report
/// `ok` while asserting nothing, which is how a broken shim stayed green once already (review D6).
/// A runner without node is a runner that cannot check the compat layer, and it should say so.
fn run_shim(block: &str, probe: &str) -> String {
    let script = format!("{block}\n{probe}\n");
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
        .expect("node must be on PATH: the compat layer is only checked in a real engine");
    assert!(
        output.status.success(),
        "the shim must run in node: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `String(value).replace(/s/g, "")` deleted every letter `s` instead of whitespace, so a
/// base64 document containing one decoded to the wrong bytes or threw outright. The block is run
/// for real: a string assertion on the regex is what let this ship.
#[test]
fn the_base64_shim_decodes_whitespace_and_keeps_every_other_character() {
    let probe = r#"
      var out = [];
      try { out.push(Array.from(Uint8Array.fromBase64("aGVsbG8=")).join(",")); } catch (e) { out.push("THREW"); }
      try { out.push(Array.from(Uint8Array.fromBase64("aGVs\n bG8=")).join(",")); } catch (e) { out.push("THREW"); }
      try { out.push(Array.from(Uint8Array.fromBase64("c3lzdGVt")).map(function (b) { return String.fromCharCode(b); }).join("")); } catch (e) { out.push("THREW"); }
      process.stdout.write(out.join(" | "));
    "#;
    let out = run_shim(UINT8_FROM_BASE64_SHIM, probe);
    // "hello" twice (once with embedded whitespace), then "system" — whose base64 contains an `s`.
    assert_eq!(out, "104,101,108,108,111 | 104,101,108,108,111 | system");
}

/// The compensated sum turned an infinity into NaN: the correction term is NaN for a non-finite
/// value, and NaN propagates. The spec asks for Infinity.
#[test]
fn the_sum_precise_shim_keeps_non_finite_results() {
    let probe = r#"
      process.stdout.write([
        Math.sumPrecise([Infinity]),
        Math.sumPrecise([1e308, 1e308]),
        Math.sumPrecise([1, 2, 3]),
        Math.sumPrecise([Infinity, -Infinity])
      ].join(" | "));
    "#;
    let out = run_shim(MATH_SUM_PRECISE_SHIM, probe);
    assert_eq!(out, "Infinity | Infinity | 6 | NaN");
}

/// With no initial value the first element becomes the accumulator, so the reducer's index starts
/// at 1 — the shim counted from 0 and handed callers a different index than the spec does.
///
/// The block is selected by the name the probe reports (`Iterator`), never by `Iterator.prototype.
/// reduce`: `compat_script` matches names exactly, so the longer spelling injected *nothing* and
/// this test then asserted on node own `Iterator.prototype.reduce`, which already numbers calls
/// the same way. It passed for the wrong reason from the day it was written (review B3).
///
/// The driver strips the native helper first and fails when it cannot, so a pass can only come
/// from the shim, and a machine that cannot run the check fails loudly instead of silently.
#[test]
fn the_iterator_reduce_shim_numbers_the_first_call_one() {
    // The native global is removed *before* the block is injected, the same order the shipped
    // bundle sees on an old engine: `Iterator` is deletable in node (`configurable: true`), while
    // the built-in prototype methods are not. Without this the block would guard itself out
    // (`if (typeof globalThis.Iterator !== "undefined") return`) and the call below would reach
    // node own implementation — exactly how this case used to pass while proving nothing.
    // Two things have to go: the global (the block guards itself out when it exists) and the
    // helper on `%IteratorPrototype%` (node 22 ships iterator helpers, and `defineHelper` skips
    // a name that is already a function — leaving the native one in place).
    let strip = [
        "var proto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));",
        "try { delete globalThis.Iterator; } catch (error) {}",
        "try { delete proto.reduce; } catch (error) {}",
        "if (typeof proto.reduce === \"function\") { process.stdout.write(\"NATIVE\"); }",
    ]
    .join("\n");
    // The probe only has to run the helper: whatever `reduce` is there now can only be the one
    // the block just installed, because the native one was deleted above (and if that delete ever
    // stops working, the strip step has already written `NATIVE` into stdout, which fails the
    // equality below instead of silently comparing the engine against itself).
    let probe = [
        "if (typeof globalThis.Iterator !== \"function\") { process.stdout.write(\"NOT-REVIVED\"); }",
        "else {",
        "  var seen = [];",
        "  [10, 20, 30].values().reduce(function (acc, v, i) { seen.push(i); return acc + v; });",
        "  process.stdout.write(seen.join(','));",
        "}",
    ]
    .join("\n");
    let script = format!(
        "{strip}\n{}\n{probe}\n",
        compat_script(&["Iterator".to_string()])
    );
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
        .expect("node must run: this test cannot pass without a real engine");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "the shim must run in node: {} {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        stdout, "NOT-REVIVED",
        "the Iterator block did not install anything: compat_script was given the wrong key"
    );
    assert_eq!(stdout, "1,2");
}
/// Syntax the client bundles use that no polyfill can add: the module has to parse.
///
/// `class static block` (Safari 16.4, macOS 13.3) is the newest syntax the bundles use, which is
/// what makes the compat layer's floor 13.3 rather than the 10.15 the bundle declares; newer
/// syntax goes in this list.
///
/// The source is not compiled from here. The splash page carries a CSP (see `tauri.conf.json`),
/// and `new Function` is exactly what `script-src` forbids — a blocked eval throws the same way
/// an unparseable program does, so probing that way would report every machine as too old.
/// `src/index.html` carries the block instead, where a script element this engine cannot parse is
/// discarded on its own while the rest of the page keeps running.
///
/// The page owns the verdict, so this is only the list this test checks that page against; the
/// label the user sees travels in the page's own report.
const REQUIRED_SYNTAX: &[(&str, &str)] = &[("class static block", "class Probe { static { 1; } }")];

/// The syntax verdict is produced by the splash page, so the two halves have to agree: every
/// feature named here must be probed there, and the page must report the same label. Splitting
/// them silently is how a machine that cannot parse a bundle gets told it can.
#[test]
fn the_page_probes_the_syntax_this_shell_expects_it_to() {
    let page = include_str!("../../../src/index.html");
    for (name, source) in REQUIRED_SYNTAX {
        assert!(
            page.contains(source),
            "index.html does not test {name:?} ({source})"
        );
        assert!(
            page.contains(&format!("\"{name}\"")),
            "index.html never reports {name:?} as missing"
        );
    }
    // The page is what sets it, so the probe must not invent the field itself.
    assert!(page.contains("__dshSyntaxMissing"));
    // The flag must sit in the same script element as the syntax it guards: an engine that
    // cannot parse the class body discards that element, and a flag in a separate one would
    // still run and report the engine as fine. Checked against the text up to the next
    // closing tag.
    for (_, source) in REQUIRED_SYNTAX {
        let after = page
            .split(source)
            .nth(1)
            .expect("the source was found above");
        let until_close = after.split("</script>").next().unwrap_or_default();
        assert!(
            until_close.contains("__dshSyntaxChecked"),
            "the syntax flag must be inside the same <script> as {source}"
        );
    }
}

/// The splash page is our own document and the only window with a capability, so it is the
/// one place a CSP can be enforced at all: the Harness window loads a remote origin that
/// Tauri never sees. Assert the config keeps that protection rather than trusting it stays.
#[test]
fn the_local_page_is_served_with_a_restrictive_csp() {
    let config: serde_json::Value =
        serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
    let csp = config["app"]["security"]["csp"]
        .as_str()
        .expect("the local page must carry a CSP");
    // No inline script and no eval: Tauri hashes the page's own scripts instead.
    assert!(!csp.contains("unsafe-inline"), "{csp}");
    assert!(!csp.contains("unsafe-eval"), "{csp}");
    // Tauri's IPC fast path is a fetch to its own scheme, which connect-src has to allow.
    assert!(csp.contains("ipc:"), "{csp}");
    assert!(csp.contains("http://ipc.localhost"), "{csp}");
    assert!(csp.contains("object-src 'none'"), "{csp}");
}

#[test]
fn every_shimmed_api_has_a_block_and_only_the_missing_ones_are_emitted() {
    let names: Vec<String> = SHIMMED_APIS
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();
    for (name, _) in SHIMMED_APIS {
        assert!(
            COMPAT_BLOCKS.iter().any(|(block, _)| block == name),
            "{name} has no compat block"
        );
    }
    let all = compat_script(&names);
    for (name, block) in COMPAT_BLOCKS {
        assert!(
            all.contains(block),
            "{name} block missing from the full script"
        );
    }
    // Only what the report asked for reaches the window.
    let one = compat_script(&["Iterator".to_string()]);
    assert!(one.contains(ITERATOR_SHIM));
    assert!(!one.contains(PROMISE_TRY_SHIM), "{one}");
    assert!(!one.contains(MATH_SUM_PRECISE_SHIM), "{one}");
    assert!(
        compat_script(&[]).is_empty(),
        "nothing missing, nothing injected"
    );
}

#[test]
fn the_compat_layer_stays_es5_and_guards_every_block() {
    let names: Vec<String> = SHIMMED_APIS
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();
    let all = compat_script(&names);
    for (name, block) in COMPAT_BLOCKS {
        assert!(
            block.contains("(function () {"),
            "{name} is not its own scope"
        );
        assert!(block.contains("typeof "), "{name} installs without asking");
    }
    // Same discipline as the probe: the engines that need this are exactly the old ones.
    for syntax in ["=>", "`", "??", "const ", "let ", "function*", "class "] {
        assert!(
            !all.contains(syntax),
            "the compat layer must stay ES5, found {syntax:?}"
        );
    }
}

/// A page that takes input but draws nothing is the reported failure, and it cannot report
/// itself: the UI coalesces streamed output onto animation frames, and the code that would
/// notice is the code that stopped running.
#[test]
fn a_page_that_stops_running_is_reloaded_and_then_reported() {
    let drawing = PageState::Drawing;
    let stalled = PageState::Frozen(Scheduling::Stalled);
    let idle = false;
    // One bad probe is a hiccup: the shell waits instead of throwing away the page state.
    assert_eq!(
        liveness_action(stalled, true, idle, 0, 0),
        LivenessAction::Wait { misses: 1 }
    );
    assert_eq!(
        liveness_action(stalled, true, idle, 1, 0),
        LivenessAction::Wait { misses: 2 }
    );
    // The third one in a row is a page that is not coming back on its own.
    assert_eq!(
        liveness_action(stalled, true, idle, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
    // Anything that draws clears the streak, whatever the reload count was.
    assert_eq!(
        liveness_action(drawing, true, idle, 5, 2),
        LivenessAction::Alive
    );
    // Reloads are budgeted: the shell stops instead of looping over a page that never returns.
    assert_eq!(
        liveness_action(stalled, true, idle, 2, 1),
        LivenessAction::Reload { attempt: 2 }
    );
    assert_eq!(
        liveness_action(stalled, true, idle, 2, 3),
        LivenessAction::Report { attempts: 3 }
    );
    // A bad probe mid-recovery does not reset the reload budget.
    assert_eq!(
        liveness_action(stalled, true, idle, 0, 3),
        LivenessAction::Wait { misses: 1 }
    );
    assert_eq!(
        liveness_action(stalled, true, idle, 2, u32::MAX),
        LivenessAction::Report { attempts: u32::MAX }
    );
    // A silent page is broken wherever it is: focus is no excuse for answering nothing.
    assert_eq!(
        liveness_action(PageState::Silent, false, idle, 1, 0),
        LivenessAction::Wait { misses: 2 }
    );
    assert_eq!(
        liveness_action(PageState::Silent, false, idle, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
    // A page whose scheduling could not be classified is treated as stalled: the shell cannot
    // prove anything is still running, and the reload budget bounds the mistake.
    assert_eq!(
        liveness_action(PageState::Frozen(Scheduling::Unknown), true, idle, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
}

/// A page WebKit stopped drawing to is not a broken page, and reloading it would throw away a
/// working session to fix the scheduler. The reported failure is exactly this state — the user
/// works in another window while the model streams — so it must not spend the reload budget.
#[test]
fn a_suspended_page_is_named_instead_of_reloaded() {
    let suspended = PageState::Frozen(Scheduling::Suspended);
    let idle = false;
    assert_eq!(
        liveness_action(suspended, false, idle, 0, 0),
        LivenessAction::SuspendedNotDrawing
    );
    // Even a long streak of unattended probes stays suspended: no reloads are spent.
    assert_eq!(
        liveness_action(suspended, false, idle, 4, 2),
        LivenessAction::SuspendedNotDrawing
    );
    // Drawing is drawing, attended or not.
    assert_eq!(
        liveness_action(PageState::Drawing, false, idle, 0, 0),
        LivenessAction::Alive
    );
    // The user came back and the page is still not drawing: the suspension no longer explains
    // it, so the page joins the streak that earns a reload instead of hinting for ever.
    assert_eq!(
        liveness_action(suspended, true, idle, 0, 0),
        LivenessAction::Wait { misses: 1 }
    );
    assert_eq!(
        liveness_action(suspended, true, idle, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
}

/// The page's own visibility decides whether a stopped frame counter means "nobody is drawing to
/// it" or "it cannot draw". The window-level check is the fallback, and the two disagree exactly
/// where the field reports come from: WebKit stops painting an occluded window (which the window
/// check calls attended, because it is visible and focused) and keeps painting a visible-but-
/// unfocused one (which the window check calls unattended).
#[test]
fn the_page_own_visibility_outranks_the_window_state() {
    // An occluded page: the page says hidden, the window says visible and focused.
    assert!(!page_attended(Some(true), true));
    // A visible but unfocused page: the page says visible, the window says unattended. The page
    // is right — it is still drawing, so a stopped counter there is a real fault.
    assert!(page_attended(Some(false), false));
    // A page that could not answer keeps whatever the window said, in both directions: an engine
    // without `visibilityState` must behave exactly as it did before this field existed.
    assert!(page_attended(None, true));
    assert!(!page_attended(None, false));
    // And the combination that matters for the reload budget: hidden beats attended, so a page
    // that says it is not being drawn to never spends the budget, however the window looks.
    let suspended = PageState::Frozen(Scheduling::Suspended);
    let attended = page_attended(Some(true), true);
    assert_eq!(
        liveness_action(suspended, attended, false, 4, 0),
        LivenessAction::SuspendedNotDrawing
    );
    // The reverse: visible page, stopped frames, unattended window — this is the fault the reload
    // exists for, so the window state must not excuse it.
    let attended = page_attended(Some(false), false);
    assert_eq!(
        liveness_action(suspended, attended, false, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
}

/// Reloading is the only recovery for a stalled page, and it is also what discards an unsent
/// prompt. Someone typing in a page that stopped drawing gets another interval first.
#[test]
fn a_page_someone_is_typing_in_is_not_reloaded_yet() {
    let stalled = PageState::Frozen(Scheduling::Stalled);
    // The user is mid-sentence: wait, and keep the streak so the budget is untouched.
    assert_eq!(
        liveness_action(stalled, true, true, 2, 0),
        LivenessAction::Busy { misses: 3 }
    );
    // The moment they stop, the reload the page still needs happens.
    assert_eq!(
        liveness_action(stalled, true, false, 3, 0),
        LivenessAction::Reload { attempt: 1 }
    );
    // Typing never spends or restores the reload budget.
    assert_eq!(
        liveness_action(stalled, true, true, 3, 2),
        LivenessAction::Busy { misses: 4 }
    );
    // A page that answers nothing is not "busy": the probe that would say so is the code
    // that stopped running, so silence must still reload rather than wait for ever.
    assert_eq!(
        liveness_action(PageState::Silent, true, false, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
    // And a page that is drawing is alive whether or not anyone is typing.
    assert_eq!(
        liveness_action(PageState::Drawing, true, true, 0, 0),
        LivenessAction::Alive
    );
}

/// The activity probe decides whether a reload would cost the user work, so its reading of
/// the page's answer is the difference between a lost prompt and a recovered window.
///
/// The answers here are built from [`ACTIVITY_PROBE`] itself rather than written by hand. That is
/// not pedantry: this test used to pass literal JSON, so it agreed with a probe that put a
/// *double-encoded string* on the wire and never once parsed in production — "someone is typing"
/// was always false, and the protection it exists for was dead code (found 2026-09-18).
#[test]
fn activity_is_read_as_busy_only_when_someone_is_working() {
    let answer = |editing: bool, idle: u64| {
        // What WebKit hands the callback: the JSON of the value the probe evaluated to.
        format!("{{\"editing\":{editing},\"idle\":{idle}}}")
    };
    // The probe must not serialise its own answer; a returned string arrives double-encoded.
    assert!(
        !ACTIVITY_PROBE.contains("JSON.stringify"),
        "活动探测不能自己序列化：返回值会被 WebKit 再序列化一次：{ACTIVITY_PROBE}"
    );
    // Every key the parser reads is a key the probe emits, spelled the same way.
    for key in ["editing", "idle"] {
        assert!(
            ACTIVITY_PROBE.contains(key),
            "探测缺少 {key}：{ACTIVITY_PROBE}"
        );
    }
    // A caret in a text box, typed into within the last interval: this is the one reading that
    // costs the user something to reload.
    assert!(page_is_busy(Some(&answer(true, 100))));
    // The caret is in the message box and has been for an hour. The Harness puts it there on
    // load and nothing takes it away, so reading this as "someone is typing" made the grace
    // permanent and left quitting the app as the only way out of a page that stopped drawing.
    assert!(!page_is_busy(Some(&answer(true, 900_000))));
    // Clicked in the conversation list a moment ago, caret nowhere it could hold a draft: a
    // reload costs nothing here, so it must not be held off.
    assert!(!page_is_busy(Some(&answer(false, 100))));
    // Focused elsewhere, idle for minutes: nothing to lose.
    assert!(!page_is_busy(Some(&answer(false, 900_000))));
    // Never typed since the page loaded.
    assert!(!page_is_busy(Some(&answer(false, 1_000_000_000))));
    // A page that cannot answer is gone, not busy: waiting would only delay recovery.
    assert!(!page_is_busy(None));
    assert!(!page_is_busy(Some("not json")));
    assert!(!page_is_busy(Some("")));
    // The regression itself: a self-serialising probe produces this, and it must never read as
    // "busy" by accident — the answer is a string, so the parse has to fail.
    let double_encoded = serde_json::to_string(&answer(true, 100)).unwrap();
    assert!(!page_is_busy(Some(&double_encoded)));
}

/// A counter report as the page sends it, from a page that says it is being drawn to.
fn counts(frames: u64, timers: u64) -> Frames {
    Frames {
        frames,
        timers,
        timers_seen: true,
        hidden: false,
        fallbacks: 0,
    }
}

/// The frame counter is the whole signal, so its algebra has to be exact.
#[test]
fn only_a_moving_frame_counter_proves_the_page_is_drawing() {
    // First answer: nothing to compare against, so a number that arrived is progress.
    assert_eq!(judge_frames(Some(counts(1, 1)), None), PageState::Drawing);
    assert_eq!(judge_frames(Some(counts(0, 0)), None), PageState::Drawing);
    // Same frames twice: JavaScript runs and no frame was produced in between.
    assert_eq!(
        judge_frames(Some(counts(7, 3)), Some(counts(7, 2))),
        PageState::Frozen(Scheduling::Suspended)
    );
    // A larger number is frames being drawn.
    assert_eq!(
        judge_frames(Some(counts(8, 3)), Some(counts(7, 3))),
        PageState::Drawing
    );
    // A smaller number is a fresh document, not a fault: a reload starts the count over.
    assert_eq!(
        judge_frames(Some(counts(1, 0)), Some(counts(40, 9))),
        PageState::Drawing
    );
    // No answer at all is the renderer being gone or wedged.
    assert_eq!(judge_frames(None, Some(counts(40, 9))), PageState::Silent);
    assert_eq!(judge_frames(None, None), PageState::Silent);
}

/// Frames stopped and timers stopped is a different failure from frames stopped alone, and the
/// two earn opposite responses: one is a busy or dead main thread, the other is a window WebKit
/// decided nobody was watching. Only the timer counter can tell them apart.
#[test]
fn a_stopped_frame_counter_is_split_by_the_timer_that_keeps_running() {
    // Frames frozen, timers advanced: the task queue runs, so the page is only not being drawn
    // to. This is the shape of the reported freeze on a window the user is not looking at.
    assert_eq!(
        judge_frames(Some(counts(9, 4)), Some(counts(9, 3))),
        PageState::Frozen(Scheduling::Suspended)
    );
    // Frames frozen *and* timers frozen: nothing ran between the two probes, which is a main
    // thread that is busy or gone.
    assert_eq!(
        judge_frames(Some(counts(9, 3)), Some(counts(9, 3))),
        PageState::Frozen(Scheduling::Stalled)
    );
    // An engine that cannot count timers must not be read as stalled *or* as suspended: it is
    // simply not saying, and the caller has to decide what an unknown verdict costs.
    let unseen = Frames {
        timers_seen: false,
        ..counts(9, 0)
    };
    assert_eq!(
        judge_frames(Some(unseen), Some(unseen)),
        PageState::Frozen(Scheduling::Unknown)
    );
    // One unclassifiable reading is not enough either: the first probe after an upgrade has no
    // timer baseline, and calling that stalled would reload a page that never stopped drawing.
    let mut first = unseen;
    first.timers_seen = true;
    assert_eq!(
        judge_frames(Some(counts(9, 1)), Some(first)),
        PageState::Frozen(Scheduling::Suspended)
    );
}

/// The fallback is the difference between "the model output stops appearing" and "it keeps
/// appearing while the window is behind another one", so its behaviour is checked in a real
/// engine rather than by reading the source.
///
/// The driver below is a window whose native frames never arrive (exactly the occluded case) and
/// whose timers can be advanced by hand, so every branch is reachable without a browser.
#[test]
fn the_render_fallback_stands_in_for_frames_the_page_will_not_get() {
    // The stub comes first: `run_shim` runs the block, then the probe, and the probe is where the
    // fallback itself has to be evaluated so that it wraps this window rather than node global.
    let stub = r#"
      var clock = 0;
      var timers = [];
      var nativeCancels = [];
      var nativeCalls = 0;
      var nativeSinks = {};
      function advance(to) {
        clock = to;
        for (var i = 0; i < timers.length; i++) {
          if (!timers[i].dead && timers[i].at <= clock) timers[i].fn();
        }
      }
      global.window = {
        performance: { now: function () { return clock; } },
        document: { visibilityState: "hidden" },
        setTimeout: function (fn, ms) { timers.push({ fn: fn, at: clock + ms, dead: false }); return timers.length; },
        clearTimeout: function (id) { if (timers[id - 1]) timers[id - 1].dead = true; },
        requestAnimationFrame: function (cb) { nativeCalls += 1; nativeSinks[1000 + nativeCalls] = cb; return 1000 + nativeCalls; },
        cancelAnimationFrame: function (id) { nativeCancels.push(id); }
      };
    "#;
    let probe = r#"
      var w = window;
      var fired = [];
      // 1) Hidden and no native frame: the fallback delivers once, with a clock stamp.
      var id = w.requestAnimationFrame(function (stamp) { fired.push(stamp); });
      advance(300);
      var afterFallback = fired.slice();
      // 2) The native frame that finally arrives must not deliver the same callback twice.
      nativeSinks[id](999999);
      var afterLateNative = fired.slice();
      // 3) A cancelled request must never fire through either path.
      var cancelledFired = false;
      w.cancelAnimationFrame(w.requestAnimationFrame(function () { cancelledFired = true; }));
      advance(1000);
      // 4) A visible page gets no fallback timer at all.
      w.document.visibilityState = "visible";
      var before = timers.length;
      w.requestAnimationFrame(function () {});
      var timersVisible = timers.length - before;
      // 5) A native frame that arrives in time cancels the fallback: one delivery, not two.
      w.document.visibilityState = "hidden";
      var id4 = w.requestAnimationFrame(function () { fired.push("native"); });
      nativeSinks[id4](4242);
      advance(2000);
      var nativeDeliveries = 0;
      for (var j = 0; j < fired.length; j++) { if (fired[j] === "native") nativeDeliveries += 1; }
      process.stdout.write(JSON.stringify({
        afterFallback: afterFallback,
        afterLateNative: afterLateNative,
        cancelledFired: cancelledFired,
        timersVisible: timersVisible,
        nativeDeliveries: nativeDeliveries,
        cancelled: nativeCancels.length
      }));
    "#;
    let out = run_shim(stub, &format!("{}\n{}", render_fallback_script(), probe));
    // The stamp is the page's own clock, not the epoch: animations interpolate against it.
    assert_eq!(
        out,
        r#"{"afterFallback":[300],"afterLateNative":[300],"cancelledFired":false,"timersVisible":0,"nativeDeliveries":1,"cancelled":2}"#
    );
}

/// The fallback runs on the same engines the compat layer exists for, and a second injection must
/// not stack a second wrapper on top of the first.
#[test]
fn the_render_fallback_stays_es5_and_wraps_once() {
    let script = render_fallback_script();
    for syntax in ["=>", "`", "??", "const ", "let ", "class "] {
        assert!(
            !script.contains(syntax),
            "渲染兜底必须保持 ES5，发现 {syntax:?}"
        );
    }
    // The guard is what makes a second injection a no-op, and it is read before anything is
    // replaced, so a page that already carries the wrapper keeps the one it has.
    assert!(script.contains("__dshRafFallback === true"), "{script}");
    // Only a hidden page pays for a fallback timer.
    assert!(
        script.contains(r#"document.visibilityState === "hidden""#),
        "{script}"
    );
    // The interval comes from the constant, not from a number typed into the script twice.
    assert!(
        script.contains(&RENDER_FALLBACK_MS.to_string()),
        "兜底间隔必须来自 RENDER_FALLBACK_MS：{script}"
    );
    assert!(
        !script.contains("%FALLBACK_MS%"),
        "占位符必须被替换掉：{script}"
    );
    // Wrapping twice would deliver every callback twice, so the guard has to be real: run the
    // script against a stub window twice and check the wrapper is the same function object.
    let probe = r#"
      var stub = {
        document: { visibilityState: "hidden" },
        setTimeout: function () { return 1; },
        clearTimeout: function () {},
        requestAnimationFrame: function () { return 1; },
        cancelAnimationFrame: function () {}
      };
      global.window = stub;
      __SCRIPT__
      var first = stub.requestAnimationFrame;
      __SCRIPT__
      process.stdout.write(stub.requestAnimationFrame === first ? "once" : "twice");
    "#
    .replace("__SCRIPT__", &script);
    assert_eq!(run_shim("", &probe), "once");
}

/// The probe has to measure the engine, not the shim that stands in for it.
///
/// This is the regression that made the whole liveness check blind: the probe counted through
/// `window.requestAnimationFrame`, the fallback replaces that function, and so the counter was
/// bumped by the fallback's own timer. A window WebKit had not painted for eighteen minutes
/// reported a rising frame count, the watchdog called it healthy, and the user was left reloading
/// by hand and finally quitting the app (field log, 2026-09-18 21:02).
#[test]
fn the_frame_probe_counts_native_frames_only() {
    // An occluded window: native frames are accepted and never delivered, timers keep running.
    let stub = r#"
      var clock = 0;
      var timers = [];
      var nativeId = 0;
      global.window = {
        performance: { now: function () { return clock; } },
        document: { visibilityState: "hidden", addEventListener: function () {} },
        setTimeout: function (fn, ms) { timers.push({ fn: fn, at: clock + ms, dead: false }); return timers.length; },
        clearTimeout: function (id) { if (timers[id - 1]) timers[id - 1].dead = true; },
        requestAnimationFrame: function () { nativeId += 1; return nativeId; },
        cancelAnimationFrame: function () {}
      };
      global.advance = function (to) {
        // One fallback interval at a time: a chain queues its next link from inside the
        // callback, so a single sweep to the target time would only ever run the first one.
        while (clock < to) {
          clock += 50;
          for (var i = 0; i < timers.length; i++) {
            if (!timers[i].dead && timers[i].at <= clock) { timers[i].dead = true; timers[i].fn(); }
          }
        }
      };
    "#;
    let probe = format!(
        r#"
      {fallback}
      // The page's own render chain: each frame asks for the next one, which is what the Harness
      // does to flush streamed output.
      var painted = 0;
      var chain = function () {{ painted += 1; window.requestAnimationFrame(chain); }};
      window.requestAnimationFrame(chain);
      var first = {probe};
      advance(300);
      var second = {probe};
      advance(900);
      var third = {probe};
      process.stdout.write(JSON.stringify({{
        painted: painted,
        frames: [first.frames, second.frames, third.frames],
        fallbacks: [first.fallbacks, second.fallbacks, third.fallbacks],
        hidden: third.hidden
      }}));
    "#,
        fallback = render_fallback_script(),
        probe = FRAME_PROBE.trim()
    );
    let out = run_shim(stub, &probe);
    let seen: serde_json::Value = serde_json::from_str(&out).expect("the probe must answer JSON");
    // The fallback is carrying the page: its chain advances while the engine draws nothing.
    assert!(
        seen["painted"].as_u64().unwrap() >= 3,
        "兜底必须在页面被遮挡时继续推进渲染链：{out}"
    );
    assert!(
        seen["fallbacks"][2].as_u64().unwrap() >= 3,
        "兜底顶起的帧数必须被上报：{out}"
    );
    // And the probe says so: not one native frame was produced, however busy the fallback was.
    // A probe that counted through the wrapper would report the fallback's own count here.
    assert_eq!(
        seen["frames"],
        serde_json::json!([0, 0, 0]),
        "帧计数只能来自原生调度器，否则看护会把停画的页面判成健康：{out}"
    );
    assert_eq!(seen["hidden"], serde_json::json!(true), "{out}");
}

/// The frame queued a moment before the window was covered.
///
/// The first version of the shim read `visibilityState` only when the request was made, so a
/// frame asked for while the page was still visible got no fallback at all — and the Harness
/// flushes through a chain of frames, so that one missing link stops the stream. It is the
/// common case, not a corner: the user switches conversation and then looks at their editor.
#[test]
fn the_render_fallback_covers_frames_queued_before_the_page_hid() {
    let stub = r#"
      var clock = 0;
      var timers = [];
      var listeners = [];
      var nativeId = 0;
      global.window = {
        performance: { now: function () { return clock; } },
        document: {
          visibilityState: "visible",
          addEventListener: function (name, fn) { if (name === "visibilitychange") listeners.push(fn); }
        },
        setTimeout: function (fn, ms) { timers.push({ fn: fn, at: clock + ms, dead: false }); return timers.length; },
        clearTimeout: function (id) { if (timers[id - 1]) timers[id - 1].dead = true; },
        // Visible: frames are accepted. Once covered, WebKit delivers none of them, including
        // the ones it already accepted.
        requestAnimationFrame: function () { nativeId += 1; return nativeId; },
        cancelAnimationFrame: function () {}
      };
      global.cover = function () {
        window.document.visibilityState = "hidden";
        for (var i = 0; i < listeners.length; i++) listeners[i]();
      };
      global.uncover = function () {
        window.document.visibilityState = "visible";
        for (var i = 0; i < listeners.length; i++) listeners[i]();
      };
      global.advance = function (to) {
        // One fallback interval at a time: a chain queues its next link from inside the
        // callback, so a single sweep to the target time would only ever run the first one.
        while (clock < to) {
          clock += 50;
          for (var i = 0; i < timers.length; i++) {
            if (!timers[i].dead && timers[i].at <= clock) { timers[i].dead = true; timers[i].fn(); }
          }
        }
      };
    "#;
    let probe = format!(
        r#"
      {fallback}
      // Three chained frames, the shape the conversation view flushes through.
      var links = 0;
      var chain = function () {{ links += 1; if (links < 3) window.requestAnimationFrame(chain); }};
      window.requestAnimationFrame(chain);   // asked for while the window is still visible
      cover();                                // and now it is behind the editor
      advance(10000);
      var covered = links;
      // A fresh request while still covered arms a stand-in of its own...
      window.requestAnimationFrame(function () {{}});
      var armedWhileCovered = 0;
      for (var i = 0; i < timers.length; i++) if (!timers[i].dead) armedWhileCovered += 1;
      // ...and uncovering stands it down again: the engine draws now, so the native frame is the
      // better one and the page must not get both.
      uncover();
      var armed = 0;
      for (var i = 0; i < timers.length; i++) if (!timers[i].dead) armed += 1;
      process.stdout.write(JSON.stringify({{
        covered: covered,
        armedWhileCovered: armedWhileCovered,
        armed: armed
      }}));
    "#,
        fallback = render_fallback_script()
    );
    let seen: serde_json::Value =
        serde_json::from_str(&run_shim(stub, &probe)).expect("the shim must answer JSON");
    // The whole chain ran. Reading `visibilityState` only at request time leaves this at 0.
    assert_eq!(
        seen["covered"],
        serde_json::json!(3),
        "被遮挡前排队的帧必须也能被兜底补上"
    );
    // Back in view, nothing is left ticking: the engine draws again, so the stand-ins stand down.
    assert_eq!(
        seen["armedWhileCovered"],
        serde_json::json!(1),
        "被遮挡时新排的帧必须挂上兜底"
    );
    assert_eq!(
        seen["armed"],
        serde_json::json!(0),
        "页面重新可见后不应留下未触发的兜底定时器"
    );
}

/// The grace for a page someone is typing in has to end.
///
/// It is there so a reload does not discard a half-typed prompt. Unbounded, it is not a delay but
/// a veto — and the page it vetoes the recovery of is one that stopped drawing, so the typist
/// cannot see their own prompt either. That combination left quitting the app as the only way out.
#[test]
fn a_busy_page_is_reloaded_after_the_grace_runs_out() {
    let stalled = PageState::Frozen(Scheduling::Stalled);
    // The streak reaches the reload point, and typing holds it off — keeping the streak, not
    // resetting it, so the grace is spent rather than restarted.
    assert_eq!(
        liveness_action(stalled, true, true, LIVENESS_MISSES - 1, 0),
        LivenessAction::Busy {
            misses: LIVENESS_MISSES
        }
    );
    // Every interval of the grace, still held off.
    for spent in 0..LIVENESS_BUSY_GRACE - 1 {
        assert_eq!(
            liveness_action(stalled, true, true, LIVENESS_MISSES + spent, 0),
            LivenessAction::Busy {
                misses: LIVENESS_MISSES + spent + 1
            }
        );
    }
    // And then it is over: the page is reloaded even though the caret is still in the box.
    assert_eq!(
        liveness_action(
            stalled,
            true,
            true,
            LIVENESS_MISSES + LIVENESS_BUSY_GRACE - 1,
            0
        ),
        LivenessAction::Reload { attempt: 1 }
    );
    // The budget is spent from there like any other reload, so a page that keeps failing still
    // ends up reported rather than reloaded for ever.
    assert_eq!(
        liveness_action(
            stalled,
            true,
            true,
            LIVENESS_MISSES + LIVENESS_BUSY_GRACE - 1,
            LIVENESS_RELOADS
        ),
        LivenessAction::Report {
            attempts: LIVENESS_RELOADS
        }
    );
}

/// A navigation the watchdog did not start leaves it holding a reading from a document that no
/// longer exists — and the new document counts frames from zero, so the very next probe reads as
/// "stopped drawing". The automatic reload arm resets in place; every other navigation has to say
/// so. Checked against the source because the mistake is a missing call, not a wrong value.
#[test]
fn every_reload_that_is_not_the_watchdogs_own_announces_itself() {
    let source = include_str!("../window.rs");
    let navigations = source.matches(".navigate(").count();
    let announcements = source.matches("note_external_reload();").count();
    assert_eq!(
        navigations,
        announcements + 1,
        "除看护自己的重载外，每处 navigate 都要调用 note_external_reload()，否则新文档会被拿旧读数判定"
    );
}

#[test]
fn the_frame_probe_is_es5_and_schedules_at_most_one_callback_of_each_kind() {
    // The probe runs on every engine this shell supports, including the old WebKits the
    // compat layer exists for.
    for syntax in ["=>", "```", "??", "const ", "let ", "class "] {
        assert!(
            !FRAME_PROBE.contains(syntax),
            "存活探测必须保持 ES5，发现 {syntax:?}"
        );
    }
    // The pending callback is itself the evidence: one at a time, never a queue.
    assert!(FRAME_PROBE.contains("__dshFramePending"), "{FRAME_PROBE}");
    assert!(
        FRAME_PROBE.contains("requestAnimationFrame"),
        "{FRAME_PROBE}"
    );
    // The timer heartbeat is what separates "not drawn to" from "not running", so both its
    // scheduling and its own pending guard are load-bearing.
    assert!(FRAME_PROBE.contains("__dshTimerPending"), "{FRAME_PROBE}");
    assert!(FRAME_PROBE.contains("setTimeout(bump, 0)"), "{FRAME_PROBE}");
    assert!(FRAME_PROBE.contains("w.__dshTimers += 1"), "{FRAME_PROBE}");
    // It has to answer with both counts, so the shell can compare two probes.
    assert!(
        FRAME_PROBE.contains("frames: w.__dshFrames"),
        "{FRAME_PROBE}"
    );
    assert!(
        FRAME_PROBE.contains("timers: w.__dshTimers"),
        "{FRAME_PROBE}"
    );
    // An engine without `setTimeout` cannot say whether it is suspended; it must report that
    // rather than answering with a counter that never moves and reading as stalled.
    assert!(
        FRAME_PROBE.contains("timersSeen: w.__dshTimerSeen === true"),
        "{FRAME_PROBE}"
    );
    // The report is the object itself, never `JSON.stringify(...)` of one: WebKit hands the shell
    // the JSON of the value the script evaluates to, so a returned *string* arrives double-encoded
    // and every answer fails to parse — which the watchdog reads as a dead renderer and reloads
    // for. Asserting the probe contained "frames:" instead of checking the wire format is what let
    // that ship once (2026-09-18); the round-trip test below is the real guard.
    assert!(
        !FRAME_PROBE.contains("JSON.stringify"),
        "探测不能自己序列化：返回值会被 WebKit 再序列化一次，变成双重编码：{FRAME_PROBE}"
    );
    assert!(
        FRAME_PROBE.contains("return {") && FRAME_PROBE.trim_end().ends_with("})()"),
        "探测必须直接返回对象字面量：{FRAME_PROBE}"
    );
    // The watchdog has to outlast a slow but healthy page, and give up before a user would.
    assert!(LIVENESS_TIMEOUT < LIVENESS_INTERVAL);
    // The budgets are compile-time facts (clippy refuses asserting on constants), so the
    // policy matrix above is what pins their behaviour.
    assert_eq!((LIVENESS_MISSES, LIVENESS_RELOADS), (3, 3));
}

/// The probe and the parser are two halves of one wire format, and a rename on either side would
/// turn every answer into "silence" — which the watchdog reads as a dead renderer and reloads for.
/// The JSON WebKit would hand the callback for a probe that evaluates to an object literal.
///
/// WebKit serialises the *value* the script evaluated to, so the wire format is derived from the
/// probe's own return statement rather than from a sample written by hand: a rename in the probe
/// has to fail this test, because a rename is exactly the mistake that turns every answer into
/// "silence" and makes the watchdog reload a page that was fine.
fn webkit_wire_json(probe: &str) -> String {
    let body = probe
        .split_once("return {")
        .expect("the probe must return an object literal")
        .1;
    let body = body
        .split_once("};")
        .expect("the returned object literal must be terminated")
        .0;
    let mut fields = Vec::new();
    for line in body.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // `timersSeen` is a comparison, so it arrives as a boolean; the counters are numbers.
        let rendered = if value.contains("===") { "true" } else { "12" };
        fields.push(format!("\"{}\":{}", key.trim(), rendered));
    }
    assert!(!fields.is_empty(), "the probe returned no fields: {probe}");
    format!("{{{}}}", fields.join(","))
}

#[test]
fn the_probe_reports_exactly_the_fields_the_shell_parses() {
    // What WebKit hands the callback: the JSON of the value the script evaluated to. Modelling it
    // from the *probe's own source* is the point — a hand-written sample cannot catch the probe
    // changing shape.
    let wire = webkit_wire_json(FRAME_PROBE);
    let parsed: Frames = serde_json::from_str(&wire).expect("the probe's own shape must parse");
    assert_eq!(
        parsed,
        Frames {
            frames: 12,
            timers: 12,
            timers_seen: true,
            hidden: true,
            fallbacks: 12,
        }
    );
    // Every key the parser needs is a key the probe emits, spelled the same way.
    for key in ["frames", "timers", "timersSeen", "hidden", "fallbacks"] {
        assert!(FRAME_PROBE.contains(key), "探测缺少 {key}：{FRAME_PROBE}");
    }
    // The wire format is modelled from the probe, and `hidden` is the field the whole verdict now
    // hangs on: the derivation above can only produce `true` for it, so this pins that the
    // comparison really is the one that answers "is this page being drawn to".
    assert!(
        FRAME_PROBE.contains("visibilityState === \"hidden\""),
        "探测必须用 visibilityState 判断是否被绘制：{FRAME_PROBE}"
    );
    // A page that answers with something else is silence, not a page with a zero counter:
    // parsing it as a default would look like a page that never drew a frame.
    assert!(serde_json::from_str::<Frames>("12").is_err());
    assert!(serde_json::from_str::<Frames>("not json").is_err());
    // A double-encoded answer — the shape a self-serialising probe produces — is silence too.
    // This is the exact regression that shipped on 2026-09-18: the probe returned
    // `JSON.stringify({...})`, WebKit serialised that string, and every probe read as a dead
    // renderer until the reload budget ran out.
    let double_encoded = serde_json::to_string(&wire).unwrap();
    assert!(
        serde_json::from_str::<Frames>(&double_encoded).is_err(),
        "双重编码必须被拒绝，否则这个 bug 会静默通过"
    );
}

#[test]
fn the_status_script_applies_now_or_waits_for_the_page() {
    let script = status_script("__setFailure", "Harness 已退出", "退出码 0\n第二行");
    // The page defines its entry points in <head>; a freshly created window may not have
    // parsed them yet, so the script must retry rather than drop the message.
    assert!(script.contains("window.__setFailure"), "{script}");
    assert!(script.contains("setInterval"), "{script}");
    // The text is embedded as a JS string literal, newlines and all.
    assert!(script.contains(r#""退出码 0\n第二行""#), "{script}");
    // Same builder for the progress face: the difference is which entry point it calls.
    let progress = status_script("__setStatus", "正在启动…", "");
    assert!(progress.contains("window.__setStatus"), "{progress}");
    assert!(!progress.contains("__setFailure"));
}

/// The page names the restart event as a string literal; the shell listens for the
/// constant. Renaming one without the other would silently drop every click.
#[test]
fn the_status_page_reports_the_restart_event_the_shell_listens_for() {
    let page = include_str!("../../../src/index.html");
    assert!(page.contains(RESTART_EVENT), "按钮必须发壳监听的那个事件名");
    assert!(
        page.contains("__setFailure"),
        "页面要有终态入口（带重启按钮的那一面）"
    );
    assert!(page.contains("__setStatus"), "页面要有进行中入口");
    assert!(
        page.contains("plugin:event|emit"),
        "只走 splash 窗口已有的 core 事件能力，不新增 capability"
    );
}

/// The takeover question is a second event on the same bridge, and the answer is matched
/// on the question id: a click that crossed a retry must not be read as the current answer.
#[test]
fn the_takeover_question_and_its_answer_stay_in_step() {
    let page = include_str!("../../../src/index.html");
    assert!(
        page.contains(CHOICE_EVENT),
        "页面的回答事件名要和壳常量一致"
    );
    assert!(page.contains("__askChoice"), "页面要有提问入口");
    assert!(page.contains("question: question"), "回答必须带回问题 id");
    // The labels come from the shell and are drawn as text: a process command line must
    // never be able to become markup on this page.
    assert!(page.contains("textContent = option.label"), "{page}");
    assert!(!page.contains("innerHTML"), "提问面板不能用 innerHTML 渲染");
}

/// The question is the one page here the user must read before acting, and it is taller than
/// the splash. Two layout rules decide whether it is readable at all, and both were wrong on a
/// real launch (2026-09-16): the window was too short for the content, and the page centred it
/// with a flex rule that clips overflow at *both* ends — the spinner and title went off the
/// top while the explanation was cut mid-line at the bottom.
#[test]
fn the_question_gets_a_window_and_a_layout_that_fit_it() {
    // The window grows for the question, and the two sizes must stay in that order: a
    // question window no taller than the splash would put the buttons back below the fold.
    assert!(
        QUESTION_SIZE.1 > SPLASH_SIZE.1,
        "the question needs more height than the splash: {QUESTION_SIZE:?} vs {SPLASH_SIZE:?}"
    );
    assert!(QUESTION_SIZE.0 >= SPLASH_SIZE.0, "{QUESTION_SIZE:?}");

    let page = include_str!("../../../src/index.html");
    // `margin: auto` on the content, not `justify-content: center` on the body: the latter
    // clips the overflow instead of scrolling it.
    assert!(page.contains("margin: auto"), "内容靠 margin:auto 居中");
    // The comment above explains why, so match the declaration and not the word: a bare
    // `contains` would also flag the note that says not to do this.
    assert!(
        !page.contains("justify-content: center;"),
        "flex 居中会裁掉溢出内容，改用 margin:auto"
    );
    // The explanation is capped so four buttons and the hint stay on screen; the box scrolls
    // rather than pushing the answer off the bottom.
    assert!(page.contains("body.asking #detail"), "{page}");
    assert!(page.contains("#detail:empty"), "空的详情框不该显示成灰条");
}

/// The question script escapes what it carries: a command line can hold quotes, and a
/// broken script would leave the user with a page that never asks anything.
#[test]
fn the_choice_script_escapes_its_payload() {
    let options = vec![ChoiceOption {
        id: "take-over".to_string(),
        label: "终止 \"node\" 并接管".to_string(),
    }];
    let script = choice_script(7, "标题", "详情", &options, "提示");
    assert!(script.contains("window.__askChoice(7,"), "{script}");
    assert!(script.contains(r#"\"node\""#), "{script}");
    // The retry wrapper is the same one the status page uses: an eval that lands before
    // the page parsed its head must not be lost.
    assert!(script.contains("setInterval"), "{script}");
}

#[test]
fn load_retry_follows_the_started_event_and_never_fails_a_live_port() {
    // A navigation that never began is sent again while attempts remain, whether or not the
    // port answers: the blank-window case usually comes with a healthy port.
    assert_eq!(load_outcome(1, 3, true, false, true), LoadOutcome::Retry);
    assert_eq!(load_outcome(2, 3, true, false, false), LoadOutcome::Retry);
    // A load that started is not interrupted, however long it takes.
    assert_eq!(load_outcome(1, 3, true, true, true), LoadOutcome::Settled);
    assert_eq!(load_outcome(3, 3, true, true, false), LoadOutcome::Settled);
    // Out of attempts: a failure page only when nothing serves the port either. A healthy
    // port means the load events themselves are missing, which a retry cannot fix.
    assert_eq!(load_outcome(3, 3, true, false, false), LoadOutcome::Report);
    assert_eq!(load_outcome(3, 3, true, false, true), LoadOutcome::Settled);
    // The user closed the window: stop watching it.
    assert_eq!(
        load_outcome(1, 3, false, false, false),
        LoadOutcome::Settled
    );
}

#[test]
fn download_names_never_collide() {
    let dir = crate::test_dir("dsh-desktop-download-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Free name is used as-is, taken names get a counter before the extension.
    assert_eq!(
        unique_download_path(&dir, std::ffi::OsStr::new("a.pdf")),
        dir.join("a.pdf")
    );
    std::fs::write(dir.join("a.pdf"), "x").unwrap();
    assert_eq!(
        unique_download_path(&dir, std::ffi::OsStr::new("a.pdf")),
        dir.join("a-1.pdf")
    );
    std::fs::write(dir.join("a-1.pdf"), "x").unwrap();
    assert_eq!(
        unique_download_path(&dir, std::ffi::OsStr::new("a.pdf")),
        dir.join("a-2.pdf")
    );
    // Extension-less names keep working.
    std::fs::write(dir.join("b"), "x").unwrap();
    assert_eq!(
        unique_download_path(&dir, std::ffi::OsStr::new("b")),
        dir.join("b-1")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Exhausting the numbered candidates must not fall back to a name that is already taken.
///
/// The old fallback returned the requested name itself, so the one case it existed for — a
/// thousand files already called `a.pdf` — silently overwrote one of them (review C4).
#[test]
fn an_exhausted_name_search_never_reuses_a_taken_file() {
    let dir = std::env::temp_dir().join(format!(
        "dsh-desktop-download-exhaust-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let name = std::ffi::OsStr::new("a.pdf");
    std::fs::write(dir.join("a.pdf"), "x").unwrap();
    for index in 1..1000 {
        std::fs::write(dir.join(format!("a-{index}.pdf")), "x").unwrap();
    }

    let chosen = unique_download_path(&dir, name);
    assert!(
        !chosen.exists(),
        "the chosen name must be free: {}",
        chosen.display()
    );
    assert_ne!(
        chosen,
        dir.join("a.pdf"),
        "the existing file must not be reused"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A watcher may only act while its window is still the one it was created for.
///
/// Both watchdogs read the window back by label, and after a rebuild that label answers with
/// the *new* window: without this test a stale loop would reload it, retitle it, or — on the
/// load watcher `Report` arm — destroy it and replace a working page with a failure page.
#[test]
fn a_watcher_stops_acting_once_its_window_was_rebuilt() {
    assert!(may_act(7, 7));
    // Any other generation is somebody else window: older or newer, the answer is the same.
    assert!(!may_act(7, 8));
    assert!(!may_act(8, 7));
    assert!(!may_act(0, 1));
}

/// One title, two messages, and the drawing problem wins.
///
/// A page that is not painting hides the model output *now*; a newer version only means the next
/// launch could be better. Composing them in one place is also what keeps the two paths from
/// overwriting each other: the watchdog recomposes when it recovers, which must not erase a
/// notice the update check left behind (reported 2026-09-18 — the notice used to live only on a
/// splash window that was destroyed seconds later).
#[test]
fn the_title_carries_the_update_notice_without_losing_the_drawing_message() {
    let notice = "有新版本 v0.1.6-alpha.2 可用（未自动安装）";
    // The ordinary case: nothing to say.
    assert_eq!(compose_title(None, false), "DeepSeek Harness");
    // A notice, with the plain title as its prefix so the window is still identifiable.
    assert_eq!(
        compose_title(Some(notice), false),
        format!("DeepSeek Harness（{notice}）")
    );
    // Not drawing outranks the notice, and says so instead of merging the two sentences.
    assert_eq!(compose_title(None, true), NOT_DRAWING_TITLE);
    assert_eq!(compose_title(Some(notice), true), NOT_DRAWING_TITLE);
    // Recovering from a stopped page keeps a pending notice rather than resetting to the plain
    // title — that is the composition the watchdog Alive arm performs.
    assert_eq!(
        compose_title(Some(notice), false),
        format!("DeepSeek Harness（{notice}）")
    );
}

/// The notice is process-wide state, so a launch that finds nothing to report must not inherit
/// the previous one's. An auto-restart re-enters `start()` in the same process.
#[test]
fn a_cleared_notice_stays_cleared() {
    let seed = "有新版本 v9.9.9 可用（未自动安装）".to_string();
    {
        let mut pending = PENDING_UPDATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = Some(seed);
    }
    assert!(pending_notice().is_some());
    assert!(compose_title(pending_notice().as_deref(), false).contains("v9.9.9"));
    // `clear_update_notice` needs a real window to retitle, so the state is cleared here exactly
    // as it does; the assertion is about the state, which is what the next launch reads.
    {
        let mut pending = PENDING_UPDATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = None;
    }
    assert_eq!(pending_notice(), None);
    assert_eq!(
        compose_title(pending_notice().as_deref(), false),
        "DeepSeek Harness"
    );
}

/// The diagnostics script runs on the same engines the compat layer exists for, and a second
/// injection must not stack a second console wrapper (which would double every line).
#[test]
fn the_page_diagnostics_script_stays_es5_and_installs_once() {
    let script = page_diagnostics_script();
    for syntax in ["=>", "`", "??", "const ", "let ", "class "] {
        assert!(
            !script.contains(syntax),
            "页面诊断必须保持 ES5，发现 {syntax:?}"
        );
    }
    // The guard is read before anything is installed, so a replayed injection is a no-op.
    assert!(script.contains("__dshPageNotes !== undefined"), "{script}");
    // Both bounds come from the constants, not from numbers typed into the script.
    assert!(script.contains(&PAGE_NOTE_LIMIT.to_string()), "{script}");
    assert!(script.contains(&PAGE_NOTE_CHARS.to_string()), "{script}");
    assert!(!script.contains("%LIMIT%"), "占位符必须被替换掉：{script}");
    assert!(!script.contains("%CHARS%"), "占位符必须被替换掉：{script}");
    // Capture phase: a subresource `error` event does not bubble, so the bubble phase would
    // never see the one branch that reports a plugin bundle that failed to load.
    assert!(
        script.contains(r#"addEventListener("error", function (event) {"#),
        "{script}"
    );
    assert!(
        script.contains("}, true);"),
        "error 监听必须走捕获阶段：{script}"
    );

    let probe = r#"
      var calls = 0;
      var stub = {
        addEventListener: function () {},
        console: { error: function () { calls += 1; }, warn: function () {} }
      };
      global.window = stub;
      __SCRIPT__
      var first = stub.console.error;
      __SCRIPT__
      stub.console.error("x");
      process.stdout.write((stub.console.error === first ? "once" : "twice") + ":" + calls);
    "#
    .replace("__SCRIPT__", &script);
    // One wrapper, and the original still ran exactly once underneath it.
    assert_eq!(run_shim("", &probe), "once:1");
}

/// The whole point of the script: a fault inside the page has to survive as a line the shell can
/// log. Run it in a real engine and drive each of the four sources.
#[test]
fn the_page_diagnostics_script_records_every_kind_of_page_fault() {
    let probe = r#"
      var listeners = {};
      var printed = [];
      var stub = {
        performance: { now: function () { return 0; } },
        addEventListener: function (type, fn) { listeners[type] = fn; },
        console: {
          error: function (m) { printed.push(m); },
          warn: function (m) { printed.push(m); }
        }
      };
      global.window = stub;
      __SCRIPT__
      listeners.error({ message: "boom", filename: "a.js", lineno: 7, error: { stack: "at f" } });
      // A failed subresource arrives on the same event with no message at all.
      listeners.error({ target: { src: "http://127.0.0.1:3080/plugin.js" } });
      listeners.unhandledrejection({ reason: { message: "stream died", stack: "at g" } });
      stub.console.error("[session-controller] control stream failed:", "closed");
      stub.console.warn("degraded");
      process.stdout.write(JSON.stringify({
        notes: stub.__dshPageNotes,
        passedThrough: printed
      }));
    "#
    .replace("__SCRIPT__", &page_diagnostics_script());
    let out = run_shim("", &probe);
    // Each source is labelled, and the console wrapper still called the original first.
    for expected in [
        "error: boom @a.js:7 at f",
        "resource: http://127.0.0.1:3080/plugin.js",
        "rejection: stream died at g",
        "error: [session-controller] control stream failed: closed",
        "warn: degraded",
    ] {
        assert!(out.contains(expected), "缺少 {expected:?}：{out}");
    }
    assert!(
        out.contains(
            r#""passedThrough":["[session-controller] control stream failed:","degraded"]"#
        ),
        "原 console 实现必须照常执行：{out}"
    );
}

/// A page erroring in a loop must not turn one probe's answer into an unbounded payload, and a
/// single huge value must not carry a whole document into the log.
#[test]
fn the_page_diagnostics_ring_is_bounded_in_both_directions() {
    let probe = r#"
      var listeners = {};
      var stub = {
        performance: { now: function () { return 0; } },
        addEventListener: function (type, fn) { listeners[type] = fn; },
        console: { error: function () {}, warn: function () {} }
      };
      global.window = stub;
      __SCRIPT__
      for (var i = 0; i < %OVERFLOW%; i++) stub.console.error("line" + i);
      var long = "";
      for (var j = 0; j < 4000; j++) long += "x";
      stub.console.error(long);
      var notes = stub.__dshPageNotes;
      process.stdout.write(JSON.stringify({
        kept: notes.length,
        oldest: notes[0],
        longest: notes[notes.length - 1].length
      }));
    "#
    .replace("__SCRIPT__", &page_diagnostics_script())
    .replace("%OVERFLOW%", &(PAGE_NOTE_LIMIT + 10).to_string());
    let out = run_shim("", &probe);
    assert!(
        out.contains(&format!(r#""kept":{PAGE_NOTE_LIMIT}"#)),
        "环形缓冲必须封顶在 PAGE_NOTE_LIMIT：{out}"
    );
    // The oldest were dropped, not the newest. `+ 1` is the long note pushed after the loop, so
    // the first survivor is however many the ring had to give up.
    let dropped = PAGE_NOTE_LIMIT + 10 + 1 - PAGE_NOTE_LIMIT;
    assert!(
        out.contains(&format!(r#""oldest":"0ms error: line{dropped}""#)),
        "丢的必须是最旧的：{out}"
    );
    // Truncation leaves the note a log line rather than a file: the 4000-character value comes
    // back as the cap plus this script's own short prefix and ellipsis, never the whole thing.
    let longest: usize = out
        .split(r#""longest":"#)
        .nth(1)
        .and_then(|rest| rest.trim_end_matches('}').parse().ok())
        .unwrap_or_else(|| panic!("探针必须报出最长记录长度：{out}"));
    assert!(
        longest > PAGE_NOTE_CHARS && longest < PAGE_NOTE_CHARS + 64,
        "超长记录必须被截断到常量附近，实测 {longest}：{out}"
    );
}

/// The probe carries the notes back and leaves the page's buffer empty, so no line is reported
/// twice and an unprobed page cannot grow one without bound.
#[test]
fn the_frame_probe_drains_the_page_notes_and_reports_pending_frames() {
    let probe = r#"
      var stub = {
        __dshPageNotes: ["0ms error: boom"],
        __dshPendingFrames: 3,
        document: { visibilityState: "visible" },
        setTimeout: function (fn) { return 1; },
        requestAnimationFrame: function () { return 1; }
      };
      global.window = stub;
      var answer = __PROBE__;
      process.stdout.write(JSON.stringify({
        reported: answer.notes,
        pending: answer.pending,
        leftOnPage: stub.__dshPageNotes
      }));
    "#
    .replace("__PROBE__", FRAME_PROBE.trim());
    let out = run_shim("", &probe);
    assert!(out.contains(r#""reported":["0ms error: boom"]"#), "{out}");
    assert!(out.contains(r#""pending":3"#), "{out}");
    assert!(
        out.contains(r#""leftOnPage":[]"#),
        "取走后页面不得再留一份：{out}"
    );
}

/// A page with no diagnostics and no fallback still has to parse, and the counters the liveness
/// policy judges must come out identical either way: the two new fields are log-only.
#[test]
fn page_notes_never_reach_the_liveness_verdict() {
    let bare = r#"{"frames":5,"timers":9,"timersSeen":true,"hidden":false,"fallbacks":0}"#;
    let rich = r#"{"frames":5,"timers":9,"timersSeen":true,"hidden":false,"fallbacks":0,
                   "pending":7,"notes":["0ms error: boom"]}"#;
    let bare: PageProbe = serde_json::from_str(bare).expect("旧页面的载荷必须仍能解析");
    let rich: PageProbe = serde_json::from_str(rich).expect("新载荷必须能解析");
    assert_eq!(bare.frames, rich.frames);
    // A page that cannot count pending frames reads as "unknown", never as zero.
    assert_eq!(bare.pending, -1);
    assert!(bare.notes.is_empty());
    assert_eq!(rich.pending, 7);
    assert_eq!(rich.notes, vec!["0ms error: boom".to_string()]);
    // The verdict is computed from `frames` alone, so both answers judge the same.
    let previous = Frames {
        frames: 5,
        timers: 4,
        timers_seen: true,
        ..Frames::default()
    };
    assert_eq!(
        judge_frames(Some(bare.frames), Some(previous)),
        judge_frames(Some(rich.frames), Some(previous))
    );
}

/// The guard runs on the same engines the compat layer exists for, and a replayed injection must
/// not stack a second listener.
#[test]
fn the_menu_focus_guard_stays_es5_and_installs_once() {
    let script = menu_focus_guard_script();
    for syntax in ["=>", "`", "??", "const ", "let ", "class "] {
        assert!(
            !script.contains(syntax),
            "菜单焦点守卫必须保持 ES5，发现 {syntax:?}"
        );
    }
    assert!(script.contains("__dshMenuFocusGuard === true"), "{script}");
    // Capture phase is load-bearing: it has to beat the page's own mousedown handling.
    assert!(script.contains("}, true);"), "必须走捕获阶段：{script}");
    // The fix is suppressing the default action, never moving the focus: focusing the pressed row
    // is what broke the first pane (see the constant's doc comment).
    assert!(script.contains("event.preventDefault();"), "{script}");
    assert!(
        !script.contains(".focus("),
        "守卫不得自己搬动焦点：{script}"
    );

    let probe = r#"
      var added = 0;
      var stub = { document: { addEventListener: function () { added += 1; } } };
      global.window = stub;
      __SCRIPT__
      __SCRIPT__
      process.stdout.write(String(added));
    "#
    .replace("__SCRIPT__", &script);
    assert_eq!(run_shim("", &probe), "1");
}

/// The bug this exists for, driven in a real engine against the shape of the real markup: a press
/// inside the popup must suppress its default action (so WebKit blurs nothing and the popup stays
/// mounted long enough for the click), and everything outside a popup must be left alone.
#[test]
fn a_press_inside_a_menu_popup_leaves_the_focus_alone() {
    let dom = r#"
      var listeners = [];
      function node(role, parent, extra) {
        var self = {
          nodeType: 1,
          parentNode: parent === undefined ? null : parent,
          tagName: (extra && extra.tagName) || "BUTTON",
          getAttribute: function (name) { return name === "role" ? role : null; }
        };
        if (extra && extra.contentEditable) self.isContentEditable = true;
        return self;
      }
      var stub = {
        document: {
          addEventListener: function (type, fn) { listeners.push({ type: type, fn: fn }); }
        }
      };
      global.window = stub;
    "#;
    let drive = r#"
      var press = function (target, options) {
        var event = { button: 0, defaultPrevented: false, target: target, prevented: false };
        event.preventDefault = function () { event.prevented = true; };
        if (options) for (var key in options) event[key] = options[key];
        for (var i = 0; i < listeners.length; i++) {
          if (listeners[i].type === "mousedown") listeners[i].fn(event);
        }
        return event.prevented ? "held" : "released";
      };
      var out = [];
      // The real markup: role="menu" popup > role="menuitem" button > <span> label.
      var menu = node("menu");
      var row = node("menuitem", menu);
      out.push(press(node(null, row, { tagName: "SPAN" })));
      // The composer around it must keep behaving normally.
      out.push(press(node(null, node(null, null, { tagName: "DIV" }), { tagName: "BUTTON" })));
      // A filter field inside a popup owns the caret: the guard must not take it.
      out.push(press(node(null, node("listbox"), { tagName: "INPUT" })));
      out.push(press(node(null, node("menu"), { contentEditable: true, tagName: "DIV" })));
      // Secondary button and an already-handled press.
      out.push(press(row, { button: 2 }));
      out.push(press(row, { defaultPrevented: true }));
      process.stdout.write(out.join(","));
    "#;
    let script = format!("{dom}\n{guard}\n{drive}", guard = menu_focus_guard_script());
    assert_eq!(
        run_shim("", &script),
        "held,released,released,released,released,released"
    );
}
