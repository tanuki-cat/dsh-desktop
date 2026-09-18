# 修复 WebView 遮挡停画导致的「渲染中断」（实施方案）

审查/复现对象：`dsh-desktop` 本地 `main`（HEAD `c075eb5`）
实施日期：2026-09-18
触发：用户报告「还是存在渲染中断问题」，附中断现场与重启后两张截图
上一轮：[`dsh-desktop-v0.4.4-freeze-review.md`](./dsh-desktop-v0.4.4-freeze-review.md)（结论：把停画归因到「窗口失去焦点」，本轮证明该归因不完整）

后续：[`design-task-fix-raf-fallback-blinds-liveness-probe.md`](./design-task-fix-raf-fallback-blinds-liveness-probe.md)（2026-09-18 审查：本文根因成立，但方案 A 的实现顶起了壳自己的帧探测，方案 B 因此失效）

本文是**根因 + 实施方案**。落地后把结果回写到文末「处理状态」，不改写各条原始结论。

---

## 1. 结论（先看这里）

**根因**：DSH 窗口被其它窗口**遮挡**（不是「失去焦点」）时，WebKit 停止调度 `requestAnimationFrame`，而 `setTimeout` 照常运行。
Harness 前端的流式输出**全部合并到 rAF 上**（`dsh-api-session-controller` 用 rAF 调度发布、`dsh-client-ui-conversation` 三连 rAF 才 flush），
所以宿主进程一直在产出、会话一直完好，**屏幕上却停在某一帧不再更新**。用户看到的就是「渲染中断」，退出重启后新文档重新调度，于是又正常。

**为什么上一轮没修好**：上一轮的判据用的是 `attended()`（可见 + 未最小化 + **有焦点**），
而实测真正的闸门是**遮挡/可见性**（`document.visibilityState`）。两者不等价：

| 状态 | `attended()` | 实测 rAF |
|---|---|---|
| 可见 + 有焦点 | true | 跑 |
| **可见 + 无焦点**（焦点在 IDE，最常见的形态） | **false** | **照常跑** |
| **被别的窗口挡住** | **true** | **停** |

于是判据在「可见但无焦点」时误报暂停（对着一个还在画的页面喊停），在真正停画的「被遮挡」时又判成「有人看着」而照常重载/沉默。

**修法**：A 让页面在被遮挡时也能画（定时器兜底 rAF），B 把判据换成页面自己的可见性并让日志说真话。两者一起上。

---

## 2. 证据链

### 2.1 现场：宿主在产出，只有 WebView 停在旧帧

用户两张截图（Snipaste 文件名给出拍摄时刻：图1 `14-44-20`、图2 `14-45-49`，相隔 90 秒），OCR 后与会话日志对齐：

| | 界面最后画出的内容 | 它在会话里的时刻 | 会话里**已经存在**但界面没画出来的记录 |
|---|---|---|---|
| 图1（中断，14:44:20 拍摄） | `Find submenu id constants in menu.rs` + 推理句 `Suspended + attended -> this is a genuine fault…` | 14:37:18（工具行）/ 14:38:07（推理句） | 14:38:11 → 14:43:23 的 `assistant/message`、`tool/call`、`tool/result` |
| 图2（14:45:49 拍摄） | `Read logger test expectations that pin format` | 14:45:33 | 无（已追平） |

即：**界面停在 14:37–14:38 的那一帧，而宿主在 14:43:23 之前一直在产出**（14:38:11、14:38:15、14:38:57、14:42:09、14:42:19、14:43:19、14:43:23 都有记录），
两图相隔 90 秒，图2 才把 14:45:33 的内容补上。这是「页面不再被绘制」的签名，不是会话卡死 —— 会话在宿主进程里，与页面是否绘制无关。

> 待确认：图2 窗口尺寸 1561×871、标题栏带 `New Tab`，与图1（1440×959，应用窗口）不同，疑似系统浏览器。
> 若是浏览器，说明该次「恢复」走的是浏览器渲染路径；不影响本方案结论，但值得单独记一笔。

### 2.2 壳自己的日志：看到了，但什么都不做

`~/Library/Application Support/com.deepseek.dsh.desktop/logs/harness.log`

修复前（`b8ab867` 构建，14:44 那次现场）连续 3617 行：

```
[dsh-desktop] 窗口不在前台，WebKit 可能已停止绘制，本轮不判定
```

