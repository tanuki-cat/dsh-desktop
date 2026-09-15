//! Splash window, Harness window, and navigation policy.

use crate::harness;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::webview::{DownloadEvent, NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

pub const SPLASH: &str = "splash";
pub const HARNESS: &str = "harness";

/// The event the splash page uses to report what this WebView can actually run.
pub const PROBE_EVENT: &str = "dsh-desktop:webview-probe";

/// The event the status page uses to ask for another attempt at starting the Harness.
pub const RESTART_EVENT: &str = "dsh-desktop:restart-harness";

/// APIs the compat layer can install when this WebView lacks them (see [`compat_script`]).
///
/// `Iterator` is the one that actually happened (2026-09-14, an Intel Mac): the bundled
/// document-preview plugin evaluates `Iterator.prototype.join` without checking that the global
/// exists, so a WebView whose JavaScriptCore predates Safari 18.4 throws during `import` and the
/// window ends up showing the harness's opaque "Failed to load plugins" page. The `Iterator`
/// global arrived in Safari 18.4 — macOS 15.4, or the Safari 18.4 update for macOS 13/14 — which
/// is far newer than the macOS versions this bundle still allows (11.0).
///
/// The rest are the same class of gap, found by grepping every shipped client bundle for the
/// APIs its build targets. Each entry is the label the probe reports and the expression whose
/// absence it tests: prototype methods are not globals, so that check is `[].findLast`.
pub const SHIMMED_APIS: &[(&str, &str)] = &[
    ("Iterator", "Iterator"),
    ("Promise.try", "Promise.try"),
    ("Promise.withResolvers", "Promise.withResolvers"),
    ("Symbol.dispose", "Symbol.dispose"),
    ("Math.sumPrecise", "Math.sumPrecise"),
    ("Uint8Array.fromBase64", "Uint8Array.fromBase64"),
    ("Object.hasOwn", "Object.hasOwn"),
    ("findLast", "[].findLast"),
];

/// APIs reported for diagnostics only: absent costs a feature, and neither the shim nor anything
/// else here can implement them faithfully.
const WATCHED_APIS: &[(&str, &str)] = &[("structuredClone", "structuredClone")];

/// Syntax the client bundles use that no polyfill can add: the module has to parse.
///
/// `new Function` is the only way to ask an engine whether it can parse something, and the splash
/// page has no CSP (`csp: null` in `tauri.conf.json`), so it works. `class static block` (Safari
/// 16.4, macOS 13.3) is the newest syntax the bundles use, which is what makes the compat layer's
/// floor 13.3 rather than the 10.15 the bundle declares; newer syntax goes in this list.
const REQUIRED_SYNTAX: &[(&str, &str)] = &[("class static block", "class Probe { static { 1; } }")];

/// What the splash page found missing in this WebView.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct WebviewReport {
    /// APIs from [`SHIMMED_APIS`] this engine lacks: the compat layer installs them.
    #[serde(default)]
    pub missing: Vec<String>,
    /// APIs whose absence only costs a feature (see [`WATCHED_APIS`]).
    #[serde(default)]
    pub degraded: Vec<String>,
    /// Syntax this engine cannot parse (see [`REQUIRED_SYNTAX`]).
    #[serde(default)]
    pub syntax: Vec<String>,
    /// `navigator.userAgent`, for the log and the failure page.
    #[serde(default)]
    pub agent: String,
}

impl WebviewReport {
    /// Parse what the page sent. A payload this shell cannot read is not a reason to refuse to
    /// start — it only costs the diagnostics.
    pub fn parse(payload: &str) -> Option<WebviewReport> {
        serde_json::from_str(payload).ok()
    }

    /// Missing APIs no compat block covers.
    ///
    /// Empty by construction — the probe reports exactly [`SHIMMED_APIS`] as `missing` — so this
    /// only matters the day someone watches an API here without writing a shim: the shell then
    /// sends the user to the browser instead of into a page that breaks later.
    pub fn uncovered(&self) -> Vec<String> {
        self.missing
            .iter()
            .filter(|api| !SHIMMED_APIS.iter().any(|(name, _)| name == api))
            .cloned()
            .collect()
    }

    /// Everything this engine lacks, as the list the user sees.
    ///
    /// `compat` is false when the shell was told not to install the compat layer
    /// (`webkit_compat: false`), which turns the shimmable gaps into real ones.
    pub fn gaps(&self, compat: bool) -> Vec<String> {
        let mut gaps = self.syntax.clone();
        gaps.extend(self.uncovered());
        if !compat {
            let uncovered: Vec<String> = self
                .missing
                .iter()
                .filter(|api| !gaps.contains(api))
                .cloned()
                .collect();
            gaps.extend(uncovered);
        }
        gaps
    }

    /// False when the dsh front end cannot load in this WebView, with or without the compat
    /// layer: syntax no shim can add, an API no block covers, or the layer switched off.
    pub fn supported(&self, compat: bool) -> bool {
        self.gaps(compat).is_empty()
    }

