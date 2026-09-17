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
fn a_page_that_stops_drawing_is_reloaded_and_then_reported() {
    let drawing = PageState::Drawing;
    let frozen = PageState::Frozen;
    let idle = false;
    // One bad probe is a hiccup: the shell waits instead of throwing away the page state.
    assert_eq!(
        liveness_action(frozen, true, idle, 0, 0),
        LivenessAction::Wait { misses: 1 }
    );
    // The second one in a row is a page that has stopped drawing.
    assert_eq!(
        liveness_action(frozen, true, idle, 1, 0),
        LivenessAction::Reload { attempt: 1 }
    );
    // Anything that draws clears the streak, whatever the reload count was.
    assert_eq!(
        liveness_action(drawing, true, idle, 5, 2),
        LivenessAction::Alive
    );
    // Reloads are budgeted: the shell stops instead of looping over a page that never returns.
    assert_eq!(
        liveness_action(frozen, true, idle, 1, 1),
        LivenessAction::Reload { attempt: 2 }
    );
    assert_eq!(
        liveness_action(frozen, true, idle, 1, 3),
        LivenessAction::Report { attempts: 3 }
    );
    // A bad probe mid-recovery does not reset the reload budget.
    assert_eq!(
        liveness_action(frozen, true, idle, 0, 3),
        LivenessAction::Wait { misses: 1 }
    );
    assert_eq!(
        liveness_action(frozen, true, idle, 1, u32::MAX),
        LivenessAction::Report { attempts: u32::MAX }
    );
    // A silent page is broken wherever it is: focus is no excuse for answering nothing.
    assert_eq!(
        liveness_action(PageState::Silent, false, idle, 1, 0),
        LivenessAction::Reload { attempt: 1 }
    );
}

/// A window the user is not looking at is one WebKit may legitimately stop drawing, and
/// reloading it would turn "the user switched away" into a reload loop.
#[test]
fn a_background_window_is_not_accused_of_being_frozen() {
    let idle = false;
    assert_eq!(
        liveness_action(PageState::Frozen, false, idle, 0, 0),
        LivenessAction::Unattended
    );
    // Even a long streak of unattended probes stays unattended: no reloads are spent.
    assert_eq!(
        liveness_action(PageState::Frozen, false, idle, 4, 2),
        LivenessAction::Unattended
    );
    // Drawing is drawing, attended or not.
    assert_eq!(
        liveness_action(PageState::Drawing, false, idle, 0, 0),
        LivenessAction::Alive
    );
}

/// Reloading is the only recovery for a frozen page, and it is also what discards an unsent
/// prompt. Someone typing in a page that stopped drawing gets another interval first.
#[test]
fn a_page_someone_is_typing_in_is_not_reloaded_yet() {
    // The user is mid-sentence: wait, and keep the streak so the budget is untouched.
    assert_eq!(
        liveness_action(PageState::Frozen, true, true, 1, 0),
        LivenessAction::Busy { misses: 2 }
    );
    // The moment they stop, the reload the page still needs happens.
    assert_eq!(
        liveness_action(PageState::Frozen, true, false, 2, 0),
        LivenessAction::Reload { attempt: 1 }
    );
    // Typing never spends or restores the reload budget.
    assert_eq!(
        liveness_action(PageState::Frozen, true, true, 2, 2),
        LivenessAction::Busy { misses: 3 }
    );
    // A page that answers nothing is not "busy": the probe that would say so is the code
    // that stopped running, so silence must still reload rather than wait for ever.
    assert_eq!(
        liveness_action(PageState::Silent, true, false, 1, 0),
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
#[test]
fn activity_is_read_as_busy_only_when_someone_is_working() {
    // Typing in a field: busy, however long the pause between keystrokes.
    assert!(page_is_busy(Some(r#"{"editing":true,"idle":900000}"#)));
    // Not focused, but typed in within the last interval.
    assert!(page_is_busy(Some(r#"{"editing":false,"idle":100}"#)));
    // Focused elsewhere, idle for minutes: nothing to lose.
    assert!(!page_is_busy(Some(r#"{"editing":false,"idle":900000}"#)));
    // Never typed since the page loaded.
    assert!(!page_is_busy(Some(
        r#"{"editing":false,"idle":1000000000}"#
    )));
    // A page that cannot answer is gone, not busy: waiting would only delay recovery.
    assert!(!page_is_busy(None));
    assert!(!page_is_busy(Some("not json")));
    assert!(!page_is_busy(Some("")));
}

/// The frame counter is the whole signal, so its algebra has to be exact.
#[test]
fn only_a_moving_frame_counter_proves_the_page_is_drawing() {
    // First answer: nothing to compare against, so a number that arrived is progress.
    assert_eq!(judge_frames(Some(1), None), PageState::Drawing);
    assert_eq!(judge_frames(Some(0), None), PageState::Drawing);
    // Same number twice: JavaScript runs and no frame was produced in between.
    assert_eq!(judge_frames(Some(7), Some(7)), PageState::Frozen);
    // A larger number is frames being drawn.
    assert_eq!(judge_frames(Some(8), Some(7)), PageState::Drawing);
    // A smaller number is a fresh document, not a fault: a reload starts the count over.
    assert_eq!(judge_frames(Some(1), Some(40)), PageState::Drawing);
    // No answer at all is the renderer being gone or wedged.
    assert_eq!(judge_frames(None, Some(40)), PageState::Silent);
    assert_eq!(judge_frames(None, None), PageState::Silent);
}

#[test]
fn the_frame_probe_is_es5_and_schedules_at_most_one_frame() {
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
    // It has to answer with the count, so the shell can compare two probes.
    assert!(
        FRAME_PROBE.contains("return w.__dshFrames"),
        "{FRAME_PROBE}"
    );
    // The watchdog has to outlast a slow but healthy page, and give up before a user would.
    assert!(LIVENESS_TIMEOUT < LIVENESS_INTERVAL);
    // The budgets are compile-time facts (clippy refuses asserting on constants), so the
    // policy matrix above is what pins their behaviour.
    assert_eq!((LIVENESS_MISSES, LIVENESS_RELOADS), (2, 3));
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