该分支只在「探测**有应答**、帧计数不动、窗口无焦点」时进入 —— 它看见了停画，但被设计成不作为。

修复后（`6384402` 起，今天 16:52–17:25）仍在复现，共 5 次、每次 1–4 分钟：

```
[16:57:03] Harness 页面停止绘制：计时器仍在运行，说明是被 WebKit 暂停绘制（多为窗口失去焦点）而非卡死；点击或聚焦窗口即可恢复，不重新加载
[16:57:39] Harness 页面在窗口重新获得焦点后仍未恢复绘制，按未响应继续处理
[16:57:54] WebView 页面恢复绘制（此前 1 次未通过检查、0 次重载）
```

注意第二行：**用户已经聚焦了窗口，帧仍然不动**。这一条直接证伪了「聚焦即可恢复」的假设。

### 2.3 本机 WKWebView 实测：闸门是遮挡，不是焦点

用最小 WKWebView 复现（脚本见 `/tmp/dsh-imgs/*.swift`，可重跑）：

- **设置确实生效**：`inactiveSchedulingPolicy` 读回 `2`（`WKInactiveSchedulingPolicyNone`），wry 0.55.1 确实把它写进 `WKPreferences`（`wry-0.55.1/src/wkwebview/mod.rs:491`，值定义见 `objc2-web-kit-0.3.2/src/generated/WKPreferences.rs:17-24`）。`background_throttling(Disabled)` 不是没生效，是**管不着这一条路径**。
- **遮挡即停画**：把探针窗口用另一个窗口盖住，帧计数立刻冻结、`setTimeout` 继续递增；移开遮挡物，帧计数立刻恢复。
- **同步信号**：同一时刻 `document.visibilityState` 由 `visible` 翻成 `hidden`；恢复遮挡、最小化、隐藏 App 三种情况**完全同步**。
- **可见但无焦点照常跑**：`appActive=false, occl=true, hf=false` 持续 180 s，帧计数 24 → 333 线性增长，从未停过。
- **遮挡时 `document.hasFocus()` 仍为 `true`**（`visibilityState` 已是 `hidden`）：页面自己的「有没有焦点」同样不是判据，`visibilityState` 才是。

（对照：`rafprobe none` / `rafprobe default` 两种策略在遮挡时都停，差别只在恢复速度 —— 该 KVC 控制的是**恢复**，不是**是否停**。）

### 2.4 前端确实挂在 rAF 上（代码核实）

- `@deepseek-ai/dsh-api-session-controller/lib/client.js:640`：`markFrameDirty()` → `schedule("frame")` → `requestAnimationFrame(publish)`。
- `@deepseek-ai/dsh-client-ui-conversation/lib/client.js:2601-2605`：`publish("animation-frame")` 连续三次 rAF 才 `flush()`。
- 输入路径走同步 flush（`notifyNow()`），所以「能打字、能发送、输出不动」可以同时成立。

### 2.5 已排除

- **不是双重编码那个 bug**：`~/Applications/DSH Desktop.app` 内二进制与 `src-tauri/target/release/dsh-desktop` sha256 相同（`9c903a72…`），即当前运行的是含 `c075eb5` 的构建。
- **不是崩溃/内存压力**：期间无新的 WebKit 崩溃报告（`~/Library/Logs/DiagnosticReports/Retired` 最新一份仍是 09-18 09:52）。
- **不是插件包裹 rAF**：`dsh-dream-skin`、`dsh-better-sidebar` 与 dsh 各 client bundle 都没有改写 `requestAnimationFrame`（grep `requestAnimationFrame *=` 无命中）。

---

## 3. 方案 A：渲染兜底（治标，让被遮挡时也画）

### 3.1 做法

在 Harness 窗口注入一段 ES5 垫片：**包裹** `requestAnimationFrame`，当页面处于 `hidden` 时给每次调用配一个定时器兜底 —— 原生帧在 `RENDER_FALLBACK_MS` 内没来，就用 `setTimeout` 顶一次，并取消挂着的原生帧；原生帧先到则清掉定时器。原生帧一恢复，垫片自动退回透明。

关键设计：**只在 `document.visibilityState === "hidden"` 时挂兜底定时器**。可见时零开销（不额外创建定时器），遮挡时以 `1 / RENDER_FALLBACK_MS` 的节奏继续推进页面自己的渲染管线。

