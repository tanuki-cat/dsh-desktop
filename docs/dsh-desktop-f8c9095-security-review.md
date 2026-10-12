# dsh-desktop 安全与性能审查报告（f8c9095 / v0.4.10）

审查对象：`dsh-desktop` 本地 `main`（Tauri 2 桌面壳，监管 `dsh web` 并在系统 WebView 中承载 Harness UI）
审查提交：`f8c9095`（`rule: agent rule project`，工作树含未跟踪的 `local-patches/`）
审查日期：2026-10-11
审查范围：`src-tauri/src`（约 15k 行 Rust）、`src/index.html`、`src-tauri/capabilities/`、`src-tauri/tauri.conf.json`、`scripts/`、`.github/workflows/`、未跟踪的 `local-patches/webRuntime-compat/`
审查性质：**只读审查**。未修改任何仓库文件；未构建、未运行测试（本文中的结论均为代码与上游依赖源码层面的事实，不声明测试通过）。

本文是**审查结论 + 修复任务清单**。修复落地后把结果回写到文末「处理状态」，不改写各条原始结论。

## 1. 总体结论

壳的整体防御姿态明显高于同类桌面壳，**没有发现可远程利用的漏洞，也没有发现可提权的问题**：

- Harness 窗口**零 capability**（`capabilities/` 只有 `splash.json`），`withGlobalTauri=false`，无 remote domain、无 devtools、无 shell/fs/opener 插件权限。
- 远程 Harness 页面导航被锁在 `http://127.0.0.1:<port>` 同源；外链只放行 `http/https`；下载被重定向到系统下载目录并做文件名清洗。
- 被监管子进程的 stdout/stderr 统一过 `harness::redact` 后才落盘与上屏；token 只经 Cookie 传递，有测试绑定日志里只出现 `***`。
- 更新走「先在别处构建 → 校验包名+版本 → rename 切换 → 失败回滚」，并有 swap 记录支撑跨进程恢复；端口探测、`ps`/`lsof`、管道读取全部有超时与字节上限。

问题集中在三处：**本机同用户进程之间的身份判据偏软**（H1/H2/L3）、**更新链路的"完整性"只到包名+版本号**（H3）、**从登录 shell 透传环境变量不设黑名单**（H4）。此外还有若干健壮性缺陷与启动路径上的性能热点。

共 4 个高、4 个中、5 个低、7 项性能发现。

## 2. 审查方法与证据

- 分三条独立通道逐文件精读：`window.rs`(3019)；`harness.rs`(1209)/`process.rs`(490)/`takeover.rs`(317)/`shellenv.rs`(134)/`identity.rs`(195)；`update.rs`(1099)/`update_flow.rs`(527)/`transaction.rs`(519)/`locator.rs`(516)/`runtime.rs`(263) 与 `lib.rs`(2767)。
- 用**本机依赖源码**交叉验证第三方行为，而不是依赖推测：
  - `wry-0.55.1/src/wkwebview/navigation.rs:50-84`——`navigation_policy` 只取 `action.request().URL()` 交给回调，**未使用 `targetFrame()/isMainFrame`**（M1 的直接依据）。
  - `cargo/registry/src/*/wry-0.55.1`、`tauri-2.11.5`（远程 origin 的 ACL 判定路径）。
- 对每条结论做**行号回读复核**，报告中的 `文件:行` 均已人工确认指向所述代码。
- 未验证/存疑的部分已在各条「置信度」中显式标注（见 §10）。

## 3. 问题汇总

