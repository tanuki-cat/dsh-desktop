# 渲染兜底垫片让存活探测失明（审查 + 修复方案）

审查对象：`dsh-desktop` 本地 `main`（HEAD `c075eb5`）+ 工作区未提交改动（渲染兜底 A / 判据换源 B）
审查日期：2026-09-18
触发：用户报告「切换会话后无法正常渲染，重启应用才恢复」，附 21:02:31 现场截图
被替代文档：[`design-task-fix-webview-render-stall.md`](./design-task-fix-webview-render-stall.md)
（该文的**根因分析仍然成立**；被推翻的是方案 A 的实现方式与方案 B 的有效性判断，原因见 §2）

---

## 1. 结论（先看这里）

上一轮的方案 A（`RENDER_FALLBACK_SCRIPT`，包裹 `requestAnimationFrame`）**把壳自己的停画探测一起顶了**。

`FRAME_PROBE` 用 `window.requestAnimationFrame` 计帧，而垫片在 document-start 就替换了这个函数。
于是页面被遮挡时：垫片的 250 ms 定时器照样把探针的回调顶起来 → `__dshFrames` 递增 → `judge_frames` 判 `Drawing` → `LivenessAction::Alive`。
**壳从此再也看不见任何停画**，包括垫片救不了的那些。

连带后果：方案 B（`hidden` 字段 + `page_attended`）成了死代码 —— `liveness_action` 的 `Drawing` 分支根本不读 `attended`。
上一轮文档 §4.4 把「A 生效后 B 的 hidden 分支几乎不再进入」当成好事，实际代价是整套看护失去了唯一的输入信号。

同时发现垫片有一个**覆盖不到的洞**（§3）和一个**即将接管的无限豁免**（§4）。三者叠加正好构成用户看到的「只能重启」。

---

## 2. 证据

### 2.1 垫片顶起壳的探针（已在真 node 里跑通）

把 `RENDER_FALLBACK_SCRIPT` 与 `FRAME_PROBE` 按真实注入顺序串在一个假 window 上，
模拟完全遮挡（原生 rAF 永不兑现、`setTimeout` 照常、`visibilityState = "hidden"`）：

```
探测 1: {"frames":0,"timers":0,"timersSeen":true,"hidden":true}
探测 2: {"frames":1,"timers":1,"timersSeen":true,"hidden":true}
探测 3: {"frames":2,"timers":2,"timersSeen":true,"hidden":true}
```

帧计数在一帧都没有被真正绘制的情况下持续递增。
`judge_frames` 的注释写着「帧计数不可能在停画时移动，所以任何变化都是在画的证明」—— 这句话在垫片装上之后**不再成立**，
而它正是整套判据的地基。

### 2.2 日志：修复后的构建全程报告健康

当前运行的二进制就是含 A + B 的构建（`~/Applications/DSH Desktop.app` 与 `target/release/dsh-desktop` sha256 同为 `1d71a9b2…`）。

`~/Library/Application Support/com.deepseek.dsh.desktop/logs/harness.log`，最近一次会话：

```
[20:43:41] 启动，注入兼容层
[20:43:56] WebView 页面恢复绘制（此前 1 次未通过检查、0 次重载）
      …… 18 分钟内一条停画日志都没有 ……
[21:02:04] 手动重新加载界面：http://127.0.0.1:3080/
[21:02:16] 手动重新加载界面：http://127.0.0.1:3080/
[21:02:25] Harness 页面停止绘制（输入仍有响应），继续观察
[21:02:33] WebView 页面恢复绘制（此前 1 次未通过检查、0 次重载）
[21:02:36] stopping Harness pid 43918
```

用户截图的文件名是 `Snipaste_2026-09-18_21-02-31.png`，落在 21:02:25 与 21:02:36 之间。
即：**用户在 18 分钟里遭遇渲染停滞、两次手动 ⌘R 都没救回来、最后退出应用，而壳全程认为页面在画。**

对照修复前（17:xx 的旧构建）同样时长内每隔几分钟就有一条「停止绘制」—— 信号是被这次修复消掉的，不是故障消失了。

### 2.3 现场画面与之吻合

截图中：左侧会话列表、标题栏、输入框、底部状态栏（`2 轮 97 步 · 235 tok/s`、`7.2M tok · 缓存命中 98%`）都在，
对话区只停在 `载入历史...` 与 `深度求索中...`，其余是皮肤背景。
宿主在产出、页面在跑 JS，只有内容区的渲染管线没有推进 —— 与上一轮认定的停画签名一致。

---

## 3. 垫片覆盖不到的洞：跨越可见性边界的那一帧

