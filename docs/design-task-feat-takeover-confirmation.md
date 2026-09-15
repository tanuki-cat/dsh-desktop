# 接管前交互确认（v0.4.1 审查第 1 项第 5 条 / 第 14 项）

> 输入：`docs/dsh-desktop-v0.4.1-code-review.md` 的「下一版本的剩余工作」第 1 项。
> 本文记录实现方案与落地结果；审查结论本身不在这里改写。

## 1. 目标

外部 Harness 占用配置端口时，**终止它之前必须让用户当场选择**。此前是「默认不接管 + 配置项切换」：

- `take_over_existing: false`（默认）→ 用系统浏览器打开外部实例，不询问；
- `take_over_existing: true` → 静默 SIGTERM 外部实例并接管，**同样不询问**。

第 2 种情形是审查里唯一还带安全语义的缺口：用户打开一个配置文件开关，就授权壳在之后每次启动时
无提示地结束别人的终端会话 / agent 任务。目标是把这一条变成运行时询问，且**不引入新的 Tauri 依赖**。

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

`foreign_instance_action()` 新增一个分支 `Ask { pid }`：**两道身份校验都通过**（401 认证栅栏 +
命令行像 `dsh web`）且 `take_over_existing: true` 时，返回 `Ask` 而不是直接 `TakeOver`。

`resolve_foreign_action()` 把 `Ask` 变成真正的问题：

1. 拼出问题文案：pid、**完整命令行**、端口、以及**本壳将要使用的 workspace**；
2. 三个选项：接管 / 保留并用浏览器打开 / 什么都不做退出本应用；
3. 等待答案，并按 id 映射回 `TakeOver` / `UseBrowser` / `Refuse`；
4. 没答复（超时、窗口缺失、页面没加载）→ 回落到 `config.take_over_existing` 的语义。

四条会对外部进程发信号的路径全部接上：

| 路径 | 位置 | 询问方式 |
| --- | --- | --- |
| 启动时检测到外部实例 | `start()` 3c 检测分支 | `resolve_foreign_action()` |
| 交接（插件市场重启）后端口被替代实例占用 | `take_over_handoff_and_start()` | `confirm_takeover()` |
| 自启失败后重试前的接管 | 同上，第二个 `match` 分支 | `confirm_takeover()` |
| 更新前停止正在服务该 CLI 树的实例 | `stop_instance_before_update()` | `confirm_takeover()`，拒绝即跳过本次更新 |

`confirm_takeover()` 在 `take_over_existing: false` 时不询问直接返回 false —— 那种配置下用户
已经明确要求保留该实例。

### 3.3 文案

问题详情里 workspace 是重点：接管会用**本壳配置的 workspace** 重启，而不是继续对方的工作目录。这是
用户最难预料的一点，因此写在详情里而不是日志里。读不到命令行时写 `<读不到命令行>`，让用户知道
壳不知道什么，而不是留空。

## 4. 验证

新增单测 4 项（`lib.rs` 2 项 + `window.rs` 2 项；另把 `a_foreign_instance_is_only_taken_over_when_it_is_identified`
改名为 `a_foreign_instance_is_only_asked_about_when_it_is_identified`），集成测试 5 → 6 项。
库内单测合计 116 → 134，其中 14 项属于更新事务（见另一文档）：

- `a_foreign_instance_is_only_asked_about_when_it_is_identified`：身份矩阵，`Ask` 取代原先的
  直接 `TakeOver`；未识别 / 无命令行仍然 `Refuse`；
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

  **这一项抓出了上面 4 项都漏掉的真实缺陷**：`__applyChoice` 里选项取自 `pending[2]`（其实是 detail 文本），
  于是面板永远不渲染 —— 而所有字符串断言照样通过，因为字符串确实都在文件里。经**负向验证**确认：把该索引
  改回去，新测试立刻以「the question must show the panel」失败；改回来即绿。这印证了「断言字符串出现在文件里」
  与「页面真的这么工作」是两回事。

门禁：`cargo test` 134 passed / 0 failed、`cargo fmt --check` 通过、`cargo clippy --all-targets`
0 warning。

**未做实机验证**：询问面板需要真实的外部 Harness 占住 3080 才能触发。本机验证方式（未执行，留给
发布前冒烟）：

1. 终端里用另一个 workspace 起 `dsh web --port 3080`；
2. `config.json` 设 `take_over_existing: true` 后启动应用；
3. 确认弹出三选项面板、详情里是那个终端的命令行与本壳的 workspace；
4. 分别验证「接管」「浏览器」「什么都不做」，以及不点任何按钮等满 120 秒的行为。

## 5. 影响范围

- `src-tauri/src/window.rs`：`CHOICE_EVENT`、`ChoiceOption`、`Choice`、`ask_choice`、
  `wait_for_choice`、`record_choice`、`choice_script`；
- `src-tauri/src/lib.rs`：`ForeignAction::Ask`、`resolve_foreign_action`、`confirm_takeover`、
  `takeover_question`、四条 kill 路径、`CHOICE_EVENT` 监听器；
- `src/index.html`：询问面板（样式、DOM 容器、渲染与回答脚本）；
- 无新依赖、无新 capability、无配置项变更（`take_over_existing` 的语义从「是否接管」细化为
  「没答复时是否接管」）。
