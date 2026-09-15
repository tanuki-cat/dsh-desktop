# 接管前交互确认（v0.4.1 审查第 1 项第 5 条 / 第 14 项）

> 输入：`docs/dsh-desktop-v0.4.1-code-review.md` 的「下一版本的剩余工作」第 1 项。
> 本文记录实现方案与落地结果；审查结论本身不在这里改写。

## 1. 目标

外部 Harness 占用配置端口时，**终止它之前必须让用户当场选择**。此前是「默认不接管 + 配置项切换」：

- `take_over_existing: false`（默认）→ 用系统浏览器打开外部实例，不询问；
- `take_over_existing: true` → 静默 SIGTERM 外部实例并接管，**同样不询问**。

第 2 种情形是审查里唯一还带安全语义的缺口：用户打开一个配置文件开关，就授权壳在之后每次启动时
无提示地结束别人的终端会话 / agent 任务。目标是把这一条变成运行时询问，且**不引入新的 Tauri 依赖**。

> **2026-09-16 修正（第一版把询问也交给了配置项）**：第一版让 `foreign_instance_action()` 在
> `take_over_existing: false` 时直接返回 `UseBrowser`，只有 `true` 才返回 `Ask`。测试与文档都按这个
> 行为写了，但它意味着**默认用户永远看不到面板** —— 选择权被前置成了一道配置题，而审查要求的是
> 检测到外部实例「让用户选择」。实机确认：默认配置下启动桌面端，直接以浏览器形式打开并停在错误页。
>
> 现在询问是**无条件**的：两道身份校验通过就问，`take_over_existing` 只是**没人答复时**的答案。
> 同一处错误也存在于更新前的停止判据（`may_stop_before_update`），已一并改掉。

## 2. 关键约束

| 约束 | 影响 |
| --- | --- |
| 仓库此前只有 `tauri` 与 `tauri-plugin-single-instance` | 不引入 `tauri-plugin-dialog`；对话框依赖会带进新的 capability 面 |
| `capabilities/splash.json` 只给 splash 窗口 `core:default` | 询问面板必须画在 splash 页面上，harness 窗口仍然零 capability |
| splash 页面有严格 CSP（`script-src 'self'`） | 面板由页面自己的 `<script>` 建 DOM；不能用 `innerHTML` 渲染外部文本 |
| 启动流程在后台线程上阻塞执行 | 询问必须有超时，且超时后回落到安全答案，否则无人值守启动会永久挂起 |
| 外部进程的命令行来自 `ps`/PowerShell，内容不可信 | 标签由 Rust 拼装、页面用 `textContent` 写入，绝不经 HTML |

## 3. 实现

### 3.1 事件桥

复用 splash 窗口已有的 core 事件能力（与「重新启动 Harness」按钮同一条路径），新增一个事件：

| 方向 | 事件名 | 载荷 |
| --- | --- | --- |
| Rust → 页面 | （`eval`，不是事件） | `__askChoice(question, status, detail, options, hint)` |
| 页面 → Rust | `dsh-desktop:takeover-choice` | `{ question, id }` |

`question` 是单调递增的问题号。页面把答案连同问题号发回，Rust 只接受**当前**问题号的答案 ——
否则一次跨越重试的点击会被当成新问题的回答。

Rust 侧（`src-tauri/src/window.rs`）：

- `ChoiceOption { id, label }`：`id` 用于匹配，`label` 是用户读到的文字；
- `ask_choice(app, status, detail, options, hint) -> Option<String>`：建问题槽 → 保证状态页在前 →
  `eval` 提问脚本 → 等待答案或超时 → 清槽 → 返回 `id`；
- `wait_for_choice(id, timeout, read)`：纯等待原语，可注入读取器，因此超时与「陈旧答案」都能单测；
- `record_choice(payload)`：事件监听器入口，问题号不匹配就丢弃；
- `CHOICE_TIMEOUT = 120s`：超时即「没答复」。

页面侧（`src/index.html`）：`__pendingChoice` 缓冲 + `__applyChoice()` 渲染，与既有的状态
缓冲同一套写法（`eval` 可能早于 `<body>` 到达）。按钮用 `document.createElement` +
`textContent` 构建，点击后禁用全部按钮并回显「已选择：…」；`invoke` 抛错时改写按钮文案说明
选择没有送达。

### 3.2 决策与四条 kill 路径