### 3.2 放置位置与注入方式

- 新常量 `RENDER_FALLBACK_SCRIPT`（`src-tauri/src/window.rs`，紧邻 `COMPAT_BLOCKS` 与各 `*_SHIM`），保持 ES5、独立 IIFE、自带幂等守卫 —— 与兼容层同一套纪律。
- 注入：`create_harness`（`window.rs:1332`）里在建窗口时无条件 `builder.initialization_script(RENDER_FALLBACK_SCRIPT)`，与既有 `compat` 注入（`window.rs:1427-1429`）并列，**不依赖 splash 的能力探测**（探测回答的是「引擎缺什么 API」，与本垫片无关）。
- 开关：`Config` 新增 `render_fallback: bool`（默认 `true`），与 `webkit_compat`（`lib.rs:139-144`）对称，理由相同 —— 垫片若本身出问题，用户要有退路而不用改代码。
- 形参：`create_harness` 现有的 `compat: Option<&str>` 换成 `scripts: &[&str]`（理由见 §5）。`lib.rs` 两处调用点（`lib.rs:2080`、`lib.rs:2410`）把兼容层与兜底脚本按「兼容层在前、兜底在后」的顺序装进一个数组传进去。

### 3.3 脚本（已实测，实施时以它为准）

下面这份脚本在真 WebKit 与 node 里都跑过，结果见 §3.5：

```js
(function () {
  var w = window;
  if (typeof w.requestAnimationFrame !== "function") return;   // 旧引擎没有 rAF：页面本来就不用它
  if (w.__dshRafFallback === true) return;                      // 幂等：重复注入不叠加包裹
  w.__dshRafFallback = true;
  var nativeRaf = w.requestAnimationFrame;
  var nativeCancel = w.cancelAnimationFrame;
  var clock = (w.performance && typeof w.performance.now === "function")
    ? function () { return w.performance.now(); }
    : function () { return Date.now(); };
  var pending = {};                                             // id -> 兜底定时器
  w.requestAnimationFrame = function (callback) {
    var settled = false;
    var id = nativeRaf.call(w, function (stamp) {
      if (settled) return;                                      // 兜底已经投递过，迟到的原生帧必须丢弃
      settled = true;
      if (pending[id] !== undefined) { w.clearTimeout(pending[id]); delete pending[id]; }
      callback(stamp);
    });
    // 只有页面自己知道「现在不会被画」时才挂兜底：可见时一个定时器都不多建。
    if (w.document && w.document.visibilityState === "hidden" && typeof w.setTimeout === "function") {
      pending[id] = w.setTimeout(function () {
        if (settled) return;
        settled = true;
        delete pending[id];
        if (typeof nativeCancel === "function") nativeCancel.call(w, id);   // 原生帧已无意义
        callback(clock());
      }, w.__dshRafFallbackMs || 250);
    }
    return id;
  };
  var wrappedCancel = w.cancelAnimationFrame;
  w.cancelAnimationFrame = function (id) {
    if (pending[id] !== undefined) { w.clearTimeout(pending[id]); delete pending[id]; }
    if (typeof wrappedCancel === "function") wrappedCancel.call(w, id);
  };
})();
```

两个细节是实测钉出来的，不要「顺手简化」：

1. **时间戳走 `performance.now()`，回落 `Date.now()`**。rAF 回调拿到的时刻是页面动画的时间基准；用 `Date.now()`（epoch 毫秒）会让基于它的插值/差分类动画算出天文数字。
2. **`settled` 闩必须同时管住原生帧与兜底**。只清定时器不够：兜底投递之后原生帧仍可能到达，重复投递会让 React 的 `useEffect` 清理与页面动画各跑一遍。

### 3.4 实测依据（A/B）

同一页面、同一遮挡动作，只差垫片：

| | 遮挡期间帧计数（`__f`） | 露出后 |
|---|---|---|
| 无垫片 | 19 → 19（29 次采样全冻结） | 立刻恢复原生节奏 |
| 有垫片 | 20 → 25（约 4 fps，与 250 ms 兜底一致） | 立刻恢复原生节奏（31 → 48） |

### 3.5 注入方式与逻辑都已实测

