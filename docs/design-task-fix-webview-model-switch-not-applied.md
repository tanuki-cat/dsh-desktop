# 修复：WebView 里切换模型/思考强度不生效（实施方案）

对象：`dsh-desktop` 本地 `main`（HEAD `4112b0e`，版本 v0.4.5）
实施日期：2026-09-18
触发：用户报告「desktop 的 webview 中切换模型和思考强度不生效，在浏览器 web 中能生效」，并补充
「desktop 的**前一个版本**（v0.4.4）可以正常切换」「点击档位后**菜单关闭了，但标签没变**」
对照浏览器：Firefox
现场 dsh：系统安装 `0.1.6-alpha.2`（`/opt/homebrew/lib/node_modules/@deepseek-ai/dsh`）

相关：[`design-task-fix-webview-render-stall.md`](./design-task-fix-webview-render-stall.md)、
[`design-task-fix-raf-fallback-blinds-liveness-probe.md`](./design-task-fix-raf-fallback-blinds-liveness-probe.md)

本文分两步：第 1 步是让 WebView 不再是黑箱（§2），第 2 步是拿现场证据定根因并修（§3、§4）。
两步都已落地。**§1 是第 1 步之前的推断，其中 §1.1 已被 §3 推翻**，按"不原地改写成相反结论"保留原文。

---

## 1. 现在能确定什么

### 1.1 宿主侧切换成功了，是客户端没把投影应用上

> **本节结论已被 §3 推翻**（保留原文以记录推断过程）：「菜单关闭 ⇒ RPC 成功」这一步是错的 ——
> 菜单是被 `onBlur` 关掉的，`onClick` 从未触发，RPC 根本没发出去。

`ModelSelect` 的 `settleSelection` 只有拿到 `result.ok` 才关菜单
（`dsh-client-ui-model-selection/lib/client.js:643`）。用户观察到**菜单关闭**，因此
`session.selectModel` 这个 RPC **成功返回**了 —— 宿主已经接受了这次切换。

而标签显示的值不是本地状态：

```js
const current = projected.next ?? catalog.value.default;   // client.js:293
```

`projected` 是宿主推下来的 `modelSelection` 投影。活路径是：

```
控制流 projection 帧
  → ClientSessions.handleControlFrame()            (dsh-api-session-controller/lib/client.js:2590)
  → ProjectionValueStore.apply(key, value, seq)    (:747)
  → changed(key) → notifier.markDirty()            (:781，microtask，不是 rAF)
  → ModelDirectory.syncInputs() → store.set()      (ui-model-selection:268)
  → useSyncExternalStore 重渲染
```

所以症状精确定位为：**控制流里那帧 `{type:"projection", key:"modelSelection"}` 没到，或到了但被 `apply()` 丢掉**
（`apply` 的规则是 `seq <= row.seq` 直接 return）。

### 1.2 v0.4.4 → v0.4.5 之间，对页面可见的改动只有两处

| 改动 | 提交 | 页面可见性 |
|---|---|---|
| 新增注入 `RENDER_FALLBACK_SCRIPT`（全局替换 `requestAnimationFrame`/`cancelAnimationFrame`） | `e7dd589` | **是** |
| 帧停不再重载页面（`Scheduling::Suspended` → `SuspendedNotDrawing`） | `6384402` | 间接：v0.4.4 会重载，把页面里卡住的状态冲掉 |

v0.4.4 的 `create_harness` 只注入兼容层一个脚本（`git show v0.4.4:src-tauri/src/window.rs:1285`），
其余窗口配置（`background_throttling`、`on_navigation`、`on_new_window`、`on_page_load`）两版一致。

### 1.3 已排除的两条

**(a) 渲染兜底判据错位** —— 不成立。
怀疑过 `covered()` 只认 `document.visibilityState === "hidden"`，覆盖不到真实停画态。但
`design-task-fix-webview-render-stall.md` §1 已实测过闸门就是可见性：「可见+无焦点」rAF 照常跑，
「被遮挡」才停，而被遮挡时 `visibilityState` 确实变 hidden。判据与实测一致。

另外把脚本单独放进 Node 跑了完整时序（可见 → 遮挡 → 隐藏期更新 → 恢复，隐藏时挂起原生 rAF）：

```
隐藏中：等 300ms（兜底应顶上）   requested=3 delivered=3 fallbacks=1
窗口重新露出                  requested=4 delivered=4 fallbacks=2
OK: 每次请求都被送达
```

`settled` latch 保证恰好一次投递，隐藏/恢复两个方向都不丢帧。

日志里那句「本轮渲染兜底顶起 0 帧」也不构成反证：兜底只顶**已经挂起的 rAF 请求**，
页面空闲时本就没有待处理请求，0 是正常值。

