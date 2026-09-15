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
    std::fs::write(&script_path, script).expect("generated script must be writable");
    std::fs::write(&driver_path, driver).expect("driver must be writable");

    let mut command = Command::new(&node);
    command.arg(&driver_path).arg(&script_path);
    if let Some(arg) = extra_arg {
        command.arg(arg);
    }
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

/// A probe run stands in for the splash page: `window.__TAURI_INTERNALS__` captures the report.
const PROBE_DRIVER: &str = r#"
const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

let captured = null;
globalThis.window = globalThis.window || {};
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

vm.runInThisContext(fs.readFileSync(process.argv[2], "utf8"), { filename: "probe.js" });

assert.ok(captured !== null, "the probe must report without retrying");
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