| # | 级别 | 问题 | 置信度 | 关键位置 |
|---|---|---|---|---|
| H1 | 高 | PID 复用竞态：发信号前只验"还活着"，不验"还是它" | 高 | `process.rs:273`、`takeover.rs:216`、`lib.rs:1386/2357`、`update_flow.rs:505` |
| H2 | 高 | `state.json` 非原子写、读取不校验，却被当作杀进程凭证 | 高 | `process.rs:21/26`、`identity.rs:47`、`lib.rs:1753-1782` |
| H3 | 高 | 核心更新不做完整性/签名验证，且默认跟随 alpha 通道 | 高（机制层面） | `update.rs:641-675`、`transaction.rs:84`、`update.rs:201` |
| H4 | 高 | 登录 shell 环境变量无危险键黑名单，直通子进程 | 高 | `shellenv.rs:24/41`、`lib.rs:1667-1691` |
| M1 | 中 | 导航回调不区分主框架/子框架，外链被反复外开 | 中（子框架行为待真机确认） | `window.rs:2007`、wry `navigation.rs:50` |
| M2 | 中 | 页面上报字符串依赖字段名脱敏，裸凭据会落盘 | 高 | `harness.rs:122`、`window.rs`（`page_diagnostics`） |
| M3 | 中 | 日志文件未设 0600，与 `config.json`/`state.json` 标准不一致 | 高 | `harness.rs:1140` vs `process.rs:441` |
| M4 | 中 | `terminate` 对"已死 leader"无条件杀整个进程组，不校验归属 | 中（概率低、后果重） | `process.rs:223/253` |
| L1 | 低 | 失败页/回退文案把带 token 的启动 URL 显示在界面上 | 高 | `window.rs:2776`、`window.rs:134-141` |
| L2 | 低 | 回滚直接信任 `update-swap.json` 中的路径（含 `remove_dir_all`） | 高 | `transaction.rs:360`、`update_flow.rs:400-407` |
| L3 | 低 | 进程身份判据是纯文本特征，overlay 路径可预测可复制 | 高 | `harness.rs:616`、`lib.rs:1067` |
| L4 | 低 | 未跟踪的 `local-patches/` 在 0.0.0.0 绑定下会放宽 `trustedHosts` | 高 | `local-patches/webRuntime-compat/plugin.js:42-49` |
| L5 | 低 | 未显式钉住 npm registry，继承环境与用户 `.npmrc` | 高 | `update.rs:479-483` |

## 4. 高：详细发现

### 4.1 H1 PID 复用竞态

**代码事实**

- `process.rs:273` `terminate_pid(pid, grace)` 内部只有 `is_alive(pid)` 这一道检查，随后 `SIGTERM` → 轮询 → `SIGKILL`。
- pid 的**身份校验发生在用户答题之前**：`takeover.rs:216`（`ForeignAction::TakeOver`）在 `resolve_foreign_action` 里拿到 pid，而该函数会 `ask_choice` 并等待 `CHOICE_TIMEOUT = 120s`（`window.rs:283`）。`lib.rs:2357` 与 `lib.rs:1386` 在等待结束后才发信号。
- 更新路径同形：`update_flow.rs:505` 的 `StopMode::PidOnly` 也走 `terminate_pid`，且"是否询问"在停止实例之前。
- `takeover.rs:284` `identified_dsh_web` 只在调用点读一次 `ps` 命令行。

**影响**

用户面对接管问题思考、或一次 npm 安装耗时 300s 的窗口内，目标进程退出且 pid 被系统复用（macOS pid 回卷虽慢但非不可能），信号落到无关进程。仓库自己的注释已经承认这类风险（`process.rs:246`："after the leader's exit is free for the OS to hand to an unrelated process"），只是把防护放在了调用点而不是发信号的函数里。

**建议**

1. 把身份判定下沉进 `terminate_pid`：kill 前**瞬时**重验 `process_command(pid)` + `looks_like_dsh_web`，不符则放弃并记日志。
2. 更彻底：spawn 时向子进程注入随机 launch nonce 并写进自己的 argv/环境，接管与清理都以 nonce 为准，废弃文本特征（同时消掉 L3）。
3. 若两者都暂不做，至少把 `CHOICE_TIMEOUT` 之后的 pid 重新探测一次，而不是沿用答题前的值。

### 4.2 H2 state.json 是杀进程凭证但不受保护

**代码事实**

- `process.rs:26-32`：`std::fs::write` 就地截断，非原子；仅 `restrict`（0600）而无属主/符号链接校验；`process.rs:21-24` `read_state` 只做 `serde_json` 反序列化，任何字段都照收。
- `identity.rs:37-61` `self_heal_action`：当 `listener != Some(state.pid)` 且 `looks_ours` 为真时返回 `Terminate`；`looks_ours` 由 `identity.rs:70-81` 判定，只要求 **pid 存活 + `ps` 命令行像 `dsh web` + 父进程是 launchd/systemd（或 ppid=0）**——**与该 pid 是否监听 `state.port` 无关**。
- `lib.rs:1764-1772` 拿到 `Terminate` 后直接 `process::terminate(state.pid, TERMINATE_GRACE)`（组信号）。

**影响**

同一用户下的任意进程写一份 `state.json` 指向另一个 `dsh web` 会话的 pid + 任意端口，下次启动即可静默杀掉它。这不是提权（同用户本就有权 kill），但它是**跨实例误杀**的真实通道：多 `DSH_HOME`、多份 dsh-desktop、用户的终端 `dsh web` 都在射程内。残缺/半截 JSON 也会让启动路径进入未定义分支。

**建议**