    /// True when the compat layer has something to install here.
    pub fn needs_compat(&self) -> bool {
        !self.missing.is_empty()
    }

    /// The text shown when the UI has to move to the system browser: this WebView cannot run
    /// it, but the browser can.
    ///
    /// That is the `dsh web` path these machines have always used: the shell still supervises
    /// the Harness (updates, stopping it on exit), the browser only renders the UI — with an
    /// engine that does keep getting updates.
    pub fn browser_fallback_detail(&self, dsh_version: &str, url: &str, compat: bool) -> String {
        format!(
            "{}\n\n界面已在默认浏览器中打开：\n{url}\n\n\
             这个窗口是 harness 的管理窗口（更新与退出清理都在这里）：在浏览器里操作时请不要关闭它，\
             关闭它会停止 harness。",
            self.describe(dsh_version, compat)
        )
    }

    /// The text of the failure page: what is missing, what this dsh version needs, what to do.
    ///
    /// The floor named here is Safari 16.4, not the 18.4 the pre-compat shell required: the
    /// compat layer covers everything above the one syntax item no shim can add.
    pub fn describe(&self, dsh_version: &str, compat: bool) -> String {
        let mut text = format!(
            "系统 WebView 缺少 dsh {dsh_version} 前端必需的能力：{}。\n\
             这个版本的界面需要 Safari 16.4（macOS 13.3）或更新的 WebKit，升级系统或安装 Safari 更新后重试。",
            self.gaps(compat).join("、")
        );
        if !self.degraded.is_empty() {
            text.push_str(&format!(
                "\n另外缺少（只影响部分功能）：{}。",
                self.degraded.join("、")
            ));
        }
        if !compat && self.needs_compat() {
            text.push_str(
                "\nconfig.json 里关闭了 webkit_compat，上面这些能力本可以由兼容层补上；需要原生窗口就打开它。",
            );
        }
        if !self.agent.is_empty() {
            text.push_str(&format!("\n当前 WebView：{}", self.agent));
        }
        text
    }
}

/// What the splash page reported, once.
///
/// `None` means "never reported" (the probe could not run, or the page never loaded). That is
/// treated as supported: refusing to start because a *diagnostic* is missing would be worse than
/// the bug it detects.
static REPORT: Mutex<Option<WebviewReport>> = Mutex::new(None);

/// How long to wait for that report. The splash page loads long before the Harness is up, so
/// only the fast path — reusing a live instance — can arrive here first.
const PROBE_WAIT: Duration = Duration::from_millis(500);

/// Store what the page reported (called from the event listener in `run`).
pub fn record_report(payload: &str) {
    let Some(report) = WebviewReport::parse(payload) else {
        harness::app_log(&format!("无法解析 WebView 能力探测的上报：{payload}"));
        return;
    };
    *REPORT.lock().unwrap() = Some(report);
}