**注入方式（真 WebKit A/B）**：把上面这段脚本按 `WKUserScript(injectionTime: .atDocumentStart)` 挂到 `WKWebViewConfiguration.userContentController` 上（即 wry `initialization_script` 的等价物），同一遮挡动作：

| | 遮挡期间帧计数 | 露出后 |
|---|---|---|
| 不注入 | 37 → 37（20 次采样全冻结） | 立刻恢复 |
| 注入 | 38 → 43（`hidden` 期间仍在推进） | 立刻恢复原生节奏 |

结论：文档开始前注入**确实生效**，且不会破坏原生路径。

**脚本逻辑（node 真跑）**：把同一份脚本喂给一个「原生帧永不到达、`setTimeout` 可手动推进」的假 window，断言六件事：

| 断言 | 实测结果 |
|---|---|
| hidden + 原生帧不来 ⇒ 兜底投递一次，且带时钟戳 | `[300]` ✓ |
| 迟到的原生帧不得二次投递同一回调 | 仍为 `[300]` ✓ |
| `cancelAnimationFrame` 能取消未触发的兜底 | `cancelledFired=false` ✓ |
| 可见时**一个定时器都不建** | `timersVisible=0` ✓ |
| 原生帧及时到达 ⇒ 只投递一次（兜底被清） | `1` ✓ |
| 重复注入不叠加包裹 | `sameWrapper=true` ✓ |

### 3.6 代价与风险

- **语义**：被遮挡时 rAF 从「每帧一次」变成「至少每 250 ms 一次」。被遮挡时本来就一次都没有，所以是净收益；代价只是这些帧的计算量（对一个看不见的窗口而言，这是把「内容追平」提前做掉，切回来不再需要追赶）。
- **叠加**：若页面自己包裹了 rAF，会串行包裹。已核实 dsh 前端与当前已装插件都没有包裹。
- **取消语义**：`cancelAnimationFrame` 必须同时能取消兜底定时器，否则 React 卸载时会留下一个已取消但仍会触发的回调。
- **回退**：`config.json` 设 `render_fallback: false`。

---

## 4. 方案 B：判据换成页面自己的可见性（治本，让判断说真话）

### 4.1 探测加一个字段

`FRAME_PROBE`（`window.rs:1190-1223`）返回值增加：

```js
hidden: document.visibilityState === "hidden"
```

ES5、无副作用、旧引擎上 `visibilityState` 为 `undefined` 时表达式为 `false`（按「可见」处理，即保守地沿用旧行为）。

`Frames`（`window.rs:1229-1241`）加 `#[serde(default)] hidden: bool`。
**线格式必须继续由探测源码推导**（`webkit_wire_json`，`window/tests.rs:764`），不手搓 JSON —— 这是 2026-09-18 那次双重编码事故留下的纪律。

### 4.2 判据改用它

`watch_page_liveness`（`window.rs:1576`）里现在传的是 `attended(&window)`（`window.rs:1880`）。改为：

```rust
// 页面自己说的可见性是第一手证据；拿不到时才回退到窗口状态。
let hidden = answer.map(|counts| counts.hidden).unwrap_or(false);
let attended = !hidden && attended(&window);
```

即：**页面自称 hidden ⇒ 一律按「无人观看」处理**（不重载、只提示一次），无论窗口状态怎么说；
页面自称可见却停画 ⇒ 走既有的重载预算（这才是重载真正该管的场景）。

### 4.3 文案按实测改写

- `SuspendedNotDrawing` 的日志与标题（`window.rs:1640-1650`、`NOT_DRAWING_TITLE` `window.rs:1761`）：
  「多为窗口失去焦点」→「**窗口被遮挡或不在前台**」，因为实测证明是遮挡而非焦点。
- 标题里的恢复指引保留 `聚焦窗口或重新加载界面`，并补上 `⌘R` —— 实测「聚焦」并不能恢复，用户需要知道还有一条确定可用的路。
- `README.md` 行为表（`:393`）、已知坑（`:221`）、功能列表（`:53`）三处按本方案结论改写。

### 4.4 与方案 A 的关系