1. `tmp + rename` 原子写。
2. `Terminate` 分支要求端口归属一致（`listener_pid(state.port) == Some(state.pid)`）才允许组信号。
3. 引入 H1 建议的 nonce，把"这是我们启动的"从启发式变成事实。
4. `read_state` 增加字段合理性检查（pid != 0、port != 0），并考虑 `O_NOFOLLOW`。

### 4.3 H3 核心更新的完整性只到包名+版本号

**代码事实**

- `update.rs:641-675` `install()` 执行 `npm install -g --no-fund --no-audit`，**未加 `--ignore-scripts`**（会执行上游生命周期脚本），**未显式指定 `--registry`**（`update.rs:479-483` `npm_command` 只覆盖 `PATH`，其余继承环境/用户 `.npmrc`）。
- 更新链路上唯一的校验是 `transaction.rs:84-92` `version_of_package`：读暂存树的 `package.json`，比较 `name` 与 `version`。**这两项正是攻击者可控 registry 也能伪造的字段**，对"这棵树里的代码是谁写的"没有证明力。
- `update.rs:201-206` 默认 tags 为 `latest/next/alpha`，即默认自动跟进预发布通道；`update.rs:377` `may_install` 只把安装范围限制在 `[0.1.5-rc.1, 0.2.1)`（`update.rs:257-258`）。
- `lib.rs:1657-1687` 会把 `npm_config_prefix`/`PNPM_HOME` 指向 app-data，但**没有清理 `npm_config_registry`**（该键由 shell 导入或应用环境继承时会被 npm 读取）。

**影响**

本机同用户进程、或用户 `.npmrc`/代理链路上的中间人，可以把任意通过"包名+版本号"检查的树送进被监管位置并在下次启动时执行。默认跟随 alpha 让暴露面进一步扩大。这是桌面壳在"自动更新第三方代码"这件事上的固有权责，不是实现 bug；但当前实现的校验强度低于该权责。

**建议**（按成本从低到高）

1. `install()` 显式钉住 registry：`--registry=https://registry.npmjs.org`，并清理 `npm_config_registry`/`NPM_CONFIG_REGISTRY`。
2. 校验 tarball 完整性：先 `npm view @deepseek-ai/dsh@<ver> dist.integrity` 取期望值（`dist.integrity` 是 registry 端声明的 tarball 摘要，用 `npm view --json` 读取；壳已用 `serde_json` 解析 dist-tags，同样的读法可以扩展到这里），安装后对 tarball 直接算 sha512 比对。
3. 若上游发布了 provenance：`npm audit signatures` 通过才允许自动安装。
4. 评估 `--ignore-scripts` 的可行性（需确认上游是否依赖 postinstall）。
5. 默认 tags 收敛为 `["latest"]`，把预发布留在显式配置里。

### 4.4 H4 登录 shell 环境直通子进程

**代码事实**

- `shellenv.rs:24-33` `RESERVED` 只含 HOME/SHELL/TMPDIR/SHLVL/PWD/OLDPWD/TERM_SESSION_ID/XPC_SERVICE_NAME，`shellenv.rs:35-37` 追加 `_` 与 `DSH_*`。
- `shellenv.rs:41-58` `parse_env` 只校验键名是合法标识符；`shellenv.rs:99` `import` 先试 `-lic`（会执行交互式 rc）。
- `lib.rs:1667-1691` `ChildEnv::assemble` 把导入的变量**逐一 push 进子进程环境**，只有 `PATH` 被替换、`npm_config_prefix`/`PNPM_HOME` 在 bundled 模式下被重写。

**影响**

用户 rc 导出的 `NODE_OPTIONS`（`--require /path/x.js`）、`LD_PRELOAD`、`DYLD_INSERT_LIBRARIES`、`BASH_ENV`、`PYTHONSTARTUP` 会原样进入被监管的 node 进程。这在"能改 `.zshrc` 的进程本就能执行代码"的前提下不算提权，但既然壳特意维护了 `RESERVED`，把危险键一并挡掉的成本极低，且能防住"配置目录可写、`.zshrc` 不可写"这类中间态。

**建议**