/// The reason this WebView cannot host the dsh front end, when the page reported one.
///
/// `compat` is what `config.json` allows: with it off, an engine this shell could patch counts as
/// unsupported, so the UI moves to the browser instead of loading without its shim.
///
/// `None` means "no reason known" — either the report says the WebView is fine or the probe
/// never arrived (see [`REPORT`]).
pub fn unsupported_webview(compat: bool) -> Option<WebviewReport> {
    let deadline = Instant::now() + PROBE_WAIT;
    loop {
        if let Some(report) = REPORT.lock().unwrap().as_ref() {
            return (!report.supported(compat)).then(|| report.clone());
        }
        if Instant::now() >= deadline {
            harness::app_log("WebView 能力探测没有上报，按支持处理");
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The compat script the Harness window should carry, when the probe found something to install.
///
/// `None` on an engine that has everything, and on one whose probe never reported: a diagnostic
/// that failed must not put a patch into a window.
pub fn needed_compat_script() -> Option<String> {
    let report = REPORT.lock().unwrap().clone()?;
    report
        .needs_compat()
        .then(|| compat_script(&report.missing))
}

/// The last report the splash page sent, for the startup log and the status page.
pub fn report() -> Option<WebviewReport> {
    REPORT.lock().unwrap().clone()
}

/// The script that asks the page what this WebView can run.
///
/// It is injected into the splash window, which is our own page and the only one holding core
/// capabilities (`capabilities/splash.json`); the Harness window deliberately gets none.
pub fn probe_script() -> String {
    let mut checks = String::new();
    for (list, apis) in [("missing", SHIMMED_APIS), ("degraded", WATCHED_APIS)] {
        for (name, expression) in apis {
            checks.push_str(&format!(
                "  try {{ if (typeof {expression} === \"undefined\") {list}.push(\"{name}\"); }} catch (error) {{ {list}.push(\"{name}\"); }}\n"
            ));
        }
    }
    // Syntax cannot be feature-detected with `typeof`: compiling it is the only question an
    // engine answers truthfully, and a failed compile is exactly what a client bundle would hit.
    for (name, source) in REQUIRED_SYNTAX {
        let source = json!(source);
        checks.push_str(&format!(
            "  try {{ new Function({source}); }} catch (error) {{ syntax.push(\"{name}\"); }}\n"
        ));
    }
    PROBE_TEMPLATE
        .replace("%CHECKS%", &checks)
        .replace("%EVENT%", PROBE_EVENT)
}

/// The probe's JavaScript. ES5 on purpose: it has to run on the very engines this exists to
/// diagnose. `window.__TAURI_INTERNALS__` may not exist yet (Tauri installs its IPC bridge in
/// its own initialization script), so the report retries briefly and then gives up quietly.
const PROBE_TEMPLATE: &str = r#"
(function () {
  var missing = [];
  var degraded = [];
  var syntax = [];
%CHECKS%
  var payload = { missing: missing, degraded: degraded, syntax: syntax, agent: navigator.userAgent };
  function report(attempt) {
    try {
      if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
        window.__TAURI_INTERNALS__.invoke("plugin:event|emit", { event: "%EVENT%", payload: payload });
        return;
      }
    } catch (error) {
      // An old WebView can throw here; fall through to the retry, then give up quietly.
    }
    if (attempt < 40) setTimeout(function () { report(attempt + 1); }, 25);
  }
  report(0);
})();
"#;

/// One polyfill per [`SHIMMED_APIS`] entry, selected by what the probe found missing.
///
/// Each block is its own IIFE with its own guard, so a block is complete on its own and cannot
/// collide with another; an engine that gained the API between the probe and the page load is
/// left untouched.
const COMPAT_BLOCKS: &[(&str, &str)] = &[
    ("Iterator", ITERATOR_SHIM),
    ("Promise.try", PROMISE_TRY_SHIM),
    ("Promise.withResolvers", PROMISE_WITH_RESOLVERS_SHIM),
    ("Symbol.dispose", SYMBOL_DISPOSE_SHIM),
    ("Math.sumPrecise", MATH_SUM_PRECISE_SHIM),
    ("Uint8Array.fromBase64", UINT8_FROM_BASE64_SHIM),
    ("Object.hasOwn", OBJECT_HAS_OWN_SHIM),
    ("findLast", FIND_LAST_SHIM),
];

/// The compat layer for one report: only the blocks whose API the probe found missing.
///
/// ES5 on purpose, like the probe: this is the layer that has to work on the very engines the
/// probe refused.
pub fn compat_script(missing: &[String]) -> String {
    let mut script = String::new();
    for (name, block) in COMPAT_BLOCKS {
        if missing.iter().any(|api| api == name) {
            script.push_str(block);
            script.push('\n');
        }
    }
    script
}

/// A module-level pdf.js guard reads `Iterator.prototype.join` while the document-preview bundle
/// is imported, so the global has to exist before that module is evaluated. `%IteratorPrototype%`
/// is this engine's own iterator prototype — the object every array, string and map iterator
/// inherits from — so installing the helpers there makes them work on iterators the page already
/// creates, which is where the spec puts them anyway.
const ITERATOR_SHIM: &str = r#"
(function () {
  // globalThis is named explicitly: a "var Iterator" in this scope would shadow the global, and
  // the guard would then answer about the local binding instead of the engine.
  if (typeof globalThis.Iterator !== "undefined") return;
  var iteratorPrototype = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
  if (typeof iteratorPrototype[Symbol.iterator] !== "function") {
    Object.defineProperty(iteratorPrototype, Symbol.iterator, {
      value: function () { return this; },
      configurable: true
    });
  }
  var IteratorShim = function () {
    throw new TypeError("Iterator is abstract: use Iterator.from");
  };
  IteratorShim.prototype = iteratorPrototype;
  try {
    Object.defineProperty(iteratorPrototype, "constructor", { value: IteratorShim, writable: true, configurable: true });
  } catch (error) {}
  IteratorShim.from = function (value) {
    if (value === null || value === undefined) throw new TypeError("Iterator.from requires an object");
    if (typeof value.next === "function") return value;
    var method = value[Symbol.iterator];
    if (typeof method !== "function") throw new TypeError("value is not iterable");
    return method.call(value);
  };
  globalThis.Iterator = IteratorShim;
  var defineHelper = function (name, helper) {
    if (typeof iteratorPrototype[name] === "function") return;
    Object.defineProperty(iteratorPrototype, name, { value: helper, writable: true, configurable: true });
  };
  // A helper result is an iterator like any other: it inherits from %IteratorPrototype%, which is
  // what makes chaining (and "instanceof Iterator") work, and that prototype is iterable.
  var wrap = function (next) {
    var result = Object.create(iteratorPrototype);
    result.next = next;
    return result;
  };
  defineHelper("map", function (mapper, thisArg) {
    var source = this, index = 0;
    return wrap(function () {
      var step = source.next();
      if (step.done) return step;
      return { value: mapper.call(thisArg, step.value, index++), done: false };
    });
  });
  defineHelper("filter", function (predicate, thisArg) {
    var source = this, index = 0;
    return wrap(function () {
      for (;;) {
        var step = source.next();
        if (step.done) return step;
        if (predicate.call(thisArg, step.value, index++)) return { value: step.value, done: false };
      }
    });
  });
  defineHelper("take", function (limit) {
    var source = this, left = Number(limit);
    return wrap(function () {
      if (!(left > 0)) return { value: undefined, done: true };
      left -= 1;
      var step = source.next();
      if (step.done) left = 0;
      return step;
    });
  });
  defineHelper("drop", function (limit) {
    var source = this, left = Number(limit), started = false;
    return wrap(function () {
      if (!started) {
        started = true;
        while (left > 0) {
          left -= 1;
          if (source.next().done) return { value: undefined, done: true };
        }
      }
      return source.next();
    });
  });
  defineHelper("flatMap", function (mapper, thisArg) {
    var source = this, inner = null, index = 0;
    return wrap(function () {
      for (;;) {
        if (inner !== null) {
          var innerStep = inner.next();
          if (!innerStep.done) return { value: innerStep.value, done: false };
          inner = null;
        }
        var step = source.next();
        if (step.done) return step;
        inner = mapper.call(thisArg, step.value, index++)[Symbol.iterator]();
      }
    });
  });
  defineHelper("reduce", function (reducer) {
    var source = this, index = 0, accumulator, started = arguments.length > 1;
    if (started) accumulator = arguments[1];
    for (;;) {
      var step = source.next();
      if (step.done) break;
      if (!started) {
        accumulator = step.value;
        started = true;
        continue;
      }
      accumulator = reducer(accumulator, step.value, index);
      index += 1;
    }
    if (!started) throw new TypeError("reduce of an empty iterator with no initial value");
    return accumulator;
  });
  defineHelper("toArray", function () {
    var source = this, values = [];
    for (;;) {
      var step = source.next();
      if (step.done) return values;
      values.push(step.value);
    }
  });
  defineHelper("forEach", function (callback, thisArg) {
    var source = this, index = 0;
    for (;;) {
      var step = source.next();
      if (step.done) return undefined;
      callback.call(thisArg, step.value, index++);
    }
  });
  defineHelper("some", function (predicate, thisArg) {
    var source = this, index = 0;
    for (;;) {
      var step = source.next();
      if (step.done) return false;
      if (predicate.call(thisArg, step.value, index++)) return true;
    }
  });
  defineHelper("every", function (predicate, thisArg) {
    var source = this, index = 0;
    for (;;) {
      var step = source.next();
      if (step.done) return true;
      if (!predicate.call(thisArg, step.value, index++)) return false;
    }
  });
  defineHelper("find", function (predicate, thisArg) {
    var source = this, index = 0;
    for (;;) {
      var step = source.next();
      if (step.done) return undefined;
      if (predicate.call(thisArg, step.value, index++)) return step.value;
    }
  });
  // Not a standard helper, but the document-preview bundle installs exactly this one, so a shim
  // that got there first must behave the same: spread the iterator, join the array.
  defineHelper("join", function (separator) {
    return this.toArray().join(separator);
  });
})();
"#;

/// `Promise.try` (Safari 18.2): call the function now, settle the promise with what it returned
/// or threw — the synchronous-throw half is what the PDF stream paths rely on.
const PROMISE_TRY_SHIM: &str = r#"
(function () {
  if (typeof Promise.try === "function") return;
  Promise.try = function (callback) {
    var args = Array.prototype.slice.call(arguments, 1);
    return new Promise(function (resolve) { resolve(callback.apply(undefined, args)); });
  };
})();
"#;

/// `Promise.withResolvers` (Safari 17.4): the deferred this code base builds by hand everywhere.
const PROMISE_WITH_RESOLVERS_SHIM: &str = r#"
(function () {
  if (typeof Promise.withResolvers === "function") return;
  Promise.withResolvers = function () {
    var resolve, reject;
    var promise = new Promise(function (res, rej) { resolve = res; reject = rej; });
    return { promise: promise, resolve: resolve, reject: reject };
  };
})();
"#;

/// `Symbol.dispose` / `Symbol.asyncDispose` (Safari 18.2): the conversation UI stores these as
/// property keys, so an absent symbol silently loses the disposal instead of throwing.
const SYMBOL_DISPOSE_SHIM: &str = r#"
(function () {
  if (typeof Symbol.dispose === "undefined") Symbol.dispose = Symbol("Symbol.dispose");
  if (typeof Symbol.asyncDispose === "undefined") Symbol.asyncDispose = Symbol("Symbol.asyncDispose");
})();
"#;

/// `Math.sumPrecise`, which no Safari release ships: the shipped PDF writer calls it, so every
/// engine — old or new — is missing this one. Neumaier compensated summation is not the spec's
/// algorithm verbatim, but it is never worse than the naive sum it replaces.
const MATH_SUM_PRECISE_SHIM: &str = r#"
(function () {
  if (typeof Math.sumPrecise === "function") return;
  Math.sumPrecise = function (values) {
    var iterator = values[Symbol.iterator](), sum = 0, correction = 0, step;
    for (;;) {
      step = iterator.next();
      if (step.done) break;
      var value = Number(step.value);
      var next = sum + value;
      correction += Math.abs(sum) >= Math.abs(value) ? (sum - next) + value : (value - next) + sum;
      sum = next;
    }
    return sum + correction;
  };
})();
"#;

/// `Uint8Array.fromBase64` (Safari 18.2): `atob` plus bytes, with the url-safe alphabet accepted
/// because the one call site in the shipped bundles decides the alphabet and does not say.
const UINT8_FROM_BASE64_SHIM: &str = r#"
(function () {
  if (typeof Uint8Array.fromBase64 === "function") return;
  Uint8Array.fromBase64 = function (value) {
    var normalized = String(value).replace(/s/g, "").replace(/-/g, "+").replace(/_/g, "/");
    var binary = atob(normalized);
    var bytes = new Uint8Array(binary.length);
    for (var index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index) & 255;
    }
    return bytes;
  };
})();
"#;

/// `Object.hasOwn` (Safari 15.4).
const OBJECT_HAS_OWN_SHIM: &str = r#"
(function () {
  if (typeof Object.hasOwn === "function") return;
  Object.hasOwn = function (object, property) {
    if (object === null || object === undefined) throw new TypeError("Object.hasOwn requires an object");
    return Object.prototype.hasOwnProperty.call(Object(object), property);
  };
})();
"#;

/// `Array.prototype.findLast` / `findLastIndex` (Safari 15.4).
const FIND_LAST_SHIM: &str = r#"
(function () {
  if (typeof [].findLast !== "function") {
    Object.defineProperty(Array.prototype, "findLast", {
      value: function (predicate, thisArg) {
        if (this === null || this === undefined) throw new TypeError("findLast requires an array");
        for (var index = this.length - 1; index >= 0; index -= 1) {
          var value = this[index];
          if (predicate.call(thisArg, value, index, this)) return value;
        }
        return undefined;
      },
      writable: true,
      configurable: true
    });
  }
  if (typeof [].findLastIndex !== "function") {
    Object.defineProperty(Array.prototype, "findLastIndex", {
      value: function (predicate, thisArg) {
        if (this === null || this === undefined) throw new TypeError("findLastIndex requires an array");
        for (var index = this.length - 1; index >= 0; index -= 1) {
          if (predicate.call(thisArg, this[index], index, this)) return index;
        }
        return -1;
      },
      writable: true,
      configurable: true
    });
  }
})();
"#;

pub fn create_splash(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, SPLASH, WebviewUrl::App("index.html".into()))
        .title("DeepSeek Harness")
        .inner_size(460.0, 300.0)
        .resizable(false)
        // Ask this WebView what it can run before anything is spawned: an engine older than
        // Safari 18.4 cannot load the current dsh front end at all (see `WebviewReport`).
        .initialization_script(probe_script())
        .build()?;
    // Tauri keeps the app alive when its last window closes, so a status window the user
    // dismisses (typically after a failed start) would leave a windowless app behind. Only
    // then may closing it end the process: the app removes this window itself once the
    // Harness window is up (via destroy), and that must not look like the user quitting.
    let handle = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            if handle.get_webview_window(HARNESS).is_none() {
                handle.exit(0);
            }
        }
    });
    Ok(())
}