`foreign_instance_action(owner, command)` 只看两个身份信号：**都通过**（401 认证栅栏 + 命令行像
`dsh web`）就返回 `Ask { pid }`，否则 `Refuse`。它**不再接收** `take_over_existing`：配置项不参与
「是否询问」，只参与「没答复怎么办」。

`resolve_foreign_action()` 把 `Ask` 变成真正的问题：

1. 拼出问题文案：pid、**完整命令行**、端口、以及**本壳将要使用的 workspace**；
2. 选项（第 3 个只在找到空闲端口时出现）：接管 / 保留并用浏览器打开 / **保留并换端口** /
   什么都不做退出本应用；
3. 等待答案，并按 id 映射回 `TakeOver` / `UseBrowser` / `UseOtherPort` / `Refuse`；
4. 没答复（超时、窗口缺失、页面没加载）→ `unanswered_choice(config.take_over_existing, pid)`。

四条会对外部进程发信号的路径全部接上，且**都以身份为闸门、以答复为准**：

| 路径 | 位置 | 询问方式 |
| --- | --- | --- |
| 启动时检测到外部实例 | `start()` 3c 检测分支 | `foreign_instance_action()` → `resolve_foreign_action()` |
| 交接（插件市场重启）后端口被替代实例占用 | `take_over_handoff_and_start()` | 不在此处询问，交回启动流程统一问（避免同一次重启问两遍） |
| 自启失败后重试前的接管 | 同上，第二个 `match` 分支 | `confirm_takeover()` |
| 更新前停止正在服务该 CLI 树的实例 | `stop_instance_before_update()` | `confirm_takeover()`，不给接管即跳过本次更新 |

`confirm_takeover()` 只有拿到 `TakeOver` 才返回 true：`UseOtherPort` 明确表示「那个实例留着」，
**不是**发信号的许可。

### 3.2.1 「保留并换端口」与重试循环

`UseOtherPort` 在配置端口之上的**第一个空闲端口**启动本壳的 Harness（`free_port_from()`，向上搜
20 个）。它只记在 `PORT_OVERRIDE`（本次启动有效）、**不写回 `config.json`**：会话 cookie 与固定端口
绑定，静默永久迁移比下次再问一次更糟。

`UseBrowser` 与 `UseOtherPort` 都必须**绕开「接管等待端口释放」那段循环**：那段代码的前提是本壳刚刚
信号过一个进程，而这两种答复恰恰是「不碰对方」。为此引入 `took_over` 标志，只有 `TakeOver` 才置位。

### 3.2.2 终态页面：不能让用户点一个必然失败的按钮

`UseBrowser` 走的是 `window::show_notice`（无重启按钮），不是 `show_failure`。`show_failure` 会
`RETRY_OFFERED.store(true)`，而那同时打开了状态页按钮、single-instance 回调与 macOS `RunEvent::Reopen`
三个入口；外部实例还活着、本壳又不会接管它，所以每一次点击都只会重跑一遍注定失败的 `start()`、
**再开一个浏览器标签页**，然后回到同一个页面。实机日志里能看到这个循环（6 次 `restart requested from
the status page`）。判断抽成 `terminal_page(&ForeignAction)` 以便单测。

### 3.3 文案

问题详情里 workspace 是重点：接管会用**本壳配置的 workspace** 重启，而不是继续对方的工作目录。这是
用户最难预料的一点，因此写在详情里而不是日志里。读不到命令行时写 `<读不到命令行>`，让用户知道
壳不知道什么，而不是留空。

**顺序也是文案的一部分**（实机截图后调整）：先写「接管会做什么」（终止会话 + 用哪个 workspace 重启），
再写「要接管的是哪个进程」。小窗口里文本底部会滚出视野，而被滚掉的必须是证据，不是用户真正要判断的后果。

命令行按 `COMMAND_DISPLAY_LIMIT = 160` **掐头去尾**（`elide_command()`）：真实命令行里有两段深路径，
动辄几百字符，而盒子会滚动 —— 滚动就等于藏内容。保留首尾是因为开头说明用的是哪个解释器与安装树、
结尾带着 `--profile web` / `--port` 这些说明它在干什么的 flag，中间那截路径深度正是让它过长的原因。