按平台维护一张危险键表（`NODE_OPTIONS`、`LD_PRELOAD`、`LD_LIBRARY_PATH`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`、`BASH_ENV`、`ENV`、`ZDOTDIR`、`PYTHONSTARTUP`、`PERL5OPT`），导入时丢弃并只记键名；`config.json` 的 `env` 保持为唯一的显式覆盖通道（它本来就是用户书写的）。

## 5. 中：详细发现

### 5.1 M1 导航回调不区分主框架/子框架

`window.rs:2007-2016` 的 `on_navigation` 只判 `scheme=="http" && host=="127.0.0.1" && port==port`，不同源即 `open_external`。上游 `wry-0.55.1/src/wkwebview/navigation.rs:50-84` 的 `navigation_policy` 把 `action.request().URL()` 直接交给回调，**没有传 `targetFrame().isMainFrame`**（已读源码确认）。因此页面里的一个外部 `<iframe>`（模型输出、抓取内容都可能带）就能触发系统 opener，且可反复触发——无用户点击的浏览器/进程洪水。

**建议**：wry 暴露主框架信息前，先对同一 URL 做时间窗去重与限流；同时向上游提 issue。**真机验证建议**：在 Harness 窗口注入一个指向外部的 iframe，观察是否触发 `open`（本条是三条通道里唯一未在真机验证的结论）。

### 5.2 M2 页面上报依赖字段名脱敏

`harness.rs:116-128` 的 `redact` 由 `redact_bearer` + 13 个 `SECRET_KEYS` 组成，只认字段名；而 `page_diagnostics` 会把页面 `console.error/warn` 的任意文本经探针带回壳日志（`window.rs` 的 `PAGE_DIAGNOSTICS_SCRIPT` 与 `wrap("error"|"warn")`）。不带头字段名的密钥（provider 报错原样回显、对象 `String()` 后的整段内容）会明文落盘。

**建议**：追加"高熵长 token 形态"的兜底脱敏；或把 console 采集降级为仅记错误类型 + 前 N 字符。

### 5.3 M3 日志文件权限

`harness.rs:1136-1150` `Logger::with_limit` 只 `create_dir_all` + `OpenOptions::create(true).append(true).open(path)`，**未调用 `process::restrict`**；而 `config.json`（`lib.rs:346/387`）与 `state.json`（`process.rs:30`）都显式设了 0600。默认 umask 下日志为 0644，内容包含 child PATH、绝对路径、错误文本。

**建议**：`Logger::open`/`with_limit` 后同样 `restrict`，并覆盖轮转出的 `.1/.2/.3`。

### 5.4 M4 terminate 对已死 leader 无条件杀组

`process.rs:223-229` → `process.rs:253-266`：`is_alive(pid)==false` 时直接 `kill(-pgid, SIGTERM)`、200ms 后 `SIGKILL`，**从不校验该 pgid 是不是本次 spawn 的那个**。注释（`process.rs:217-222`）说明这是为了收拾 node worker 与 MCP 子进程，动机成立；但 pgid 与 pid 一同可被复用，落在旧 `state.json` 记录上时可能命中无关进程组。

**建议**：仅当该 pid/pgid 曾在本次进程的 `LIVE` 表登记过时才用组信号，否则降级为逐 pid + 命令行复核。

## 6. 低：详细发现

| # | 事实 | 建议 |
|---|---|---|
| L1 | `window.rs:2776`（加载失败页）与 `window.rs:134-141` `browser_fallback_detail` 把 `http://127.0.0.1:PORT/?token=…` 原样显示。日志侧已由 `redact` 处理，页面侧没有 | 展示前剥离 query，与日志同等对待 |
| L2 | `transaction.rs:360-363` 读入的 `target`/`backup` 被 `update_flow.rs:400-407` 原样交给 `rollback_with`（内部含 `remove_dir_all`）。需本机同用户写权限，属健壮性而非提权 | 校验路径必须位于 app-data/`runtime` 前缀内 |
| L3 | `harness.rs:616-621` `looks_like_dsh_web` 是 `contains("dsh") && !contains("plugin") && ("--profile web" 或 token=="web")`；`lib.rs:1067-1069` `is_our_own_launch` 只要求 argv 含 overlay 路径，而该 YAML 路径（`<app-data>/force-print-url.yml`）可预测、内容可复制 | 用 nonce 替代文本特征（与 H1/H3 同一改造） |
| L4 | `local-patches/webRuntime-compat/plugin.js:42-49`：`bindHost === "0.0.0.0"` 时把本机所有非内部 IPv4 并入 `trustedHosts`。当前壳固定 `127.0.0.1`，该值为空数组、无实际放大；但垫片会随 CLI 的绑定策略变化静默放宽 fence。另 `package.json` 写 `"main": "index.js"` 而目录里没有该文件（靠 `apply.sh` 复制 `plugin.js` 才自洽） | 该目录纳入 git 或删除；`main` 改为与实际文件一致；注释里写清 trust 边界的触发条件 |
| L5 | `update.rs:479-483` `npm_command` 只覆盖 `PATH`，registry 由环境/用户 `.npmrc` 决定；`lib.rs:1657-1687` 未清理 `npm_config_registry` | 见 H3 建议 1 |