/// Set by the first terminal page. A second one (the load watcher giving up after the watchdog
/// already reported the exit) must not replace the explanation the user is reading.
///
/// Cleared by [`allow_next_failure`] whenever the shell starts another attempt, so a retry that
/// fails again can replace the page that asked for it.
static FAILURE_SHOWN: AtomicBool = AtomicBool::new(false);

/// True while the page on screen is the failure page — the one that offers a restart.
///
/// Distinct from [`FAILURE_SHOWN`] on purpose: the browser fallback ([show_notice]) is terminal
/// for the same reason (nothing may overwrite its explanation) but must not be answered with a
/// restart, since its Harness is alive and supervised.
static RETRY_OFFERED: AtomicBool = AtomicBool::new(false);

/// True when "start the Harness again" is what the user is asking for by reopening the app.
pub fn retry_offered() -> bool {
    RETRY_OFFERED.load(Ordering::SeqCst)
}

/// Let the next terminal page replace whatever is on screen. Called when the shell starts a
/// recovery, or a restart the user asked for.
pub fn allow_next_failure() {
    FAILURE_SHOWN.store(false, Ordering::SeqCst);
}

/// Put a status page in front of the user for something they must see, such as a Harness that
/// died after a successful start. Rebuilds the splash when it was already closed, and offers the
/// restart button: the shell can start another Harness in this very process.
pub fn show_failure(app: &AppHandle, status: &str, detail: &str) {
    if FAILURE_SHOWN.swap(true, Ordering::SeqCst) {
        harness::app_log(&format!(
            "failure page already shown; keeping it instead of: {status}"
        ));
        return;
    }
    if present_status_page(app) {
        RETRY_OFFERED.store(true, Ordering::SeqCst);
        eval_status(app, "__setFailure", status, detail);
    }
}