垫片**只在 `requestAnimationFrame` 被调用的那一刻**判断一次 `visibilityState`。
在页面可见时排队的帧，随后窗口被遮挡 —— 这一帧没有任何兜底，而 Harness 前端的 flush 是三连 rAF（`dsh-client-ui-conversation`），
断在任何一环整条链就停住。

真 node 验证（可见时排第 1 环 → 切到 hidden → WebKit 不再兑现已排队的帧）：

```
60 秒后走到第 0 环；挂了 0 个兜底定时器
```

这正是用户的操作形态：**正看着窗口切换会话（前端排 rAF）→ 切到 IDE / 被别的窗口盖住 → 回来发现停在「载入历史…」**。
垫片对这一路径完全无效，而 §2 的失明又让壳不会去救它。

---

## 4. 已装好引信的无限豁免：`Busy`

`liveness_action` 在 `misses >= LIVENESS_MISSES` 且 `busy` 时返回 `LivenessAction::Busy`，**没有任何上限**：
`misses` 继续累加，但只要 `busy` 为真就永远走不到 `Reload`。
函数注释写的是「It delays the reload rather than cancelling it」，代码里这个 delay 是无限的。

而 `page_is_busy` 的判据是 `activity.editing || activity.idle < 15s`，其中
`editing` = `document.activeElement` 是 `input` / `textarea` / `contenteditable`。
Harness 的消息输入框在页面加载后就持有焦点（截图里输入框带光标），**`editing` 因此接近恒为真**。

这一条至今没有爆发过，只是因为它此前是死代码：`ACTIVITY_PROBE` 的双重编码直到今天 `c075eb5` 才修好
（日志里「检测到正在输入」出现 **0 次**）。修好之后判据第一次真正生效，方向恰好翻转 ——
**从「永不保护」翻成「永不重载」**。一旦 §2 的失明被修复、壳重新能判出 `Frozen`，`Busy` 会立刻接管并永久阻止自动重载。

---

## 5. 次要问题

| # | 问题 | 位置 |
|---|---|---|
| 5.1 | 手动重载（`RELOAD_MENU_ID`）与崩溃恢复重载**不重置**看护循环的 `frames` / `misses` / `stuck`，而自动重载分支明确重置了 `frames = None` 并写明了理由。新文档的计数器从 0 开始，会与旧文档的残留读数相比较 —— 21:02:25 那条日志就是这么来的 | `window.rs` `watch_page_liveness` / 重载入口 |
| 5.2 | 日志文案「Harness 页面停止绘制（**输入仍有响应**），继续观察」实际含义是「JS/计时器仍在响应」，读起来像「用户正在输入」，与 `Busy` 分支的文案语义撞车 | `window.rs` `Wait` 分支 |
| 5.3 | `Report` 分支只把标题改成「请重启应用」。用户实测**重启才恢复、⌘R 不恢复**，说明存在 `navigate` 救不回的故障态；`GENERATION` 机制已经支持重建窗口，壳有能力自己走这一步却没走 | `window.rs` `LivenessAction::Report` |
| 5.4 | `probe_frames` 的注释称 WebKit 通过 **UI 进程**求值 JavaScript。实际 `evaluateJavaScript` 在 Web Content 进程里执行。结论（有应答 = 渲染进程还活着）不受影响，但依据写反了 | `window.rs` `watch_page_liveness` 文档注释 |

---

## 6. 修复方案

按必要性排序。1–3 是让这次报告的故障能被发现并自愈的最小集合。

### 6.1 探针必须问原生调度器（必须）

垫片保留一份原生引用并暴露出来，探针优先用它计帧：

- 垫片：`w.__dshNativeRaf = nativeRaf;`（以及 `__dshNativeCancel`），并维护一个 `w.__dshFallbackFrames` 计数，记录兜底顶了多少帧。
- `FRAME_PROBE`：计帧改走 `w.__dshNativeRaf || w.requestAnimationFrame`，并把 `fallbacks: w.__dshFallbackFrames || 0` 一起报上来。

这样两个诉求各自成立：**页面拿到它需要的帧，壳拿到「WebKit 是否真的在画」的第一手答案**。
`fallbacks` 还让日志第一次能区分「原生在画」与「兜底在顶」。

> 纪律：`Frames` 新增字段的线格式继续由探测源码推导（`webkit_wire_json`，`window/tests.rs`），不手搓 JSON。

### 6.2 垫片补上可见性转换（必须）

垫片改为持有**全部在途请求**（id → callback），并监听 `visibilitychange`：
页面转入 `hidden` 时，给每一个还没兑现的请求补挂兜底定时器。
转回 `visible` 时清掉未触发的兜底（原生帧会自己来）。