浏览器回退页额外写明两条用户下一句一定会问的：**没有可重启的 Harness**（所以也没有重启按钮），
以及浏览器需要已有该 authority 的登录 cookie —— 否则会看到 `authentication required`（实测无 cookie
访问根路径就是 401 栅栏）。

### 3.4 布局：小窗口里的取舍

实机截图暴露两处渲染缺陷，都不是逻辑问题，而是「内容比窗口高」时 CSS 的行为：

1. **flex 居中会裁掉溢出**：`body { justify-content: center }` 在内容高于容器时两端一起被裁 ——
   转圈与标题被顶出可视区、说明文字从中间断掉。改成内容自身 `margin: auto`（有空间时居中、没空间时
   塌成 0），页面改为可滚动，于是最坏情况是「需要滚一下」而不是「看不见」；
2. **上边被顶掉**：同一处裁切的后果，随第 1 条一并修掉。

配套的尺寸决策：

- 窗口平时 `SPLASH_SIZE = 460×300`（转圈 + 状态行）；
- 提问时由 `size_for_question()` 长到 `QUESTION_SIZE = 520×560` 并重新 `center()` —— 提问是这个窗口
  承载过的最大内容（标题 + 多行说明 + 四个按钮 + 超时提示），而它不可缩放，尺寸只能从这里来；
  重新居中是因为从左上角长大会顶出屏幕底部，正好藏起用户要按的按钮；
- 详情框在提问态放宽到 `max-height: 240px` 并提高对比度，但**仍然有上限**：命令行可以任意长，
  让它撑开就会把最后一个按钮挤到折叠线以下 —— 那是这个页面上唯一绝不能发生的事；超过上限就内部滚动；
- `#choice` / `#retry` 设 `flex: 0 0 auto`，按钮不为文本让位；
- 空的详情框 `#detail:empty` 隐藏：启动过程中它就是空的，一块灰条看起来像渲染故障。

## 4. 验证

新增单测 11 项（`lib.rs` 8 项 + `window.rs` 3 项），集成测试 5 → 6 项。库内单测合计 116 → 141，
其中 14 项属于更新事务（见另一文档）：

- `an_identified_foreign_instance_is_always_asked_about`：身份矩阵，识别出来就问、**与配置项无关**；
  未识别 / 无命令行仍然 `Refuse`。**负向验证**：把配置闸门加回去，测试立刻以
  `left: UseBrowser, right: Ask { pid: 4242 }` 失败。这正是第一版放行的缺陷；
- `an_unanswered_question_follows_the_config`：没答复 → `true` 接管 / `false` 浏览器；
- `another_port_is_offered_only_when_one_is_actually_free`：先占住一个端口，再向上搜，
  断言结果跳过被占的那个、落在范围之内且真的能 bind；
- `a_port_override_lasts_for_one_launch_only`：`PORT_OVERRIDE` 置位生效、清零后回到配置端口；
- `leaving_the_instance_alone_does_not_offer_a_restart`：`UseBrowser` → `Notice`（无按钮），
  `Refuse` / `TakeOver` → `Failure`（有按钮）；
- `a_long_command_line_is_elided_around_its_middle`：短命令行原样、长命令行首尾保留且长度受限；
- `the_question_leads_with_what_takeover_would_do`：后果段落在证据段落之前，workspace 也在其之前；
- `the_question_gets_a_window_and_a_layout_that_fit_it`：提问窗口高于普通窗口、页面用 `margin: auto`
  而非 flex 居中、提问态详情框有上限、空详情框隐藏。**负向验证**：把 flex 居中改回去，测试立刻以
  「flex 居中会裁掉溢出内容」失败 —— 正是实机截图里的现象；
- `the_takeover_question_names_the_process_and_this_shells_workspace`：文案含端口、pid、命令行、
  workspace；命令行缺失时有明确占位；
- `an_unanswered_takeover_question_follows_the_config_default`：超时返回 `None`；问题号不匹配的
  答案被丢弃；匹配的答案被接受；
- `the_takeover_question_and_its_answer_stay_in_step`：页面含 `CHOICE_EVENT` 字面量、
  `__askChoice`、回传 `question`，且**不使用 `innerHTML`**；