A 生效时，遮挡期间帧计数会动 ⇒ 判 `Drawing` ⇒ B 的 hidden 分支几乎不再进入，日志也不会刷屏。
B 仍然必要，因为：
1. A 只对「页面自己调 rAF」有效；页面若在 `hidden` 时**提前 return 不做工作**（前端多处 `if (typeof requestAnimationFrame === "function")` 式的判断），A 也救不了渲染内容。
2. 露出后仍不画是真故障，必须由 B 计入重载预算，而不是像现在这样被「聚焦了但仍不动 → 按未响应继续处理」含糊带过。
3. 日志要能复盘：`hidden` 是唯一能区分「没被画」与「画不出来」的第一手证据。

---

## 5. 关键约束

- **ES5**：垫片与探测都要在兼容层覆盖的旧引擎上运行（Safari 16.4 起）。禁 `=>`、模板串、`??`、`const`/`let`、`class`。
- **注入顺序**：`WebviewWindowBuilder::initialization_script` 每次调用往 `webview_attributes.initialization_scripts` **追加**一项（`tauri-2.11.5/src/webview/mod.rs:868-877`），wry 侧按注册顺序注入、且在**文档解析之前、页面任何脚本之前**执行（`tauri-2.11.5/src/webview/mod.rs:879-881` 的文档原文）。所以垫片只要用 `initialization_script` 注册就一定早于页面脚本，**不要**用页面加载后的 eval。
  兼容层与垫片都要注入时，注册顺序即执行顺序：兼容层在前（它补的是引擎能力），垫片在后（它包裹的是已被补过的 `requestAnimationFrame`）。
- **不做的事**：不改 Harness、不改前端 bundle、不引入新依赖、不动 `background_throttling`（实测它管的是恢复速度，不是是否停画）。
- **私有 API 不碰**：不读回 `inactiveSchedulingPolicy`（上一轮已论证：它不改变任何决策）。

### 两个已定的小决策（不必再议）

1. `create_harness` 的注入参数：把 `compat: Option<&str>` 换成 `scripts: &[&str]`。理由：注入项会从一项变成两项（兼容层 + 兜底），布尔参数会随之变成两个且顺序隐含；切片天然表达顺序，也免掉「传了 true 但脚本为空」这类组合。
2. `RENDER_FALLBACK_MS` = **250 ms**。实测该节奏约 4 fps，足够让文本流追平（人眼对遮挡中的窗口不敏感，切回来的第一帧已经是最新内容）；再省 CPU 的收益不值得让「切回来还差一截」重新出现。

---

## 6. 影响范围

| 文件 | 改动 |
|---|---|
| `src-tauri/src/window.rs` | 新增 `RENDER_FALLBACK_SCRIPT` 与 `RENDER_FALLBACK_MS`；`FRAME_PROBE` 加 `hidden`；`Frames` 加字段；`watch_page_liveness` 判据换源；`create_harness` 注入；日志/标题文案 |
| `src-tauri/src/window/tests.rs` | 见 §7 |
| `src-tauri/src/lib.rs` | `Config` 加 `render_fallback`（默认 true）、`Default` 构造、传参 |
| `README.md` | 行为表、已知坑、功能列表三处按实测改写 |
| `docs/dsh-desktop-v0.4.4-freeze-review.md` | 不改写（已冻结）；在本文件交叉引用 |

---

## 7. 测试

沿用本项目既有纪律：**能在真引擎里跑的，就不要只做字符串断言**（`run_shim`，`window/tests.rs:222`）。

| 用例 | 钉住什么 |
|---|---|
| `the_render_fallback_hands_the_callback_over_when_frames_stop`（新，用 `run_shim` 真跑） | 用一个「原生帧永不到达、`setTimeout` 可手动推进」的假 window 驱动垫片：兜底投递一次且带时钟戳；迟到的原生帧不得二次投递；`cancelAnimationFrame` 能取消未触发的兜底；可见时**不建**定时器；原生帧及时到达只投递一次 |
| `the_render_fallback_stays_es5_and_is_idempotent`（新） | ES5 语法；重复注入不叠加包裹（`__dshRafFallback` 守卫）；可见时不建定时器 |
| `the_frame_probe_is_es5_and_schedules_at_most_one_callback_of_each_kind`（扩） | 探测仍 ES5、仍只排一个回调，且报告 `hidden` |
| `the_probe_reports_exactly_the_fields_the_shell_parses`（扩） | `hidden` 的线格式由探测源码推导；双重编码仍必须解析失败 |
| `a_page_that_hides_itself_is_named_instead_of_reloaded`（新） | `hidden=true` 的停画不花重载预算（无论窗口状态）；`hidden=false` 的停画照旧三次重载 |
| `a_page_that_stops_running_is_reloaded_and_then_reported`（扩） | 既有矩阵在 `hidden=false` 下不变 |