## 7. 性能发现

| # | 事实 | 建议 |
|---|---|---|
| P1 | **启动路径最长阻塞**：自动更新允许 `INSTALL_TIMEOUT = 300s`（`update.rs:23`），且发生在 spawn Harness **之前**（`lib.rs:2419-2478` 的提交换在 spawn 之前完成）；登录 shell 抓取另有 8s 预算（`shellenv.rs:17`），虽然与回滚检查并行但 join 在主线程（`lib.rs:1823-1824`）。用户看到的是停在 splash 的"正在检查 dsh 更新…" | 把安装挪到 Harness 起来之后的后台任务；或先起旧版本、装完再提示重启 |
| P2 | 探针双份预算：`PROBE_TIMEOUT` 连接 600ms + 交换再 600ms（`harness.rs:403/431-439/459-471`），单次最坏 ~1.2s；而 `takeover.rs:303-313`、`lib.rs:1133-1141`、`lib.rs:2366-2373` 以 200ms 轮询它；`takeover.rs:289-300` 每 400ms 调一次 `harness_listener`（probe + `lsof` 全进程表扫描），8s 宽限内最多 20 轮 | 合并为单一 deadline；轮询退避 |
| P3 | 逐行脱敏：`harness.rs:122-128` 对每行执行 1 次 bearer 扫描 + 13 个字段各一遍 `to_ascii_lowercase` 与分配（仓库测试注释记录 256KiB 单行 ≈180ms / 3.4MB 临时内存） | 先做一次小写副本 + 单遍扫描 |
| P4 | 页面常驻定时器：`SCROLL_ANCHOR_SCRIPT` 的 1s `setInterval` 从不清除（`window.rs:1468-1473`）；稳态只做一次 `isConnected` 判断，但 `data-*` 钩子被改名后会触发 DOM 祖先遍历 + `getComputedStyle`（`window.rs:1311-1335`） | 命中钩子时降频或改 `ResizeObserver` 驱动；钩子缺失时退避 |
| P5 | 存活探测回传：每 15s 一次 `eval`（`window.rs:1032-1035`），最坏回传 50 条 × 500 字符（`window.rs:1488-1492`，约 25KB）并逐行写盘 | 单次上限降到 10 条或聚合成一行摘要 |
| P6 | 初始化脚本每次文档加载重复注入（`window.rs:2067-2069`、`lib.rs:2729-2753`）：默认 5 段合计约 22KB，自动重载（`LIVENESS_RELOADS=3`）时叠加 | 单次开销小，维持现状；脚本增长时考虑合并 |
| P7 | 日志每行 `writeln!` + Mutex（`harness.rs:1152-1168`）；`LOGGERS`/`Ring`/`Drained` 的 `unwrap()`（`harness.rs:63/1124`、`process.rs:144`）在毒化时会 panic（`app_log` 已做毒化容忍，其余没有） | 与 `app_log` 一致的毒化容忍；必要时批量写 |

## 8. 已确认无问题（复核过代码或上游源码）

1. **注入脚本无字符串拼接逃逸**：`status_script`/`choice_script` 全部经 `serde_json` 序列化后插入；其余脚本的占位符只替换常量；测试绑定该行为。
2. **注入 JS 不外泄数据**：不读表单/剪贴板/token，探针只上报 `navigator.userAgent` 与特性探测结果。
3. **远程 Harness 页面拿不到 IPC**：capability 只绑定 `splash` 窗口；Tauri 对非本机 origin 强制 `Origin::Remote` ACL 判定。
4. **外链只放行 `http/https`**（`window.rs:2885-2904`）：`file:`/`javascript:`/`data:`/自定义 scheme 全部拒绝；Windows 走 `ShellExecuteW`（`parameters=null`，`window.rs:2993-3000`）无 shell 注入。
5. **状态页无 `innerHTML`**：`src/index.html` 全程 `textContent`，按钮用 `createElement` 构造，无 `target=_blank`。
6. **无危险 `unsafe`**：唯一的 Windows `ShellExecuteW` 绑定指针均 NUL 结尾且存活到调用结束，`CoInitializeEx`/`CoUninitialize` 正确配对。
7. **所有监听与连接限 `127.0.0.1`**（`lib.rs:486`、`harness.rs:455/572`）；CLI 明确拒绝 `--host 0.0.0.0`（`docs/design-task-feat-dsh-tauri-desktop-shell.md:43`）。
8. **子进程有独立进程组**（`harness.rs:802-806`），`pid_alive` 用 `waitpid(WNOHANG)` 区分僵尸（`process.rs:413-424`）。
9. **资源有界**：日志 5MiB×3 轮转、子进程管道 256KiB/64KiB/1MiB 上限、`ps`/`lsof` 5s 预算、启动 90s/30s 超时、自动重启 3 次上限。
10. **token 不经日志外泄**：写入前统一 `redact`，URL 只从原始行解析（`harness.rs:925`），测试断言日志只出现 `***`。
11. **未提交凭据**：`.github/workflows/` 与 `scripts/` 无硬编码密钥（已 grep 复核）。
12. **核心更新事务本身是稳的**：`Staging::create` 先清除同名残留（`transaction.rs:42-46`）、`verify_install` 在替换前校验、`commit` 失败会把旧树放回（`transaction.rs:219-234`）、`write_swap` 先于替换、`recover_pending_swap` 跨进程可续。

