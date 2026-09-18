# dsh-desktop 冻结问题审查报告（v0.4.4 / b8ab867）

审查对象：`dsh-desktop` 本地 `main`  
审查提交：`b8ab867`（`fix: 版本闸门前移，identity 按平台分区，并同步文档与 README`）  
审查日期：2026-09-18  
触发：用户报告「模型响应渲染卡住，think 模式尤其容易卡住，退出应用重新启动后恢复正常」  
上一轮审查：[`dsh-desktop-v0.4.4-code-review.md`](./dsh-desktop-v0.4.4-code-review.md)

本文是**审查结论 + 修复任务清单**。修复落地后把结果回写到文末「处理状态」，不改写各条原始结论。

## 1. 总体结论

上一轮（`6d5f58a`，2026-09-15）按「页面还活着但不再刷新」重做了看护，并显式关掉 WebKit 背景节流。**本轮证据表明那次修复没有解决问题**：

- 壳自己的日志证明，节流被关掉之后，窗口失去焦点时**动画帧依然停止调度**（连续数千次探测读到同一个帧计数），而 JS 仍在执行（探测有应答）。看护把这一状态判为 `Unattended`，**什么都不做** —— 这正是「卡住 → 退出重启」的现场。
- 看护的判据分不清「主线程忙」与「页面挂了」，两者都导向重载；日志里 5 次 `Frozen` 有 4 次在 15 s 内自愈，第 5 次触发重载后新页面**立刻又 `Frozen`** —— 说明至少有一部分「停画」是页面自己在忙，而重载把同一段重内容又渲染了一遍。
- think 模式是全链路最贵的一条路径（折叠态仍把整段思考文本放进 DOM、每帧 O(n) 扫描、三连 rAF 才 flush、上游已知单块 markdown O(n²) 重解析），所以卡顿集中在 think 阶段是必然结果，不是巧合。
- 现场证据不可用：日志**没有时间戳**，且 81% 是同一行噪声；渲染进程被杀的原因被写成「多为内存压力」，而本机唯一一份相关崩溃报告是 **JavaScriptCore 主线程断言**，且没有任何对应的 JetsamEvent。
- 恢复手段只有「退出应用」：没有 ⌘R、没有菜单、没有重载按钮。

共 4 个 P1、4 个 P2。全部为**只读审查**，未修改代码。

## 2. 审查方法与证据