---

## 8. 验证方式

1. **静态**：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`（host 与 `x86_64-pc-windows-gnu`）、`cargo test`、`make test-scripts`。
2. **变异测试**：把垫片里的 `document.visibilityState === "hidden"` 条件去掉（改成永远挂兜底）与把 `hidden` 字段从探测里删掉，确认相应用例失败 —— 只做「能抓住回归」的断言。
3. **真机（必做，写进交付说明）**：
   - 把 DSH 窗口用 IDE 盖住 5 分钟：日志**不应**再出现连续「停止绘制」；切回来时内容**已经追平**（不需重载、不丢滚动位置）。
   - 露出后人为制造停画（例如把 `RENDER_FALLBACK_MS` 临时调成极大值并保持遮挡再露出）：确认 `hidden=false` 的停画在 3 次探测内触发重载。
   - `⌘R` 仍能重新加载当前会话。

---

## 9. 验收标准

1. 窗口被遮挡 ≥ 5 分钟后切回，**无需重载**即看到最新输出（A）。
2. 日志不再出现无法自愈的沉默分支；遮挡期间每个连续段最多一条说明，且文案指向「被遮挡」（B）。
3. 页面自称可见却停画 ⇒ 3 次探测（≈45 s）内重载（B）。
4. 全部单测与静态检查通过，新增用例经变异测试确认能抓住回归。
5. `render_fallback: false` 能退回修复前的行为（用于对比与自救）。

---

## 10. 风险与回退

| 风险 | 处置 |
|---|---|
| 垫片改变了 rAF 节奏，某个依赖精确帧时序的动画在被遮挡时表现不同 | 被遮挡时本来无帧，风险面仅限「看不见的窗口」；回退开关一键关闭 |
| 垫片与页面自身包裹叠加导致兜底延迟叠加 | 已核实当前无包裹；垫片只在 hidden 时挂定时器，叠加最坏情况为 `N × 250 ms` |
| `hidden` 字段解析失败（旧页面/线格式漂移） | `#[serde(default)]` ⇒ `false` ⇒ 回退到旧的 `attended()` 行为，不会误判成「无人观看」而停止重载 |
| 判据换源后遮挡场景不再重载，用户以为「没救了」 | 标题给出 `聚焦窗口或重新加载界面（⌘R）`，且露出后仍停画会立刻转入重载判定 |

---

## 11. 处理状态

**A、B 均已实施（2026-09-18）**，各项原始结论未改动；与本文设计的差异在 §11.2。

| # | 状态 | 实际修法 |
|---|---|---|
| A 渲染兜底 | **已实施** | `window::RENDER_FALLBACK_SCRIPT` + `RENDER_FALLBACK_MS`（250），`render_fallback_script()` 把占位符替换成间隔；`create_harness` 的 `compat: Option<&str>` 换成 `scripts: &[&str]`，两处调用点改走新的 `harness_scripts(&config)`（顺序：兼容层 → 兜底）；`Config` 新增 `render_fallback`（默认 true） |
| B 判据换源 | **已实施** | `FRAME_PROBE` 返回值加 `hidden`（`document.visibilityState === "hidden"`，`document` 不存在时为 `false`）；`Frames` 加 `#[serde(default)] hidden`；新增纯函数 `page_attended(hidden: Option<bool>, window_attended: bool)`，页面答了就听页面的，答不上来才用 `attended()` |
| B 文案 | **已实施** | 停画日志改为「窗口被遮挡、最小化或不在前台」，`NOT_DRAWING_TITLE` 改为「露出窗口即可恢复，或用 ⌘R 重新加载界面」，「重新获得焦点后仍未恢复」改为「重新可见后仍未恢复」；README 功能列表、行为表、已知坑三处按实测改写 |

### 11.1 验证

- `cargo test`：**199 passed / 0 failed**（197 → 199，新增 2 个用例；另有 2 + 1 + 4 个集成用例通过）
- `cargo fmt --check` 通过；host 与 `x86_64-pc-windows-gnu` 的 `cargo clippy --all-targets -- -D warnings` 均 0 warning
- `make test-scripts` 通过（`check-runtime-stage.sh 自检通过`）
- `make build` 成功；从产物二进制里读回注入脚本，`__dshRafFallback` 与 `visibilityState === "hidden"` 都在，且间隔已是替换后的数值（不是 `%FALLBACK_MS%`）

