# dsh-desktop

DeepSeek Harness 的 Tauri 桌面壳：启动 `dsh web`、捕获启动 URL、用系统 WebView 承载 UI、退出时回收子进程，
并在每次启动前检查/安装 dsh 核心的新版本。

> 许可证：MIT（见 `LICENSE`）。
>
> **发行形态**：每个平台都出两种产物 —— **自带运行时版**（文件名带 `-bundled`，随包带 Node + dsh + 插件市场，
> 目标机器无需预装任何东西）与**精简版**（只有壳，要求机器上已装 `node` 与 `dsh`）。两者都由
> `.github/workflows/release.yml` 在打 tag 时产出，见[发布](#发布github-actions)与
> [自带运行时](#自带运行时已并入-main)。
>
> **macOS 的 WebView 下限**：dsh 的前端在更旧的引擎上会在加载期抛 `Can't find variable: Iterator`
> （随包 document-preview 插件里 pdf.js 的模块级补丁先读方法、后判全局）。壳会在打开界面前探测缺失的
> 能力，**能补的自己补上**（`Iterator`、`Promise.try`、`Promise.withResolvers`、`Symbol.dispose`、
> `Math.sumPrecise`、`Uint8Array.fromBase64`、`Object.hasOwn`、`findLast`），于是 **Safari 16.4
> （macOS 13.3）以上都可以用原生窗口**；只有缺 `class static block` 这类补不了的语法（macOS ≤ 12）
> 才**改用默认浏览器打开界面**（就是 `dsh web` 一直在用的那条路，浏览器有独立的 JS 引擎、会持续更新），
> 并在状态窗口里写清原因 —— 不会再把用户丢给 harness 那句 `Failed to load plugins`。
> 其中 `Math.sumPrecise` 是**所有** Safari 都没有的 API（随包 PDF 写路径一直因此抛错），所以现代系统上
> 也会注入这一小块。
>
> **三个不同的下限，别混为一谈**：
>
> | 说的是什么 | 下限 | 由谁决定 |
> |---|---|---|
> | 精简壳能装、能启动 | macOS 10.15 | `tauri.conf.json` 的 `minimumSystemVersion` |
> | **自带运行时**版能装、能启动 | macOS 11.0 | 随包 node 22 的 `minos`；`make bundle-bundled` 用 `--config` 覆盖。声明 10.15 会「能装、能开壳、一起 harness 就崩」 |
> | 界面能用**原生窗口** | Safari 16.4（macOS 13.3） | 壳在打开界面前的运行时能力探测 + 兼容层 |
>
> 低于第三个下限不是「不能用」：壳会改用系统浏览器渲染界面，自己继续监管 harness（见[已知坑](#已知坑)）。
>
> **分支策略**：自带运行时开发用的 `feat/bundled-runtime` 已合并回 `main`（2026-09-13），
> 后续开发直接在 `main` 上进行；该分支只作历史快照保留。

设计依据：[`docs/design-task-feat-dsh-tauri-desktop-shell.md`](docs/design-task-feat-dsh-tauri-desktop-shell.md)
（正文为设计，实施偏差与验证证据见其 §13.1–13.4，Harness 退出后的自动恢复见 §13.14、绘制看护见 §13.15）。
WebView 兼容层：[`docs/design-task-feat-legacy-webkit-compat-layer.md`](docs/design-task-feat-legacy-webkit-compat-layer.md)
（取代 [`docs/design-task-fix-webview-compat-audit.md`](docs/design-task-fix-webview-compat-audit.md) 中"缺能力即改用浏览器"的结论）。

## 当前状态

**已实现**

- locator：解析 dsh 启动器、其真实 `lib/bin.js`（穿软链）、以及 node（GUI 启动没有 Homebrew PATH）
- 启动：launcher flag 顺序、显式 cwd（agent workspace）、`--patch` overlay 强制 `printUrl`、独立进程组
- 输出：持续读 stdout/stderr、**凭据脱敏**写日志（launch token、`Bearer`、`api_key`、`Cookie`/`Set-Cookie`、`password`、`secret` 等，含 JSON 与带引号的写法，见下）、5MB × 3 份轮转（**计入即将写入的字节**）、环缓冲 200 行 / 1 MiB 用于错误页。每行只脱敏一次；**单行上限 256 KiB**，超限继续排空到换行但不扩张内存；非法 UTF-8 字节不会中断后续行（两者见下）
- URL：解析首个 token（对 `(LAN: ...)` 后缀健壮）、首启 90s / 常态 30s 超时
- 实例：固定端口；探测 → 复用自己上次的实例 / 接管外部 Harness / 占用时错误页
- 生命周期：退出即 SIGTERM 进程组（5s 后 SIGKILL）并删除 state.json —— 关闭窗口（`RunEvent::ExitRequested`）
  与 ⌘Q / Dock 退出（`RunEvent::Exit`）两条事件链都处理；复用的上次实例也登记，退出时一并停掉
- 启动失败即收尾：等待 URL 超时或窗口创建失败时，先停掉刚起的进程再报错，不留"假失败 + 端口被占"
- **绘制看护**：Harness 窗口每 15s 被问一次"你还在画吗"（`eval_with_callback`，5s 超时）。页面里的探测脚本维护**两个**计数器并连同一句**自我可见性**一起报回来 —— **动画帧**、**`setTimeout(0)` 心跳**、**`document.visibilityState`**（帧计数走**未被包裹的原生调度器**，否则会被渲染兜底自己顶动、把停画的页面报成健康）：帧不动但心跳在跳 = 任务队列还活着、只是不被绘制（WebKit 对被遮挡/最小化的窗口就是这样），页面完好，**不重载**，只在标题与日志里说明；帧和心跳都不动 = 主线程忙或挂了；**完全没应答 = 渲染进程没了**。后两种连续 3 次才重载当前 URL（最多 3 次）。"有没有人在看"以**页面自己报的可见性**为准（拿不到才回退到窗口状态），因为实测真正的闸门是**遮挡**而不是焦点。聚焦窗口会让看护立刻判定一次，而不是等下一个周期。macOS 上另有 `on_web_content_process_terminate` 回调，渲染进程一结束就立刻重载。加载完成标题自动恢复；3 次仍不回来则停在"页面已停止刷新，请重启应用"并记日志
- **渲染兜底**：建 Harness 窗口时注入一段 ES5 垫片，**包裹** `requestAnimationFrame`：页面自报 `hidden` 且 250ms 内没有原生帧时，用定时器顶一帧并取消那个已无意义的原生帧。垫片持有**全部在途请求**并监听 `visibilitychange`，所以**在窗口被遮挡之前就排好队的那一帧也会被补上** —— 前端的刷新是一条帧接帧的链，少一环整条就停，而"正看着窗口切会话、然后去看编辑器"恰好命中这一环。被遮挡期间流式输出因此继续推进，切回窗口时内容已经追平，不必重载；重新可见会撤掉还没触发的兜底（原生帧更好）。垫片同时把原生 `requestAnimationFrame` 留在 `__dshNativeRaf` 上供看护探测使用。可见页面上垫片不建任何定时器；`config.json` 的 `render_fallback: false` 可关掉它
- 看护线程：启动成功后继续 `wait` 子进程，Harness 意外退出会清掉 state.json 并**自动把它拉起来**：先给 8s（崩溃/被杀只给 2s）等一次"交接"——插件市场的「立即重启」就是宿主干净退出、由 detached helper 在同一端口拉起替代进程；端口重新服务时先停掉那个替代进程（它的 cwd 是 CLI 目录、不是本壳配置的 workspace），再按本壳的 runtime/workspace/凭据启动自己的实例，成功后直接把窗口切到新的 token URL。连续 3 次短命重启都没稳定下来才弹终态状态页；运行满 60s 记一次健康、计数归零。状态页带「重新启动 Harness」按钮（同一进程内重跑启动流程），此时另起一个实例（single-instance 回调）或点 Dock 图标（macOS 的 `RunEvent::Reopen`）也等价于按这个按钮；浏览器回退页没有按钮、也不响应这两个入口
- 残留自愈：只有强杀/崩溃这类拿不到回调的场景才靠下次启动清理；state.json 0600。记录的 pid 仍监听记录的端口时才发信号；仍服务**本次端口**的记录会保留，让启动流程直接复用而不是重启；pid 已不拥有该端口时，只有"命令行含 `--profile web`/`dsh`，且父进程已消失或由 launchd / `systemd --user` 接管"的残留才会被清理（pid 复用、以及别人正在跑的会话都不动）
- 安全：Harness 窗口零 capability、导航限定当次 authority、外链与 `window.open` 只把 `http`/`https` 交系统浏览器（`file:`、自定义 scheme 记 `external scheme blocked` 后丢弃）
- CSP：本地 splash 页面走 `tauri.conf.json` 的 `app.security.csp`（无 `unsafe-inline`/`unsafe-eval`，Tauri 为页面内联脚本自动补 sha256、为样式补 nonce）。**作用范围仅限本地资产**：harness 窗口加载的是 `http://127.0.0.1:<port>/`，Tauri 不参与该请求，也就无法给它加 CSP —— 那一边靠的是零 capability + 导航围栏。
- 下载：`on_download` 落盘到 `~/Downloads`，同名文件自动加 `-1`/`-2` 后缀（不静默覆盖）；
  附件上传由 wry 的 `runOpenPanel` 原生处理
- **升级**：每次启动查 registry（默认取 `latest` + `next` + `alpha` 中最高者）—— dsh 核心有新版、且**在已测试区间内**时才装并重启实例；
  用户自己装的那棵树默认只提示（`system_updates: notify`）。profile 里的插件市场 `dshmarket` 走同一套机制
  （自己的缓存窗口，装完同样重启让新插件生效），但默认关闭（`auto_update_plugins: false`）；
  npm 子进程显式带上 node 所在目录的 PATH（否则 GUI 启动下 `#!/usr/bin/env node` 必然 exit 127），
  且整个子进程有**进程级超时**（`--fetch-timeout` 只管单次请求；代理挂起、锁等待、生命周期脚本都可能
  让它永不返回，而那时 harness 已经被停掉了）
- 打包：`pnpm tauri build --bundles app` → `DSH Desktop.app`

**验证**

- 实机（2026-09-13，macOS）：接管外部 Harness → 自启并拿到 token URL → WebView 内 token→cookie 成功，
  会话列表/文件卡片/输入框正常渲染；`state.json` 与端口监听者一致；日志 0 行明文 token。
- Harness 意外退出的自动恢复（2026-09-15 实现；证据来自 2026-09-14 的真实日志，**实机演练待做**）：
  日志里插件市场的 `{"event":"restart","detail":"scheduled pid=99772 helper=1726"}` 正对上壳的
  `Harness pid 99772 exited unexpectedly (code Some(0))` —— 用户在市场点「立即重启」后，壳把这次
  干净退出当成崩溃：销毁窗口、弹终态报错页，而替代实例随后已在 3080 上服务，用户只能退出应用重开
  （重开还会 SIGTERM 掉那个替代实例）。现在改成自动恢复：干净退出先等 8s 交接，端口回来就停掉替代实例
  并按本壳的 workspace 重启；崩溃/被杀 2s 后直接自启。历史上那条 `kill -9` 记录
  （`Harness pid 94329 exited unexpectedly (code None)`）同样落在"2s 后自启"这一支。
  实测本地 `dsh web` 从 spawn 到打印 URL 约 4s、SIGTERM 退出码为 0（`node lib/bin.js --profile web
  --no-open --port 3099`，临时 `DSH_HOME`）；策略分支由 `exit_action` / `exit_reason` 单测覆盖。
- 退出路径实机复现（2026-09-13，macOS）：⌘Q 等价的 Apple Event 退出后，日志出现
  `stopping Harness pid 92619` / `Harness stopped`，3080 端口释放、`state.json` 删除；
  修复前只处理红点关闭（`ExitRequested`），⌘Q 走的是 `RunEvent::Exit`，清理从未执行。
- 插件市场自动更新实机验证（2026-09-13，macOS，构建的 `.app`）：启动后日志依次出现
  `plugin update available: dshmarket 1.45.1 -> 1.46.1, installing`、`plugin updated: dshmarket 1.45.1 -> 1.46.1`，
  随后 harness 重启并正常服务；profile 的依赖范围被改写为 `^1.46.1`、`node_modules` 内实装 1.46.1，
  且 `plugin-check.json` 与核心的 `update-check.json` 各自独立（时间戳与内容互不影响）。
- 离线测试：`cargo test` **218 passed**，另有 **7 个集成用例**（在真实 node 引擎里跑兼容层与探测脚本）。单测覆盖：URL 解析含 LAN 后缀、凭据脱敏（`Bearer`/`api_key`/`Cookie` 等）、状态文件往返、locator 软链解析与
  `DSH_DESKTOP_DSH` 优先级、6 个 semver 比较用例、Linux `ss` 输出解析、judge 判定、缓存新鲜度规则、
  缓存落盘往返与旧缓存文件兼容（含新增的 tags 字段缺失时判为不新鲜）、"装到别处 → 同窗口与跨窗口都不重复安装"、
  **标签选择**（默认覆盖上游全部渠道、alpha 可以是最新版本、alpha 落后时 latest 仍胜出、
  `next` 是唯一比已装版本新时判为可更新、registry 没有该标签时只要另一个匹配就不算失败）、
  npm PATH 前缀、端口探针超时（沉默对端与持续发送对端两类）、
  package.json 版本解析、部分/损坏 config.json 处理、workspace / dsh_path 回退不改文件、HOME 缺失不回落 `/`、
  进程组终止与僵尸进程识别、外链 scheme 白名单、页面加载重试判定矩阵、自愈三分支与
  "Keep ⇒ 进程组终止"的组合断言、孤儿残留的 `ps` 判定与父进程分类（launchd / `systemd --user` / 父进程已消失 / 活着的会话）、
  更新前停止模式、
  日志运行中轮转与备份份数、日志句柄共享、计数器不提前轮转、spawn 路径校验点名、
  下载重名避让、URL 解析兜底、CLI 版本区间判定、Harness 退出后的恢复策略矩阵
  （干净退出给长交接等待 / 崩溃给短等待、连续自动重启预算与"健康运行后计数归零"、退出码与信号的人话文案、
  状态页按钮与壳监听的事件名一致、兼容层的判定矩阵与逐块选择 / 探针的语法探测 / 兼容层 ES5 纪律、
  候选 dsh 的身份判定与"无法识别只在答出版本时采用"、核心更新闸门、播种四态与插件跳过文案、
  绘制看护策略矩阵（帧计数代数：同值=停帧、变化=在画、变小=新文档、无应答=渲染进程没了；
  停帧再按**计时器心跳**分成"帧停心跳在跑=被暂停绘制（不重载）"与"两者都停=主线程忙或挂了（重载）"，
  未表态的引擎归为后者；一次坏探测只观察、连续三次才重载、重载预算用尽即报告、后台窗口不算故障、
  重新可见后仍不绘制会并入重载判定、"有没有人在看"以页面自报的可见性优先、窗口状态只作回退、
  探测脚本保持 ES5 且帧与心跳各自一次只排一个回调、
  重载前的"正在输入"宽限有上限、每处不属于看护自己的 navigate 都通知看护重置读数、
  渲染兜底（原生帧不来时兜底只投递一次、迟到的原生帧不二次投递、取消能撤销未触发的兜底、
  可见时一个定时器都不建、重复注入不叠加包裹、间隔来自 RENDER_FALLBACK_MS 而非写死、
  遮挡前排队的帧由 visibilitychange 补上、重新可见后撤掉未触发的兜底）、
  探测只数原生帧（把它改回走包裹版 rAF，用例会在真引擎里看到"一帧没画、计数照涨"而失败）、
  探测脚本与解析结构体是同一份线上格式（字段大小写不一致会把每次应答变成"无应答"）、
  手动重新加载菜单项与处理器用同一个常量绑定且带 ⌘R、壳日志每条带本地时间戳且时钟不可用时只丢时间戳）、
  外部输出的内存边界（单行超 256 KiB 截断且其后仍继续读、非法 UTF-8 字节不再中断整个流、空行不算流结束、
  CRLF 剥 `\r`、环缓冲字节预算、环缓冲只存已脱敏内容、单条巨型日志不越轮转上限、空日志不为一条超限行轮转、
  子进程管道有界且仍被读空、`ps` 三种答复（超时/无输出/有父进程）的分类不混淆、
  同一行多个凭据全部脱敏且中文/emoji 前缀不 panic、散文与 `tokenizer` 不被误脱敏、`Basic`/`Digest` 方案、
  首更无备份仍可回滚且旧记录不误删、组长已退出仍清理进程组、孙进程持有管道时按时返回、
  符号链接快照与恢复（含真实 profile）、`fromBase64`/`sumPrecise`/`reduce` 在真实 node 引擎里跑通、
  JSON / 带引号的值 / `key = value` 脱敏且截断行不漏、满字段长行的脱敏保持线性、`netstat` 只认监听行
  （出站连接与 UDP 不算、状态词本地化无影响）、同一版本连续失败时重试间隔递增、进程被杀后的回滚也记失败、
  登录 shell 的 PATH 参与定位、重启不重复询问已回答的问题、被保留实例只阻止它可能在用的那棵树的切换、
  未读完的输出丢掉残行、
  滚动锚定垫片（ES5 纪律与单次安装、按 UA 只在 WebKit 上安装而放过已会补偿的 Chromium/Gecko、
  上方内容收起/展开后读者正在看的行留在原处且偏移按插入高度补偿、第二次回调不再补一次、
  跟尾与贴底留给页面、读者滚动不被抵消））；
- 兼容层在真实 JS 引擎里跑通（2026-09-15）：`webkit_compat_shim` 集成测试把清单里的 8 个 API 先删掉
  （`Symbol.dispose` 在 node 里不可删、已在测试里注明），注入后用断言跑行为 —— pdf.js 那条
  `Iterator.prototype.join` guard 不再抛、`Iterator.from(...).map(...).filter(...).toArray()` 链式可用、
  `instanceof Iterator` 不抛、`Promise.try`（含同步抛→reject）、`Promise.withResolvers`、
  `Math.sumPrecise`、`Uint8Array.fromBase64`、`Object.hasOwn`、`findLast` 全部通过，重复注入是 no-op；
  同一文件里还有一条在 node 里跑**探针本体**的用例（删掉 `Iterator`/`Promise.try` 后断言 `missing` 里
  有它们、`syntax` 为空、事件名与壳的常量一致）。
  **旧 macOS 真机（issue #1 的 15.0.1）仍待报告者验证**；
  合并自带运行时后另有：运行时决策矩阵与能力门槛、
  影子前缀与版本仲裁、`Origin::Env` 不算自带、播种与首启超时、更新后切树回读、
  插件市场 profile 判定与插件缓存隔离）。
- 权限：`config.json`（`env` 字段可能放 API Key）与 `state.json` 在 Unix 上为 0600，Windows 上为「去掉继承 + 仅当前用户」的显式 ACL；旧版本留下的 0644 文件会在下次启动时被就地收紧（实机已确认）。
- 更新链路的失败证据也来自实机日志：修复前每次 GUI 启动都记 `npm view 失败: env: node: No such file or directory`
  （见设计文档 §13.8）。
- 联网测试：`make test-live` → registry head = 0.1.5-rc.2 且不降级；**冷查询 1168–1217 ms → 缓存命中 0 ms**。
  另有一条专门复现 GUI 失败场景的回归测试（把进程 PATH 换成不含 node 的值、npm cache 指向临时目录）：
  先断言裸调 `npm` 会 exit 127，再断言走修复后的路径能正常查到 registry。

**待实机点击确认**：附件上传、下载、`window.open` 实际效果、macOS TCC 授权归因，以及 2026-09-18 修复
的两条路径 —— ① 手动重载（菜单 View → 重新加载界面 / ⌘R）真的重新加载当前会话；② 用 IDE 盖住 harness
窗口 5 分钟再切回：日志出现 `页面停止绘制：计时器仍在运行`、标题出现"已暂停绘制"、**没有**触发重载，
且切回时内容**已经追平**（这是渲染兜底要证的那条）；③ **正看着窗口切换会话，立刻切到别的应用停留两分钟
再切回**：内容已经追平而不是停在"载入历史…"（这是遮挡前排队的那一帧要证的）。
（旧行为是每个周期记一条"窗口不在前台…本轮不判定"，在真正停画时什么都不做。）

**平台**：`make` 的构建入口只覆盖 macOS 与 Linux（其他平台在解析阶段直接报错）。Windows 走
`.github/workflows/windows-portable.yml` 的原生 staging + 免安装包（已实机验证，见下一节），
`process.rs` 的存活探测用的是 `OpenProcess` + `GetExitCodeProcess`。本机可用
`cargo clippy --target x86_64-pc-windows-gnu` 对 `cfg(windows)` 代码做回归（只检查类型与 lint，不能运行 PE）。

**未做**：签名与公证（对外分发必需）、多 workspace 切换 UI、"回退 + last-known-good"（方案 §2.3 规则 2，
即更新事务/原子切换/失败回滚）。这三项都不在代码层可独立完成：前两项需要开发者证书，第三项是独立特性。
**自带运行时（打包 Node/dsh）**已随 `feat/bundled-runtime` 并入 `main`（2026-09-13）：macOS `.app`
本机实测可跑、Windows 免安装包实机验证通过，见下方[自带运行时](#自带运行时已并入-main)一节。

## 构建（Makefile，支持 macOS 与 Linux）

`make` 是统一构建入口；平台由 `uname -s` 自动判定（`Darwin` → `app`，`Linux` → `deb`）。

| 目标 | 作用 | 备注 |
|---|---|---|
| `make` / `make help` | 列出全部目标 | 默认目标 |
| `make doctor` | 检查工具链与平台依赖 | Linux 用 `pkg-config` 查 `webkit2gtk-4.1` / `gtk+-3.0` / `libsoup-3.0` 并给出 Debian/Fedora/Arch 安装命令；macOS 提示装 Xcode CLT |
| `make check` | `cargo check --all-targets` | 与 CI 口径一致 |
| `make fmt` / `make fmt-check` / `make clippy` | 格式化 / 只检查格式（CI 门禁用，不改工作区）/ lint（`clippy -D warnings`） | |
| `make test` | `make test-scripts` ＋ 离线单元测试（当前 **203** 个）＋ 在真实 JS 引擎里跑兼容层的集成测试 | 不联网；集成测试需要 PATH 上有 `node`，**没有就失败**（曾经的静默跳过会让 3 个用例 0 断言地报 ok） |
| `make test-scripts` | `scripts/` 自检：用假目录验证 staging 闸门本身仍然会失败 | 纯 shell，不依赖构建产物；`make test` 会先跑它 |
| `make test-live` | 联网集成测试（全部） | 自动设 `DSH_DESKTOP_LIVE_TESTS=1`：查真实 registry、对比冷/热缓存耗时，并在「PATH 里没有 node」的模拟 GUI 环境下验证 npm 仍可运行 |
| `make dev` | 运行 debug 版 | 等价 `cargo run --manifest-path src-tauri/Cargo.toml` |
| `make build` | 编译 release 可执行文件 | 产物 `src-tauri/target/release/dsh-desktop` |
| `make bundle` | 打包安装包 | macOS → `.../bundle/macos/DSH Desktop.app`；Linux → `.../bundle/deb`；依赖 `node-deps`（自动 `pnpm install` 拉 `@tauri-apps/cli`） |
| `make run` | 构建后启动 | macOS 用 `open` 打开打包产物；Linux 直接跑 release 二进制 |
| `make icons` | 由 `icon.png` 重生成 `icon.icns` | 仅 macOS（`sips` + `iconutil`）；Linux 打包直接用 png |
| `make icon-art` | 由 `src-tauri/icons/make_icon.py` 重绘 `icon.png` | 自绘小黑鲸，需 `python3` + Pillow；改设计改脚本，不要手改 png |
| `make runtime-fetch` | 下载官方 Node，并**对照仓库里的 `src-tauri/runtime.lock`** 校验 SHA256 | 缓存到 `.runtime-cache/`（48 MB，可复用）；只校验下载来的 `SHASUMS256.txt` 挡不住清单被换 |
| `make runtime-stage` | 组装 `src-tauri/runtime/`：Node + dsh 树 + pnpm + profile 模板 + 许可清单 | 约 **520 MB**；先清空整个 `runtime/`（只留 README.md），再跑脚本自检 + 必需文件/悬空链接/**绝对链接**/quarantine/**文件数与体积**闸门 |
| `make bundle-bundled` | `runtime-stage` + 打包 | 产出**无需预装 node/dsh** 的安装包（macOS `.app` 约 598 MB，Linux `.deb` 同量级）；macOS 上额外把 `minimumSystemVersion` 覆盖为 **11.0**（随包 node 是 `minos 11.0`）。带 `TARGET=` 交叉编译时按**目标**架构 stage 运行时（`x86_64-apple-darwin` → darwin-x64） |
| `make runtime-clean` | 回收 staging 与下载缓存 | 保留 `runtime/README.md` |
| `make clean` | 清理构建产物 | `cargo clean` |
| `make distclean` | 连依赖缓存一起清理 | 额外删 `node_modules`、`.pnpm-store`、`.cargo-home`、`src-tauri/gen` |

可覆盖变量：`CARGO=`、`PNPM=`、`BUNDLE_TARGETS=`（如 `make bundle BUNDLE_TARGETS=appimage`）、
`CARGO_HOME=`、`PNPM_STORE=`——后两个留空即用工具自身默认，便于沙箱/CI 把缓存重定向到工作区。

### 源码与测试布局

`src-tauri/src/` 里每个模块的单元测试都放在它自己的 `tests.rs`，模块文件只留一行声明：

```
src-tauri/src/lib.rs            -> src-tauri/src/tests.rs
src-tauri/src/window.rs         -> src-tauri/src/window/tests.rs
src-tauri/src/harness.rs        -> src-tauri/src/harness/tests.rs
```

`tests.rs` 是模块的子模块（`#[cfg(test)] mod tests;`），因此仍能用 `use super::*;` 够到私有项，
只编译进测试目标。测试跟着它断言的那个模块走：判断"哪个进程才是自家 Harness"的用例在
`identity/tests.rs`，接管其它 Harness 的问答在 `takeover/tests.rs`，更新事务的编排在
`update_flow/tests.rs`。跨模块的组合断言（例如"Keep ⇒ 进程组终止"）留在 `src/tests.rs`，
因为只有那里两个模块都在作用域里。

集成测试另放 `src-tauri/tests/`（`make test-live` 才联网），`scripts/` 的自检放在
`scripts/tests/`。

### 已知坑

- **`bundle.resources` 只由 `make bundle-bundled` 声明**（`--config` 注入），基座 `tauri.conf.json` 里没有它：
  常开这条 glob 会把残留的 staging（实测 520 MB）静默打进普通 `make bundle` 的包 —— 合并前两边各自吃过一次这个亏。
  因此 `make bundle` 与 `src-tauri/runtime/` 无关，自带运行时版必须走 `make bundle-bundled`；
  该目标要求 `src-tauri/runtime/` 里至少有一个文件，`README.md` 就是那个 marker（`.gitignore` 只忽略 payload）。
- `make bundle` / `make run` 需要 `pnpm`；只跑 `make dev` / `check` / `test` 不需要。
- Linux 首次构建前请先 `make doctor` 装齐系统依赖；即便将来采用自带 Node/dsh 的发行方式，
  **WebKitGTK 仍来自系统**（见后续实施计划）。
- `make test-live` 与首次 `make bundle` 需要网络（查 registry / 拉 Tauri CLI）。
- **旧 WebView 上的界面**（`Failed to load plugins` / `Can't find variable: Iterator`）：随包的
  `dsh-client-ui-sidebar-documentpreview` 内联 pdfjs，其中给 `Iterator.prototype.join` 打补丁的那行
  先读方法、后判全局，而 `Iterator` 全局是 **Safari 18.4** 才有的（macOS ≤ 12 拿不到，13/14 需要装
  Safari 18.4 更新）。壳现在在打开界面前探测缺失的能力：**可补的在 harness 窗口里注入兼容层**
  （ES5、逐块自守卫、只装缺的那些，见 `window.rs::compat_script`），于是 Safari 16.4（macOS 13.3）以上
  都能用原生窗口；**补不了的**（目前只有 `class static block` 语法）才改用默认浏览器打开界面
  （`window.rs::WebviewReport` + `lib.rs::hand_the_gui_to_the_browser`），状态窗口里保留 harness 的
  管理职责（更新、退出清理），关掉它就停 harness。兼容层可用 `"webkit_compat": false` 关掉，关掉后
  行为等同旧版（缺任何能力都走浏览器）。彻底修法仍在上游的插件包里（那行 guard 加个全局判断）。
  完整证据与设计（含为什么推翻 2026-09-14"不在 WebView 里打补丁"的结论）见
  [`docs/design-task-feat-legacy-webkit-compat-layer.md`](docs/design-task-feat-legacy-webkit-compat-layer.md)。
  Intel 机器更容易停在旧系统，所以这个现象看着像「macos-x64 专属」。
- **页面看着没死但模型输出不刷新**：这是**绘制**停了，不是连接断了 —— Harness UI 把流式输出合并到动画帧上（`requestAnimationFrame`），而输入框走的是同步刷新，所以"能打字、能发送、输出不动"完全可能同时成立。看护用两个计数器把两种停画分开（见行为表）：**帧停了但计时器还在跑** = 被 WebKit 暂停绘制，页面完好、**不重载**，标题会提示"已暂停绘制：露出窗口即可恢复，或用 ⌘R 重新加载界面"；**两者都停** = 主线程忙或挂了，才会重载。日志关键字 `Harness 页面停止绘制`。
  **真正的触发条件是"窗口被遮挡"，不是"失去焦点"**（2026-09-18 实测，macOS 27 + WKWebView）：用另一个窗口盖住时帧计数立刻冻结、`document.visibilityState` 同时翻成 `hidden`，而**可见但没有焦点**的窗口帧照常画（连续 180s 线性增长）。`background_throttling(Disabled)` 与 `inactiveSchedulingPolicy=None` 都挡不住遮挡这一条（wry 确实把它们写进去了，读回值就是 `None`）。所以壳有两条应对：**渲染兜底**（被遮挡时用定时器顶帧，输出继续追平，切回来即最新；**遮挡前就排好队的那一帧由 `visibilitychange` 补上** —— 少了这一条，"正看着窗口切会话再切走"会把整条刷新链卡死，界面停在"载入历史…"，2026-09-18 现场）与 **⌘R**（拿到一份新文档）。
  还有一条只影响看护自己、但后果最大：**帧探测必须数原生帧**。兜底垫片替换了 `requestAnimationFrame`，探测若走包裹版就会被兜底自己的定时器顶动 —— 页面十八分钟没被绘制，日志里却一条停画都没有（2026-09-18 现场）。探测因此走垫片留下的 `__dshNativeRaf`，并把兜底顶起的帧数单独上报。渲染进程被系统结束时另有一条立即重载的路径（日志 `WebView 渲染进程已结束`；崩溃报告在 `~/Library/Logs/DiagnosticReports`）。完整证据与实测脚本见 [`docs/design-task-fix-webview-render-stall.md`](docs/design-task-fix-webview-render-stall.md)
- **界面没有插件市场**：市场来自 profile（`~/.dsh/profiles/web`），只有三条来源 —— ① **自带运行时版**首启
  播种的 profile 模板（**精简版永远不播种**）；② 你自己 `dsh plugin --profile web add dshmarket`；
  ③ 市场装好后自我更新。profile 已经存在时模板不会覆盖（跳过会记日志），profile 里没声明 dshmarket 时
  也会记一条说明 —— 这两种情况以前是完全静默的。
- **显示的 dsh 版本不像 dsh**：状态页现在同时写**版本与来源**（`system` / `bundled` / `env` + 树路径）。
  若版本号可疑、或部署里存在同名 `dsh`，先看日志的 `runtime:` 与 `spawn:` 两行：`spawn:` 里的路径
  就是壳真正监管的那棵树。

### 自带运行时（已并入 main）

已完成并可实机复现，并已**并入 `main`**（2026-09-13 合并 `feat/bundled-runtime`）：

- `make runtime-fetch / runtime-stage / runtime-clean / bundle-bundled` 四个目标全部跑通；
- staging 自带校验（`scripts/check-runtime-stage.sh`；`scripts/tests/check-runtime-stage-test.sh` 对**每一条闸门**各造一个假目录反例）：必需文件、**可执行位**（tauri 保留 mode，丢 `+x` 的 node/pnpm 会一路进包）、**悬空符号链接**（会让 `tauri build` 失败）、**绝对符号链接**（会被解引用，把宿主机文件复制进包）、**quarantine 属性**（会带进 .app）、**模板内的 pnpm store**、**文件数/体积闸门**（多放 2 万个文件这类残留以前会被静默打包）；
- profile 模板由真 pnpm 生成（含插件市场 dshmarket，见方案 §2.5）；
- 实测：`env -i PATH=/usr/bin:/bin` 下自带 node 跑自带 dsh → `0.1.5-rc.2`；
  全新 DSH_HOME + 模板播种 → `dsh web` **6 秒**出 URL、stderr 干净、`.dsh-market` 出现；
- 实测：`make bundle-bundled` 产出 598 MB 的 `.app`，包内 node 可直接执行、31,064 个文件；
  macOS 上还会用 `--config` 把 `minimumSystemVersion` 覆盖为 11.0（随包 node 是 `minos 11.0`）；
- 模板随包钉住打包时的 `DSHMARKET_VERSION`（当前 1.46.1）：新机器首启播种该版本，之后由
  插件市场自动更新跟到 registry 最新。

**运行时解析已接进启动流程**（`src-tauri/src/runtime.rs` + `lib.rs::resolve_runtime`）：

- 来源：`DSH_DESKTOP_RUNTIME` → 应用资源目录 → `tauri dev` 的 `target/<profile>/runtime`；
- 候选：显式环境变量 → 系统安装（先过能力门槛）→ 自带（seed 与影子前缀取版本更高者）；一个候选都没有时给
  「装 node + dsh」错误页（若系统安装被门槛拒掉，错误页会说明是哪一道）；
- 门槛：`module.stripTypeScriptTypes`（Node 22.13 以上）是硬性的——dsh 没有它跑不起来；架构不一致只在
  **本构建确实带运行时**时才拒绝，否则降级为警告继续用系统安装（Rosetta 下的 x64 node 自洽、可用）。
  能力探测带 5 秒超时，版本管理器 shim 挂住时不会把启动页卡死；
- 自带时：首启播种 profile 模板（含插件市场）、PATH 前置自带工具目录、注入 `npm_config_prefix`／
  `PNPM_HOME` 指向可写前缀；核心更新只落到 `app-data/runtime/prefix`；
- 用系统安装时：**默认只提示不升级**（`system_updates: notify`）—— 那棵树是用户自己装的，
  `npm install -g` 会改动机器上别的工具也在用的全局前缀；想让壳就地升级就写 `system_updates: install`。
  提示写在 **Harness 窗口标题**上（见行为表），不是只写日志；
- **node 与 dsh 分开解析**：系统装了 node 但没装 dsh 时，可以「系统 node + 自带 dsh」混用（方案 §2.4 的那一格以前不可达）；
- **只有本壳自己的树才算「自带」**：seed（包内）与影子前缀（`app-data/runtime/prefix`）会得到 PATH 前置、
  `npm_config_prefix`／`PNPM_HOME` 注入与首启播种；`DSH_DESKTOP_DSH`／`DSH_DESKTOP_RUNTIME` 指向的树按**用户的**处理 ——
  不动它的子进程环境、不往里装更新（排障开关不再带来副作用）；
- **能力门槛仍然管 `runtime: system`**：node 缺 `module.stripTypeScriptTypes`、或有自带运行时时架构不一致，
  系统安装会被拒（错误页说明原因）。这是有意为之（旧 node 本来就跑不起 dsh），与方案 §2.4「保留手动覆盖」的措辞
  有出入，以本条为准；
- 探测短路：`runtime: bundled` 或两个 `DSH_DESKTOP_*` 都已指定时，跳过系统运行时探测（省掉一次登录 shell + 最长 5 秒的探测）；
- 首启超时：判定发生在播种**之前**，所以带模板播种的首启仍然是 90 秒预算；播种失败不会留下半棵 profile（先写 `.tmp` 再改名）；
- 想看效果：`DSH_DESKTOP_RUNTIME_PREFERENCE=bundled|system|auto` 可覆盖 `config.json` 的 `runtime`；
- **WebView 能力探测与兼容层**：splash 页面（我们自己的页面，唯一持有 core 权限的窗口）在加载时探测
  「可补的 API」清单（`Iterator`、`Promise.try`、`Promise.withResolvers`、`Symbol.dispose`、
  `Math.sumPrecise`、`Uint8Array.fromBase64`、`Object.hasOwn`、`findLast`）、只上报不处理的降级项
  （`structuredClone`），以及用 `new Function` 编译 `class static block` 的语法判定，然后上报；壳在打开
  harness 窗口**之前**判定 —— 可补的注入兼容层（`initialization_script`，harness 窗口仍然零 capability），
  补不了的显示写明「缺什么 + 需要 Safari 16.4」的失败页并改用默认浏览器（`window.rs::WebviewReport`、
  `lib.rs::hand_the_gui_to_the_browser`）；探测没上报时按支持处理，不会因为诊断本身出问题而把人挡在门外。

**Windows 免安装包已实机验证通过**（2026-09-13：解压到 `D:\dsh` 双击即启动，自带 node + dsh 拉起 Web GUI，
插件市场与会话内工具调用正常）。

**CI 已覆盖全平台构建**（`.github/workflows/release.yml`）：打 tag 时每个 macOS/Linux 平台先出精简版、
再出 `-bundled` 的自带运行时版，运行时按 `TARGET` 的架构 staging（macOS x64 交叉编译拿的是 darwin-x64 的 node，
不是 runner 自己的 arm64）。Windows 侧继续复用 `windows-portable.yml` 的免安装包。
其中**只有 macOS arm64 与 Windows 做过实机验证**，Linux 侧（x64/arm64）尚未在真机上跑过。

尚未做：macOS 自带运行时的 **GUI 实机验证**、**"回退 + last-known-good"**（方案 §2.3 规则 2：选中的树起不来时自动
换另一个候选重试一次）、签名与公证、Linux 侧的实机验证。
细节见[方案文档](docs/design-task-feat-dsh-bundled-runtime.md) §20。

不带 `-bundled` 后缀的精简版（以及本仓库 `make bundle` 的默认产物）仍然需要系统中已有 `node` 与 `dsh`。

### 与 CLI 的兼容边界（已实测）

壳对 `dsh` CLI 有 5 个隐含契约：`--profile web`、`--patch <yaml>`、`--no-open`、`--port N`（顺序固定），
以及 stdout 里的启动 URL 行。区间外的版本**默认拒绝启动**（`require_tested_dsh: true`），错误页写明原因；
设成 `false` 则只记一条 warning 并在状态页标注「未测试版本」。同一开关也约束自动安装 —— 会被拒绝启动的
版本不会被装上（否则壳会把你正在用的版本换成它自己又拒绝运行的那一个）。当前测试区间：
`>= 0.1.5-rc.1, < 0.2.0`（常量在 `update.rs`）。

启动 URL 的解析**不依赖**上游那行文案：优先按 `dsh web:` 前缀取第一个 token，前缀不在时就退化为
「扫描行内第一个 loopback `http://127.0.0.1:…/?token=…`」，scheme/host/token query 仍然强校验。

## 配置（`config.json`）

首次运行写入应用数据目录，字段与默认值：

| 字段 | 默认 | 说明 |
|---|---|---|
| `port` | `3080` | 固定端口。authority 稳定才能让 cookie 跨重启复用、也才能接管上次实例 |
| `workspace` | `$HOME` | 传给 dsh 的工作目录 = agent 的 workspace root。不是已存在的绝对目录时本次回落到默认值并记日志（**不改写你的文件**） |
| `dsh_path` | `null` | 记住的 `dsh` 启动器绝对路径：自动搜索顺序为 `DSH_DESKTOP_DSH` → 本字段 → PATH → 常见目录 → login shell。相对路径或不存在的文件本次忽略并记日志 |
| `dsh_home` | `null` | `null` 表示共用 `~/.dsh`（插件/设置/会话全保留）；指向别的目录则隔离 |
| `take_over_existing` | `false` | **没答复时**怎么处理（不是「是否询问」）。端口被**外部** Harness（不是本应用启动的）占用、且该进程命令行确实像 `dsh web` 时，**一定会弹面板**让你当场选：接管 / 保留并用浏览器打开 / 保留并让本应用换端口 / 什么都不做。面板写明对方 pid、完整命令行与本应用将要使用的 workspace。**120 秒没有选择才按本项决定**：`false`（默认）= 用系统浏览器打开，`true` = 终止并接管 |
| `auto_update` | `true` | 启动时检查并安装 dsh 新版本（自带/影子运行时总是更新自己的树） |
| `auto_update_plugins` | `false` | 是否把 profile 里的插件市场（`dshmarket`）也更新到 registry 上的最新版。默认关：它会改写你 profile 的 `package.json`/锁文件，而 profile 是用户数据 |
| `update_tags` | `["latest","next","alpha"]` | 取其中最高版本。默认跟**上游发布的全部渠道**：领先的是哪个渠道会随发版变化，只读其中一部分，最新构建落在那部分之外时就看不见——实测 2026-09-18 领先的是 `alpha`（`latest=0.1.5-rc.2`、`alpha=0.1.6-alpha.2`），2026-09-24 领先的却是 `next`（`latest=0.1.5-rc.3`、`next=0.1.7-rc.1`、`alpha=0.1.7-alpha.2`，且 `next` 是唯一比已装版本新的）。registry 上没有的标签只要还有一个匹配就不算失败（插件市场 `dshmarket` 就没有 `alpha`）。**想只跟正式版就写 `["latest"]`**；`require_tested_dsh` 仍然管住安装范围，区间外的版本只报告不安装 |
| `update_check_interval_minutes` | `60` | 一次成功的查询结果缓存多久（0 = 每次启动都查）。查询实测约 1.2–1.9 s，缓存命中 0 ms |
| `import_shell_env` | `true` | 启动时导入登录 shell 的环境变量（见下节）。`false` 则只用 App 自身环境 |
| `require_tested_dsh` | `true` | CLI 版本落在已测试区间外时是否拒绝启动。默认拒绝：本壳靠解析 CLI 的启动行工作，区间外可能以看不懂的方式失败。设 `false` 则只告警并继续（状态页标注「未测试版本」）。它同时约束**自动安装**：会被拒绝启动的版本也不会被装上 |
| `system_updates` | `"notify"` | 被监管的 CLI 是**用户自己装的**且上游有新版本时：`notify` 只提示、不动你的全局前缀；`install` 就地升级（自带/影子运行时不受此项影响，它总是更新自己的树） |
| `webkit_compat` | `true` | 旧 WebView 上注入兼容层（`Iterator` 等，见[已知坑](#已知坑)），让 macOS 13.3+ 用原生窗口。`false` 恢复旧行为：缺任何能力都改用默认浏览器 |
| `render_fallback` | `true` | 被遮挡期间用定时器顶替动画帧，让流式输出继续推进（见[已知坑](#已知坑)）。`false` 恢复引擎自己的调度，用于对照 |
| `keep_menu_focus` | `true` | 点击菜单弹层时原地按住焦点，修掉 WebKit 上模型/思考强度点了没反应（见[已知坑](#已知坑)）。`false` 恢复引擎自己的焦点行为 |
| `page_diagnostics` | `true` | 把页面内的未捕获错误、未处理 rejection 与 `console.error/warn` 带回壳日志（Harness 窗口本身没有控制台，见[已知坑](#已知坑)）。`false` 不注入 |
| `scroll_anchor` | `true` | WebKit 没有实现 CSS 滚动锚定，这个垫片替它补上：读者上方的展开/收起改变高度时，按被观察行移动的距离补偿滚动偏移（见[已知坑](#已知坑)）。`false` 恢复引擎自己的行为，用于对照 |
| `runtime` | `"auto"` | 运行时来源：`auto` 用系统已装的（通过门槛时），否则用自带；`bundled` 强制自带；`system` 保持旧行为（开发用） |
| —— | —— | **候选必须真的是 `@deepseek-ai/dsh`**：PATH 上同名但不是这个 npm 包的 `dsh`（Homebrew 的 Dancer's shell、自定义 shim…）会被跳过并记日志；全部候选都不合格时报错页，而不是随便监管一个同名程序。识别不出来的 node 启动脚本只有在 `--version` 真的打印出版本号时才被采用（旧布局的兜底） |
| `env` | `{}` | 显式追加/覆盖传给 harness 的环境变量，优先级最高，如 `{"DEEPSEEK_API_KEY": "sk-…"}` |

只要写你想改的字段即可：`port` / `workspace` 缺失会取默认值，其余字段本就有默认值。
若文件整体无法解析，本次运行使用默认配置并记一条日志，**但不会覆盖你的文件**（只有文件不存在时才会写入）。

状态与日志：`<app-data>/state.json`、`<app-data>/logs/harness.log`（**壳自己的每条日志带本地时间戳 `[YYYY-MM-DD hh:mm:ss]`**，便于与崩溃报告、内存压力事件对齐；脱敏；写入前判定 5MB × 3 份轮转，
即 `harness.log` + `.1`/`.2`/`.3`）。**外部输出都有上界**：Harness 单行超过 256 KiB 即截断（继续排空到
换行，行尾标注 `…[truncated: line exceeded 256 KiB]`），且一个非法 UTF-8 字节不再像 `lines()` 那样中断
其后所有行（`dsh web:` URL 行会跟着一起丢，页面上只表现为启动超时）；npm/pnpm 的 stdout/stderr 分别保留
4 MiB / 1 MiB 并照旧读空管道；`command -v`、`npm prefix -g`、`ps` 这些辅助调用也都有超时 —— 它们跑的是
`$SHELL -lc`，会 source 用户的 rc 文件。两个文件都按「仅当前用户可读」写入：Unix 上是 `0600`，
Windows 上用 `icacls` 去掉继承并只授权当前用户 —— `config.json` 的 `env` 可能存着 API Key，
而用户目录下的文件默认会继承父目录的 ACL（共享/域机器上可能包含其他账户）。

**日志脱敏范围**：`token=` 只是第一个。凡是经过日志行的内容都会把下列字段的值替换为 `***` ——
`token`、`api_key`/`apikey`/`api-key`、`authorization`（含 `Bearer <凭据>`）、`cookie`/`set-cookie`、
`password`/`passwd`、`secret`、`private_key`、`access_key`、`session_id`。provider 报错常把请求头原样回显，
这条覆盖的是那种情况。识别的写法有 `KEY=value`、`key: value`、`key = value`，以及 JSON / JS 对象 /
Python dict 里的 `"key": "value"`（含转义成 `\"` 的 JSON 字符串）；带引号的值以配对的引号为界，
`Authorization: Basic|Digest|Token <凭据>` 保留方案名、替换其后的凭据。**不含** workspace 路径等隐私信息：日志要能定位问题，路径是其中一部分，
需要更少留存时请自行清理 `<app-data>/logs/`。
macOS 的 app data 目录为 `~/Library/Application Support/com.deepseek.dsh.desktop/`。

## 环境变量（API Key 等）

**问题**：macOS 的 GUI 应用继承的是 launchd 的环境，**不会读取 `.zshrc`/`.zprofile`**。
所以在终端里 `export DEEPSEEK_API_KEY=…` 对双击启动的 `.app` 无效，harness 会报
`no API key for provider "deepseek-official"`。

**做法**：启动时执行一次登录 shell 的环境导入（`$SHELL -lic env`，超时 8 s，失败则回退 `-lc` 或跳过），
把结果合并进 harness 子进程的环境：

- 实测开销 **约 160 ms**（与上次更新的回滚检查并行执行，在解析运行时之前汇合），
  取到约 46 个变量，输出 0 行噪音；
- 导入的 `PATH` 也用来**定位你自己装的 node / dsh**：nvm、fnm 在 `.zshrc` 里初始化，只有 `-lic`
  读得到它，单独的 `$SHELL -lc 'command -v …'` 看不见。拿到导入的 PATH 后就不再额外起登录 shell 查找；
- 输出没读完（后台进程一直占着管道）或超过上限时，**丢掉最后一行不完整的内容**，不导入被截断的值；
- **它会执行你的 `.zprofile`/`.zshrc`**：这两个文件里若有联网、改文件、拉起后台进程或耗时操作，
  每次启动应用都会连带发生。8 秒超时只能让壳不再等它，**不能撤销已经发生的副作用**。
  不想这样就设 `"import_shell_env": false`，改用下面的 `env` 字段显式给出需要的变量；
- **只把变量名写进日志**（值可能是密钥，永不落盘）；
- 保留 App 自己管理的变量：`HOME`/`TMPDIR`/`SHELL`/`SHLVL`/`PWD`/`_`/`DSH_*` 等不导入；
- `PATH` 走合并策略：**自带 node 目录 → `/opt/homebrew/bin` → `/usr/local/bin` → shell 的 PATH → App 的 PATH**（去重、只保留绝对路径），
  这样 agent 执行的 `git`/`node`/`python` 与你的终端一致，而壳自己用的 node 仍优先。

想关掉：`config.json` 里设 `"import_shell_env": false`；想强制某几个值：用 `env` 字段（最后应用，优先级最高）。
另一种等价做法是在 Web GUI 的 Models 页面里填写 API Key —— 那会写进 `~/.dsh` 的凭证服务，与 shell 环境无关。

## 行为

| 场景 | 行为 |
|---|---|
| 端口无监听 | 定位 dsh/node（**逐个候选校验身份**，见配置表末行）→ 启动 → 等 URL → 打开窗口。状态页写明用的是哪棵树：`dsh 0.1.5-rc.2 · system /opt/homebrew/lib/node_modules/… · 端口 3080` |
| 端口上是本应用上次启动的实例（state.json 对得上且存活） | 直接复用（cookie 对同一 authority 仍有效，实测跨重启有效） |
| 端口上是**别人**启动的 Harness（CLI / Automator） | 先做两道身份校验（401 特征 + 该 PID 命令行像 `dsh web`，`plugin` 子命令不算）；都通过就**弹面板询问**（与 `take_over_existing` 无关，那一项只决定 120 秒无答复时怎么办）：**接管**（对该 PID 发 SIGTERM，只发单进程不碰它的进程组 → 等端口释放 → 自启拿新 token）／**保留并用系统浏览器打开**（本应用退出，页面**不带**重启按钮）／**保留并换端口**（本应用改用配置端口之上的第一个空闲端口启动，对方不受影响；仅当找到空闲端口时出现）／**什么都不做退出**。面板写明对方 pid、完整命令行、端口，以及**接管后会改用本应用配置的 workspace**（不会继续对方的工作目录）；并说明**浏览器里那个旧标签页不用手动关**（刷新就会连到重启后的实例 —— cookie 的签名密钥对同一 `dsh_home` 稳定，实测接管后旧 cookie 仍返回 200），唯一的例外是那个实例用了另一个 `dsh_home`，此时会看到 `authentication required`，需在启动它的终端里重新打开它打印的 URL |
| 启动前发现新版 dsh | 先升级 CLI（splash 显示进度），随后重启实例跑新版本。用户自己装的那棵树默认**只提示不升级**（`system_updates: notify`）；新版本若超出已测试区间，在 `require_tested_dsh` 默认开启时**不安装**（装了也会被拒绝启动） |
| 启动前发现新版插件市场 | 先停实例 → `dsh plugin --profile web add dshmarket@<版本>` → 重启实例。默认**不做**（`auto_update_plugins: false`），设为 `true` 才开启 |
| 有新版本但本次不安装 | 写进 **Harness 窗口标题**：`DeepSeek Harness（有新版本 vX.Y.Z 可用（未自动安装））`。**只有标题能承载它**：更新检查跑在 Harness 窗口建好之前，splash 上那句话几秒后既被"正在启动 Harness…"覆盖、又随 splash 一起销毁，用户等于永远看不到（2026-09-18 实测）。标题与停画提示共用一个槽位，**停画优先**（页面不画是当下就看不到输出，新版本只是下次启动可能更好）；页面恢复绘制后重新合成，不会抹掉更新提示 |
| WebView 缺可补的 API（如 `Iterator`） | 向 harness 窗口注入兼容层（ES5、逐块自守卫、只装缺的那些）后照常开原生窗口，日志记 `WebView 缺少 …：已注入兼容层`；`webkit_compat: false` 时改成"未注入"并走浏览器 |
| WebView 缺补不了的能力（目前只有 `class static block` 语法） | 不打开 harness 窗口：改用默认浏览器 + 状态窗口写明缺什么、界面需要 Safari 16.4 及以上 |
| 端口被别的程序占用（不是 Harness 协议） | 错误页，提示改 `config.json` 的端口。**普通 HTTP 服务（含返回 200 的 Vite/Node/Java）走这一条**：401 认证栅栏是唯一的 Harness 判据 |
| 选了「保留并用系统浏览器打开」 | 打开 `http://127.0.0.1:<端口>/` 并在状态页说明；**没有重启按钮** —— 外部实例还活着，本应用没有可重启的 Harness，给了按钮只会重复失败并再开一个标签页。浏览器需要已有该 authority 的登录 cookie；若显示 `authentication required`，请在启动那个实例的终端里重新打开一次它打印的 URL |
| 选了「保留并换端口」 | 本应用在配置端口之上的**第一个空闲端口**启动自己的 Harness。这个选择**只对本次启动有效**、不写回 `config.json`：会话 cookie 与固定端口绑定，静默永久迁移比下次再问一次更糟 |
| 端口按 Harness 协议应答，但读不到 / 不像 `dsh web` 的命令行 | 不接管也不报"被别的程序占用"：拒绝接管并说明无法确认身份（避免误杀），提示手动停止或换端口 |
| 关闭窗口（红点 / ⌘W） | `AppHandle::exit(0)` → `RunEvent::ExitRequested` → SIGTERM 进程组 → 删状态文件 |
| ⌘Q / Dock 退出 / `quit app` | tao `application_will_terminate` → `RunEvent::Exit` → 同样的清理（幂等） |
| 退出发生在 Harness 启动途中 | `EXITING` 标志：刚起来的子进程立即停掉，不会漏成孤儿 |
| 复用上次实例后退出 | 该实例已登记，退出时照样 SIGTERM（旧行为不登记 → 留孤儿） |
| 强杀（SIGKILL）/ 崩溃 | 拿不到任何回调；下次启动先自愈：仍在服务本次端口的记录保留下来交给复用分支，其余情况清掉残留与记录 |
| 启动等待 URL 超时 / 窗口创建失败 | 停掉刚起的子进程 → 状态页报错（不会留下占着端口的半启动实例） |
| 启动成功后 Harness 意外退出 | 看护线程发现退出 → 清 state.json → 销毁 Harness 窗口、状态页显示"正在重新启动…" → **自动重启**：干净退出（退出码 0，插件市场「立即重启」就是这一种）先等 8s，端口被替代实例接管时先停掉它再按本壳的 workspace 启自己的；崩溃/被杀 2s 后直接自启。成功后切到新的 token URL；连续 3 次短命重启未稳定才停在终态报错页（带「重新启动 Harness」按钮） |
| 窗口首次加载失败 | 20 s 内没有收到"加载开始"事件（导航根本没起来，无论端口是否健康）→ 退避重试同一 token URL，最多 3 次；已经开始加载则一律不打扰。次数用尽且端口也不再服务才弹状态页；端口仍健康时只记日志，不在事件不投递的环境里给正常应用弹错误页 |
| 页面停止绘制（能点击能输入、模型输出不更新） | 15 s 一次的探测同时读**动画帧计数**、**计时器计数**与**页面自报的可见性**。**帧不动、计时器在动** = 被 WebKit 暂停绘制（实测对应"窗口被遮挡 / 最小化 / 应用被隐藏"），页面本身完好：**不重载**，标题改为"已暂停绘制：露出窗口即可恢复，或用 ⌘R 重新加载界面"，日志记 `页面停止绘制：计时器仍在运行`（每个连续段只记一条，并带上本轮渲染兜底顶起了多少帧 —— 这是"引擎在画"与"垫片在扛"的唯一区分）。**有没有人在看以页面自己报的 `document.visibilityState` 为准**，窗口状态只在页面答不上来时兜底 —— 因为实测"可见但无焦点"的窗口照常绘制，而"可见且有焦点但被遮挡"的窗口停画，两者与窗口状态正好相反。窗口重新可见会立即触发一次判定，不必等下一个周期；若重新可见后仍不绘制，它按"未响应"并入下面那条。**帧和计时器都不动**（主线程忙/挂）或**探测无应答**：连续 3 次未通过才重载当前 URL（最多 3 次），标题显示"正在重新加载…"。**重载前会先问页面是否正在被输入**：焦点在输入框**且**最近 15 s 内有按键/粘贴 → 最多推迟 2 轮（标题显示"等待输入结束…"）；输入停下或宽限用完后照常重载，重载预算不受影响。两个条件缺一不可 —— Harness 一加载就把光标放进输入框且再不拿走，只看焦点等于永久豁免（那正是"只能退出应用"的那条路）；只看"最近有输入"又会把会话列表里的一次点击算成有草稿要保 |
| 被遮挡期间仍要更新（渲染兜底） | 建窗口时注入的 ES5 垫片包裹 `requestAnimationFrame`：页面自报 `hidden` 且 250 ms 内没有原生帧到达时，用 `setTimeout` 顶一帧（时间戳取 `performance.now()`），并取消那个已无意义的原生帧。原生帧先到则清掉兜底定时器；两条路径共用一个 `settled` 闩，回调只会投递一次。垫片持有全部在途请求并监听 `visibilitychange`：**转入遮挡时给还没兑现的帧补挂兜底**（前端的刷新是帧接帧的链，遮挡前排的那一帧没有兜底就整条卡死），重新可见时撤掉还没触发的兜底。原生 `requestAnimationFrame` 留在 `__dshNativeRaf` 上，**看护的帧探测走它** —— 走包裹版会被兜底自己顶动，停画就再也测不出来。可见页面上一个定时器都不建；重复注入不叠加包裹。`config.json` 的 `render_fallback: false` 关闭 |
| 展开思考过程时页面闪一下（读者上方的布局变化把视口顶飞） | **WebKit 没有实现 CSS 滚动锚定**（Safari 直到 27 都没有，Chromium/Gecko 早已实现），所以读者上方的高度变化会把视口按变化量整体推走 —— dsh 0.1.7 展开思考块正好是一次大高度变化（`contain: size layout` 撤销、sticky 头栏出现、整段 Markdown 首次挂载，实测一次布局里从 22px 跳到 33164px），且在浏览器里**无法复现**，因为那里的引擎会自己补偿。修法是建窗口时注入的 ES5 垫片：记住读者正在看的那一行，`ResizeObserver`（布局之后、绘制之前）按**同一行的位移**补偿滚动偏移，读者看到的位置不动。两条界限是刻意留白的：**跟尾**（页面自己的 `followTail` 正在贴底）与**读者已在底部**都不补偿，否则会与页面自己的滚动打架。垫片自己写 `scrollTop` 也会触发 scroll 事件，所以写入值被记住并识别，否则这次写入会被当成"读者在滚动"、把快照提前作废，下一次真实布局变化就被跳过（测试抓到的回归）。按 **UA 而不是特性探测**决定是否安装：`CSS.supports("overflow-anchor")` 在 WebKit 上返回 true（实现了属性、没实现行为），用特性探测会在已经会补偿的引擎上再叠一层。`config.json` 的 `scroll_anchor: false` 关闭 |
| 页面完全无响应（渲染进程被杀 / 主线程卡死） | 探测连续 3 次收不到应答 → 同样重新加载（最多 3 次）。**无应答不算"正在输入"**：能回答"我在输入"的代码正是已经停掉的那部分，所以静默一律按需要恢复处理；macOS 上渲染进程被系统结束时会立刻重载（日志记 `WebView 渲染进程已结束`，崩溃报告在 `~/Library/Logs/DiagnosticReports`）。加载完成后标题自动恢复；3 次仍不回来 → 标题停在"页面已停止刷新，请重启应用"并记日志 |
| 手动恢复界面 | 菜单 **View → 重新加载界面**（⌘R）：重新加载 Harness 当前 URL。**重载会告诉看护它手里的读数已经作废** —— 新文档的帧计数从零开始，拿旧读数去比会让一次成功的重载在下一个周期被判成"停止绘制"（崩溃恢复与首次加载重试同理）。与看护走同一条路径，丢掉页面内存里的东西（滚动位置、展开的 Think 行、未发出的草稿），但**不丢会话**——会话在 Harness 进程里。这是"页面只是被暂停绘制"时唯一需要的手段，也是不必退出应用就能拿到新文档的入口 |
| 页面里的非 http/https 链接（`file:`、自定义 scheme…） | 不交给系统：日志记 `external scheme blocked: <scheme> (...)` 后丢弃 |
| 有新版 dsh 但端口上是外部实例且不允许接管 | 跳过本次更新（不重写别人正在用的树），实例继续服务，日志记 `update deferred`。允许接管时同样**先弹面板**：拒绝接管即跳过本次更新，而不是先停掉对方再失败 |
| 下载同名文件 | 自动改名 `name-1.ext`，不覆盖已有文件 |
| 关闭状态页（启动失败时） | 直接退出应用（此时没有 Harness 窗口，不会留下无窗口进程）；应用自己移除该窗口走 `destroy`，不触发这条 |
| 终态状态页上点「重新启动 Harness」 | 同一进程内复位失败标志并重跑启动流程：占端口的外部 Harness 会被接管，成功即销毁状态页、开新窗口；再失败则原地更新报错页 |
| 终态状态页在屏幕上时又启动一个实例 | single-instance 回调只 show/focus 那个窗口会让人以为"重开也没用"，因此它等价于点「重新启动 Harness」 |
| 终态状态页在屏幕上时点 Dock 图标 / Finder 里再打开一次 | macOS 只会激活已有进程（single-instance 不会触发），所以走 `RunEvent::Reopen`：同样等价于点「重新启动 Harness」。浏览器回退页（Harness 还活着）不在此列，它没有重启按钮也不响应这两个入口 |
| 壳被强杀后再次启动 | 读取 state.json：残留进程仍在服务本次端口 → 复用它（保留会话）；否则自愈清理后再自启 |

> 并发边界：壳只保证**自己这个端口**上不会同时跑两个实例（接管 + single-instance 插件）。`dsh_home` 为 `null` 时
> 共用 `~/.dsh`，你在终端另开一个同 profile 的 `dsh` 仍会与壳并发读写同一份会话存储 —— 需要严格隔离就把 `dsh_home`
> 指向别的目录（代价是 marketplace 插件、设置与会话不再共享）。
>
> 复用依赖 WebView 数据目录里那张 30 天有效的 cookie。清过应用数据、或距上次成功登录超过 30 天时，
> 复用的窗口会直接显示 `dsh web authentication required`；此时**退出应用再启动**即可（退出会停掉残留实例并清掉
> state.json，下次启动拿的是新的 token URL）。
>
> 为什么必须接管：启动 URL 里的 launch token 是 `randomBytes(32)` 且只存在于那个进程内存中，外部无法取得；
> 不重启就永远拿不到会话（无 cookie 访问一定是 401）。

## dsh 核心升级

0. **先看缓存**：`<app-data>/update-check.json` 里的结论在 `update_check_interval_minutes`（默认 60）内且已安装版本未变 → 直接沿用，不联网（实测 0 ms vs 1.2 s）。查询失败也会缓存，但只缓存 5 分钟，离线时不至于每次启动都干等；
1. 需要联网时：`npm view @deepseek-ai/dsh dist-tags --json` → 在 `update_tags`（默认 `latest` + `next` + `alpha`）里取版本号最高者。
   缓存条目记下**是哪组标签问出来的**：换过 `update_tags` 就等于换了一个问题，旧答案不再复用，默认值改动下次启动即生效而不是等一个缓存周期
   （fetch 超时压到 **8 s**，且整个 npm 子进程有**进程级超时** —— `--fetch-timeout` 只管单次请求，
   代理挂起、锁等待、生命周期脚本都可能让 npm 永远不返回；超时后连同其子进程一起清理）；
2. 与已安装版本做 semver 比较（`0.1.6-alpha.2 > 0.1.5-rc.2`，因为先比数字段；同号时正式版高于预发布版，`rc.2 > rc.1 > alpha.2`），**从不降级**；
3. **先装进暂存目录，不碰正在用的那棵树**：有新版则
   `npm install -g --no-fund --no-audit --cache <app-data>/runtime/npm-cache --prefix <app-data>/runtime/staging/staging-<版本>/prefix @deepseek-ai/dsh@<解析出的具体版本>`；
   npm 取自 node 同目录；`runtime/{prefix,tools,npm-cache,staging,rollback,profile-backup}` 在启动时幂等创建。
   暂存目录每次尝试前清空 —— 上一次留下的半棵树绝不能被当成这一次的成果。
   **这一步不停止任何实例**：它写的是 `runtime/staging/…`，在用的树原封不动，所以下载的 300 秒里
   用户仍有一个能用的 Harness，registry 半途失败也不会有任何损失；
4. 所有 npm 子进程都在 PATH 最前面插入 npm 所在目录：npm 是 `#!/usr/bin/env node` 脚本，
   而 GUI 启动的壳只有 launchd 的 PATH（不含 node），不这样处理会直接 `exit 127`（详见设计文档 §13.8）；
5. **校验暂存树**：包名必须是 `@deepseek-ai/dsh`、版本必须是请求的那个、入口脚本 `lib/bin.js` 必须存在。
   任一条不满足就放弃本次更新并记日志 —— registry 给了别的版本、下载被截断、npm 忽略了 `--prefix`，
   都在这里变成一条日志，而不是一棵坏掉的树；
6. **原子切换**：把活动树 `rename` 到 `runtime/rollback/<版本>`（last-known-good，保留 2 代），
   再把暂存树 `rename` 到活动位置。切换**之前**先写 `runtime/update-swap.json` 记录这次切换，
   它要等新版本真的打印出启动 URL 才被删掉；
7. 切换推迟到**确定要启动**的那一刻：这中间还可能因为端口被占、外部实例不接管、版本超出测试区间而
   根本不启动 Harness，为一次不会发生的启动做切换只会给下次启动留一条要回滚的记录。
   **停止实例也发生在这里**（不在第 3 步）：npm 是原地重写依赖树，而 node 按需懒加载模块 ——
   树被换走后运行中的 harness 下一次 `require()` 会直接 `MODULE_NOT_FOUND`（实测），所以切换前先
   SIGTERM 停实例、等端口释放；不能停时（用户在面板里选择保留该外部实例、或拿不到 PID）**跳过本次
   切换**并记日志。用户在面板里选「保留它，改用其它端口」时，若被保留的实例可能在用待替换的那棵树，
   也放弃切换（本次启动照常进行）—— 否则 Windows 上 rename 会失败、Unix 上那个实例会在下次 `require()` 崩。
   判定：待替换的树还不存在（打包版首更）→ 不冲突；用户自己的安装前缀（`system_updates: install`）→ 一律视为
   冲突（终端里的 `dsh` 经软链启动，命令行里看不出树的路径）；本壳的影子前缀 → 看该实例的命令行是否含
   这棵树的路径，读不到按冲突处理。检测之后端口上又冒出外部 Harness 时同样放弃切换，交给后续启动的报错路径；
8. 新版本起不来（等启动 URL 超时）→ **立即回滚**并把原因写进错误页；进程在切换与确认之间被强杀 →
   下次启动读到 `update-swap.json` 就先把旧树放回，再去解析运行时（否则会挑中同一棵起不来的树、
   以同样的方式再失败一次）。失败的那棵树改名为 `<name>.failed` 留在旁边，作为排查证据；
9. 只有**确实没变**（npm 装到了别处）才记那条日志，并把这次尝试写进缓存（`attempted`），
    **同一缓存窗口内不再重复安装**（否则每次启动都会先停掉 Harness 再重建一棵约 289 MB 的依赖树）；
10. 刚更新过 → 强制重启实例（否则复用旧进程仍跑旧二进制）。

结果写入日志：`dsh is up to date` / `update available: A -> B` / `update staged: A -> B（校验通过，待切换）` /
`dsh updated: A -> B` / `update staged but not committed, keeping vA` / `update failed, keeping vA` /
`vB 未能启动，已回滚到上一棵树` / `update A was already attempted and changed nothing; not installing again` /
`update B failed to start last time; not retrying yet (the wait grows with each failure)`（失败标记，见下）。

**失败会被记住**：暂存失败、提交失败、以及"切换后起不来而回滚"（包括进程在切换与确认之间被强杀、下次启动
才回滚的情况），都会把该版本写进 `update-check.json` 的 `failed` 并累计 `failures`。首次失败后 5 分钟内不再重试，同一版本每再失败一次，
等待时间乘 4（5 → 20 → 80 → 320 分钟），最长 24 小时；换了新版本就从头计数，标记在最后一次失败 7 天后遗忘。
没有这一步，一个起不来的版本会变成每次启动都重新下载约 290 MB、切换、等超时、再回滚的循环。
刚切换上的新树首次启动按**首启的 90 s** 等待启动 URL：新版本第一次启动可能要迁移 profile 或重建缓存，
按常态的 30 s 判失败会把一次只是慢的更新回滚掉。

**回滚点与磁盘占用**：`runtime/rollback/` 最多保留 2 代 CLI 树，`runtime/profile-backup/` 最多保留 2 份
profile 快照（都是**上限**：一次更新只产生一份，两次指向同一版本时会覆盖）。常见情况是活动树 + 1 份回滚树
+ 1 份快照；最坏情况约 3 × 290 MB + 2 × profile。旧代在每次成功确认后自动裁剪，被强杀留下的暂存树在下次启动时清理。

### 插件市场（`dshmarket`）的自动更新

核心之外，这个壳还会保持 profile 里的插件市场为最新 —— 它是新机器唯一的插件安装入口：

- 同一个 registry 检查（`update_tags`、`update_check_interval_minutes`），答案缓存在**自己的**
  `<app-data>/plugin-check.json`，与核心的 `update-check.json` 互不干扰；
- 只在该 profile 的 `package.json` **声明了** `dshmarket` 时才动手（你自己删掉的插件不会被装回来），
  比较的是 `node_modules/dshmarket/package.json` 里的**实际安装版本**，不是范围；
- 有新版时：**先把 profile 快照到 `<app-data>/runtime/profile-backup/<版本>/`**（只读依赖树，实例照常服务；
  跳过 `data/`、`.dsh-market/`：凭据、会话状态与市场日志在 Harness 运行期间一直在写，恢复旧值回去是第二个、
  更糟的故障；符号链接按链接复制）→ **再停实例**（pnpm 原地重写 profile 的 `node_modules`，运行中的 harness
  下次 lazy require 会崩）→ `dsh plugin --profile web add dshmarket@<版本>`（CLI 自己转发 pnpm，属于你的 profile 文件会被改写）
  → 回读版本 → 本轮重启实例让新插件生效；
- **安装失败就把快照放回**：pnpm 可能在失败前已经改写了 `node_modules`，而半棵插件树不是下次启动能自愈的。
  恢复只动快照里有的条目，因此安装期间 Harness 写下的实时状态不受影响。拿不到快照时**不做本次安装** ——
  插件市场不值得一次不可逆的 profile 改写；
- 子进程用的是壳**组装好的那份 PATH**（node 目录 → 可写的 `<app-data>/runtime/tools/bin` →
  随包的 `<seed>/tools/bin` → 登录 shell 的 PATH），pnpm 就装在随包的 `tools` 前缀里 ——
  Finder 启动的应用继承的是 launchd 的 PATH，本来找不到它；
- 动手前先解析 pnpm：解析不到就只记一条 `PATH 上没有 pnpm，跳过插件市场更新`，**不会先把实例停掉
  再失败**；快照失败或安装失败（网络、registry、pnpm 自身出错）首次只抑制 **5 分钟**，之后自动重试 ——
  瞬时故障不该把某个版本钉死；同一版本反复失败时间隔递增（与核心相同，最长 24 小时）；
- **首启播种 profile 模板的那一轮不查插件市场**：模板已经钉住一个版本，首启不该产生联网下载，
  从下一次启动起照常检查；
- 装了但版本没变（pnpm 写到了别处）只记日志，并把这次尝试写进缓存，同一版本不重复装；
- 不想要就设 `"auto_update_plugins": false`（核心的 `auto_update` 不受影响）。

**排障**：手工修好 npm 全局前缀（或换安装方式）后，壳在出现更高版本前不会再尝试安装 —— 日志里那句
`already attempted and changed nothing` 是唯一线索，复位就是删掉对应的缓存文件：核心是
`<app-data>/update-check.json`，插件市场是 `<app-data>/plugin-check.json`（两者各存各的，互不影响）。
安装**失败**（`failed to install last time`）则是另一回事：先抑制 5 分钟，同一版本再失败则间隔递增（最长
24 小时），到期自动重试，不用手工复位；想立刻重试就删掉对应的缓存文件。

## 启动耗时与性能

这份壳是进程编排 + I/O，**空闲时 CPU ≈ 0，没有常驻轮询或定时器**（唯一的定时行为是窗口首次加载看护：最多 3 次、每次等 20 s，加载成功或放弃后线程立即结束）。启动路径上各环节实测（macOS，2026-09-13）：

| 环节 | 实测 | 说明 |
|---|---|---|
| 定位 dsh/node | < 5 ms | 沿 App 的 PATH、再沿导入的登录 shell PATH 逐目录做文件存在性检查（19 个目录的等价 shell 循环实测 4 ms）；只有没导入环境时才兜底起登录 shell（~160 ms，装了 nvm 时可达 1 s） |
| 版本读取 | **~1 ms** | 读 CLI 所属 `package.json` 的 `version`；读不到才回退 `node dsh.js --version`（约 80 ms） |
| 更新检查（缓存命中） | **0 ms** | 60 分钟内沿用 `<app-data>/update-check.json`；冷查询约 1.2–1.9 s |
| 登录 shell 环境抓取 | ~160 ms | 与上次更新的回滚检查并行执行，解析运行时之前汇合（定位要用它的 PATH） |
| 端口探针 | ~12 ms | 整个交换共用一个 600 ms deadline（每次读前按剩余时间重设），沉默与持续发送的对端都不会挂住它 |
| 启动 harness 到拿到 URL | 1–4 s | 由 `dsh web` 自身决定，首启（初始化 profile）更久，超时 90 s / 30 s |

日志写入是每行一次 `write(2)`（无缓冲）：单次会话通常只有几十行，有意不做缓冲，以免崩溃时丢日志。
轮转由内存字节计数判定，不额外 `stat`；计数在打开日志时以文件实际大小 seed，所以上次遗留的超大文件仍会在下次写入前轮转。
判定把**即将写入的这一行**算进去（`written + incoming > limit`），否则一条超限日志会先把文件撑到远超 5 MiB，
要等下一条日志才轮转；当前文件为空时不轮转 —— 那条日志总得写到某个文件里。

## 打包成 .app
```bash
pnpm install --store-dir=./.pnpm-store
pnpm tauri build --bundles app     # 产物：src-tauri/target/release/bundle/macos/DSH Desktop.app
```

图标来自 `src-tauri/icons/icon.icns`（由 `icon.png` 用 `sips`+`iconutil` 生成），
`icon.png` 由 `src-tauri/icons/make_icon.py` 自绘生成（矢量路径 + 4× 超采样，非官方素材）。当前**未签名**：
本机可运行；分发给别人需要 Developer ID 签名 + 公证（`codesign` / `notarytool`）。

## Windows 免安装包（手动触发）

自带运行时已并入 `main`（2026-09-13），所以这个工作流默认就从 `main` 构建；`build_ref` 仍可指向任意
分支或 tag（例如临时验证某个分支的 staging）。

工作流：`.github/workflows/windows-portable.yml` —— 在 Actions 页面 **Run workflow** 手动触发（`workflow_dispatch`），
在 `windows-latest` 上**原生** stage 运行时，产出 `dsh-desktop_<版本>_windows-x64-portable.zip`：
内含 `DSH Desktop/dsh-desktop.exe` + `WebView2Loader.dll` + `runtime/`（Node + dsh + pnpm + 插件市场模板 + 许可清单），
目标机器**无需安装任何东西**。另附同名 `.exe`（7z 自解压包，双击解压，绕开资源管理器解压的 260 字符路径限制）
与 `SHA256SUMS-windows-x64`。

| 输入 | 默认 | 说明 |
|---|---|---|
| `dsh_version` | `0.1.5-rc.2` | 随包附带的 dsh 版本 |
| `node_version` | `22.23.2` | 随包附带的 Node 版本 |
| `build_ref` | `main` | 从哪个分支/tag 构建 |
| `attach_to_release` | 空 | 填一个已存在的 Release tag 就把 zip 一并传上去；留空只作为 workflow artifact |

**打 tag 发版**：`release.yml` 的 `windows-portable` 任务用 `workflow_call` 复用同一份实现
（`build_ref` 传 tag 名），所以**在 `main` 上打 tag** 就能把 Windows 免安装包和 macOS/Linux 产物一起发出去。
macOS/Linux 每个平台会先出精简版，再出 `-bundled` 的自带运行时版（`make bundle-bundled`，见下节流程）。

**实机验证（2026-09-13，run 34752267176 的产物）**：Windows 11 解压到 `D:\dsh` 后双击即启动，
自带 Node 22.23.2 + dsh 0.1.5-rc.2 拉起 Web GUI，首启播种的插件市场可用，会话内
`glob`/`write`/`read`/`grep`/`edit`/`patch`/`todo_write`/`present` 正常。

**为什么必须在 Windows 上 stage**：在 macOS 上用 `npm install --os=win32 --cpu=x64` 装 dsh 时，koffi 的平台包
`@koromix/koffi-win32-x64` 不会被装进依赖树（npm 的 `--os/--cpu` 对全局安装的依赖树不生效），于是 postinstall
回退到就地编译并因缺 CMake 报 `CMake does not seem to be available`（实测）。本地等价脚本：`scripts/stage-runtime.sh`。
## 发布（GitHub Actions）

编排文件：`.github/workflows/release.yml`。

**触发**：推送形如 `v0.1.0` 的 tag（`on.push.tags: v*`）→ 构建并把产物附到同名 Release；
也可在 Actions 页面手动 `workflow_dispatch`（只产出 workflow artifact，不创建 Release）。

**矩阵**：

| 运行器 | 平台标识 | 精简版（需自备 node/dsh） | 自带运行时版（`-bundled`，无需预装） |
|---|---|---|---|
| `macos-14` | `macos-arm64` | `dsh-desktop_<v>_macos-arm64.app.zip` | `dsh-desktop_<v>_macos-arm64-bundled.app.zip` |
| `macos-14`（交叉 `x86_64-apple-darwin`） | `macos-x64` | `dsh-desktop_<v>_macos-x64.app.zip` | `dsh-desktop_<v>_macos-x64-bundled.app.zip` |
| `ubuntu-22.04` | `linux-x64` | `dsh-desktop_<v>_linux-x64.deb` | `dsh-desktop_<v>_linux-x64-bundled.deb` |
| `ubuntu-24.04-arm` | `linux-arm64` | `dsh-desktop_<v>_linux-arm64.deb` | `dsh-desktop_<v>_linux-arm64-bundled.deb` |
| `windows-latest`（复用 `windows-portable.yml`） | `windows-x64` | ——（Windows 只出免安装包） | `dsh-desktop_<v>_windows-x64-portable.zip` ＋ 同名 `.exe` |

**流程**：`preflight`（校验 tag 与 `tauri.conf.json` / `Cargo.toml` / `package.json` 三处版本一致）→
每平台 `make fmt-check`（只检查、不改工作区）+ `make clippy` + `make test`（发布门禁）→ `make bundle`（精简版）→
`make bundle-bundled`（自带运行时版：`runtime-stage` 按 `TARGET` 组装本平台运行时，再以 `--config` 注入
`bundle.resources`）→ 打包并生成每平台 `SHA256SUMS-<suffix>`（该平台两种产物一起）→ `release` job 汇总成
`SHA256SUMS` 并 `gh release upload --clobber`（可重复运行）。

**缓存**：cargo 的 registry 与 `src-tauri/target` 由 `Swatinem/rust-cache` 按平台各缓存一份，
`node_modules` 由 `setup-node` 的 pnpm store 缓存；本轮又给自带运行时的 staging 输入加了缓存
（`.runtime-cache/`：Node 压缩包 + SHASUMS + npm/pnpm store，key 里带平台标识，`runtime.lock`
或版本变量变化时自动换 key，缓存只省下载 —— SHA256 校验照旧）。刻意**不缓存** `src-tauri/runtime/`：
520 MB × 5 个平台会挤占仓库 10 GB 的缓存配额，而它每次都会被整体清空重建；也没有引入第三方的
apt 缓存 action，Linux 的系统依赖仍是每次 `apt-get install`。

**发版步骤**：

```bash
# 1) 三处版本号一起改（preflight 会拦住不一致的情况）
#    src-tauri/tauri.conf.json、src-tauri/Cargo.toml、package.json
# 2) 提交后打 tag 并推送
git tag v0.2.0 && git push origin v0.2.0
```

**校验下载**：`shasum -a 256 -c SHA256SUMS`（Linux 用 `sha256sum -c SHA256SUMS`）。

**当前限制**：产物均未签名、未公证（macOS 首次打开需右键 →「打开」，或 `xattr -dr com.apple.quarantine`）。
自带运行时版（`-bundled`）已在 CI 里全平台构建，但**只有 macOS arm64 与 Windows 做过实机验证**——
Linux 侧能否在干净机器上启动尚未实机确认（见下节）。精简版**需要机器已装 `node` 与 `dsh`**。
按需扩展：把 dmg 加进 macOS 的 `bundles`（`app,dmg`）、AppImage 加进 Linux、以及签名/公证（需要证书 secrets，做法见自带运行时方案 §7）。
## 后续实施计划：自带运行时（无需预装 Node/dsh）

目标：在**没有 Node、没有 dsh** 的机器上双击即用，且首次启动不依赖网络下载。

完整方案：[`docs/design-task-feat-dsh-bundled-runtime.md`](docs/design-task-feat-dsh-bundled-runtime.md)（**v3**，含实测数据与两轮修订记录）。

已经用原型验证过的关键结论（方案据此成立）：

| 结论 | 实测 |
|---|---|
| 官方 Node 发行版**自带 npm** | 48 MB → 解压 187 MB，含 `node/npm/npx/corepack` |
| 只用自带 node 能启动 harness | `env -i PATH=/usr/bin:/bin` 下成功输出 `dsh web:` URL |
| harness **不写自己的安装树** | 安装树被修改文件数 = **0** ⇒ 只读 seed 成立 |
| dsh 树可整体搬迁 | 复制到任意 prefix 后照常启动 |
| 镜像 `bundle.resources` | 已在本仓库配置并实测（空 glob 会让构建失败） |
| 资源复制保真 | mode 与代码签名**保留**；**符号链接被解引用**，悬空链接会让 `tauri build` 直接失败 |
| 签名后 Node 可运行 | 必须 `--preserve-metadata=entitlements`：丢掉 JIT entitlements 时 `node` 启动即 `Trace/BPT trap: 5` |

代价与做法（摘要）：

- 解压后约 **490 MB**（Node 187 + dsh 树 289 + 壳 11），分发包约 150–250 MB；
  自带运行时版本的最低系统版本要提到 **macOS 11.0**（实测 node 22.23.2 的 `minos 11.0`，当前壳声明的是 10.15）；
- **只读 seed + 可写影子前缀**：seed 放在 `Contents/Resources/runtime/`，dsh 核心更新落到
  `app-data/runtime/prefix`（不写签名的 bundle）；
- 实施前必须先落的 P0：**回退/last-known-good、seed 与前缀的版本仲裁、pnpm 随包分发、按平台 staging
  （12 个原生模块）、随包附带插件市场**（全新 DSH_HOME 的 profile 是空的、没有安装入口 ⇒ 用真 pnpm 生成
  6.5 MB 的 profile 模板，仅在 profile 不存在时于首启播种）；
- 实测中升级为 P0 的两项：**签名必须保留 Node 的 entitlements**（否则 node 启动即崩）、
  **staging 必须校验悬空符号链接**（否则 `tauri build` 直接失败）；
- 首启更新策略待定：默认 `auto_update` 会在首启就下载整棵依赖树，与"离线首启"的宣传冲突（方案 §4 给了两个选项）；
- 分阶段：P1 macOS arm64（无 node 可跑）✅ → P2 签名公证（未做）→
  P3 Linux x64/arm64（CI 已产出 `-bundled` 包，实机未验证）→
  P4 universal/Windows/自更新（Windows 免安装包已实机验证；dsh 核心与插件市场的自动更新均已落地）。

## 开机自启

系统设置 → 通用 → 登录项 → 添加 `DSH Desktop.app` 即可（不需要额外代码）。
