//! Runs the legacy-WebKit compat layer in a real JavaScript engine.
//!
//! `window::compat_script` is only ever injected into an engine that lacks the APIs it installs,
//! and no machine that can build this shell still has one of those. Deleting the APIs first is
//! the only way to exercise the shim — and the pdf.js guard it exists to revive — end to end.
//!
//! Skipped when `node` is not on PATH. It needs no network, so the default `cargo test` runs it.

use dsh_desktop_lib::window;
use std::path::PathBuf;
use std::process::Command;

fn path_lookup(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// A watched API's check expression as a global path: `[]` is shorthand for the array prototype.
fn global_path(expression: &str) -> String {
    expression.replace("[]", "Array.prototype")
}

/// Every API the compat layer installs, as the driver must strip them: a machine that still has
/// `Iterator` would prove nothing about the shim.
fn stripped_paths() -> Vec<String> {
    window::SHIMMED_APIS
        .iter()
        .map(|(_, expression)| global_path(expression))
        .collect()
}

/// The splash page's own head scripts, in order.
///
/// The probe no longer decides the syntax question itself: the page carries a CSP, and the eval it
/// would need is exactly what `script-src` forbids. The page answers instead, and the probe waits
/// for that answer. A driver that only runs the probe therefore models a page that never reported —
/// which is why these scripts are fed to it: together they are the contract under test.
///
/// Scripts that touch `document` are skipped: those belong to the status page in the body, and the
/// syntax verdict is settled in the head before any of them run.
fn page_head_scripts() -> String {
    let page = include_str!("../../src/index.html");
    let mut scripts = String::new();
    for block in page.split("<script>").skip(1) {
        let Some(body) = block.split("</script>").next() else {
            continue;
        };
        if body.contains("document.") {
            continue;
        }
        scripts.push_str(body);
        scripts.push('\n');
    }
    assert!(
        scripts.contains("__dshSyntaxMissing"),
        "the page head must establish the syntax verdict"
    );
    scripts
}

/// Runs one generated script through a node driver, returning its stdout.
fn run_in_node(name: &str, driver: &str, script: &str, extra_arg: Option<&str>) -> String {
    let Some(node) = path_lookup("node") else {
        eprintln!("skipped: node is not on PATH");
        return String::new();
    };
    let dir = std::env::temp_dir().join(format!("dsh-desktop-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir must be creatable");
    let script_path = dir.join("script.js");
    let driver_path = dir.join("driver.js");
    let page_path = dir.join("page.js");
    std::fs::write(&script_path, script).expect("generated script must be writable");
    std::fs::write(&driver_path, driver).expect("driver must be writable");
    std::fs::write(&page_path, page_head_scripts()).expect("page scripts must be writable");

    let mut command = Command::new(&node);
    command.arg(&driver_path).arg(&script_path);
    if let Some(arg) = extra_arg {
        command.arg(arg);
    }
    command.arg(&page_path);
    let output = command.output().expect("node must be spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "{name} failed in node:\n{stdout}\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    stdout
}

/// Like [`run_in_node`], but the driver gets the splash page **in full**.
///
/// The compat probe only needs the head: it is a diagnostic that runs before the body exists. The
/// status page is the opposite — its buttons and panels live in the body — so a driver for that
/// half has to be handed the whole document.
fn run_in_node_with_page(
    name: &str,
    driver: &str,
    script: &str,
    extra_arg: Option<&str>,
) -> String {
    let Some(node) = path_lookup("node") else {
        eprintln!("skipped: node is not on PATH");
        return String::new();
    };
    let dir = std::env::temp_dir().join(format!("dsh-desktop-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir must be creatable");
    let driver_path = dir.join("driver.js");
    let script_path = dir.join("script.js");
    let page_path = dir.join("index.html");
    std::fs::write(&driver_path, driver).expect("driver must be writable");
    std::fs::write(&script_path, script).expect("generated script must be writable");
    std::fs::write(&page_path, include_str!("../../src/index.html"))
        .expect("page must be writable");

    let mut command = Command::new(&node);
    command.arg(&driver_path).arg(&script_path);
    if let Some(arg) = extra_arg {
        command.arg(arg);
    }
    command.arg(&page_path);
    let output = command.output().expect("node must be spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "{name} failed in node:\n{stdout}\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    stdout
}

/// A probe run stands in for the splash page: the page's head scripts run first (they produce the
/// syntax verdict the probe waits for), and `window.__TAURI_INTERNALS__` captures the report.
const PROBE_DRIVER: &str = r#"
const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

let captured = null;
// In a browser `window` IS the global object, and the page scripts rely on that: they assign
// `window.__dshSyntaxMissing` and the probe reads it back through the same name. Modelling it as
// a separate object would test a page shape that never ships.
globalThis.window = globalThis;
globalThis.window.__TAURI_INTERNALS__ = {
  invoke: function (command, args) { captured = { command: command, args: args }; }
};
// Node's own navigator global is read-only in some versions; either way the probe must report
// whatever agent string it can read.
try {
  Object.defineProperty(globalThis, "navigator", {
    value: { userAgent: "dsh-compat-probe-test" },
    configurable: true
  });
} catch (error) {}
delete globalThis.Iterator;
delete globalThis.Promise.try;

// The splash page head, exactly as it ships: this is what tells the probe the syntax verdict.
vm.runInThisContext(fs.readFileSync(process.argv[4], "utf8"), { filename: "page.js" });
assert.ok(
  Array.isArray(globalThis.__dshSyntaxMissing),
  "the page must establish the syntax verdict the probe waits for"
);

vm.runInThisContext(fs.readFileSync(process.argv[2], "utf8"), { filename: "probe.js" });

assert.ok(captured !== null, "the probe must report once the page has answered");
assert.strictEqual(captured.command, "plugin:event|emit");
assert.strictEqual(captured.args.event, process.argv[3]);
const payload = captured.args.payload;
assert.ok(Array.isArray(payload.missing), "missing must be a list");
assert.ok(Array.isArray(payload.degraded), "degraded must be a list");
assert.ok(Array.isArray(payload.syntax), "syntax must be a list");
assert.ok(payload.missing.indexOf("Iterator") >= 0, "Iterator must be reported: " + JSON.stringify(payload));
assert.ok(payload.missing.indexOf("Promise.try") >= 0, "Promise.try must be reported: " + JSON.stringify(payload));
assert.strictEqual(payload.syntax.length, 0, "this node parses class static blocks: " + JSON.stringify(payload.syntax));
assert.strictEqual(typeof payload.agent, "string");
assert.ok(payload.agent.length > 0, "the probe must name the engine it ran on");
console.log("probe ok, missing = " + JSON.stringify(payload.missing));
"#;

const DRIVER: &str = r#"
const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

const read = function (path) {
  const parts = path.split(".");
  let holder = globalThis;
  for (let index = 0; index < parts.length - 1; index += 1) {
    if (holder === undefined || holder === null) return undefined;
    holder = holder[parts[index]];
  }
  return holder === undefined || holder === null ? undefined : holder[parts[parts.length - 1]];
};
const stripFrom = function (path) {
  const parts = path.split(".");
  let holder = globalThis;
  for (let index = 0; index < parts.length - 1; index += 1) holder = holder[parts[index]];
  try { delete holder[parts[parts.length - 1]]; } catch (error) {}
};

const paths = %PATHS%;
let stripped = 0;
paths.forEach(function (path) {
  stripFrom(path);
  if (read(path) === undefined) {
    stripped += 1;
    return;
  }
  // A well-known symbol property is non-configurable, so node cannot be made to look like
  // Safari 18.0 for this one: its block guards itself out here. Everything else is stripped.
  console.log("note: " + path + " cannot be stripped in node; its block stays guarded off");
});
// Two APIs the shim installs alongside a watched one, stripped for the same reason.
["Symbol.asyncDispose", "Array.prototype.findLastIndex"].forEach(stripFrom);
assert.ok(stripped >= paths.length - 2, "premise: the engine must lack what the shim installs");

const script = fs.readFileSync(process.argv[2], "utf8");
const run = function () { vm.runInThisContext(script, { filename: "compat.js" }); };
run();
// A second injection must be a no-op, not a redefinition: the window navigates more than once.
run();

paths.concat(["Symbol.asyncDispose", "Array.prototype.findLastIndex"]).forEach(function (path) {
  assert.notStrictEqual(read(path), undefined, path + " must be installed");
});

// The guard the shipped document-preview bundle evaluates while it is imported.
assert.doesNotThrow(function () {
  if (typeof Iterator.prototype.join !== "function") {
    Iterator.prototype.join = function (separator) { return [...this].join(separator); };
  }
});
assert.strictEqual(typeof Iterator, "function");
assert.strictEqual([][Symbol.iterator]() instanceof Iterator, true, "instanceof must not throw");
assert.strictEqual([...Iterator.from([1, 2, 3])].join("-"), "1-2-3");
assert.strictEqual(Iterator.from(new Set([1, 2])).toArray().join(","), "1,2");
assert.strictEqual(
  Iterator.from([1, 2, 3, 4]).map(function (value) { return value * 10; }).filter(function (value) { return value > 20; }).toArray().join(","),
  "30,40"
);
assert.strictEqual(Iterator.from([1, 2, 3, 4]).take(2).toArray().join(","), "1,2");
assert.strictEqual(Iterator.from([1, 2, 3, 4]).drop(2).toArray().join(","), "3,4");
assert.strictEqual(Iterator.from([0, 1, 2]).take(0).toArray().length, 0);
assert.strictEqual(Iterator.from([[1], [2, 3]]).flatMap(function (values) { return values; }).toArray().join(","), "1,2,3");
assert.strictEqual(Iterator.from([1, 2, 3]).reduce(function (sum, value) { return sum + value; }), 6);
assert.strictEqual(Iterator.from([1, 2, 3]).reduce(function (sum, value) { return sum + value; }, 10), 16);
assert.strictEqual(Iterator.from([1, 2, 3]).some(function (value) { return value === 2; }), true);
assert.strictEqual(Iterator.from([1, 2, 3]).every(function (value) { return value > 0; }), true);
assert.strictEqual(Iterator.from([1, 2, 3]).find(function (value) { return value > 1; }), 2);
let seen = 0;
Iterator.from([1, 2]).forEach(function () { seen += 1; });
assert.strictEqual(seen, 2);
assert.strictEqual(Iterator.from([1, 2]).join("+"), "1+2");
assert.strictEqual([1, 2, 3][Symbol.iterator]().join("-"), "1-2-3", "the shipped helper must work on real iterators");
assert.throws(function () { Iterator(); }, TypeError, "Iterator itself stays abstract");

Promise.try(function (value) { return value + 1; }, 41).then(function (value) {
  assert.strictEqual(value, 42);
});
assert.strictEqual(typeof Promise.try(function () {}).then, "function");
Promise.try(function () { throw new Error("sync"); }).then(
  function () { throw new Error("Promise.try must reject"); },
  function (error) { assert.strictEqual(error.message, "sync"); }
);

const deferred = Promise.withResolvers();
assert.strictEqual(typeof deferred.resolve, "function");
deferred.resolve("ok");
assert.strictEqual(typeof deferred.promise.then, "function");

assert.strictEqual(typeof Symbol.dispose, "symbol");
assert.strictEqual(typeof Symbol.asyncDispose, "symbol");
assert.strictEqual(Math.sumPrecise(new Set([1, 2, 3])), 6);
assert.ok(Math.abs(Math.sumPrecise([0.1, 0.2, 0.3]) - 0.6) < 1e-15, "compensated summation");
assert.deepStrictEqual(Array.from(Uint8Array.fromBase64("aGk=")), [104, 105]);
assert.strictEqual(Object.hasOwn({ a: 1 }, "a"), true);
assert.strictEqual(Object.hasOwn({ a: 1 }, "b"), false);
assert.strictEqual([1, 2, 3].findLast(function (value) { return value < 3; }), 2);
assert.strictEqual([1, 2, 3].findLastIndex(function (value) { return value < 3; }), 1);

console.log(
  "compat shim ok: " + paths.length + " watched APIs, " + stripped + " stripped and revived"
);
"#;

#[test]
fn the_compat_layer_revives_an_engine_without_those_apis() {
    let script = window::compat_script(
        &window::SHIMMED_APIS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect::<Vec<_>>(),
    );
    let paths = stripped_paths()
        .iter()
        .map(|path| format!("\"{path}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let driver = DRIVER.replace("%PATHS%", &format!("[{paths}]"));
    let stdout = run_in_node("dsh-desktop-compat-shim-test", &driver, &script, None);
    println!("{}", stdout.trim());
}

/// Runs the whole splash page — head and body — against a minimal DOM.
///
/// The status page's behaviour is DOM work, and the Rust-side tests can only assert that certain
/// strings appear in the file. That is not the same claim: an index into the wrong buffer slot
/// renders an empty panel while every string is present (found exactly that way, 2026-09-16).
/// Node has no DOM, so the driver supplies the smallest one the page actually uses and records
/// what the page does with it.
const CHOICE_DRIVER: &str = r#"
const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

// Every element the page touches, with just enough behaviour to observe the result.
const makeElement = function (tag) {
  const element = {
    tagName: tag.toUpperCase(),
    children: [],
    hidden: false,
    disabled: false,
    listeners: {},
    _text: "",
    appendChild(child) { this.children.push(child); return child; },
    removeChild(child) { this.children = this.children.filter((c) => c !== child); return child; },
    addEventListener(name, fn) { (this.listeners[name] = this.listeners[name] || []).push(fn); },
    click() { (this.listeners.click || []).forEach((fn) => fn.call(this)); },
    querySelectorAll() {
      const found = [];
      const walk = (node) => { if (node.tagName === "BUTTON") found.push(node); node.children.forEach(walk); };
      this.children.forEach(walk);
      return found;
    },
    classList: { toggle() {} }
  };
  Object.defineProperty(element, "firstChild", { get() { return this.children[0] || null; } });
  Object.defineProperty(element, "textContent", {
    get() { return this._text; },
    set(value) { this._text = String(value); this.children = []; }
  });
  return element;
};

const byId = {};
const sent = [];
globalThis.document = {
  body: makeElement("body"),
  getElementById(id) { return (byId[id] = byId[id] || makeElement("div")); },
  createElement(tag) { return makeElement(tag); }
};
globalThis.window = globalThis;
globalThis.__TAURI_INTERNALS__ = {
  invoke(command, args) { sent.push({ command: command, args: args }); return Promise.resolve(); }
};

// The shipped page, in full: every script block runs, in document order.
// argv: node, driver.js, script, <extra arg>, page — the extra arg is the event name here.
const page = fs.readFileSync(process.argv[4], "utf8");
const bodies = [...page.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
assert.ok(bodies.length >= 4, "the page must ship its head and body scripts");
bodies.forEach((body) => vm.runInThisContext(body, { filename: "page.js" }));

// Rust asks the question, exactly as `window::choice_script` does. Four options is the full
// case: the port choice is offered only when a free port was found.
const options = [
  { id: "take-over", label: "终止 pid 4242 并接管端口 3080" },
  { id: "browser", label: "保留它，用系统浏览器打开" },
  { id: "port", label: "保留它，本应用改用端口 3091" },
  { id: "cancel", label: "什么都不做，退出本应用" }
];
globalThis.__askChoice(7, "检测到其它 Harness", "进程: pid 4242\nworkspace: /Users/me/project", options, "120 秒内没有选择将按配置处理。");

const panel = byId.choice;
assert.strictEqual(panel.hidden, false, "the question must show the panel");
assert.strictEqual(byId.status.textContent, "检测到其它 Harness");
assert.ok(byId.detail.textContent.indexOf("pid 4242") >= 0, "the detail must carry the process");
assert.strictEqual(byId.retry.hidden, true, "the restart button must not race the answer");

const buttons = panel.querySelectorAll();
assert.strictEqual(buttons.length, options.length, "one button per option, no more");
assert.strictEqual(buttons[0].textContent, options[0].label);
assert.strictEqual(buttons[1].textContent, options[1].label);
assert.strictEqual(buttons[2].textContent, options[2].label);
assert.strictEqual(buttons[3].textContent, options[3].label);
assert.ok(panel.children.length > options.length, "the timeout hint is shown as well");

// The third button is the one that changes the port; clicking it must report its own id, not
// a neighbour index.
buttons[2].click();
assert.strictEqual(sent.length, 1, "one click, one answer");
assert.strictEqual(sent[0].command, "plugin:event|emit");
assert.strictEqual(sent[0].args.event, process.argv[3]);
assert.strictEqual(sent[0].args.payload.question, 7, "the answer must name its question");
assert.strictEqual(sent[0].args.payload.id, "port");
assert.ok(buttons.every((b) => b.disabled), "a second click must not answer twice");
assert.strictEqual(buttons[2].textContent, "已选择：" + options[2].label);

// Clearing the question takes the panel away again.
globalThis.__askChoice(8, "正在启动 Harness…", "", [], "");
assert.strictEqual(panel.hidden, true, "an empty question hides the panel");
console.log("choice panel ok: " + buttons.length + " options, answer " + sent[0].args.payload.id);
"#;

#[test]
fn the_takeover_panel_renders_its_options_and_reports_the_click() {
    let stdout = run_in_node_with_page(
        "dsh-desktop-choice-panel-test",
        CHOICE_DRIVER,
        "",
        Some(window::CHOICE_EVENT),
    );
    println!("{}", stdout.trim());
}

/// The other half of the contract: the shell only installs what the probe noticed.
#[test]
fn the_probe_reports_what_an_old_engine_lacks() {
    let stdout = run_in_node(
        "dsh-desktop-compat-probe-test",
        PROBE_DRIVER,
        &window::probe_script(),
        Some(window::PROBE_EVENT),
    );
    println!("{}", stdout.trim());
}