/// The same page without the restart offer.
///
/// Used where another Harness must not be started: the old-WebView fallback keeps supervising
/// the running instance and hands only the rendering to the browser, so a restart button there
/// would kill a working Harness and open a second browser tab.
pub fn show_notice(app: &AppHandle, status: &str, detail: &str) {
    if FAILURE_SHOWN.swap(true, Ordering::SeqCst) {
        harness::app_log(&format!(
            "failure page already shown; keeping it instead of: {status}"
        ));
        return;
    }
    if present_status_page(app) {
        RETRY_OFFERED.store(false, Ordering::SeqCst);
        eval_status(app, "__setStatus", status, detail);
    }
}

/// Bring the status page back while the shell itself is still working on the Harness.
///
/// Not latched, and without the restart button: this is the "an attempt is running" face of the
/// same page. A success removes it again through `create_harness`; a failure replaces it through
/// [`show_failure`].
pub fn show_progress(app: &AppHandle, status: &str, detail: &str) {
    if present_status_page(app) {
        RETRY_OFFERED.store(false, Ordering::SeqCst);
        eval_status(app, "__setStatus", status, detail);
    }
}

/// Rebuild the splash when it was already closed, with no Harness window left in front of it.
///
/// Every caller reaches this with a Harness window that is dead or unusable, and the status page
/// tells the user that closing it quits the app. Destroy (not close: that would look like a user
/// quit) the Harness window so that sentence is true — the splash only exits the app while no
/// Harness window is left.
fn present_status_page(app: &AppHandle) -> bool {
    if let Some(harness_window) = app.get_webview_window(HARNESS) {
        let _ = harness_window.destroy();
    }
    if app.get_webview_window(SPLASH).is_none() {
        if let Err(error) = create_splash(app) {
            harness::app_log(&format!("could not reopen the status window: {error}"));
            return false;
        }
    }
    true
}