**(b) 系统 WebView 缺 `Symbol.dispose`** —— 是事实，但不是本次回归。
日志每次启动都有 `WebView 缺少 Symbol.dispose：已注入兼容层`；这台机器（macOS 27）其余
`Iterator` / `Promise.try` / `Math.sumPrecise` / `Uint8Array.fromBase64` 都在，只差这一个。
dsh 客户端确实拿它当释放键（`dsh-api-session-controller/lib/client.js:51,2968`、
`dsh-api-gateway/lib/client.js:695`）。但 v0.4.4 注入的是同一个垫片，两版无差异，
解释不了「上个版本能切」。保留为观察项。

### 1.4 差的那一环

`modelSelection` 的通知链走 microtask，不走 rAF —— 所以 §1.2 的两处改动**都不能直接解释**标签不变。
静态代码能给的已经到头了：下一步必须看现场，而这个 WebView 现在没有任何把页面内错误交出来的通道。

---

## 2. 第 1 步：把 Harness 页面的错误接出来（本次实施）

### 2.1 为什么值得做成常驻能力

本仓库的历史故障（`Failed to load plugins`、渲染中断、兜底致盲探测）有一个共同形态：
**只在 WebView 里复现，而 WebView 没有控制台**。每次都要用户装 dev 构建、挂 Safari 开发菜单复述一遍。
壳已经有一条每 15 秒执行一次页面 JS 并取回结果的通道（`probe_frames` 的 `eval_with_callback`），
顺路把页面攒下的错误捎回来，成本接近零。

### 2.2 设计

**注入脚本 `PAGE_DIAGNOSTICS_SCRIPT`**（ES5，自守卫，重复注入是 no-op）：

- `window.addEventListener("error", …)` 与 `"unhandledrejection"`：被动监听，不替换任何东西。
- 包一层 `console.error` / `console.warn`，**调用原实现后**再记录。这两个是有意包的：
  控制流断开时客户端打的正是
  `console.error("[session-controller] control stream failed:", error)`
  （`dsh-api-session-controller/lib/client.js:3478`），那行就是本次要找的东西。
- 写进一个有界环形缓冲 `window.__dshPageNotes`（上限 50 条，每条 `"<相对毫秒> <kind> <text>"`），
  满了丢最旧的。超长文本截断到 500 字符。**页面自己的对象一律不入库**，只留字符串，
  避免把用户内容或凭据带进日志（`harness::app_log` 的 `redact` 仍然在出口兜底）。

**探测扩展**：`FRAME_PROBE` 返回值加两个字段

- `notes`：把 `__dshPageNotes` **取走并清空**（每条只上报一次）。
- `pending`：当前挂起的 rAF 请求数（由 `RENDER_FALLBACK_SCRIPT` 维护 `__dshPendingFrames`）。
  停画时这个数字直接回答「页面是不是在等一个永远不来的帧」。

**壳侧**：`probe_frames` 改成解析一个 `PageProbe { #[serde(flatten)] frames, notes, pending }`，
把 `notes` 逐条写进 `harness.log`（前缀 `page:`），返回值仍是 `Option<Frames>`。
存活判据、`liveness_action`、`judge_frames` 一行不改 —— 这一步只增加可见性，不改变任何行为。

### 2.3 配置

`config.json` 新增 `"page_diagnostics": true`（默认开）。关掉后不注入该脚本，探测里 `notes` 恒为空。
默认开的理由：它是纯旁观（监听 + 透传），而本仓库每一次 WebView-only 故障的第一步都是"让用户再复现一次"。

### 2.4 约束

1. **脚本仍是 ES5**：和探针、兼容层一样要在旧引擎上跑，单测沿用既有纪律断言（无箭头函数、反引号、`??`、`const`/`let`、`class`）。
2. **不得致盲存活探测**：`pending` 只作日志，不参与 `judge_frames`；`notes` 同理。
   这是 `design-task-fix-raf-fallback-blinds-liveness-probe.md` 的教训，必须由单测钉死。
3. **不得改变页面行为**：`console.error` 包装先调原实现再记录，异常不外泄（整段 try/catch）。
4. **有界**：环形缓冲上限与单条长度都是常量，一次探测最多带回 50 条。

---

## 3. 根因（已确认，2026-09-19）

**WebKit 上鼠标点击 `<button>` 不给它焦点**，把 dsh 菜单「失焦即关」的写法打穿了：
`click` 事件根本不会产生，所以没有 RPC、没有报错、菜单却关了。

### 3.1 测量过程