## 9. 建议的修复顺序

1. **H1 + H2 + L3 一起做**（同一次改造：spawn nonce 替代文本特征 + 原子写 + 端口归属复核）。这三条是同一根因，单独修任何一条收益都有限。
2. **M3**（日志 0600）：一行改动，立即收益。
3. **H3 第 1、2 步**（钉 registry + 校验 integrity）：纯增量，不改变正常路径行为。
4. **H4**（危险环境键黑名单）：独立小改动。
5. **M1**：先加同 URL 时间窗去重（低风险），真机确认子框架行为后再决定是否依赖 wry 上游。
6. 其余按 P1 → P2 的性能项排序，其中 P1 需要产品判断（是否接受"先起旧版本再更新"）。

## 10. 未验证 / 存疑

- **M1 的子框架行为**：结论来自 `wry 0.55.1` 源码（回调未携带 `isMainFrame`），未在真机用 WKWebView 委托抓取确认 `decidePolicyForNavigationAction` 对子框架的调用形态。按"中"级处理，修复前建议先真机验证。
- **H3 的中间人可行性**：取决于用户 `.npmrc`、代理与环境；本文只陈述"壳未做超出包名+版本的校验"这一机制事实，未构造 PoC。
- **本文未运行任何测试或构建**。仓库既有基线（`cargo test`）不在本次审查范围内，未复核其当前通过状态。

## 11. 处理状态

修复落地于 2026-10-11（同一工作树，未提交）。验证：`cargo fmt --check` 干净、
`cargo clippy --all-targets -- -D warnings` 干净、`cargo test` 239 个 lib 测试 + 7 个集成测试
全部通过（基线为 226 + 7，新增 13 个）。**未运行** `make test-live` 与真机 GUI 验证。