这是 §3 那个洞的直接修法，也是最常见路径的修法。ES5、幂等守卫、`settled` 闩三条约束不变。

### 6.3 `Busy` 不能是无限豁免（必须）

- 判据：`editing` 单独不足以证明有人在输入 —— 焦点停在输入框是 Harness 的常态。改为 `editing && idle < LIVENESS_INTERVAL`，或直接只看 `idle`。
- 上限：`Busy` 最多推迟固定轮数（建议 2 轮 ≈ 30 s），超过就照常重载，让注释说的「delays rather than cancels」名副其实。

### 6.4 重载入口统一重置看护状态（应做）

手动重载与崩溃恢复重载走同一条重置路径（`frames = None`、`misses = 0`、`stuck = false`），与自动重载一致。

### 6.5 重载救不回时升级为重建窗口（建议，需先取证）

`Report` 之前加一级：销毁并重建 Harness 窗口（`GENERATION` 已支持），这正是用户手工「重启应用」所做的事。
落地前需要先确认「⌘R 无效、重启有效」是否真的是 WKWebView 实例级故障 —— 当前无 WebContent 崩溃报告（最近一份是 09-18 09:52，早于现场），**尚未取证**。

---

## 7. 影响范围

| 文件 | 改动 |
|---|---|
| `src-tauri/src/window.rs` | 垫片暴露原生引用 + 兜底计数 + `visibilitychange` 补挂；`FRAME_PROBE` 改走原生 rAF 并上报 `fallbacks`；`Frames` 加字段；`page_is_busy` 判据收紧；`liveness_action` 给 `Busy` 上限；重载入口统一重置；5.2 / 5.4 文案 |
| `src-tauri/src/window/tests.rs` | 见 §8 |
| `src-tauri/src/lib.rs` | 无（`render_fallback` 开关保持） |
| `README.md` | 行为表与已知坑按本轮结论再修一次（上一轮刚按「遮挡」改过） |

---

## 8. 测试

沿用既有纪律：能在真引擎里跑的就不做字符串断言（`run_shim`）。

| 用例 | 钉住什么 |
|---|---|
| `the_frame_probe_counts_native_frames_only`（新，`run_shim`） | 垫片 + 探针串在同一个假 window 上，原生帧永不到达：`frames` **必须不动**，`fallbacks` 递增，`hidden` 为真。这条直接钉住本轮的失明 |
| `the_render_fallback_covers_frames_queued_before_the_page_hid`（新，`run_shim`） | 可见时排队 → 转 hidden → 兜底必须补上；三连 rAF 链能走完 |
| `a_busy_page_is_reloaded_after_the_grace_runs_out`（新） | `Busy` 有上限；`editing` 单独不再构成豁免 |
| `a_manual_reload_resets_the_liveness_streak`（新） | 手动重载后 `frames`/`misses`/`stuck` 与自动重载一样被重置 |
| 既有 `the_render_fallback_*` / `the_page_own_visibility_*` | 保持通过（垫片对页面的行为不变，只是多暴露了原生引用） |

**变异测试**：把 `FRAME_PROBE` 改回走 `w.requestAnimationFrame`，`the_frame_probe_counts_native_frames_only` 必须失败。

---

## 9. 验证方式