/// Update the splash without granting the page any Tauri permission (Rust-side eval).
pub fn set_status(app: &AppHandle, status: &str, detail: &str) {
    eval_status(app, "__setStatus", status, detail);
}

/// Call one of the page's two status entry points: `__setStatus` for progress, `__setFailure`
/// for the terminal page that carries the restart button.
///
/// A page this call just created may not have parsed its `<head>` yet, and that is where both
/// entry points are defined — an eval landing before them would simply be lost. The retry costs
/// nothing when the page is ready (the first call succeeds) and is the difference between the
/// user seeing the page and it staying on "正在启动…".
fn eval_status(app: &AppHandle, function: &str, status: &str, detail: &str) {
    if let Some(window) = app.get_webview_window(SPLASH) {
        let _ = window.eval(status_script(function, status, detail));
    }
}

/// The script `eval_status` sends, kept out of the window call so the escaping is testable.
fn status_script(function: &str, status: &str, detail: &str) -> String {
    let status = json!(status);
    let detail = json!(detail);
    format!(
        "(function () {{\n\
         var apply = function () {{\n\
         if (!window.{function}) return false;\n\
         window.{function}({status}, {detail});\n\
         return true;\n\
         }};\n\
         if (apply()) return;\n\
         var attempts = 0;\n\
         var timer = setInterval(function () {{\n\
         if (apply() || ++attempts > 40) clearInterval(timer);\n\
         }}, 25);\n\
         }})()"
    )
}

/// Wait this long for the first load to report "finished". The port already answered a probe
/// before the window was built, so a healthy load completes well inside it.
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// Navigations of the same startup URL, including the first one.
const LOAD_ATTEMPTS: u32 = 3;

/// What the webview reported about the navigations this shell started.
///
/// The two events answer different questions, which is why both are tracked: no event at all
/// means the navigation never began, while a start without a finish means a load is genuinely
/// running and must not be restarted.
#[derive(Default)]
struct LoadSignals {
    started: AtomicBool,
    finished: AtomicBool,
}