- `the_choice_script_escapes_its_payload`：含引号的标签经 `json!` 转义，且脚本带页面未就绪时的重试；
- `the_takeover_panel_renders_its_options_and_reports_the_click`（`tests/webkit_compat_shim.rs`，**在真实 JS 引擎里跑整张页面**）：
  把 `src/index.html` 的全部 `<script>` 块按文档顺序喂给一个最小 DOM，断言面板可见、重启按钮被隐藏、
  按钮数等于选项数、点击后发出的是 `{question, id}`、所有按钮随即禁用、清空问题后面板收起。
  选项列表用**四项**的那一版（含换端口），并点击第 3 个按钮 —— 索引与 id 的对应关系因此也被覆盖。

  **这一项抓出了上面 4 项都漏掉的真实缺陷**：`__applyChoice` 里选项取自 `pending[2]`（其实是 detail 文本），
  于是面板永远不渲染 —— 而所有字符串断言照样通过，因为字符串确实都在文件里。经**负向验证**确认：把该索引
  改回去，新测试立刻以「the question must show the panel」失败；改回来即绿。这印证了「断言字符串出现在文件里」
  与「页面真的这么工作」是两回事。

**渲染验证（2026-09-16）**：用无头 Edge 按窗口尺寸实拍 `src/index.html`，覆盖四种状态 —— 启动中
（460×300）、失败页（460×300）、提问（520×560，正常命令行）、提问（520×560，Toolbox 深路径的超长
命令行）。前三张正常，第四张暴露出「最后一个按钮被挤出可视区」，于是有了详情框上限与文案重排。
截图也确认了修复前那种「转圈与标题被顶出可视区、说明从中间断掉」的现象不再出现。

门禁：`cargo test` **141 passed / 0 failed**、`cargo fmt --check` 通过、`cargo clippy --all-targets`
0 warning。产物冒烟：`make bundle` 后确认 `.app` 里含换端口选项与新的浏览器回退文案。

**实机复现（2026-09-16，用户报告 → 已确认）**：默认配置下，端口上有别人启动的 `dsh web` 时启动桌面端，
会直接以浏览器形式打开并停在错误页。根因是 3.2 修正的那处配置闸门，日志里的 6 次
`restart requested from the status page` 则对应 3.2.2 的重试循环。

**实机确认功能可用（同日，用户截图）**：面板弹出、四个选项齐全、点击生效。同一次截图暴露了 3.4 的
两处渲染问题（转圈与标题被裁掉、说明文字从中间断掉），本轮的布局改动即针对它。

**未做完整实机验证**：面板本身需要真实的外部 Harness 占住端口才能弹出，而当前沙箱不允许 `ps` /
`lsof`（身份校验的第二道信号读不到），因此无法在这里跑 GUI 冒烟。已能验证的部分：

1. 起一个真实的 `dsh web --port 3199`（隔离 `DSH_HOME`），确认它对无 cookie 请求回 401 认证栅栏；
2. 用它的真实命令行跑 `looks_like_dsh_web` → `true`，`plugin add` 形态 → `false`；
3. 确认清理后 3199 已关闭、用户的 3080 实例不受影响。

留给发布前冒烟：`config.json` 保持默认（不写 `take_over_existing`），在有外部实例的端口上启动，
确认弹出**四**选项面板，并分别验证四个选项与「不点任何按钮等满 120 秒」。

## 5. 影响范围

- `src-tauri/src/window.rs`：`CHOICE_EVENT`、`ChoiceOption`、`Choice`、`ask_choice`、
  `wait_for_choice`、`record_choice`、`choice_script`；
- `src-tauri/src/lib.rs`：`ForeignAction::{Ask, UseOtherPort}`、`resolve_foreign_action`、
  `unanswered_choice`、`confirm_takeover`、`terminal_page`、`takeover_question`、`may_stop_before_update`、
  `runtime_port` / `free_port_from` / `PORT_OVERRIDE`、四条 kill 路径、`CHOICE_EVENT` 监听器；
- `src/index.html`：询问面板（样式、DOM 容器、渲染与回答脚本）；
- 无新依赖、无新 capability、无新配置项。`take_over_existing` 的语义从「**是否**接管」收敛为
  「**没答复时**是否接管」，默认值不变。

## 6. 一处行为变更需要知会用户

默认用户从「什么都不问、直接开浏览器」变成「**先问一次，不答才开浏览器**」。这会多一个最多 120 秒的
阻塞点，但只在真的检测到外部 Harness（且身份确认）时出现 —— 正是需要人来决策的时刻。README 的配置表
与行为表已同步。