- 逐文件阅读 `src-tauri/src/window.rs`、`harness.rs`、`lib.rs` 及对应测试；用依赖源码交叉验证：`tauri 2.11.5`、`tauri-runtime-wry 2.11.4`、`wry 0.55.1`、`muda 0.19.3`。
- 本机运行日志：`~/Library/Application Support/com.deepseek.dsh.desktop/logs/harness.log`（4456 行；壳行 4263；其中 3617 行为 `窗口不在前台，WebKit 可能已停止绘制，本轮不判定`）。
- 崩溃报告：`~/Library/Logs/DiagnosticReports/Retired/com.apple.WebKit.WebContent-2026-09-18-095206.ips`。
- 前端行为：`@deepseek-ai/dsh` **0.1.5-rc.2** 的客户端 bundle（`dsh-api-session-controller`、`dsh-client-ui-chat`、`dsh-client-ui-conversation`、`dsh-web-frontend`）与本机 profile 插件（`dsh-dream-skin`、`dsh-better-sidebar`）。
- 上游证据：[deepseek-harness discussion #5023](https://github.com/deepseek-ai/deepseek-harness/discussions/5023)（单个大 markdown 块每 delta 重解析整篇导致 O(n²) 与渲染进程崩溃）、Agent Note `2026-08-03-opt-in-reasoning-chunk-browser-stress.md`（10 万 reasoning chunk 压测）。
- 运行环境：macOS 27.0（arm64），已安装 `~/Applications/DSH Desktop.app` v0.4.4（binary 2026-09-17 17:10，即含 `b617ab2` + `6d5f58a`），harness pid 5621，`dsh 0.1.5-rc.2`（system 运行时）。
- 基线：`cargo test` 185 passed / 0 failed（另集成 4 passed），`cargo fmt --check` 与 `cargo clippy --all-targets -- -D warnings` 均干净。

## 3. 问题汇总

| # | 级别 | 问题 | 证据 |
|---|---|---|---|
| A1 | P1 | 关掉节流后，窗口不在前台时动画帧仍停止调度，而看护对这一态**刻意不作为** | 日志 3617 行 `Unattended`；`window.rs:1095`、`1489-1495` |
| A2 | P1 | 判据分不清「忙」与「挂起」，重载打在活页面上；`busy` 保护在最需要时失效 | 日志 5 次 `Frozen`，4 次自愈；`window.rs:1118`、`1193`、`1477` |
| A3 | P1 | 日志无时间戳且被噪声淹没，现场不可复盘 | `harness.rs:904`；4456 行中 3617 行噪声 |
| A4 | P1 | 无任何手动恢复入口（无 ⌘R / 菜单 / 重载按钮） | `src-tauri/src` 无菜单 API 调用；tauri 默认 macOS 菜单不含 Reload |
| B1 | P2 | 渲染进程死因被误标为「多为内存压力」，实际是 JSC 主线程断言 | 崩溃报告栈顶 `WTFCrashWithInfoImpl` ← `CodeBlock::setOptimizationThresholdBasedOnCompilationResult`；无对应 Jetsam |
| B2 | P2 | README 与设计文档的表述超出已验证事实 | `README.md:213`、`README.md:383`、设计文档 §13.15 |
| B3 | P3 | `Unattended` 分支注释写「Say so once」，代码每 15 s 打印一次 | `window.rs:1490-1494` |
| B4 | P3 | `attended()` 把「可见但未聚焦」等同于「用户看不到」 | `window.rs:1577-1581` |

## 4. P1 详细发现

### 4.1 A1：节流禁用没有阻止停画，而看护对该状态不作为

**代码事实**

- `window.rs:1268`：`create_harness` 设 `.background_throttling(BackgroundThrottlingPolicy::Disabled)`。
- wry 0.55.1 把它写进 `WKPreferences` 的 KVC 键 `inactiveSchedulingPolicy`，且**只在 macOS ≥ 14 生效**（`wry-0.55.1/src/wkwebview/mod.rs:473-498`）。本机 macOS 27.0，条件满足，设置**确实被写入**（wry 源码可见 `_preference` 就是 `config.preferences()`，而 `config` 在同一函数 `wkwebview/mod.rs:426` 处传给 `initWithFrame:configuration:`）。
- 探测 `FRAME_PROBE`（`window.rs:1132-1148`）在页面里维护 `__dshFrames`，且**最多只排一个待处理帧**：一个待处理回调本身就是「帧停了」的证据，帧一恢复它立刻触发。

**运行证据**

日志里 `窗口不在前台，WebKit 可能已停止绘制，本轮不判定` 出现 **3617 次**，当前会话 628 行壳行中占 626 行。这个分支只在 `state == Frozen` 时进入（`window.rs:1495`），而 `Frozen` 要求**探测有应答**（`judge_frames`，`window.rs:1118-1126`：`None` 是 `Silent`）。也就是说：**JS 一直在跑，动画帧在这段时间里一帧都没画**，从上一个探测周期一直持续到窗口重新获得焦点。

**影响**

用户最常见的形态（DSH 窗口在一侧、焦点在 IDE/终端）下，流式输出不绘制。看护看到 `Frozen` 但因 `attended() == false` 走 `Unattended`，**不重载、不改标题、不提示**，只有一行日志。这与用户描述的「卡住、只能重启应用」完全一致。

**结论**：`background_throttling(Disabled)` 不足以阻止停画（或它阻止的是其中一种，而本机命中的是另一种，例如遮挡/可见性节流）；而「窗口不在前台」恰好是唯一被豁免的状态，所以这条路径永远不会自愈。

**建议**

1. 读回并记录：启动后用 `with_webview` / KVC 读回 `inactiveSchedulingPolicy` 的实际值，写进日志，先把「设置是否落到这个 webview 上」变成事实。
2. `Unattended` 不能再等同于「什么都不做」：窗口重新获得焦点时**立即**判定一次（而不是等下一个 15 s 周期），并在长期停画时把状态写进标题。
3. 不把恢复押在这个私有 KVC 上 —— 恢复路径要有手动入口（见 A4）。

### 4.2 A2：判据分不清「主线程忙」与「页面挂了」

**代码事实**

- `probe_frames`（`window.rs:1588-1596`）与 `probe_activity`（`window.rs:1201-1209`）都是同一个 webview 主线程上的 `eval_with_callback`。主线程被一段昂贵的渲染拖住时，**两个判据同时退化**：帧数不动 → `Frozen`；活动探测超时 → `page_is_busy(None) == false`（`window.rs:1193-1198`）。也就是说「用户刚打了字，别丢未提交的提示词」这层保护，恰好在最需要它的时候失效。
- `Silent` 更连问都不问（`window.rs:1477-1478`）。
- 动作策略：连续 2 次未通过（≥15 s，最多 30 s）即重载（`window.rs:1096-1109`）。

**运行证据**

全日志 5 次 `Harness 页面停止绘制（输入仍有响应），继续观察`：4 次在下一周期记 `WebView 页面恢复绘制（此前 1 次未通过检查、0 次重载）`；第 5 次连续两次后重载，而重载之后**新文档立刻又 `Frozen` 一次**（3814 行 `停止绘制`，3815 行 `恢复绘制`）。15 s 内自愈的比例说明「停画」里有相当一部分只是页面在忙。

**影响**

- 误判会在用户正看长流时重载整个页面，丢掉滚动位置和展开状态，并在最贵的一帧上再叠加一次全量 re-mount。
- 而 think 长流期间用户通常不打字，`busy` 天然为假，所以 think 阶段触发重载的概率最高 —— 与用户「think 模式尤其容易卡住」的观察一致。

**建议**

1. 探测里同时排一个 `setTimeout(0)` 心跳：**定时器还火、帧不火** ⇒ 调度被挂起；**两者都不火** ⇒ 主线程真忙/死了。这两类的正确动作相反（前者应当恢复绘制，后者才该重载）。
2. 页面处于「运行中」时（think 行 `data-state="running"`）不重载，改为延长观察。
3. `LIVENESS_MISSES` 从 2 提到 3（≥45 s），把「只是慢」排除掉。

### 4.3 A3：唯一的现场证据不可用

**代码事实**

- `harness::app_log`（`harness.rs:904-909`）只加 `[dsh-desktop] ` 前缀，**不带时间**。全日志 0 行匹配日期/时间模式。
- `Unattended` 分支注释写「Say so once, then keep the streak as it was」，但条件是 `misses == 0 && reloads == 0`（`window.rs:1492`），而这两个计数在 `Unattended` 分支**从不增加** → 每 15 s 打印一次，永不上锁。

**影响**

- 间歇性冻结无法与崩溃报告、Jetsam、用户操作对齐（本轮只能靠日志行号与进程 pid 推断时间顺序）。
- 真正有价值的 5 条事件被 3600 行噪声埋掉。
- `LOG_LIMIT_BYTES = 5 MiB` + 3 份备份（`harness.rs:29-31`）会更快滚掉现场。

**建议**：给 `app_log` 加本地时间前缀；给 `Unattended` 加闩（一个连续段只打一条），顺带修掉注释与代码不一致（B3）。

### 4.4 A4：没有手动恢复入口

**代码事实**：`src-tauri/src` 里没有任何 Tauri 菜单 API 调用（`Menu` 一词只出现在注释里）；tauri 2.11.5 的默认 macOS 菜单只有 App / File / Edit / View(fullscreen) / Window / Help，**不含 Reload**（`tauri-2.11.5/src/menu/menu.rs:142-232`）；`tauri.conf.json` 也没有快捷键配置。

**影响**：一次冻结的代价是「退出应用 + 等 harness 重新连接」，而页面里其实什么都没有丢（会话在宿主进程里）。

**建议**：加一个 View 菜单项（⌘R）直接 `window.navigate(current_url())`，走与看护相同的路径；这是最轻的恢复手段，也是 A1 的兜底。

## 5. P2 详细发现

### 5.1 B1：渲染进程死因被写成「多为内存压力」

`window.rs:1384` 与 README 行为表都写「WebView 渲染进程被系统结束（多为内存压力）」。本机唯一一份相关报告 `com.apple.WebKit.WebContent-2026-09-18-095206.ips`：

- `responsibleProc = dsh-desktop`，进程生命周期 2026-09-17 17:10 → 09-18 09:52；
- `exception: EXC_BREAKPOINT / SIGKILL`，faulting thread = main thread；
- 栈顶：`WTFCrashWithInfoImpl` ← `JSC::CodeBlock::setOptimizationThresholdBasedOnCompilationResult` ← `JITToDFGDeferredCompilationCallback::compilationDidComplete` ← `JITWorklist::completeAllReadyPlansForVM`；
- `/Library/Logs/DiagnosticReports/Retired` 下没有任何与该时刻对应的 JetsamEvent。

这是 **JavaScriptCore 主动断言**（重 JS 下的崩溃），不是内存压力回收。当前的日志文案会把排查引向内存，而真实方向与上游 #5023 的「重内容打崩渲染进程」同类。另有一份 2026-09-13 的 `cpu_resource.diag`：`com.deepseek.dsh.desktop` 的 WebContent 52% CPU 持续 172 s，栈在 `RemoteLayerTreeDrawingArea::updateRendering` → `Page::layoutIfNeeded` → `RenderLayerCompositor::computeCompositingRequirements`（合成/layout 热点）。

**建议**：文案改为包含 WebKit 崩溃；把报告路径与 `responsiblePid` 一并写进日志，便于事后取证。

### 5.2 B2：文档表述超出已验证事实

- `README.md:213`：「触发条件通常是窗口被 WebKit 判为不活跃（`background_throttling` 默认 `suspend`，本壳已显式设为 `disabled`）」—— 但 2026-09-17/18 的 `Frozen` 事件正是发生在带该设置的 0.4.4 二进制上。
- `README.md:383` 与设计文档 §13.15 承诺「看护会自己重载」，而 `Unattended` 这一态明确不重载。

按仓库文档生命周期规则：已冻结的设计文档不原地改写结论，新建 supersede/修正记录并交叉链接。

## 6. 建议修复顺序

1. **A3**（时间戳 + `Unattended` 上锁）—— 成本最低，先把现场变可读。
2. **A4**（⌘R / 菜单重载）—— 给用户一条比「重启应用」轻得多的恢复路径。
3. **A2**（区分「忙」与「挂起」的探测、提高重载门槛、运行中不重载）。
4. **A1**（读回并记录 `inactiveSchedulingPolicy`；窗口重新获得焦点时立即判定）。
5. **B1 / B2 / B3 / B4**（文案与文档修正）。

## 7. 处理状态

**已全部处理（2026-09-18）**，各项原始结论未改动；与本文建议的差异在 §7.2 说明。

| # | 状态 | 修法 |
|---|---|---|
| A1 | **已修（判据侧）+ 仍需实机确认（根因侧）** | 探测增加 `setTimeout(0)` 心跳：帧不动而心跳在跳 = 被暂停绘制，**不重载**，标题提示、日志每个连续段记一条；`FRAME_PROBE` 改为返回 `{frames,timers,timersSeen}`，`judge_frames` 据此分成 `Suspended`/`Stalled`/`Unknown`。`background_throttling(Disabled)` 保持不动：它能证伪的只是"没生效"，而它在 wry 里确实被写入了（`wry-0.55.1/src/wkwebview/mod.rs:473-498`），所以读回值仍待实机确认 |
| A2 | **已修** | 帧与心跳分离（同 A1）；`LIVENESS_MISSES` 2 → 3（≥45 s）；窗口 `Focused(true)` 让看护立即判定一次（等待由 `sleep` 改为 `recv_timeout`）；`busy` 只在 `Frozen` 时询问且保持"推迟不取消" |
| A3 | **已修** | `app_log` 每条加本地时间戳（Unix `localtime_r` / Windows `GetLocalTime`，时钟不可用只丢时间戳不丢行）；`Unattended` 分支的每周期重复改为"每个连续段一条"，顺带修掉 B3 的注释与代码不一致 |
| A4 | **已修** | 新增应用菜单：默认菜单 + View → 「重新加载界面」（⌘R），`window::reload_harness` 走 `navigate(current_url())`，与看护同一条路径；菜单 id 与处理器共用 `window::RELOAD_MENU_ID` |
| B1 | **已修** | 日志文案改为"WebKit 崩溃或内存压力"并给出崩溃报告目录；标题改为"渲染进程已结束" |
| B2 | **已修** | README 的已知坑、功能列表、行为表三处按实测改写：明确 `background_throttling(Disabled)` **不足以**阻止停画；设计文档新增 §13.19（不原地改写已冻结的 §13.15，按文档生命周期规则交叉链接） |
| B3 | **已修** | 同 A3：闩在连续段上，注释与代码一致 |
| B4 | **已修** | `attended()` 保留但语义收窄：只决定"暂停绘制是提示还是并入重载判定"。窗口重新获得焦点后仍不绘制，页面立即并入 `Stalled` 的重载判定，不会永远停在提示上 |

### 7.1 验证

- `cargo test`：**192 passed / 0 failed**（185 → 192），集成用例不变；
- `cargo fmt --check` 通过；本机与 `x86_64-pc-windows-gnu` 的 `cargo check` 均 0 warning，`clippy --all-targets -- -D warnings` 0 warning；
- `make test-scripts` 通过（`check-runtime-stage.sh 自检通过`）。

新增用例与它们钉住的回归：

| 用例 | 钉住什么 |
|---|---|
| `a_page_that_stops_running_is_reloaded_and_then_reported` | 连续 3 次才重载、预算用尽即报告、坏探测不重置预算、`Unknown` 归入重载一侧 |
| `a_suspended_page_is_named_instead_of_reloaded` | 暂停绘制不花重载预算；**聚焦后仍不画则并入重载判定** |
| `a_stopped_frame_counter_is_split_by_the_timer_that_keeps_running` | 帧/心跳四种组合、`timersSeen=false` 归为 `Unknown` |
| `the_probe_reports_exactly_the_fields_the_shell_parses` | **该用例当场抓到真 bug**：结构体用 `timers_seen` 而页面发 `timersSeen`，会让每次应答解析失败、被读成"渲染进程没了"并触发重载。修法是 `#[serde(rename_all = "camelCase")]` |
| `the_frame_probe_is_es5_and_schedules_at_most_one_callback_of_each_kind` | 探测保持 ES5、帧与心跳各自一次只排一个回调、输出为 JSON |
| `every_shell_line_is_stamped_with_local_time` | 时间戳格式、时钟不可用只丢时间戳、脱敏仍在写入前发生 |
| `the_reload_menu_item_is_the_one_the_handler_watches_for` | 菜单注册与事件匹配共用同一个常量，且带 ⌘R |

### 7.2 与本文建议的差异

- **A1 的"读回 `inactiveSchedulingPolicy`"没有实现**：读回需要 `with_webview` 拿到底层 `WKWebView` 再取 `configuration.preferences`，属于平台专属的不安全调用，而它给出的信息**不改变任何决策**——心跳已经把"是暂停还是卡死"判出来了。留下的是实机确认项，不是代码项。
- **A2 的"运行中不重载"没有实现**：页面没有稳定的"正在流式输出"契约（`data-state="running"` 属于上游 client 插件内部实现，随时可能改名），依据它做重载决策会把壳绑在上游 DOM 上。替代方案是更保守的阈值（3 次 ≈ 45 s）加手动重载入口。
- **A4 的快捷键是菜单加速键而不是全局热键**：⌘R 只在应用菜单存在时生效，这与"给用户一条比退出应用轻的恢复路径"的目标一致，且不引入新的 capability。

### 7.3 遗留

- 真机验证：切后台再切回是否出现 `页面停止绘制：计时器仍在运行` 且不重载；⌘R 是否保留会话。
- macOS 遮挡（occlusion）节流是否独立于 `inactiveSchedulingPolicy`，仍未证实（见 §4.1）。
- think 路径本身的渲染成本（折叠态仍渲染整段文本、三连 rAF、上游 #5023 的单块 O(n²)重解析）**不在壳的修复范围**：壳只减少了误重载并提供了手动入口。

## 8. 修复落地后的追加发现（2026-09-18）

修复装进 `.app` 后实机启动，**当场暴露两个此前不知道的缺陷**，都属于"探测与解析不是同一份线上格式"。
两个都已修复并加了能真正抓住它们的用例。

### 8.1 我自己引入的：帧探测被双重编码，看护把好页面当死页面

**现象**：装上含 A1/A2 修复的构建后启动，日志（16:09–16:12）连续出现
`Harness 页面没有响应存活检查`，每 3 次重载一轮，3 轮用尽后 `交给用户处理`。
即新判据**一次都没生效** —— 页面全部答成 `Silent`（无应答），而不是 `Suspended` 或 `Stalled`。

**根因**：`FRAME_PROBE` 写成 `return JSON.stringify({...})`。wry 的 `eval_with_callback` 把脚本
**求值结果**用 `NSJSONSerialization` 序列化成 JSON 再交给回调（`wry-0.55.1/src/wkwebview/mod.rs:735-751`），
所以返回一个字符串时，壳收到的是**双重编码**的 `"{\"frames\":…}"`，`serde_json::from_str::<Frames>` 必然失败 →
`None` → `Silent` → 重载。旧探测返回裸数字 `w.__dshFrames`，序列化一次正好是 `5`，所以从来没暴露这个问题。

**为什么测试没抓住**：用例断言的是探测**文本**里含有 `frames:` / `timers:` 等字样，并另写一段手搓 JSON
喂给解析器。两边各自自洽，中间那层真实线格式没人验证。

**修法**：探测直接 `return { ... }` 对象字面量。测试改为**从探测源码推导线格式**
（`webkit_wire_json()` 解析 `return {` 到 `};` 之间的字段），并显式断言双重编码的输入必须解析失败。
变异测试确认：把 `JSON.stringify` 加回去，两个用例同时失败。

### 8.2 既有的：活动探测同样双重编码，"正在输入就不重载"从来没生效过

**发现方式**：修 8.1 时按同一模式排查另外两个探测，发现 `ACTIVITY_PROBE` 也写成
`return JSON.stringify({ editing: editing, idle: idle })`。

**影响**：`page_is_busy` 的解析同样必然失败，于是它**永远返回 false**。也就是说
`LivenessAction::Busy` 这条分支在生产里从未被执行过 —— "焦点在输入框、或最近 15 s 内有按键就推迟重载，
避免丢掉未提交的提示词"这个保护是**死代码**。引入于 `4068408`（v0.4.1 审查修复）。

**为什么测试没抓住**：同 8.1，用例直接喂 `r#"{"editing":true,"idle":900000}"#` 这类手写 JSON，
而真实线格式是带外层引号的字符串。

**修法**：同样改为返回对象；测试改为从探测源码推导字段名与线格式，并断言双重编码不得被当作 "busy"。
变异测试确认：把 `JSON.stringify` 加回去，用例失败。

### 8.3 教训（写给后续改动）

`eval_with_callback` 的契约是「返回**值**，由 WebKit 序列化」，不是「返回 JSON 字符串」。
任何新的页面探测都必须：① 直接返回对象；② 用例从探测源码推导线格式，不手搓 JSON；
③ 断言双重编码被拒绝。三条都做，才不会再出现"两边测试都过、线上全挂"。

**验证**：单测 196（新增/改写的两个用例各自经变异测试确认能抓住回归）；
`cargo fmt --check`、host 与 `x86_64-pc-windows-gnu` 的 `clippy -D warnings` 均 0 warning。