页面诊断上线后，一次点击的现场是：**页面侧零记录**（无异常、无 `console.error`）。
同时用一个每秒轮询 `~/.dsh/settings.yaml` 的监视器记录宿主侧写入（新会话页的模型座位写的是
`agent-default-model` 这个全局默认，不是会话投影）：

```
[00:11:59] deepseek-official / deepseek-flash / high    ← 基线
[00:12:31] router / dsf-workbuddy / high                ← desktop 点模型：写入成功
（点 Low：标签 dsf-workbuddy High → 仍是 High，监视器零写入）
[00:18:46] router / workbuddy-en-flash / high           ← desktop 用键盘切模型：成功
[00:19:0x] router / workbuddy-en-flash / max            ← desktop 用键盘切强度：成功
```

三个判别性事实：

1. **鼠标点档位：不写入、不报错、菜单关闭** → `onClick` 从未触发。
2. **同一窗口用键盘（↓ 移动 + 回车）：立刻生效** → RPC、连接、投影、回显全部正常。
3. **Firefox 鼠标点击：正常** → 不是 dsh 的逻辑，是引擎行为差异。

### 3.2 链条

```
点「推理等级」进二级面板 → paneFocus.current = "drill"        (ui-model-selection/lib/client.js:593)
  → effect 把焦点 .focus() 到当前选中的档位按钮                (:534-541)
鼠标 mousedown 到「Low」
  → WebKit 不给 <button> 焦点（Firefox/Chrome 会给）
  → 原先那个菜单项失焦，relatedTarget = null
  → onBlur 守卫 `relatedTarget instanceof Node && menuRef.contains(...)` 落空 → close()  (:640)
  → portal 卸载 → mouseup 落空 → click 永不触发 → chooseEffort 从未被调用
```

模型行与档位行都是 `role="menuitemradio"` 的 `<button>`，所以两者都坏。
`paneFocus` 只在 drill/back 时设置（`:593,:598`），所以**一级面板刚打开时焦点不在菜单里、没有 blur，
模型点击反而是好的** —— 00:12:31 那次写入就是这么来的。这条"时灵时不灵"正是之前判断反复的原因。

### 3.3 修在哪一层

真正的修法在 dsh：`onBlur` 不该把 `relatedTarget === null` 一律当成"焦点离开了菜单"。
但那不在本仓库，且用户现在就要能用，所以壳这边补一层**行为垫片**（§4），
并把上游问题单独记下来（§6）。

### 3.4 顺带发现（独立 bug，不是本次根因）

页面加载时有一条未捕获异常：

```
Error: list slot "conversation.chat.turnTail" requires options.id
```

肇事者是用户 profile 里的第三方插件 **`dsh-better-sidebar` v0.19.1**
（`lib/client-registry.js:3491`）：往 `kind: "list"` 的槽上按 chain 槽的形状注册
（给了 `select:`、没给 `id`）。该槽由 `dsh-client-ui-chat/lib/client.js:3787` 声明为 list，
两个核心注册方给的都是字面量 `id`。按该插件自己的注释，`slots.inject` 的回调在槽尚未声明时
会推迟到**声明方**的 `register()` 里跑，抓到的栈正是这条路径 —— 也就是说它炸的是
`dsh-client-ui-chat` 的注册。与本次模型切换无关，但应报给插件作者。

---

## 4. 修复：按下菜单时按住焦点（`MENU_FOCUS_GUARD_SCRIPT`）

### 4.0 被否掉的第一版（补焦点）

第一版按 Chrome 的行为做：mousedown 时把焦点 `focus()` 给被按的菜单项。**实测更坏**
（2026-09-19）：一级面板的「模型」「推理等级」两行也是 `role="menuitem"`，点它们会
`drill()` 把整个面板换掉 —— React 卸载的正是刚拿到焦点的那一行，焦点掉回 `body`，
于是又产生一次 `relatedTarget === null` 的 blur，`onBlur` 照样 `close()`。
结果是把二级面板的病提前搬到了一级面板：连列表都展不开。

**教训**：对一个"焦点一离开就关"的弹层，任何搬动焦点的修法都是在喂它触发条件。

### 4.1 设计

同样是文档级 `mousedown` **捕获**监听，但动作换成 `event.preventDefault()`：
mousedown 的默认动作正是"改变焦点"，压掉它焦点就**原地不动** —— 没有 blur，弹层不卸载，
mouseup 落在按下的那一项上，`click` 正常触发。`preventDefault` 压的是聚焦、选区和拖拽，
**不压随后的 `click`**。

它不碰焦点，所以既不会夺走输入框的光标，也不会制造"被卸载的焦点元素"。