1. 静态：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`（host + `x86_64-pc-windows-gnu`）、`cargo test`、`make test-scripts`。
2. 真机（必做）：
   - 用 IDE 盖住 Harness 窗口 5 分钟：日志**必须**出现一条停画说明（不是沉默），切回来内容已追平、无重载。
   - 正看着窗口时切换会话，立刻切到 IDE 停留 2 分钟再切回：内容必须已经追平（这是 §3 那个洞的验收）。
   - 人为制造「可见却不画」（例如临时关掉 `render_fallback` 并遮挡后露出）：3 次探测内触发重载，输入框持有焦点**不得**阻止它。

## 10. 验收标准

1. 遮挡期间壳的帧探测报告 `frames` 不动（而不是被自己的兜底顶动），日志有且仅有一条说明。
2. 跨越可见性边界排队的帧能被兜底补上，切回时内容已追平。
3. 输入框持有焦点不再无限期阻止自动重载。
4. 新增用例经变异测试确认能抓住回归。

---

## 11. 处理状态

**§6.1–6.4 已实施（2026-09-18）**，§6.5 未做（需先取证）。各条原始结论未改动。

| # | 状态 | 实际修法 |
|---|---|---|
| 6.1 探针问原生调度器 | **已实施** | 垫片把原生 `requestAnimationFrame`/`cancelAnimationFrame` 留在 `__dshNativeRaf`/`__dshNativeCancel`，并维护 `__dshFallbackFrames`；`FRAME_PROBE` 计帧改走 `__dshNativeRaf`（拿不到才回退包裹版），并上报 `fallbacks`；`Frames` 加 `#[serde(default)] fallbacks`；停画日志带上本轮兜底顶起的帧数 |
| 6.2 垫片补可见性转换 | **已实施** | 垫片改为持有全部在途请求（`id -> request`），`visibilitychange` 转 hidden 时给未兑现的请求补挂兜底、转 visible 时撤掉未触发的兜底；`settled` 闩与幂等守卫不变 |
| 6.3 `Busy` 不再是无限豁免 | **已实施** | `page_is_busy` 由 `editing \|\| idle < 15s` 改为 `editing && idle < 15s`；新增 `LIVENESS_BUSY_GRACE = 2`，`liveness_action` 在 `misses < LIVENESS_MISSES + LIVENESS_BUSY_GRACE` 内才豁免 |
| 6.4 重载入口统一重置 | **已实施** | 新增 `EXTERNAL_RELOADS` 计数与 `note_external_reload()`，手动重载 / 崩溃恢复 / 首次加载重试三处调用；看护循环每轮比对，发现变化就重置 `frames`/`misses`/`stuck`（重载预算保留 —— 它记的是壳自己的重载） |
| 5.2 / 5.4 文案 | **已实施** | `Wait` 分支「输入仍有响应」→「JavaScript 仍在响应」；`probe_frames` 的注释改正为「脚本在 web content 进程执行、从 UI 进程应答」 |

### 11.1 验证

- `cargo test`：**203 passed / 0 failed**（199 → 203，新增 4 个用例；另有 2 + 1 + 4 个集成用例通过）
- `cargo fmt --check` 通过；host 与 `x86_64-pc-windows-gnu` 的 `cargo clippy --all-targets -- -D warnings` 均 0 warning
- `make test-scripts` 通过

**变异测试**（跑完即还原）：

| 变异 | 结果 |
|---|---|
| `FRAME_PROBE` 改回走 `w.requestAnimationFrame` | `the_frame_probe_counts_native_frames_only` 失败，实测读数 `frames:[0,1,2] fallbacks:[0,2,5]` —— 一帧都没被绘制而计数照涨，正是线上那次失明 ✓ |
| 垫片去掉 `visibilitychange` 补挂 | `the_render_fallback_covers_frames_queued_before_the_page_hid` 失败（链走 0 环 / 应为 3 环）✓ |
| `Busy` 恢复成无上限 | `a_busy_page_is_reloaded_after_the_grace_runs_out` 失败（`Busy{5}` / 应为 `Reload{1}`）✓ |
| 手动重载不调用 `note_external_reload()` | `every_reload_that_is_not_the_watchdogs_own_announces_itself` 失败 ✓ |

### 11.2 与本文设计的差异

- **`fallbacks` 的探针写法**：线格式由 `webkit_wire_json` 从探测源码推导，返回对象里的三元表达式会被建模成布尔值。所以计数器在探针开头 seed（`if (typeof w.__dshFallbackFrames !== "number") w.__dshFallbackFrames = 0;`），返回字面量里只写 `fallbacks: w.__dshFallbackFrames`。
- **`page_is_busy` 取 `editing && idle`**，而不是 §6.3 里备选的"只看 idle"。只看 idle 会把会话列表里的一次点击算成"有草稿要保"，而那里重载不花任何代价。
- **`every_reload_that_is_not_the_watchdogs_own_announces_itself` 是源码断言**。这条回归是"少调用了一次"，不是"值算错了"，纯函数测不到；断言 `.navigate(` 的处数等于 `note_external_reload();` 的处数加一（看护自己那处就地重置）。

### 11.3 遗留

- **真机验证待做**：README「待实机点击确认」已加第 ③ 条（正看着窗口切会话 → 立刻切走两分钟 → 切回，内容应已追平而不是停在「载入历史…」）。
- **§6.5（重载救不回时重建 webview）未做**：用户实测「⌘R 无效、重启有效」，但现场无 WebContent 崩溃报告（最近一份 09-18 09:52，早于 21:02 的现场），故障态尚未取证。下次复现时先抓 `~/Library/Logs/DiagnosticReports`，再决定要不要加这一级。
- **垫片的 `pending` 表在"页面可见却长期不给帧"时会持续增长**。这正是本文要救的故障态，且每项只是一个小对象；若将来发现内存压力，再考虑给它上限。