| # | 状态 | 处理说明 |
|---|---|---|
| H1 | 已修复 | 新增进程身份复核：`process::process_start_marker`（`ps -o lstart=`）在发信号前比对，不符则不发信号；`Identity{Ours,Pinned,Unreadable}` + `still_the_same` 区分"自家子进程 / 已钉住 / 读不到"。读不到一律按"未知"放行，因此没有 `ps` 的精简 Linux 仍能自清理。`terminate_pid`（外部实例）同样复核 |
| H2 | 已修复 | `write_state` 改 `tmp + rename` 原子写并收紧权限；`HarnessState` 新增 `started_marker`（`#[serde(default)]`，旧记录读作"未知"）；`lib.rs` 在自愈分支前用 `recorded_process_still_here` 复核；`terminate` 的组信号只对 `spawned_here` 的 pid 生效 |
| H3 | 部分修复 | **已做**：`npm_command` 显式钉 `--registry https://registry.npmjs.org`，并清除 `npm_config_registry`/`NPM_CONFIG_REGISTRY`/`npm_config_userconfig`/`NPM_CONFIG_USERCONFIG`（含回归测试）。**未做**：tarball 完整性校验与 `npm audit signatures` — 需要引入 `sha2`/`base64` 直接依赖或重构安装路径（`npm pack` + `tar` 解包），属于安装路径的独立改造。`--ignore-scripts` 未加：需先确认上游不依赖 postinstall。默认 tags 未收敛（属产品决策） |
| H4 | 已修复 | `shellenv` 新增 `DANGEROUS` 表（`NODE_OPTIONS`/`LD_PRELOAD`/`DYLD_*`/`BASH_ENV`/`ZDOTDIR`/`PYTHONSTARTUP`/`npm_config_registry` 等 19 项）与 `is_dangerous`；`ChildEnv::assemble` 丢弃并**只记键名**，`config.json` 的 `env` 仍是显式通道 |
| M1 | 部分修复 | **已做**：`open_external_once` 对同一 URL 做 60 秒时间窗去重，两个回调（`on_navigation`/`on_new_window`）都走它 —— 重复外开从"每次导航一次"降到"每分钟一次"。**未做**：主框架判定，`wry 0.55.1` 不提供 `isMainFrame`，需上游改动；限流把影响面压到可接受 |
| M2 | 已修复 | `redact` 末尾新增 `redact_credential_shapes`：token 前缀（连字符+下划线）、或"≥32 字符 + 字母数字混合 + 至少有连字符"的 run 一律替换为 `***`；纯十六进制摘要（SHA/MD5、随机 id）与短值保持可读。7 个新测试（真 key 形态 + 5 类普通文本不误伤） |
| M3 | 已修复 | `Logger::with_limit` 与 `write` 中重建句柄后都调用 `process::restrict`（0600），覆盖轮转产生的新文件 |
| M4 | 已修复 | `clean_orphaned_group` 不再无条件执行：仅当 pid 在 `SPAWNED` 登记表内（`adopt` 登记，`forget_spawned` 注销）才发组信号；新增**反向测试**确认未登记的 pid 不会被打到 |
| L1 | 已修复 | 新增 `window::display_url`（去掉 query/fragment），用于浏览器回退文案与加载失败页；`window/tests.rs` 的断言改为"不得出现 token" |
| L2 | 已修复 | `UpdatePaths::owns` 校验记录里的路径（必须在运行时根内，或位于 `node_modules` 之下的包目录）；`recover_pending_swap` 与 `roll_back_core_update` 都在动作前校验，不合格则丢弃记录并记日志。2 个新测试（含"记录外的目录不得被移动或删除"的反向用例） |
| L3 | 部分修复 | 身份不再只靠命令行：`looks_like_dsh_web` 之外增加了启动时间复核（见 H1/H2）。overlay 路径仍是文本特征，未引入 nonce |
| L4 | 已修复 | `package.json` 的 `main` 改为 `plugin.js`（原来是靠 `apply.sh` 复制成 `index.js` 自洽）、`apply.sh` 与 `plugin.js` 的注释补上信任边界说明（`0.0.0.0` 绑定下 `lanAddresses` 才非空）。该目录仍未纳入 git |
| L5 | 已修复 | 同 H3 第一条：registry 钉死并清除环境覆盖 |
| P1 | 未修复（产品决策） | 把 300s 的 npm 安装挪到 Harness 起来之后，会改变"更新失败就不用重启"的既有语义；需要先决定是否接受"先起旧版本、装完再提示重启" |
| P2 | 已修复 | `probe` 与 `request` 改为**单一 deadline**（连接与交换共用 600ms），最坏耗时从 1.2s 降到 0.6s |
| P3 | 部分修复 | 报文行的分派与形状判定改为单遍扫描；`redact_field` 仍按 13 个键各扫一次（每条 `to_ascii_lowercase` 都在原地做，已是线性）。未做 Aho-Corasick 式合并 |
| P4 | 已修复 | 两个循环共用一个 `settle` 预算（`SETTLE_TRIES=120`），用尽后 `armed=false` 彻底停止探测；`detach`（会话切换）才重置预算。新增测试：500 次心跳只查询 240 次 DOM |
| P5 | 已修复 | `PAGE_NOTE_LIMIT` 50 → 10 |
| P6 | 按结论维持 | 报告本身的建议就是"维持现状"，未改动 |
| P7 | 已修复 | `Ring`/`Logger`/`APP_LOGGER`/`LIVE`（含 `shutdown` 与 `disown`）全部改为毒化容忍 |

### 修复过程中被测试推翻的两个假设

1. **`upper > 0 && lower > 0` 不可用**：真实的 DeepSeek key（`sk-` + 32 位小写十六进制）没有大写字母。改为"字母 + 数字 + 连字符"，并用 `is_hex_digest` 把纯十六进制摘要排除。
2. **"找到滚动容器就停"不足以停掉探测**：`box` 一旦非空就永远非空，失效的循环会继续每秒探测。测试（500 次心跳的 DOM 查询计数）在一个"看起来已经修好"的版本上仍然报出 1002 次查询，才暴露出必须用一个共享的、可耗尽的预算。

### 仍未验证

- `process_start_marker` 的 `lstart` 解析只在本机 `ps` 可用的机器上有意义；本次运行环境禁止 `ps`（`Operation not permitted`），因此测试走的是"读不到 → 未知 → 放行"分支，**带 marker 的比对路径没有在真实 `ps` 输出上跑过**。
- H3 的完整性校验、M1 的主框架判定、P1 的启动时序均未实施，理由见上表。