/// The Harness page is remote content: it gets no capability, and navigation is fenced
/// to the current loopback authority. Everything else opens in the system browser.
///
/// `compat` is the legacy-WebKit shim from [`needed_compat_script`], when this engine needs it.
/// It is injected by the shell into the webview rather than by the page, so the remote document
/// still receives nothing it could call back with.
pub fn create_harness(
    app: &AppHandle,
    url: &Url,
    port: u16,
    compat: Option<&str>,
) -> tauri::Result<()> {
    // `destroy` rather than `close`: close fires the window listeners (and the Harness window
    // ends the app on a user close), which must stay a user-only signal.
    if let Some(existing) = app.get_webview_window(HARNESS) {
        let _ = existing.destroy();
    }
    // Set as the page reports its progress. A navigation the webview drops reports nothing at
    // all, which is what the retry thread below watches for.
    let signals = Arc::new(LoadSignals::default());
    let load_signals = signals.clone();
    let mut builder = WebviewWindowBuilder::new(app, HARNESS, WebviewUrl::External(url.clone()))
        .title("DeepSeek Harness")
        .inner_size(1440.0, 960.0)
        .min_inner_size(900.0, 600.0)
        .on_navigation(move |target| {
            let same_origin = target.scheme() == "http"
                && target.host_str() == Some("127.0.0.1")
                && target.port() == Some(port);
            if !same_origin {
                harness::app_log(&format!("navigation blocked, opening externally: {target}"));
                open_external(target.as_str());
            }
            same_origin
        })
        // window.open / target=_blank: never spawn a Tauri window for remote content.
        .on_new_window(move |target, _features| {
            harness::app_log(&format!("new window request, opening externally: {target}"));
            open_external(target.as_str());
            NewWindowResponse::Deny
        })
        // The only evidence of what the first navigation did. `PageLoadEvent` is
        // non-exhaustive, so each variant is matched on its own.
        .on_page_load(move |_window, payload| {
            let event = payload.event();
            if matches!(event, PageLoadEvent::Started) {
                load_signals.started.store(true, Ordering::SeqCst);
            }
            if matches!(event, PageLoadEvent::Finished) {
                load_signals.finished.store(true, Ordering::SeqCst);
            }
        })
        // Downloads land in the user's Downloads folder instead of vanishing.
        .on_download(move |_webview, event| {
            match event {
                DownloadEvent::Requested { url, destination } => {
                    *destination =
                        unique_download_path(&downloads_dir(), &file_name_of(destination));
                    harness::app_log(&format!(
                        "download started: {url} -> {}",
                        destination.display()
                    ));
                }
                DownloadEvent::Finished { url, path, .. } => {
                    let where_to = path
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unknown".into());
                    harness::app_log(&format!("download finished: {url} -> {where_to}"));
                }
                // The enum is #[non_exhaustive]: future variants must not break the build.
                _ => {}
            }
            true
        });
    if let Some(script) = compat {
        builder = builder.initialization_script(script);
    }
    let window = builder.build()?;

    // A first navigation that never renders used to leave a blank window with nothing in the
    // log. The token URL is safe to revisit (measured: it is not single-use), so retry it.
    let watcher = window.clone();
    let watcher_app = window.app_handle().clone();
    let target = url.clone();
    std::thread::spawn(move || watch_first_load(watcher_app, watcher, target, port, signals));

    // Closing the Harness window quits the app, which stops the supervised process.
    let handle = window.app_handle().clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            handle.exit(0);
        }
    });

    if let Some(splash) = app.get_webview_window(SPLASH) {
        // Same reason as above: removing the status window is not a user close.
        let _ = splash.destroy();
    }
    Ok(())
}

/// What to do after the page missed its load deadline.
#[derive(Debug, PartialEq, Eq)]
enum LoadOutcome {
    /// Stop watching: the window is gone, or a load genuinely began (its `Started` arrived), so
    /// restarting it would interrupt work in progress.
    Settled,
    /// The navigation never began: send it again.
    Retry,
    /// Attempts ran out with nothing serving the port: tell the user.
    Report,
}

/// Pure retry policy, kept out of the thread so the matrix is testable.
///
/// The discriminator is the `Started` event, not the port: a blank window whose webview never
/// received the navigation looks exactly like a healthy port from the outside, and that is the
/// case this watch exists for. A port that still answers is never turned into a failure page —
/// if an environment delivers no load events at all, the retry must not escalate into an error
/// over a working app.
fn load_outcome(
    attempt: u32,
    attempts: u32,
    window_alive: bool,
    started: bool,
    port_serving: bool,
) -> LoadOutcome {
    if !window_alive || started {
        return LoadOutcome::Settled;
    }
    if attempt < attempts {
        return LoadOutcome::Retry;
    }
    if port_serving {
        LoadOutcome::Settled
    } else {
        LoadOutcome::Report
    }
}