**作用面**：`role` 为 `menu` / `menubar` / `listbox` 的弹层内部。
**例外**：目标是 `INPUT`/`TEXTAREA`/`SELECT` 或 `contentEditable` 时放行 —— 弹层里带筛选框的
combobox 必须还能拿到光标。其余：只处理主键（`button === 0`）、跳过 `defaultPrevented`。

### 4.2 配置

`config.json` 新增 `"keep_menu_focus": true`（默认开）。这是本仓库第一个**改变页面行为**的垫片
（既有兼容层只补缺失的 API），所以必须可关：当它与某个页面打架时，用户能立刻自救。

### 4.3 为什么不按平台 cfg 门控

在 mousedown 本来就会给按钮焦点的引擎上，压掉默认动作只是让焦点留在原处，弹层行为不变；
用一个可测的配置开关比用 `#[cfg(target_os)]` 更容易在单测里钉死行为。

---

## 5. 验证

第 1 步（诊断）：

- `cargo test`：诊断脚本的 ES5 纪律与单次安装；`page_diagnostics` 开关；
  `PageProbe` 带/不带 `notes`/`pending` 两种载荷都能解析且 `Frames` 部分一致；判据不受影响。
- 手工：已完成 —— 现场抓到 §3.4 那条未捕获异常，并证明点击时页面侧零记录。

第 2 步（补焦点）：

- `cargo test`：ES5 纪律与单次安装；开关独立；在真实引擎（node + 手搭 DOM 桩）里驱动
  「焦点在菜单项 A、mousedown 到菜单项 B」，断言 B 拿到焦点；断言非菜单角色的按钮**不**被补焦点；
  断言 `disabled` 项、非主键、`defaultPrevented` 都跳过。
- 手工：desktop 里鼠标点模型与思考强度都能切换，且监视器看到对应写入。

## 6. 验收标准

1. desktop 里**鼠标**切换模型和思考强度生效，写入与键盘路径一致。
2. `menu_click_focus=false` 可回到当前行为；`page_diagnostics=false` 同理。
3. 存活探测行为与 v0.4.5 完全一致（判据用例全绿）。
4. 上游待办：dsh 的 `ModelSelect.onBlur` 不应把 `relatedTarget === null` 当作"焦点离开菜单"；
   `dsh-better-sidebar` 的 `turnTail` 注册缺 `id`。两条都应报给各自作者 —— 壳这边的垫片只是止血。

---

## 6. 处理状态

- [x] §2 第 1 步落地（2026-09-18）

  改动：
  - `lib.rs`：`Config.page_diagnostics`（默认 true）、`harness_scripts` 按开关追加脚本（放在最后注入）。
  - `window.rs`：新增 `PAGE_DIAGNOSTICS_SCRIPT` / `page_diagnostics_script()` 与两个上限常量；
    `RENDER_FALLBACK_SCRIPT` 增加 `__dshPendingFrames`（收敛到 `forget()` 一个出口，计数不会与表漂移）；
    `FRAME_PROBE` 增加 `pending` 与 `notes`（取走即清空）；新增 `PageProbe`，
    `probe_frames` 返回它并把 `notes` 写进日志，判据仍只读 `Frames`；
    「停止绘制」那行日志补上「页面仍在等 N 帧」，区分表里的第 3、4 行。

  门禁：`make fmt-check` / `make clippy`（`-D warnings`）/ `make test` 全绿，
  单测 210 个（新增 7 个：ES5 纪律与单次安装、四类页面故障各记一条且原 console 照常执行、
  环形缓冲两个方向都有界、探针取走后页面不留副本、旧载荷仍可解析且判据不受影响、
  三个注入开关各自独立、缺 key 的旧配置默认开）。

- [x] §3 根因确认（2026-09-19，测量见 §3.1）
- [x] §4 焦点守卫落地（2026-09-19；第一版补焦点被实测否掉，见 §4.0）

  改动：
  - `window.rs`：新增 `MENU_FOCUS_GUARD_SCRIPT` / `menu_focus_guard_script()`。
  - `lib.rs`：`Config.keep_menu_focus`（默认 true），`harness_scripts` 在诊断之前追加它。

  门禁：`make fmt-check` / `make clippy` / `make test` 全绿，单测 212 个。新增 2 个：
  ES5 纪律与单次安装，外加一条断言守卫**自己不得调用 `.focus(`**（把 §4.0 的错法钉死）；
  以及在 node 里用 DOM 桩按真实 markup 驱动（`role="menu"` > `role="menuitem"` > `<span>`）：
  弹层内的按下被压住默认动作，弹层外的按钮、弹层内的 `INPUT`、`contentEditable`、
  右键、已 `defaultPrevented` 五种情况全部放行。

- [ ] 手工验收：desktop 里用**鼠标**切模型与思考强度，确认写入
- [ ] 上游两条（见 §6.4）尚未上报