## 12. 复核修正与追加修复（2026-10-12）

本节替代第 11 节对当前工作树的状态描述；原始结论和首次处理记录保留为历史，不作为当前实现的依据。

### 结论修正

- H1/H2/H4 等主要属于同用户身份误判、误清理及环境加固，不据此认定可提权的高危漏洞。
- npm 自身通过 pacote 校验 registry 给出的 tarball integrity；壳额外检查包名、版本。来自同一 registry 的第二份摘要不能独立证明发布者身份。
- global 安装树不能直接运行 `npm audit signatures`。签名/provenance 门禁需要独立设计，并在任何不可信脚本执行前完成。
- M1 只确认 wry 回调缺少主框架信息，尚未验证模型输出到外部 iframe 再到系统 opener 的完整真机利用链。

### 已完成的追加修复

| 范围 | 当前实现与验证 |
|---|---|
| H1/H2/M4 | macOS 使用内核启动秒/微秒，Linux 使用启动 ticks + boot ID，Windows 使用进程 creation FILETIME。外部接管在等待确认前采集身份，TERM/KILL 前分别复核；读取失败不授权终止。本次创建的组在 spawn 后登记，历史接管只拥有 PID-only 清理，终止成功和 disown 注销登记。原生身份稳定、不同进程区分、错误 marker 不发信号、未登记和接管不获得组权限都有回归测试。 |
| H2 状态持久化 | 校验 PID、非零端口、绝对 cwd；排他创建唯一临时文件，Unix 创建时即 0600，写入同步后替换。Windows 使用 replace-existing 的系统移动接口并收紧临时文件 ACL。旧 marker/无 marker 不用于自动清理；固定临时链接不会被跟随。 |
| L2 | target 必须匹配独立解析并授权的具体 DSH 安装目录，backup 必须为 rollback 下版本对应的单代目录。拒绝路径穿越、未知目标、根目录、重叠路径和符号链接重定向；检查 failed 目的地，正常提交及回滚/恢复均校验。系统安装不能重新独立定位时不依据 swap 记录授权它。 |
| H4 | 在 Harness、npm、启动探针/helper 启动前删除继承的危险变量。Harness 再应用显式 config.env 覆盖；不记录变量值。测试不修改进程全局环境。 |
| H3/L5 | npm 查询和安装使用各自隔离的 user/global/project 配置及 cwd，清理 npm_config_* 继承覆盖（保留显式 cache 路径供隔离缓存使用），固定官方默认及 @deepseek-ai scoped registry，并开启 TLS 校验。保留标准代理/CA 环境；不再继承用户 npmrc 的 registry、prefix 等配置。真实 npm 离线 config 查询验证 scoped 覆盖无效。 |
| M1 | 同 URL 2 秒去重 + 全局每 10 秒最多 4 次外开；交替 URL、随机 query 不能突破全局上限，限流状态有界。仍不是主框架或用户手势验证。 |
| M2/P3 | 已知凭据前缀不再要求数字；补充裸 JWT 识别，扫描 underscore 等长 run 不反复扫描已累计内容。无数字前缀 key、JWT、普通文本及长标识符均有测试；任意无特征的裸秘密仍不能保证自动识别。 |
| M3 | 同时收紧保留的旧日志备份权限；新文件与轮转文件继续收紧权限。 |
| P4 | 绑定成功停止快速发现循环；每个绑定只创建一个 ResizeObserver，失效先 disconnect，再发现新滚动器。预算耗尽后由 DOM 变动重新发现；旧引擎低频补偿。测试验证 100 次绑定后心跳仍只有一个 observer，以及耗尽预算后的会话出现和替换。 |

### 验证及边界

实际执行并通过：

- `cargo fmt --manifest-path src-tauri/Cargo.toml --check`
- `cargo clippy --offline --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --offline --manifest-path src-tauri/Cargo.toml`：250 个单元测试通过，4 个 Node 执行的兼容层集成测试通过。3 个受 DSH_DESKTOP_LIVE_TESTS 控制的联网用例走未启用分支，不作为联网验证证据。

未运行真机 GUI、联网安装、Linux/Windows 本机验证。保留默认 tags、生命周期脚本策略和同步更新时序；provenance 策略不在本次改动中。启动标记不构成对同用户恶意进程的身份认证，原生采样与 Unix 发信号之间仍存在小的系统调用竞态窗口。本次不宣称消除了同用户任意代码执行风险。

IDE 未打开本项目，IDE 编译接口不可用；以上编译、lint 与测试证据来自 Cargo。