新增用例与它们钉住的回归：

| 用例 | 钉住什么 |
|---|---|
| `the_render_fallback_stands_in_for_frames_the_page_will_not_get` | 用 `run_shim` 在真 node 里驱动垫片（假 window：原生帧永不到达、定时器可手动推进）：兜底投递一次且带 `performance.now()` 时间戳、迟到的原生帧不二次投递、取消能撤销未触发的兜底、可见时**一个定时器都不建**、原生帧及时到达只投递一次 |
| `the_render_fallback_stays_es5_and_wraps_once` | ES5 语法、`__dshRafFallback` 守卫、`hidden` 条件存在、间隔来自 `RENDER_FALLBACK_MS` 且占位符已被替换；并在真 node 里连续注入两次，断言包裹函数是同一个对象（不叠加） |
| `the_page_own_visibility_outranks_the_window_state` | 页面答 `hidden` 时无视窗口状态；页面答可见而窗口无焦点时按「有人看着」处理（这正是重载该管的场景）；页面答不上来时两侧都沿用窗口状态；两种组合接进 `liveness_action` 后的实际动作 |
| `the_probe_reports_exactly_the_fields_the_shell_parses`（扩） | `hidden` 的线格式仍由探测源码推导；字段表加了 `hidden`；探测里确实用的是 `visibilityState === "hidden"` |

**变异测试**（确认用例真能抓住回归，跑完即还原）：

| 变异 | 结果 |
|---|---|
| `page_attended` 忽略页面答案（`Some(_) => window_attended`） | `the_page_own_visibility_outranks_the_window_state` 失败 ✓ |
| 垫片去掉 `visibilityState === "hidden"` 条件（永远挂兜底） | 两个渲染兜底用例同时失败 ✓ |
| 探测删掉 `hidden` 字段 | `the_probe_reports_exactly_the_fields_the_shell_parses` 失败 ✓ |

### 11.2 与本文设计的差异

- **`RENDER_FALLBACK_SCRIPT` 保留 `%FALLBACK_MS%` 占位符**，由 `render_fallback_script()` 替换。这样脚本仍是常量（测试能直接读），间隔也仍是常量（数字只出现一次），两个诉求都要到了；用例同时断言替换后不再含占位符。
- **`page_attended` 提成了纯函数**，而不是在 `watch_page_liveness` 里写 `!hidden && attended(&window)`。原因是那个表达式无法在不起窗口的情况下测；提出来之后「页面答了就听页面的、答不上来才回退」这条规则有了直接的用例（含 §11.1 的变异测试）。
- **`harness_compat` 换成了 `harness_scripts`**，返回有序的脚本列表；`lib.rs` 里那条「已注入兼容层 / 探测未到达 / webkit_compat=false」的日志判据相应改为单独调用 `needed_compat_script()` 判断 —— 若沿用 `scripts.first()`，在关闭兼容层时会把兜底脚本误当成兼容层，日志会说反。
- **`attended()` 的文档改了，行为没改**：它仍在页面答不上来时兜底，但注释不再声称「不在这些状态就是 WebKit 允许停画的状态」——实测正好相反。

### 11.3 遗留

- **真机验证待做**：用 IDE 盖住 Harness 窗口 5 分钟再切回，确认日志出现 `页面停止绘制：计时器仍在运行`、标题出现「已暂停绘制」、**没有**重载，且切回时内容已经追平（这是兜底要证的那条）。
- **方案 B 的 hidden 分支在兜底生效后基本不再进入**（帧会动 ⇒ 判 `Drawing`）。它仍要在，因为：兜底只帮「页面确实调了 rAF」的情形；露出后仍不画必须能并入重载判定；日志要能区分「没被画」与「画不出来」。
- **未验证**：垫片在页面自己包裹 rAF 时的叠加延迟（已核实当前 dsh 前端与已装插件都不包裹）。
- **图2 的窗口来源仍未确认**（尺寸 1561×871、标题栏带 `New Tab`，疑似系统浏览器）；若是浏览器，说明该次恢复走的是浏览器渲染路径，与本方案无关但值得单独记一笔。