/// Wait for the first successful render, re-navigating the startup URL when it never comes.
///
/// Design §4 steps 8/9: a failed load may retry the same token URL. The retry covers the case
/// where the webview never got as far as starting the navigation; a load that started is left
/// alone, and a page that never renders while the port is healthy is reported in the log only.
fn watch_first_load(
    app: AppHandle,
    window: WebviewWindow,
    url: Url,
    port: u16,
    signals: Arc<LoadSignals>,
) {
    for attempt in 1..=LOAD_ATTEMPTS {
        if wait_for_load(&signals, LOAD_TIMEOUT) {
            return;
        }
        let window_alive = app.get_webview_window(HARNESS).is_some();
        let started = signals.started.load(Ordering::SeqCst);
        let serving = port_serving(port);
        match load_outcome(attempt, LOAD_ATTEMPTS, window_alive, started, serving) {
            LoadOutcome::Settled => {
                if window_alive {
                    harness::app_log(&format!(
                        "page load did not finish within {LOAD_TIMEOUT:?} (started={started}, port {port} serving={serving}); leaving the window alone"
                    ));
                }
                return;
            }
            LoadOutcome::Retry => {
                harness::app_log(&format!(
                    "navigation never started, retrying {url} (attempt {}/{LOAD_ATTEMPTS})",
                    attempt + 1
                ));
                if let Err(error) = window.navigate(url.clone()) {
                    harness::app_log(&format!("retry navigation failed: {error}"));
                }
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
            }
            LoadOutcome::Report => {
                harness::app_log(&format!(
                    "navigation never started and port {port} stopped serving after {LOAD_ATTEMPTS} attempts: {url}"
                ));
                show_failure(
                    &app,
                    "Harness 页面加载失败",
                    &format!(
                        "窗口连续 {LOAD_ATTEMPTS} 次都没有加载出 {url}，端口 {port} 也没有在提供服务。\n\n关闭本窗口即退出应用；重新启动会重新拉起 Harness 并换一个新的启动 URL。"
                    ),
                );
                return;
            }
        }
    }
}

/// Poll the finished flag instead of blocking on a channel: the watcher also has to give up when
/// no event ever arrives, which a blocking receive cannot express.
fn wait_for_load(signals: &LoadSignals, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if signals.finished.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    signals.finished.load(Ordering::SeqCst)
}

/// True while the port answers as a Harness: the launch URL is only worth revisiting while the
/// process behind it is still there.
fn port_serving(port: u16) -> bool {
    matches!(
        harness::probe(port),
        harness::Probe::HarnessWithSession | harness::Probe::HarnessNoSession
    )
}

fn downloads_dir() -> PathBuf {
    let base = crate::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Downloads");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Never overwrite an existing download: `report.pdf` becomes `report-1.pdf` when taken.
fn unique_download_path(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let name = Path::new(name);
    let stem = name
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "dsh-download".to_string());
    let extension = name
        .extension()
        .map(|ext| ext.to_string_lossy().to_string());
    for index in 1..1000 {
        let file = match &extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = dir.join(file);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

fn file_name_of(suggested: &std::path::Path) -> std::ffi::OsString {
    suggested
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("dsh-download"))
}

/// Schemes the shell is willing to hand to the operating system.
///
/// The Harness page renders whatever the agent produced — model-authored links, fetched pages —
/// so every URL arriving here is attacker-influenced. `open` / `xdg-open` act on far more than
/// web pages: `file:` launches local applications on macOS, and any registered custom scheme
/// reaches its app handler. Only web navigation leaves the shell (design §7).
pub fn may_open(target: &Url) -> bool {
    matches!(target.scheme(), "http" | "https")
}

/// Hand a link to the system browser, or drop it with a log line.
pub fn open_external(target: &str) {
    let url = match Url::parse(target) {
        Ok(url) => url,
        Err(error) => {
            harness::app_log(&format!("external link dropped ({error}): {target}"));
            return;
        }
    };
    if !may_open(&url) {
        harness::app_log(&format!(
            "external scheme blocked: {} ({target})",
            url.scheme()
        ));
        return;
    }

    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "cmd";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    command.args(["/C", "start", ""]);
    let _ = command.arg(url.as_str()).spawn();
}

#[cfg(test)]
mod tests {
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

        let text =
            report.browser_fallback_detail("0.1.5-rc.2", "http://127.0.0.1:3080/?token=t", true);
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

    #[test]
    fn the_probe_asks_about_every_shimmed_api_and_the_syntax_floor() {
        let script = probe_script();
        // The one that actually failed in the field (see SHIMMED_APIS).
        assert!(script.contains("typeof Iterator === \"undefined\""));
        // Prototype methods are not globals: the check names the expression it asks about.
        assert!(script.contains("typeof [].findLast === \"undefined\""));
        assert!(script.contains("Math.sumPrecise"));
        // Syntax is compiled, not probed with typeof.
        assert!(script.contains("new Function(\"class Probe { static { 1; } }\")"));
        assert!(script.contains("syntax.push(\"class static block\")"));
        // It reports through the splash window's core capability.
        assert!(script.contains(PROBE_EVENT));
        assert!(script.contains("plugin:event|emit"));
        // It runs on exactly the engines it exists to diagnose: keep it ES5.
        assert!(!script.contains("=>"), "the probe must stay ES5");
        assert!(!script.contains('`'), "the probe must stay ES5");
        assert!(!script.contains("??"), "the probe must stay ES5");
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
        let page = include_str!("../../src/index.html");
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
        let dir = std::env::temp_dir().join("dsh-desktop-download-test");
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
}
