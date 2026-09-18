# DeepSeek Harness Tauri 桌面壳实施方案（v2 · 已按实现核对）

> 本文随 dsh-desktop 仓库分发；在该仓库中，文中的 dsh-desktop/ 即仓库根目录。

> 取代 v1 附件 `deepseek-harness-tauri-desktop-plan.md`。本文保留 v1 的架构决策，修正其与实现不符或有遗漏的部分，
> 并对每条关键假设标注**实测**或**文档/代码核实**的结果。
>
> 核对基准：`@deepseek-ai/dsh` **0.1.5-rc.2**（`/opt/homebrew/lib/node_modules/@deepseek-ai/dsh`），macOS 实测环境。

---

## 0. 相对 v1 的修正清单

| # | v1 的说法 | 核对结果 | v2 的结论 |
|---|---|---|---|
| 1 | locator 只需找到 `dsh` | `dsh` 的真实文件 `lib/bin.js` 首行是 `#!/usr/bin/env node`；GUI 启动的进程 PATH 不含 `/opt/homebrew/bin`（**实测 exit 127**：`env: node: No such file or directory`） | 必须**同时解析 node**，并用 `<node> <dsh 真实 js>` 启动，不要依赖 shebang |
| 2 | 未提工作目录 | README：“The invoking directory is the default workspace root” | 必须显式设置 `current_dir`（= agent workspace），默认 `$HOME` 且可配置、可记忆 |
| 3 | stdout URL 行一定存在 | `printUrl` 是 web profile 的 config（默认 true），可被 `$DSH_HOME/cordis.patch.yml` 关掉；**实测关掉后进程正常运行但 stdout 完全为空** | 启动时用 launcher 级 `--patch` 覆盖强制 `printUrl: true`（**实测有效**），并设置 URL 超时 |
| 4 | launcher 参数 | **实测**：`--patch` 放在 app flag（`--no-open --port 0`）之后会被当成 app 参数，报 `error: unknown option '--patch'` | launcher flag 必须全部写在 app flag **之前** |
| 5 | token 是一次性的，Rust 不能碰 | **实测**：token URL 可重复访问（两次都是 `303 See Other`），303 带 `Set-Cookie`（`HttpOnly; SameSite=Strict; Max-Age=30d`）与 `location: /` | 仍由 WebView 首次导航；但**失败可安全重试 token URL** |
| 6 | 未讨论重启后的会话恢复 | **实测**：cookie 载荷含 `"authority":"127.0.0.1:<port>"`，而 `--port 0` 每次端口不同 | 每次启动都必须使用**当次** token URL；不得复用上次 cookie 或直接打开根路径 |
| 7 | 未提已有实例 | `--port 0` 保证每次新起一个 harness；用户可能已有 CLI/Automator 起的 `dsh web` | 增加“实例策略”一节（隔离 `DSH_HOME` 或检测提示） |
| 8 | macOS/Linux 只覆盖正常退出 | 无 Job Object 等价物；Tauri 崩溃/被强杀会留下 node 进程 | 增加状态文件 + 下次启动自愈清理 |
| 9 | 未提与 Electron 桌面版的关系 | `lib/bin.js` 有 `rejectElectronProfile`：`profile "desktop" is managed exclusively by the Electron application` | 明确边界：本方案是轻量替代壳，只用 `web` profile，禁用 `desktop` 名 |
| 10 | WebView 能力只提外链 | 附件上传、下载、`window.open` 均未覆盖 | 增加 WebView 集成清单（含不需 capability 的说明） |
| 11 | 日志直接落盘 | URL 行**含 token** | 日志脱敏 + 轮转 |
| 12 | 验收清单缺项 | — | 增补：休眠唤醒、首启、崩溃残留、多显示器/缩放 |

---

## 1. 目标与范围（沿用 v1）

用 **Tauri 2 + 系统 WebView** 给 `dsh web` 提供一个桌面窗口，不修改 Harness、不复制前端、不重实现业务逻辑。

四项职责：启动 Harness → 捕获访问 URL → 系统 WebView 打开 → 管理子进程生命周期。

---

## 2. 已核对的 Harness 事实（后续设计全部基于这些事实）

**F1 启动与参数**
- `--port 0` 官方支持：`--help` 文案为 “listen port; pass 0 to let the OS pick a free one”，`dsh-host-webserver` 中 `port: z.natural().max(65535)`，`get port()` 注释 “the OS-assigned value when config.port is 0”。**实测**绑定 `127.0.0.1:59753` / `59887` 等随机端口。
- `--host 0.0.0.0` 被明确拒绝（**实测** exit 1，提示 “intentionally not supported yet for safety”）。
- launcher flag（`--profile`/`--patch`/`--from-default-profile`）必须在 app flag 之前；第一个未识别 token 之后的参数全部转交 app（**实测**）。

**F2 就绪信号**
- URL 行由 `console.log` 输出，即 **stdout**（**实测** stdout 有、stderr 为空），且仅在 loader settled 后播报，每个 root 一次（`ANNOUNCED_ROOTS` 去重）。
- **实测**：web profile 为 `patchReload: live`，运行中新增 home patch 后**没有**再次播报，进程保持存活 ⇒ **不能用“等待重新播报”做恢复**；恢复路径改为重试当前 URL。仍建议持续读 stdout（写日志/诊断），但不要依赖它。

**F3 认证模型**
- `GET <token URL>` → `303 See Other` + `Set-Cookie: dsh-auth-<hash>=v1.<payload>.<sig>; Max-Age=2592000; Path=/; HttpOnly; SameSite=Strict` + `location: /`。
- 带 cookie → `200`；**无 cookie → 401**；**Host 非法 → 401**。
- cookie payload 含 `authority: 127.0.0.1:<port>`；签名密钥持久化在 `DSH_HOME` 的 credential store 中
  ⇒ **同一 host:port 下 cookie 跨进程重启仍有效**（**实测**：旧 cookie 访问重启后的新进程 → `200`，无 cookie → `401`）。
- `launchToken` 是 `encodeBase64Url(randomBytes(32))` 并按进程缓存在内存 Map 中（**代码核实**）
  ⇒ **其它进程启动的实例，外部无法恢复其 token**，只能用它自己打印的那条 URL。
- 无 cookie 探测的特征响应（**实测**）：`401` + body `dsh web authentication required; reopen the URL printed by dsh web.` —— 可作为"端口上是否是 harness"的无副作用探针。

> **2026-09-15 修正（判据收紧）**：上面这条探针此后被实现成「`401` **或** 任意 `200` 都算 Harness」，
> 理由是 `200` 代表"已有会话的 Harness"。这个推论不成立：本探针**从不带 cookie**，而认证栅栏是
> **无条件**的（`BrowserAuth::writeUnauthorized` 对所有无 launch-token cookie 的请求一律回 401），
> 所以 `200` 只可能来自非 Harness。该分支已删除，判据收敛为唯一的 401 栅栏 —— 见
> `docs/dsh-desktop-v0.4.1-code-review.md` 第 1 项。

**F4 进程与运行环境**
- `dsh` 的 shebang：`#!/usr/bin/env node`；PATH 无 node 时 **exit 127**。
- `SIGTERM` → 优雅退出（**实测** 2 秒内退出且端口释放）。
- 冷启动（全新 `DSH_HOME`，从模板初始化 web profile）**实测 4 秒**，无需 pnpm 安装。
- **现实注意**：本机 `~/.dsh/profiles/web/package.json` 显示已装第三方插件（`dshmarket`、`dsh-better-sidebar`、`dsh-dream-skin`）且 `patchReload: live`。真实 profile 自带 `node_modules`，桌面壳若改用独立 `DSH_HOME` 会得到“干净但没有这些插件”的环境 —— 该取舍需写进 §6。
- 工作目录 = 调用目录 = 默认 workspace root（README）。

**F5 名空间**
- `--profile desktop` 被 CLI 拒绝（Electron 专属）；自定义 profile 名必须是非 shipped 的名字。

---

## 3. 架构（v2）

```text
DSH Desktop (Tauri 2)
├── Rust Supervisor
│   ├── locator      → 解析 dsh 真实 js 路径 + node 路径
│   ├── spawn        → node <dsh.js> --profile web --patch <overlay> --no-open --port 0
│   ├── 持续读 stdout/stderr → 日志(脱敏) + 环缓冲 + URL 事件
│   ├── 状态文件      → pid / port / cwd（崩溃自愈）
│   └── lifecycle    → 进程组 SIGTERM → 5s → SIGKILL
└── System WebView（Harness 页面：无任何 Tauri capability）
```

**不新增 Tauri ↔ Harness IPC，不给远程页面 capability**（v1 的核心安全决策，保留）。

---

## 4. 启动时序（修正版）

| 步 | 动作 | 失败处理 |
|---|---|---|
| 1 | Tauri single-instance：已有窗口则 focus | — |
| 2 | 读状态文件，若上次的 pid 仍存活且命令行匹配 → SIGTERM 清理（自愈） | 清理失败则记日志继续 |
| 3 | locator：`DSH_DESKTOP_DSH` 环境变量 → 记忆路径（`config.json` 的 `dsh_path`）→ PATH → 常见目录 → login shell（`/bin/zsh -lc 'command -v dsh'`，**实测可用**） | 找不到 → 错误页，文案指向 `dsh_path` / `DSH_DESKTOP_DSH`（**2026-09-13 修正**：不做“选择 dsh 路径…”选择器，记忆路径改为手写配置字段） |
| 4 | `realpath` 解析 symlink 得 `dsh.js`；定位 node：`dsh` 同目录优先 → PATH → login shell（**实测** 两条路径都拿到 `/opt/homebrew/bin/node`） | 找不到 node → 错误页明确提示“缺 node” |
| 4b | **解析运行时**（自带运行时启用时）：按自带方案 §2.3 的顺序选 node 与 dsh 树（显式环境变量 → 自带/影子前缀取版本更高者 → 系统），记录来源与版本；架构/能力门槛不过则回退 | 无可用运行时 → 错误页 |
| 5 | 确定 workspace：记忆值 → 用户选择 → `$HOME` | — |
| 6 | 写 overlay 文件（强制 `printUrl: true`），启动进程（独立进程组） | spawn 失败 → 错误页 |
| 7 | 持续读 stdout/stderr；解析 URL 行 | 超时（首启 90s / 常态 30s）→ 错误页 + 最近 200 行日志 |
| 8 | 创建主窗口加载 token URL | 加载失败 → 允许**重试同一 URL**（token 非一次性，实测；**2026-09-13 落地**：`on_page_load` 未在 20s 内报完成才判失败） |
| 9 | 不依赖重新播报：窗口加载失败就重试同一 token URL（**实测可重复使用**） | 连续失败 → 重启 harness 取新 URL（**2026-09-13 修正**：只实现“退避重试同一 URL，最多 3 次”，且仅在重试时端口已不再以 Harness 身份应答才重试；重试耗尽给错误页，不重启 harness） |
| 10 | 退出/崩溃 → 清理状态文件 | — |

> 自带运行时（打包 Node + dsh）下的运行时解析、门槛与 PATH 语义见
> [`design-task-feat-dsh-bundled-runtime.md`](./design-task-feat-dsh-bundled-runtime.md) §2.3 / §5 / §18；
> 两者的接口完全一致：只换 `node` 与 `dsh.js` 的来源，启动参数、URL 解析、进程组与退出回收不变。

**启动命令（关键修正）**

```rust
// launcher flags 必须全部在 app flags 之前
let child = Command::new(&node_path)                 // 不用 dsh 的 shebang
    .arg(&dsh_js_path)                               // realpath 后的 lib/bin.js
    .args(["--profile", "web"])
    .arg("--patch").arg(&overlay_path)               // 强制 printUrl
    .args(["--no-open"])                              // 端口用固定值（config.port，默认 3080）
    .arg("--port").arg(port.to_string())              // authority 稳定才能复用 cookie / 接管上次实例
    .current_dir(&workspace)                          // 必须显式指定
    .env("DSH_HOME", &dsh_home)                       // 见 §6 实例策略
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0)                                 // Unix：独立进程组
    .spawn()?;
```

**端口策略权衡（v2.1 修正）**

- `--port 0`：永不冲突，但每次 authority 都变 ⇒ **放弃 cookie 复用与"接管上次实例"的能力**。
- **固定端口**（如默认 3080）：authority 稳定 ⇒ 首次兑换过 cookie 后，**后续启动可直接加载根路径、无需 token**（实测 cookie 跨重启有效），也才能"接管"自己上次启动且仍存活的实例。
- 建议：默认固定端口 + 冲突时走 §6 的探测分支（占用者是 harness → 复用/接管；是别的程序 → 提示换端口）。

**overlay 文件（实测可覆盖 printUrl；注意 patch 会替换整行 config，必须写全）**

```yaml
- id: web-runtime
  config:
    openBrowser: !!js ctx.webStartup.openBrowser
    printUrl: true
    surfaceContext: true
    trustedHosts: !!js ctx.webStartup.trustedHosts
```

**URL 解析（对 LAN 后缀健壮）**

```rust
// 行形如：dsh web: http://127.0.0.1:59753/?token=xxx[ (LAN: http://10.0.0.5:59753/?token=xxx)]
fn parse_dsh_url(line: &str) -> Option<Url> {
    let rest = line.split_once("dsh web:")?.1;
    let raw = rest.split_whitespace().next()?;   // 关键：只取第一个 token，丢掉 "(LAN: ...)"
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.query().is_some())
        .then_some(url)
}
```

---

## 5. 进程生命周期（修正版）

- **正常退出**：向进程组发 SIGTERM → 等 5s → SIGKILL；实测 harness 2s 内优雅退出。
- **崩溃孤儿（macOS/Linux）**：Tauri 被强杀时子进程会存活。壳在 `App Data/dsh-desktop/state.json` 记录 `{pid, port, cwd, startedAt}`，**下次启动先自愈清理**（只在记录的 pid 仍监听记录的端口时才发信号，避免 pid 复用误杀）；启动后不轮询，改由看护线程阻塞 `wait` 子进程（**2026-09-13 修正**：早期设计的“启动后每 30s 校验一次存活”从未实施，§13.8 已记录用阻塞 `wait` 取代）；退出**不再停在报错页** —— **2026-09-15 修正**：干净退出（插件市场「立即重启」）先等 8s 交接并接管替代实例、崩溃/被杀 2s 后自启，连续 3 次短命重启未稳定才弹带「重新启动 Harness」按钮的终态页（另起实例或点 Dock 图标等价于按它），见 §13.12。
- **Windows**：V1 用 `taskkill /PID <pid> /T /F`；正式版换 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）。
- **单实例**：`tauri-plugin-single-instance` 只保证“一个壳”，不代表“一个 harness”，需与 §6 一起看。

---

## 6. 实例策略与插件保留（v1 缺失，含实测）

前提事实：**第三方插件装在 profile 里**（`$DSH_HOME/profiles/web/`），不在 DSH_HOME 本身。本机实测该 profile 内容：

```text
~/.dsh/profiles/web/
├── package.json          # dependencies: dshmarket ^1.45.1 / dsh-better-sidebar ^0.18.1 / dsh-dream-skin ^8.30.1
├── cordis.patch.yml      # 该 profile 的用户 patch
├── cordis.yml            # 每次 boot 由 prepareProfile 重写
├── node_modules/         # pnpm 安装的插件依赖树（102 项）
├── pnpm-lock.yaml / pnpm-workspace.yaml
└── .dsh-market/          # 插件自身的状态（market）
```

因此“保留插件”= 保留这份 profile。`--port 0` 意味着桌面壳每次新起一个 harness，需要在下面三条路里选：

### 方案 A：共用 `~/.dsh`（推荐，零配置）

桌面壳不设置 `DSH_HOME`。插件、market 状态、sessions、credentials、settings 全部天然保留，无迁移成本。

- 代价：桌面壳起的 harness 与 CLI/Automator 起的 harness 共享同一份 `~/.dsh`。**同时运行**时属于未声明支持的并发场景（各自写自己的 session 文件没问题，但 `profiles/web/cordis.yml`、`settings.yaml`、插件状态文件是共享可写文件）。
- 缓解：桌面壳启动前检测是否已有其它 harness 在跑（扫描 `dsh` 进程 / 常见端口），有则提示并默认不新开。

### 方案 B：独立 DSH_HOME + 共享 profile 依赖（**已实测可行**）

在独立 home 里只放三样东西，`node_modules` 软链到真实 profile：

```text
~/.dsh-desktop/profiles/web/
├── package.json              # 从 ~/.dsh 复制
├── cordis.patch.yml          # 从 ~/.dsh 复制
├── node_modules -> ~/.dsh/profiles/web/node_modules   # 软链，插件与依赖树共享
└── cordis.yml                # 由独立 home 自己生成
```

**实测**（`DSH_HOME=<隔离目录> dsh --profile web --dump-config`）：配置树 551 行，`dshmarket` / `dsh-better-sidebar` / `dsh-dream-skin` 三个插件全部加载；`cordis.yml` 写在隔离 home 内，真实 profile 未被写入。

- 收益：插件与依赖树共享（无重复安装、无版本漂移），而 sessions / storages / credentials / settings 隔离。
- 注意：插件写在 **profile 目录**的状态（如 `.dsh-market`）与写在 **DSH_HOME 根**的状态（如 `dream-skin.json`）不会自动共享，需要各自配置一次；必要时把这两处也软链到隔离 home。
- 插件升级仍由 CLI 在真实 profile 执行即可，桌面壳通过软链自动看到新版本；两侧的 `pnpm-lock.yaml` 会各自存在（不影响运行）。

### 方案 C：独立 DSH_HOME + 重装插件

在隔离 home 执行 `dsh plugin --profile web add dshmarket dsh-better-sidebar dsh-dream-skin`（官方 pnpm 转发路径）。干净但代价最大：依赖磁盘 ×2、版本会各自主张、market/skin 需重新配置。

### 方案 A 的检测与“使用正在运行的实例”（v2.1 补充，含实测）

**检测（可行）**：候选端口 = 桌面壳自己记录的端口 + 默认 3080 + 监听中的 `dsh` 进程端口；对每个端口 `GET http://127.0.0.1:<port>/`：

| 响应 | 含义 | 动作 |
|---|---|---|
| `401` + body 含 `dsh web authentication required` | 确认是 harness，且当前 WebView 无 cookie | 走“复用/接管”分支 |
| `200` | harness 已接受当前 WebView 的 cookie | 直接复用 |
| 其它 | 端口被别的程序占用 | 提示换端口 |

**复用的三种情形**：

1. **自己的 WebView 数据目录里已有该 authority 的 cookie**（固定端口 + 同一 `DSH_HOME`）→ **可以直接复用**：加载根路径即可，无需 token（实测 cookie 跨进程重启有效；有效期 30 天）。
2. **桌面壳自己启动、仍存活的实例** → 复用它需要 token URL：把当次 URL 写入状态文件（0600、退出即删）即可直接复用；token 可重复使用（实测）。安全权衡：token 等同完整控制权，建议默认关闭该行为并提供开关。
3. **其它进程（CLI / Automator）启动的实例** → **无法复用**（launchToken 每进程随机且不落盘）。可提供三个动作：
   - **用系统浏览器打开** `http://127.0.0.1:<port>/`（该浏览器已有 cookie 时直接可用，例如用户长期使用的 3080）；
   - **停止它并接管**：按端口定位监听进程 → 校验是 node/dsh → `SIGTERM` → 等待 → `SIGKILL`（与 §5 同一套逻辑），然后由桌面壳启动；
   - **仍然新开一个**：需明确提示“两个 harness 共享 `~/.dsh` 属未声明支持”。

**cookie 失效时的恢复**：若探测到 `401` 且无可用 token（cookie 过期/被清理），唯一出路是**重启 harness 以取得新的 URL**——因此桌面壳必须在“接管”分支里具备重启能力。

### 共同约束

无论哪种：**不要**尝试复用另一个进程的会话——其 cookie 绑定对方的 `authority`（host:port），且没有对方的 token；每次启动都必须用当次新 URL。

---

## 7. 安全与 WebView 集成（修正版）

- Harness 窗口：**零 capability**；页面无法访问 `invoke`/fs/shell/process。
- 导航限制：只允许 `http://127.0.0.1:<当次端口>`；其他 `https` 链接在 Rust 侧用 opener 打开系统浏览器（**Rust 侧调用不受前端 capability 约束**）。
- Splash 窗口：本地资源，需要 `core:event:default`（监听启动状态）——v1 的 `capabilities/splash.json` 不能为空。
- 需实测确认（本机 WebView 行为，非文档结论）：
  - `<input type=file>`（附件上传）是否直接可用；
  - 下载（GUI 里的 Download）是否需要在窗口上处理 download 事件；
  - `window.open`/新窗口请求的拦截路径。
- 剪贴板：`http://127.0.0.1` 属 secure context，`navigator.clipboard` 可用，无需额外 capability。

---

## 8. 日志（修正版）

- stdout/stderr 双写：文件 + 内存环缓冲（最近 200 行 / 1 MiB，用于错误页）。
- **URL 行脱敏**：日志中把 `token=...` 替换为 `token=***`，避免凭证长期留档。每行只脱敏一次，
  环缓冲与日志共用同一份结果。
- **单行上限 256 KiB**：超限继续排空到换行但不扩张内存，行尾追加截断提示（§13.16）。
- 文件轮转：单文件 5MB × 3 份；轮转判定计入即将写入的字节数（§13.16）。

---

## 9. 平台差异

| 平台 | 必做 | 备注 |
|---|---|---|
| macOS | 签名 + 公证（对外分发）；用 `process_group` 管理；关注 TCC：子进程的文件访问会以宿主 App 身份触发授权弹窗 | TCC 行为需实机确认 |
| Windows | `cmd.exe /D /S /C` 起 `.cmd`；`CREATE_NO_WINDOW`；`taskkill /T` | 后续 Job Object |
| Linux | 声明 WebKitGTK 运行依赖 | — |

---

## 10. 验收清单（含实施后状态）

启动/窗口/单实例/安全/日志（沿用 v1）之外：

| 验收项 | 状态 |
|---|---|
| PATH 中无 node 时也能启动（locator 解析 node） | ✅ 已实现，单测覆盖软链+node 解析（未单独剥离 PATH 跑 GUI） |
| workspace 正确 | ✅ 实机：config 默认 `$HOME`，state.json 记录 cwd；切换 UI 未做 |
| 用户 patch 关掉 `printUrl` 时仍能拿到 URL | ✅ overlay 生效（真实 dsh 实测） |
| 首启 90s / 常态 30s 超时给出可诊断错误页 | ✅ 已实现，未触发过 |
| Tauri 被 `kill -9` 后重启：残留 harness 被自愈清理 | ✅ 已实现，待实机演练确认 |
| 同屏已有 CLI `dsh web` | ✅ **实机验证**：接管成功（§13.2） |
| WebView 内 token→cookie 与会话可用 | ✅ **实机验证**（§13.2） |
| dsh 核心升级（检查 + 安装 + 重启生效） | ✅ 单测 + 联网集成测试；实机升级待下次发版确认 |
| 下载落盘 `~/Downloads`、外链/新窗走系统浏览器 | ✅ 已实现，待点击确认 |
| 附件上传（`<input type=file>`） | ✅ wry 原生实现，待点击确认 |
| 打包为 `.app` | ✅ 已产出并核验 Info.plist/图标 |
| 合盖休眠再唤醒 | ⚠️ 不适用/待确认：harness 是壳的子进程，随壳一起挂起唤醒 |
| 多显示器/缩放切换 | ⚠️ 待实测 |
| 签名与公证 | ❌ 未做（对外分发必需） |

---

## 11. 待验证项（实施后状态）

1. ~~WKWebView 对 token→cookie 的处理~~ → ✅ 已实机验证（§13.2）。
2. `<input type=file>` / 下载 / `window.open` 的默认行为 → 已按 Tauri 2.11 的
   `on_new_window` / `on_download` 与 wry 的 `runOpenPanel` 实现，**待点击确认**。
3. macOS TCC 授权归因（子进程访问用户目录时的弹窗归属）→ 未验证。
4. 运行中 reload（新增/修改 patch）对已建立 WebView 会话的影响 → 实测不会重新播报 URL；会话是否受影响仍未验证。

---

## 12. 实施顺序（修正版）

全部完成（对应 `dsh-desktop/src-tauri/src/`）：

1. ✅ Tauri Vanilla 项目 + single-instance（`lib.rs`）
2. ✅ locator（dsh **+ node**，含 login shell 兜底）（`locator.rs`）
3. ✅ spawn（launcher flag 顺序、cwd、`--patch` overlay、process group）（`harness.rs`）
4. ✅ 持续读 stdout/stderr + URL 解析（含 LAN 后缀）+ 超时（`harness.rs`）
5. ✅ `WebviewUrl::External` 动态窗口 + 导航限制 + 外链/新窗走系统浏览器 + 下载落盘（`window.rs`）
6. ✅ 状态文件 + 自愈清理（`process.rs`）
7. ✅ 退出清理（SIGTERM → SIGKILL；Windows `taskkill /T /F`）
8. ✅ Splash + 错误页（Rust 侧 eval 更新，页面零权限）
9. ✅ 日志脱敏 + 5MB 轮转（`harness.rs`）
10. ✅ dsh 核心升级：检查 + 安装 + 重启生效（`update.rs`，§13.4）
11. ✅ 打包 `.app`（`icons/icon.icns` + `bundle.targets=["app"]`）

仍然不做：插件管理 UI、模型管理 UI、Tauri ↔ Harness IPC、打包 Node/Harness、自定义 UI、
签名与公证、Windows Job Object。

---

## 13. 实施状态（本次已落地并验证）

包位置：`dsh-desktop/`（与本文同目录）。

已实施：locator（dsh + node，含 login shell 兜底）、启动（launcher flag 顺序 / cwd / overlay / 进程组）、
stdout+stderr 持续读取、token 脱敏、URL 解析（LAN 后缀健壮）、探测与复用、状态文件与残留自愈、
退出清理（SIGTERM→SIGKILL）、Splash/错误页、导航限制、外链走系统浏览器、日志轮转、单实例。

验证证据：

- `cargo check --all-targets`：通过，无告警。
- `cargo test`：**6 passed**（URL 解析含 LAN 后缀、拒绝非 127.0.0.1、拒绝对话行、token 脱敏、
  状态文件往返、locator 穿软链解析 `dsh.js` 并解析到 `node`）。
- 所有 §2 的实测结论均来自本机真实运行（隔离 `DSH_HOME`，未触碰 `~/.dsh`）。

实施期新增发现（本文原未覆盖）：

1. Tauri **2.11** 的 `app.security` 已无 `dangerousRemoteDomainIpcAccess` 字段（`tauri-build` 会报
   `unknown field`）；等价约束是"没有任何 capability 覆盖 Harness 窗口"，已按此实现，
   `capabilities/splash.json` 只作用于 splash。
2. `tauri::generate_context!()` 要求 `src-tauri/icons/icon.png` 存在，即使 `bundle.active=false`。
3. 关窗语义：macOS 默认关窗不退出进程，实现里显式把 `CloseRequested` 绑定为 `app.exit(0)`，
   否则会出现"关窗后 harness 仍在跑"——正是本方案要避免的情况。

未在本机验证（需实机 GUI 会话）：WebView 内 token→cookie 流程、附件上传/下载/`window.open`、
macOS TCC 授权归因。

### 13.1 首次实机运行暴露的问题与修正（已改）

现象：桌面壳窗口显示 `dsh web authentication required; reopen the URL printed by dsh web.`

原因：检测到 3080 上已有外部 Harness 时，实现直接加载了**无会话的根 URL**（探测请求不带 cookie，必然 401），
且 `create_harness` 随即关闭 splash，把解释文字一起关掉。

修正（已实现并重新通过 `cargo check`）：

1. 区分"自己上次启动的实例"与"外部实例"：前者用 state.json 的 pid 与端口监听者比对，命中则**复用**（cookie 对同一
   authority 仍有效，实测跨重启 200）。
2. 外部实例一律**接管**：先用 401 特征确认是 Harness（不误杀未知进程）→ 用 `lsof -tiTCP:<port>` 取监听者 PID →
   只对该 PID 发 `SIGTERM`（不用进程组，避免连带杀掉用户终端）→ 等端口释放 → 自己启动拿新 token → 开窗。
3. 新增配置 `take_over_existing`（默认 true）；置 false 时改为用系统浏览器打开并给出说明。

> **2026-09-15 修正（默认值反转）**：`take_over_existing` 默认改为 **`false`**。上面的「默认 true」是当时的
> 决策，理由写在第 2 条 —— 但那条只校验了「端口上是 Harness」，没有校验「这个进程就是 dsh」，而
> `probe()` 当时还把**任意 `HTTP 200`** 也算作 Harness（见下方 F3 的注）。两者叠加的后果是：任何监听该
> 端口并返回 200 的普通服务都会被当作外部 Harness 并收到信号。
>
> 现在接管需要**两道信号同时成立**：401 认证栅栏（`harness::probe`）+ 该 PID 命令行确为 `dsh web`
> （`harness::looks_like_dsh_web`）。该规则已应用到全部四条 kill 路径。默认不接管时改用系统浏览器打开，
> 并提示另外两个选项（设 `true` 接管 / 换端口）。
>
> **2026-09-16 修正（交互确认已实现）**：两道身份校验通过后**一定会弹面板让用户当场选**（接管 / 保留并用
> 浏览器打开 / 保留并换端口 / 什么都不做），120 秒无答复才回落到 `take_over_existing`。面板画在 splash
> 页面上（该窗口本就持有 `core:default`），复用重启按钮那条 core 事件桥，**没有引入 `tauri-plugin-dialog`**
> —— 上面写的「需要新增对话框依赖」这一判断不成立。`take_over_existing` 的语义因此收敛为
> 「**没答复时**是否接管」，既不决定「是否询问」，也不再让 `false` 等于静默走浏览器。
>
> 第一版仍按该配置项决定**要不要问**，于是默认用户看不到面板（实机确认：直接开浏览器并停在错误页）；
> 同日修正。详见 [`design-task-feat-takeover-confirmation.md`](./design-task-feat-takeover-confirmation.md)。
4. 关闭窗口前不再提前关闭 splash；只有拿到**带 token 的 URL**后才创建 Harness 窗口。

### 13.2 实机验证结论（2026-09-13）

用户实机运行通过，核对痕迹如下：

| 检查项 | 结果 |
|---|---|
| 3080 监听者 | `node 73596`，即桌面壳自己启动的实例（对外部实例的**接管**成功） |
| `state.json` | `{pid: 73596, port: 3080, cwd: /Users/<you>}`，与监听者一致 ⇒ 下次启动走**复用**分支 |
| 日志脱敏 | `token=***`，明文 token 0 行 |
| WebView 会话 | token→cookie 在 WKWebView 内成功，会话列表、文件卡片、输入框均正常 |

据此 §11 的待验证项 1（WKWebView 内的 token→cookie 处理）**已关闭**；附件上传/下载/`window.open`/
TCC 归因仍待验证。

### 13.3 缺失部分补齐（2026-09-13）

| 项 | 处理 |
|---|---|
| 外链 / `window.open` | `on_new_window` → 记录日志 + `open_external` + `NewWindowResponse::Deny`（远程内容永不开 Tauri 新窗） |
| 下载 | `on_download`：`Requested` 时把目的地改到 `~/Downloads/<原文件名>`，`Finished` 记日志；`DownloadEvent` 为 `#[non_exhaustive]`，已加兜底分支 |
| 附件上传（`<input type=file>`） | wry 已实现 `webView:runOpenPanelWithParameters:`（WKWebView 原生面板），**无需 capability**，仅待实机点击确认 |
| 壳侧事件日志 | 新增 `harness::init_app_log` / `app_log`，与 Harness 输出写同一份日志并统一 token 脱敏 |
| 打包 `.app` | 生成 `icons/icon.icns`（sips + iconutil，10 档尺寸）；`bundle.active=true`、`targets=["app"]`、`macOS.minimumSystemVersion=10.15`；构建命令 `pnpm tauri build --bundles app` |
| 开机自启 | 走系统"登录项"，不需要代码 |
| 签名/公证 | 未做：对外分发必需，本机运行不需要 |
| Windows Job Object | 仍为 TODO（当前 `taskkill /T /F`），无法在 macOS 验证 |

生成 / 验证：`cargo check --all-targets` 无告警、`cargo test` 6 passed。

### 13.4 dsh 核心升级（新增，2026-09-13）

新增 `src-tauri/src/update.rs`，把启动脚本里验证过的升级策略搬进壳：

- 版本比较：自实现 semver（含 prerelease 排序），纯函数、6 个单测覆盖
  （`1.2.10 > 1.2.9`、`1.0.0 > 1.0.0-rc.1`、`rc.2 > rc.1 > alpha.2`、畸形输入拒绝）。
- 取版本：`npm view <pkg> dist-tags --json` → 在配置的 tag 列表里取最高版本（默认值见本节末修正注），从不降级。
- 安装：`npm install -g --no-fund --no-audit <pkg>@<具体版本>`；npm 从 node 同目录解析（GUI 无 Homebrew PATH），
  并在 npm 全局前缀 ≠ CLI 实际位置时带 `--prefix` —— 与启动脚本一致。
- 时序：**更新在探测之前**；更新成功则强制重启实例（否则复用旧进程仍跑旧二进制）。
- 可见性：检查/安装/失败都写进 `logs/harness.log`；splash 显示"正在检查 dsh 更新…/发现新版本…"。
- 配置：`auto_update`（默认 true）、`update_tags`（当时默认 `["latest","next"]`；后经两次修正，
  现为 `["latest","alpha"]` —— 见本节末两条修正注）。

> **2026-09-15 修正（默认值收敛）**：`update_tags` 默认改为 **`["latest"]`** —— 预发布渠道 `next` 不该是
> 桌面用户的默认目标。同轮一并收敛的还有：`auto_update_plugins` 默认 **`false`**（profile 是用户数据）、
> `system_updates` 默认 **`notify`**（不就地改写用户自己装的全局前缀）。
>
> 注意 `system_updates` 的默认值在 `design-task-feat-dsh-bundled-runtime.md` §3 里被定为 `install`
> （理由：老用户静默失去自动升级更糟）。本次改动**推翻了那个取舍**，理由见该文档同处的注。
>
> 另外 `require_tested_dsh` 默认改为 `true` 后，安装与启动必须受同一区间约束，否则会自锁（装上一个
> 随后拒绝启动的版本），因此新增 `update::may_install()`。见 `docs/dsh-desktop-v0.4.1-code-review.md` 第 2 项。

> **2026-09-18 再次修正（alpha 纳入范围）**：`update_tags` 默认改为 **`["latest","alpha"]`**，
> **推翻上面那次"只留 `latest`"的收敛**。理由：上游的 `alpha` 标签会领先 `latest` —— 实测当天
> registry 返回 `{"latest":"0.1.5-rc.2","next":"0.1.5-rc.2","alpha":"0.1.6-alpha.2"}`，只读 `latest`
> 的壳永远看不到 `0.1.6-alpha.2`。`next` 仍排除在外：它当时与 `latest` 同版本，不提供额外信息。
>
> 上面那次收敛的理由是"预发布渠道不该是桌面用户的默认目标"，这条**对 `next` 仍然成立**，但对
> `alpha` 不成立：`alpha` 是上游发布最新构建的渠道，而 `require_tested_dsh`（默认开）仍然把
> 安装范围限制在已测试区间内 —— 区间外的 alpha 只报告、不安装，所以"默认跟 alpha"不会把用户
> 静默推上未测试的版本。想只跟正式版写 `["latest"]` 即可。
>
> 随之新增：`update::default_tags()`（默认值的唯一定义，配置默认值与联网集成测试共用）、
> `Cache::tags`（缓存条目记下是哪组标签问出来的，换标签即视为换问题、旧答案不复用，因此默认值
> 改动下次启动生效而非等一个缓存周期；旧缓存文件没有该字段，反序列化为空列表，同样判为不新鲜）。
>
> 验证：单测 192 → **194**；联网集成测试改用 `update::default_tags()` 并打印 registry 实际标签，
> 实测 `live registry head = 0.1.6-alpha.2 (dist-tags: ["latest", "alpha"])`（改动前为 `0.1.5-rc.2`）。

验证：`cargo test` **12 passed**（6 原有 + 6 版本比较）；另加一个联网集成测试
`tests/update_live.rs`（默认跳过，设 `DSH_DESKTOP_LIVE_TESTS=1` 才跑），实测本机 registry head =
**0.1.5-rc.2**，且 99.0.0 判为已是最新（不降级）。

### 13.5 构建入口与 Linux 支持（2026-09-13）

**Makefile**（`dsh-desktop/Makefile`，141 行）统一了构建入口，并用 `uname -s` 分平台：

- 目标：`help`（默认）、`doctor`、`check`、`fmt`、`clippy`、`test`、`test-live`、`dev`、`build`、
  `bundle`（`bundle` 依赖 `node-deps` 自动 `pnpm install`）、`run`、`icons`、`clean`、`distclean`；
- 平台：macOS → `--bundles app`（产物 `.../macos/DSH Desktop.app`），Linux → `--bundles deb`
  （可 `BUNDLE_TARGETS=appimage|rpm` 覆盖）；其他平台在解析阶段直接报错；
- `doctor` 在 Linux 上用 `pkg-config` 检查 `webkit2gtk-4.1` / `gtk+-3.0` / `libsoup-3.0`，
  并打印 Debian/Fedora/Arch 三家的安装命令；macOS 提示 Xcode CLT；
- `CARGO_HOME` / `PNPM_STORE` 可覆盖且默认留空（沙箱/CI 重定向缓存用，不影响日常）。

**Linux 可移植性修复**：`listener_pid` 原先只用 `lsof`，而不少发行版不预装它，会让"接管外部实例"
直接失败。现在改为 `lsof` → `ss -ltnpH 'sport = :PORT'`（iproute2，通常自带）回退，
解析函数 `parse_ss_pid` 抽出并有单测。

验证：`make help` / `make doctor` / `make -n bundle` / `make check` / `make test` 均在本机跑通，
`make test` = **13 passed**。Linux 分支未在 Linux 机器上实测（环境限制）。

### 13.6 启动延迟优化（2026-09-13）

性能审查的结论：这份代码是进程编排 + I/O，CPU/内存没有热点；唯一有量级的是**每次启动都联网查一次
npm registry**。实测各环节：

| 环节 | 实测 | 处理 |
|---|---|---|
| `npm view ... dist-tags` | **1.875 s**（挂起时最长等 fetch 超时） | 加缓存 + 超时压到 8 s |
| login shell 兜底 `zsh -lc` | 16 ms | 不动（收益 < 复杂度） |
| `node <dsh.js> --version` | 103 ms | 不动 |
| 端口探针 | 12 ms | 不动 |
| Ring/Logger 互斥、日志写入 | 低频、每行一次 syscall | 不动（无缓冲是有意为之，崩溃时要能立刻落盘） |

实现：

- 新增 `update::Cache { checked_at, installed, latest }`，落在 `<app-data>/update-check.json`；
  `is_fresh(now, installed, interval)` 的规则：已安装版本变了即失效；**查询失败只缓存 5 分钟**；
  `interval = 0` 表示每次启动都查（等价旧行为）。
- 抽出纯函数 `judge(latest, current)`，让"新查询"和"缓存答案"走同一套判定。
- `check_cached(...)` 作为唯一入口；命中缓存时记一条 `update check: cached answer`。
- `FETCH_TIMEOUT_MS = 8_000` 取代原来的 25 s。
- 配置新增 `update_check_interval_minutes`（默认 60，serde default 兼容旧 config.json）。

验证：`cargo test` **16 passed**（新增 judge 判定、缓存新鲜度、缓存落盘往返）；
联网集成测试新增"冷/热对比"，实测 **cold = 1168 ms（真实查询）→ warm = 0 ms（缓存）**，且两次结论一致。

### 13.7 修复：GUI 启动读不到登录 shell 的环境变量（2026-09-13）

**现象**：桌面壳启动后，GUI 报 `llm-deepseek: no API key for provider route "deepseek-official"`。

**根因**：macOS 的 GUI 应用继承 launchd 的环境，不读取 `.zshrc`/`.zprofile`；用户在终端 export 的
`DEEPSEEK_API_KEY` 不会出现在子进程环境里。这与本项目早先"GUI 没有 Homebrew PATH"是同一类问题。

**实测**（决定方案）：`/bin/zsh -lic env` → **164 ms**、46 个变量、**0 行噪音**，且**包含 `DEEPSEEK_API_KEY`**；
`-lc`（非交互）少一个变量，故按 `-lic` → `-lc` 的顺序回退。

**实现**：

- 新增 `src-tauri/src/shellenv.rs`：`import()`（带 8 s 超时，超时即 kill）、`parse_env()`（丢弃非 KEY=VALUE 噪音行、
  拒绝保留名、重复取后者）、`merge_path()`（只保留绝对路径并去重）；
- `SpawnOptions` 增加 `env`，`spawn` 用 `Command::envs` 注入；
- `lib.rs` 在 spawn 前导入，且**只记录变量名**；`PATH` 合并顺序：
  自带 node 目录 → `/opt/homebrew/bin` → `/usr/local/bin` → shell PATH → App PATH；
- 配置新增 `import_shell_env`（默认 true）与 `env`（显式覆盖，最后应用）。

**验证**：`cargo test` **20 passed**（新增 4 个：噪音行/保留名/重复键/PATH 合并去重）；
真机变量的捕获能力由上面的 164 ms 实测佐证。

### 13.8 性能审查后的修复（2026-09-13）

第二轮审查逐文件读完 1728 行 Rust 代码。结论：这份壳是进程编排 + I/O，**没有任何后台轮询或定时器，
空闲 CPU ≈ 0**，没有 CPU/内存热点；问题集中在启动路径的串行开销，外加两个非性能缺陷。以下全部已修。

**P0：npm 子进程缺 PATH，导致更新功能在 GUI 启动时 100% 失效（功能性 bug）**

- 证据（应用自己的日志，两次 GUI 启动各一条）：
  `update check failed, keeping v0.1.5-rc.2: npm view 失败: env: node: No such file or directory`。
- 根因：`fetch_dist_tags` / `install` / `global_prefix` 用 `Command::new(npm)` 启动 npm，而 npm 是 `#!/usr/bin/env node` 的 JS 脚本；
  GUI 启动的壳继承 launchd 的 PATH（不含 node），shebang 直接 exit 127。
- 实测：`env -i PATH=/usr/bin:/bin /opt/homebrew/bin/npm --version` → 127；把 node 目录前置后 → `10.9.8`。
  13.6 里"冷查询 1.2–1.9 s"的测量是在终端环境（PATH 含 Homebrew）跑的，因此掩盖了这个 bug ——
  **回归测试必须在"没有 node 的 PATH"下跑**才有效。
- 修复：新增纯函数 `npm_path(npm, existing)`（npm 所在目录前置 + 去重）与 `npm_command(npm)`，
  三处 npm 调用统一改走它。

**P1：探针只有连接超时，没有读写超时（可能永久挂死启动线程）**

- `probe()` 用 `connect_timeout(600ms)` 建连后直接 `read_to_string`；对端 accept 后不回包/不关连接时
  会无限等待，而接管循环的 deadline 只在两次 probe 之间检查，启动线程就此卡死且无取消路径。
- 修复：抽出 `PROBE_TIMEOUT = 600 ms`，建连、读、写都用它；新增回归测试
  `probe_gives_up_on_a_silent_listener`（起一个只 accept 不说话的监听器，断言 1 s 内返回 `Probe::Other`）。
- **修补（2026-09-16）：上面这版只挡住了沉默的对端，会说话的对端仍能永久挂死。** socket 超时约束的是
  单次 `read` 调用，不是整个交换；`read_to_string` 会一直循环到 EOF，所以对端只要在每个窗口内送一个字节，
  `read` 就永远返回 `Ok`，既不返回也不超时，`body` 还无上限增长。端口上是任何流式服务
  （SSE / 日志跟随 / 长轮询）即可触发，而 `probe()` 正是接管与更新路径在循环里轮询的东西 ——
  它们以秒计的 deadline 因此永不生效，实测 8 s 后仍未返回。
  修复：整个交换用一个 wall-clock deadline（每次 `read` 前按剩余时间重设 IO 超时），响应体加上
  `PROBE_RESPONSE_LIMIT` 上限，并按块读 —— 状态行一到就判定，非 401 立即返回（`Connection: close` 下
  这一行就是全部答案）。回归测试 `probe_gives_up_on_a_listener_that_keeps_sending` 补上这一类对端：
  静默对端的用例抓不到它。

**P2：启动路径上的两处开销**

| 改动 | 前 | 后 |
|---|---|---|
| 版本读取 `locator::version` | `node dsh.js --version`，实测 **80 ms**（更新后还会再跑一次） | 先读 CLI 所属 `package.json` 的 `version`（**~1 ms**），失败才回退跑 node |
| 登录 shell 环境抓取 | 排在更新检查之后串行执行，**160 ms** 全在关键路径上 | 与版本读取/更新检查并行（`std::thread::spawn`，组装子进程环境前 join） |

**附带：config.json 不再被静默覆盖**

- `Config` 的 `port` / `workspace` 原本没有 serde default，缺字段即整体解析失败，
  随后代码会用默认值**覆盖用户文件**。现在两者都有 `#[serde(default)]`（部分配置照常生效）；
  文件整体无法解析时只在本进程用默认值、**不写回文件**，并记一条 `config.json 无法解析…` 日志。
- 只有文件不存在时才写入默认配置。

**顺带清理**：`update.rs` 注释里 6 处「反斜杠 + 反引号」的转义残留改回普通反引号；
`Ring` 补 `Default` 以满足 13.5 定下的 `clippy -D warnings` 门禁。

验证：`make fmt` / `make clippy`（0 warning）/ `make test` → **24 passed**
（新增 4 个：npm PATH 前缀、沉默对端探针、package.json 版本解析、部分/损坏 config.json）。

新增联网回归测试 `tests/update_npm_env_live.rs`（独立 test binary，因为它会改写进程 PATH）
直接复现 GUI 失败场景：解析出 node/npm 后把 `PATH` 换成不含 node 的值、`npm_config_cache` 指向临时目录，
先断言裸调 npm 退出码为 **127**（前提成立），再断言 `update::check` 能正常拿到 registry head。
实测输出：`premise confirmed: plain npm exits Some(127) without node on PATH` → `live registry head = 0.1.5-rc.2 with PATH=/nonexistent-bin`。
`make test-live` 相应改为跑全部联网测试（原先是只跑 `--test update_live`）。

### 13.9 修复：退出应用不终止 Harness（2026-09-13）

**现象**：退出桌面壳后 `dsh web` 仍在运行、`state.json` 仍存在，下次启动才由"自愈"路径清理。
等于把自愈当成了正常退出路径。

**根因（读框架源码定位）**：macOS 上退出有两条不同的事件链，壳只处理了其中一条。

| 退出方式 | tao / Tauri 事件链 | 旧代码 |
|---|---|---|
| 窗口红点 / ⌘W（`on_window_event(CloseRequested)` → `AppHandle::exit(0)`） | `Message::RequestExit` → `RunEvent::ExitRequested` | ✅ 已处理 |
| ⌘Q / Dock 退出 / `quit app`（`application_will_terminate` → `AppState::exit()`） | `Event::LoopDestroyed` → `RunEvent::Exit` | ❌ 未处理 |

- Tauri 只在 `Message::RequestExit` 分支里发 `ExitRequested`（`tauri-runtime-wry/src/lib.rs:4354`）；
  `application_will_terminate`（`tao/src/platform_impl/macos/app_delegate.rs:131`）只产生 `LoopDestroyed`，
  它映射为 `RunEvent::Exit`（`tauri-runtime-wry/src/lib.rs:4185`）。
- 结论：⌘Q 退出时 `shutdown()` 从未执行 —— 与"日志里只有启动没有停止、state.json 残留"完全一致。

**顺带查出的第二个 bug（僵尸进程）**：`is_alive()` 用 `kill(pid, 0)` 判断存活，而**自己 fork 的子进程
在退出后、被回收前是僵尸**，`kill(pid, 0)` 对僵尸同样成功。即使 `shutdown()` 跑起来，
也会把 5 s grace 用满再发 SIGKILL —— 表现为"关窗口后应用卡 5 秒"。现在 unix 下先用
`waitpid(WNOHANG)` 探（本进程的子进程会被就地回收），非本进程的子进程（上次残留、外部实例）才回退到 `kill(pid, 0)`。

**实现**：

- `.run()` 同时处理 `RunEvent::ExitRequested` 与 `RunEvent::Exit`；`shutdown()` 幂等，重复触发无副作用；
- 新增 `EXITING: AtomicBool`：退出瞬间置位，启动线程在 spawn 前后各查一次（两侧都是 SeqCst，
  因此"启动线程登记的 pid"与"退出线程取走的 pid"不可能互相漏掉）。窗口在启动途中被关掉时，
  刚起来的 Harness 会被立即停掉，而不是漏成孤儿；
- 复用上次实例（`state.json` pid 命中且存活）时也登记进 `LIVE`（`adopt()`）：这条路径此前完全不登记，
  所以"复用后退出"必然留孤儿；
- 停掉 Harness 时写日志 `stopping Harness pid N` / `Harness stopped`，让退出行为可观测。

**验证**：

- 新增单测 `terminate_stops_a_spawned_process_group`：真起一个独立进程组的 `sh -c "sleep 30"`，
  断言 SIGTERM 后进程消失且耗时 < 2 s（防僵尸回归）。
- 实机复现（macOS）：强杀旧实例 → 启动新构建（harness pid 92619）→ 用与 ⌘Q 等价的 Apple Event 退出，
  应用日志：

      [dsh-desktop] stopping Harness pid 92619
      [dsh-desktop] Harness stopped

  退出后 3080 端口释放、`state.json` 被删除；重新启动得到新 pid（92700）。
  修复前的形态（日志只有启动、无 stopping、state.json 残留）不再出现。

**仍只能靠自愈的场景**：SIGKILL 强杀、崩溃、断电 —— 任何回调都拿不到，下次启动按原有自愈路径清理。

### 13.10 第三轮审查：剩余问题与修复（2026-09-13）

复查范围：进程生命周期、退出路径、状态页与文件权限。共 2 个 P1、2 个 P2、3 个 P3，全部已修。

**P1-1 启动超时后子进程没有被终止**

`wait_for_url(timeout)` 失败后直接 `?` 返回，`fail()` 只更新状态页；
`Spawned` / `Child` 被 drop，而 Rust 的 `Child` drop 不杀进程。于是"启动失败"的页面对着
仍在后台运行的 `dsh web`，它可能继续起来并占住端口，下次启动再把它当外部实例接管。

修复：新增 `abort_start(pid, reason)`，超时与 `create_harness` 失败两条路径都先
`process::terminate` 再从 `LIVE` 摘除（`disown`），最后才返回错误。

**P1-2 关掉状态页会留下没有窗口的进程**

Tauri 2 在最后一个窗口关闭时不会退出应用：`tauri/src/app.rs:2544` 的 `Destroyed` 只调
`manager.on_window_close`，`manager/mod.rs:653` 仅把窗口从注册表移除；tao 也没有
`applicationShouldTerminateAfterLastWindowClosed`。所以启动失败后用户点红点关掉错误页，
Dock 里就只剩一个没有窗口的进程。

修复：`create_splash` 挂 `CloseRequested`，**且仅当 Harness 窗口不存在时**才 `exit(0)`。

> **本轮回归（已修，值得记住）**：给状态页挂上关闭处理之后，应用启动完会立刻自己退出 —— 日志形如
> `dsh web: …` 紧跟 `stopping Harness pid N`。根因是 `WebviewWindow::close()` 与用户点红点
> 走的是同一条链路：`close()` → `WindowMessage::Close` → `on_close_requested()`（`tauri-runtime-wry/src/lib.rs:4368`）
> → 逐个调用窗口监听器；而 `create_harness` 收尾时正是用 `close()` 关掉状态页。
> 修法两条一起上：程序化移除一律改 `destroy()`（"Similar to close but does not emit any events"，
> `webview_window.rs:2221`），状态页的关闭处理再加"Harness 窗口不存在"的判据。

**P2-1 Harness 中途退出无人知晓**

启动完成后不再观察子进程，`Child` 从不 `wait`。修复：`Spawned::into_child()` 把子句柄交给
`watch_harness` 看护线程 —— `wait` 返回且 `EXITING` 为假时，记一条
`Harness pid N exited unexpectedly (code …)`，`disown` + 清 state.json，并用 `window::show_failure` 弹回状态页
显示退出码与最近输出。

> **2026-09-15 修正**：这一步不再直接停在报错页 —— 退出先走自动恢复（交接 → 接管 → 自启），只有自动
> 重试用完或失败才弹终态页。状态页也拆成了三面：`show_progress`（进行中）、`show_failure`（终态，
> 带「重新启动 Harness」按钮）、`show_notice`（终态无按钮，旧 WebView 的浏览器回退用）。见 §13.14。

**P2-2 config.json 是 0644**

`restrict(0600)` 原先只作用于 state.json，而 README 建议把 API Key 放进 config.json 的 `env`。
修复：`process::restrict` 改为公开，写入与读取 config.json 时都收紧权限（读取也收紧，顺带修好旧文件）。

**P3**

- 下载同名文件会被静默覆盖 → 新增 `unique_download_path()`，重名自动 `name-1.ext`（含单测）；
- 状态页最早几条状态会丢（`eval` 早于页面加载）→ `index.html` 把 `__setStatus` 提到 `<head>`，
  值先缓存，`__applyStatus` 在 DOM 就绪后套用；
- Windows 分支未支持且存活探测恒真 → 代码注释写明要移植必须先换 `OpenProcess` + `GetExitCodeProcess`，
  README 明确平台范围只覆盖 macOS/Linux。

**验证**

- `cargo test` **26 passed**（新增下载重名避让用例），`make clippy` 0 warning。
- 实机：启动后应用保持运行、日志以 URL 行结尾（不再出现紧跟的 `stopping`）、state.json 为新 pid；
  `config.json` 权限由 0644 就地改成 0600。
- 崩溃提示路径（P2-1）已实机验证：启动后 `kill -9 <harness pid>`，日志记
  `Harness pid 94329 exited unexpectedly (code None)`，状态页重新弹出报错、
  `state.json` 被清除、应用自身保持运行。（该轮的"停在报错页"行为已于 2026-09-15 改为自动恢复，
  见 §13.14。）

### 13.11 收尾：安装结果校验与公开仓库前的检查（2026-09-13）

**发现（P3，已修）**：`install()` 成功返回后，应用直接置 `just_updated = true` 并回读版本，
但从不校验版本是否真的变了。若被监管的 CLI 不在 npm 全局前缀下（自定义 prefix、pnpm/yarn/volta 布局，
或 `install_prefix()` 因路径不含 `/lib/node_modules/` 而返回 `None`），
npm 会把包装到别处：实际运行的仍是旧版，日志却写 `dsh updated: A -> B`，还白重启一次实例；
此后每个缓存周期都会重装一遍。

修复：安装后回读版本，与 `from` 相同则只记一条
`update installed but the supervised CLI is still A; check npm global prefix`，
不置 `just_updated`、不重启实例，也不再谎报成功。

**公开仓库前的检查**：

- 按密钥模式（`sk-…`、`ghp_…`、PRIVATE KEY、password 等）扫描工作区与全部历史提交：
  无命中；唯一匹配是 shellenv 单测里的假值 `sk-abc=def`。
- 文档里唯一一处本机路径 `/Users/wangzy` 改为 `/Users/<you>`。
- 仓库 28 个跟踪文件不含日志、配置或凭证：应用数据目录从未入库。

**验证**：`cargo test` 26 passed、`make clippy` 0 warning；崩溃提示路径的实机证据见 13.10。

### 13.12 第四轮审查：外部评审清单的逐条核实（2026-09-13）

外部给了一份 10 条的 P0–P2 清单（共用 DSH_HOME、退出 force kill、token 被提前访问、管道塞满、
超时 15s、URL parser 太死、token 明文入日志、Harness 拿到 capability、只处理 on_navigation、
追随任意 dsh 版本）。逐条对着代码核实：**7 条不存在**、2 条部分存在、1 条真实存在。

| 清单项 | 结论 | 证据 |
|---|---|---|
| 共用 DSH_HOME 且同时运行 | 部分存在（有意取舍 + 端口级互斥） | `lib.rs` 默认 `dsh_home: None`（方案 A）；互斥靠固定端口 + 接管 + single-instance；端口之外的并发 CLI 会话拦不住 |
| 退出直接 `taskkill /T /F` / `Child::kill()` | 不存在（macOS/Linux） | `process.rs`：SIGTERM 进程组 → 100ms 轮询 → 才 SIGKILL；harness 路径无 `Child::kill()` |
| token URL 被 Rust/health check 提前访问 | 不存在 | `probe()` 只发裸 `GET /`，全部调用点都在 `spawn` 之前；token URL 只交给 WebView |
| 找到 URL 后停止 drain stdout/stderr | 不存在 | `forward_lines` 从 spawn 起读到 EOF，与是否拿到 URL 无关 |
| 启动超时太短（15s） | 不存在 | 首启 90 s / 常态 30 s（`lib.rs` 顶部常量） |
| URL parser 太死 | **部分存在 → 已修** | 原先只认 `URL_PREFIX` 前缀，现已加"扫描行内第一个 loopback 启动 URL"兜底 |
| token 明文写入日志 | 不存在 | `redact()` 覆盖 Ring 与日志；实测日志为 `token=***` |
| Harness WebView 获得 capability | 不存在 | `capabilities/splash.json` 只列 `splash`；harness 窗口零 capability，`withGlobalTauri: false` |
| 只处理 on_navigation | 不存在 | `window.rs` 同时有 `on_new_window` → 外链 + `NewWindowResponse::Deny` |
| 追随系统安装的任意 dsh 版本 | **存在 → 已修** | 原先只有"失败可见"，现加版本区间判定与可选的拒绝启动 |

**修复 1：启动 URL 解析的兜底**（`harness.rs`）

- 新增 `startup_url()`（scheme/host/token query 三重校验）与"前缀缺失时扫描行内 token"的路径；
- LAN 后缀仍被天然拒绝（host 非 loopback）；无 token 的裸 `http://127.0.0.1:PORT/` 不会被误认；
- 新增 2 个单测：`accepts_a_url_without_the_documented_prefix`（改写前缀、坏前缀 + 正常 URL 混排）与
  既有拒绝用例的扩展（LAN host、无 token 的 health URL）。

**修复 2：CLI 版本区间判定**（`update.rs` + `lib.rs`）

- 壳对 CLI 有 5 个隐含契约：`--profile web`、`--patch`、`--no-open`、`--port N`（顺序固定）与启动 URL 行；
- 新增常量 `TESTED_MIN` / `TESTED_MAX_EXCLUSIVE` 与纯函数 `compatibility()`，
  返回 Tested / Older / Newer / Unknown，并给出可读原因；
- 启动时（更新检查之后，因为更新可能把版本带进区间）判定：区间外记 warning、状态页显示「未测试版本」；
- 新增配置 `require_tested_dsh`（默认 false）：开启后区间外直接拒绝启动并给出改法。

> **2026-09-15 修正（默认值反转）**：默认改为 **`true`** —— 本壳靠解析 CLI 的启动行工作，区间外的版本
> 可能以看不懂的方式失败，默认拒绝并给出改法比默认放行更可诊断。设 `false` 恢复「只告警并继续」。
>
> 同一开关现在也约束**自动安装**（`update::may_install()`）：会被拒绝启动的版本不会被装上，否则
> `auto_update` 会把用户可用的版本换成壳自己又拒绝运行的那一个。见
> `docs/dsh-desktop-v0.4.1-code-review.md` 第 2 项。

**未改的一处，改为文档说明**：共用 `~/.dsh` 时的并发边界 —— 壳只保证自己端口上没有第二个实例，
终端里另跑同 profile 的 `dsh` 仍会并发写会话存储。README 的「行为」节已写明该边界与 `dsh_home` 隔离选项。

验证：`cargo test` **28 passed**（新增版本区间判定与 URL 兜底两组用例）、`make clippy` 0 warning。

### 13.13 修复：更新会重写正在服务中的 CLI 树（2026-09-13）

**问题（自查 + 实验确认）**：更新检查与安装排在"检测实例"之前，所以当端口上已经有实例在跑
（用户自己起的 CLI，或强杀后 state.json 丢失的残留）时，`npm install -g` 会**一边原地重写它正在服务的那棵树、
一边让它继续工作**。node 是按需懒加载的，树被换掉之后运行中的进程下一次 `require()` 就会失败 ——
这正是"更新期间损坏会话"的真实来源。

**实验**（macOS，node 22.23.2）：

```bash
node -e "setTimeout(() => require('/tmp/tree-demo/mod.js'), 1500)" &
sleep 0.4 && rm -rf /tmp/tree-demo            # 模拟"树被换走"
# => lazy require FAILED: MODULE_NOT_FOUND
```

**修复**：把"停实例"提到安装之前，并抽出一个纯函数承载策略（可单测）：

- `may_stop_before_update(probe)`：端口空闲 → 放行；端口被非 Harness 占用 → 拒绝；是 Harness（自家的或
  别人的）→ 放行，是否真能停由下一步的用户答复决定。
  （2026-09-16 两次修正：先是让 `take_over_existing=true` 也先问用户；随后发现**只要按配置项决定问不问，
  默认用户就看不到面板**，于是把 `take_over_existing` 从判据里整个移除，只留「没答复时是否接管」这一层
  语义。`stop_instance_before_update()` 对非自家实例先调用 `confirm_takeover()`，拿不到接管答复即跳过本次更新）
- `stop_instance_before_update()`：按策略判定后 SIGTERM 目标进程组、等端口释放（复用 `TERMINATE_GRACE`），
  成功后清掉过期的 state.json，并记一条 `stopped Harness pid N before updating the CLI`；
- **拒绝时跳过本次更新**（不是跳过启动）：记 `update deferred, keeping vA: …`，随后照常走检测/接管/启动流程，
  实例完全不受影响；
- 启动状态页按顺序显示：检查更新 → 发现新版本 → **更新前先停止正在运行的 Harness…** → 更新中。

**验证**：新增单测 `update_only_stops_an_instance_it_is_allowed_to_stop` 覆盖策略矩阵（空闲 / 他人占用 / 自家残留 /
允许接管的外部实例 / 不允许接管的外部实例），`cargo test` **29 passed**、`make clippy` 0 warning。
真实更新路径需要 registry 有新版本才能触发，本轮只做策略级验证。

### 13.14 Harness 意外退出后的自动恢复（2026-09-15）

**触发**：用户反馈"插件更新后要求重启 dsh，dsh 重启后桌面端停在错误页、无法恢复"。

**证据（两份日志对得上）**：插件市场 `.dsh-market/log.ndjson` 记
`{"event":"restart","detail":"scheduled pid=99772 helper=1726"}`，紧接着桌面壳日志
`Harness pid 99772 exited unexpectedly (code Some(0))`。即市场的「立即重启」让宿主干净退出
（`profile-boot` 里 `process.on("SIGTERM", () => interrupt(0))`），detached helper 随后在同一端口
拉起替代实例；而看护线程把这次退出当崩溃：清 state.json、销毁窗口、弹终态报错页。此后没有任何回到
harness 的代码路径（`FAILURE_SHOWN` 是一次性闩锁，也没有线程再看端口），macOS 上再点 Dock 图标只会
激活同一个失败页，用户只能退出应用重开 —— 重开还会 SIGTERM 掉市场刚拉起的实例。

**修复**：

- `exit_action(uptime, previous, clean)`（纯函数，可单测）：干净退出（退出码 0）先等 `HANDOFF_GRACE`
  8s 交接，崩溃/被杀只等 2s；连续短命重启到达 `MAX_AUTO_RESTARTS`（3 次）就报错；运行满
  `HEALTHY_RUN`（60s）记一次健康、计数归零；
- `take_over_handoff_and_start()`：交接窗内端口重新服务时先 `terminate_pid` 停掉替代实例 —— 它重放了
  CLI 自身 argv，cwd 是 CLI 目录而不是本壳配置的 workspace —— 再走本壳的 `start()`（正确的
  runtime／workspace／凭据 + 新 token URL）；晚到的交接在自启失败后接管重试一次；
  该实例不是自家的时候不再在此处询问：交回启动流程统一问一次（同一次重启问两遍只会让人困惑）；
- 状态页三面：`show_progress`（进行中，不闩锁）、`show_failure`（终态，页面带「重新启动 Harness」
  按钮）、`show_notice`（终态无按钮，旧 WebView 的浏览器回退用）；`allow_next_failure()` 让重试失败
  能替换页面，状态注入脚本在页面 `<head>` 就绪前重试；
- "再开一次"三个入口等价于按按钮：页面按钮（`dsh-desktop:restart-harness` 事件）、single-instance
  回调、macOS `RunEvent::Reopen`（点 Dock 图标不会启动第二个进程，只能靠它）；
- 退出码文案不再泄露 Rust `Option` 的调试形态：`Some(0)`/`None` → `退出码 0`/`被信号 9 终止`。

**验证**：单测 83 → 88（`exit_action` 策略矩阵、`exit_reason` 文案、状态页脚本转义、页面事件名与壳
常量一致）；实测 `dsh web` 冷启到打印 URL 约 4s、SIGTERM 退出码 0（临时 `DSH_HOME` + 3099 端口，
作为 8s 交接窗的依据）。**真机演练待做**：kill 掉 harness 看它自动回来、在市场点「立即重启」不再进
错误页，步骤写在 README 的验证一节。

**同轮后续**：紧接着落地的旧 WebView 兼容层复用了这套状态页拆分，见
[`design-task-feat-legacy-webkit-compat-layer.md`](./design-task-feat-legacy-webkit-compat-layer.md)。

### 13.15 绘制看护：页面没死但不再刷新（2026-09-15）

**触发**：用户反馈"模型工作时偶尔页面无法继续渲染，提问也发不出去，只能重启应用"。

**用户补充的关键事实（推翻第一版判断）**：页面**有响应** —— 可以点击、可以输入、可以发送；
只有**运行中的模型输出渲染**卡住。重启后模型其实已经跑完，输出一次性正常显示。
所以这不是"渲染进程死了"，第一版按"进程被杀/主线程卡死"做的看护**判据是错的**：
那种页面根本不会响应输入。

**真实机制（代码证据）**：

- Harness UI 把**流式输出**合并到动画帧上：`dsh-api-session-controller/lib/client.js` 的 `Notifier`
  文档原文 —— "Batches structural updates in microtasks and **stream updates by animation frame**"，
  实现即 `markFrameDirty()` → `schedule("frame")` → `requestAnimationFrame(publish)`；
  前端 `dsh-web-frontend` 里同一套合并（`flush:"raf"` 的 store 工厂 + 一次性闩锁）。
- 输入框**不走这条路径**：`notifyNow()` 是同步 flush，注释写明理由 —— 受控输入必须同 tick 通知，
  否则 React 会把 DOM 回滚到旧值、光标跳到末尾。**这就是"能打字、输出不动"能同时成立的原因。**
- 帧一停，`requestAnimationFrame` 的回调就永远排在队里，流式增量全部积压在快照里不发布；
  而模型在宿主进程里继续跑完 —— 与用户观察到的"重启后已跑完"完全一致。
- 谁会让帧停下：WebKit 对**判为不活跃**的窗口停止调度（`WKPreferences.inactiveSchedulingPolicy`，
  wry 暴露为 `BackgroundThrottlingPolicy`，**默认 `Suspend`**，而本壳此前从未设置过）；
  渲染进程被内存压力回收也会如此（本机确有 Jetsam 记录：
  `/Library/Logs/DiagnosticReports/JetsamEvent-2026-09-15-115235.ips`，`idea` 11.4 GB、
  `WebContent` 627 MB、free 约 97 MB）。无论哪一种，结果都是"页面活着但不画"。

**修复**：

- **关掉节流**（根因侧）：Harness 窗口显式 `.background_throttling(BackgroundThrottlingPolicy::Disabled)`
  （macOS 14+ 生效；更早系统该键不存在，wry 会跳过）。
- **改判据**（探测侧）：`FRAME_PROBE` 在页面里维护 `__dshFrames` 计数器，并**最多只排一个**
  待处理帧；每 15s 求值一次并把计数报回来（5s 超时）。`judge_frames(answer, previous)` 的代数：
  **数字没变 = `Frozen`**（JS 在跑、一帧没画）、**变了 = `Drawing`**、**变小 = 新文档**（重载后计数归零，
  不是故障）、**没应答 = `Silent`**（进程没了或主线程卡死）。待处理的那个回调本身就是证据：
  帧一恢复它立刻触发。
- **后台窗口不冤枉**：`attended()`（可见 + 未最小化 + 有焦点）只对 `Frozen` 生效 —— 用户切走时
  WebKit 本就可以停画，判它会把"切了个窗口"变成重载循环；`Silent` 在任何状态都算故障。
- **动作策略** `liveness_action(state, attended, misses, reloads)`：连续 **2** 次才重载（一次抖动不丢
  页面状态），最多 **3** 次，用尽后停在"页面已停止刷新，请重启应用"；重载时清掉帧基线，避免拿新文档
  的计数跟旧文档比。
- 仍保留 `on_web_content_process_terminate`（macOS/iOS）：渲染进程真被系统结束的那一刻立即重载，
  这条与绘制看护互补，不重复。
- 标题承载状态（`页面已停止刷新，正在重新加载…`），`PageLoadEvent::Finished` 到达即恢复默认标题；
  `GENERATION` 计数让窗口重建后的旧看护线程立即退出。

**验证**：单测 100 → **102**（帧计数代数四条分支、停画/静默/后台三种动作矩阵、重载预算与
"坏探测不重置预算"、探测脚本 ES5 且一次只排一帧、`LIVENESS_TIMEOUT < LIVENESS_INTERVAL`）。
**真机触发待做**：复现一次"输出不刷新"，确认日志出现 `Harness 页面停止绘制（输入仍有响应）`
并自动重载；关掉节流后应不再复发。

**边界（如实说明）**：

- `background_throttling` 只在 macOS 14+/iOS 17+ 生效，更早系统上该键无效（wry 会跳过），
  那里只剩探测与重载这条兜底路径。
- **"窗口被判为不活跃"是解释得通的一种触发，不是已证实的唯一一种**：用户报告的现象与代码路径
  完全吻合，本机也有内存压力前科，但没有抓到停画当刻的现场（帧计数、WebKit 日志、Jetsam 记录
  三者缺一）。探测与重载对**所有**导致停画的原因都有效，因为判据是"帧有没有动"而不是"为什么没动"；
  关掉节流则针对其中最可预防的一种。
- 若停画来自第三方 client 插件把主线程占满（`dsh-better-sidebar`、`dsh-dream-skin`、
  `dsh-router-*` 都渲染进主 React 树），重载是恢复手段而非根治，需要插件侧优化。

### 13.16 外部输出的内存边界（2026-09-16）

触发：[`dsh-desktop-latest-code-review.md`](./dsh-desktop-latest-code-review.md)（针对 `44a347e`）。
该文列出的 5 项经核对**全部属实**，本轮全部修复；核对中另发现 3 项该文未列出的问题，一并修复。
原始审查结论与被推翻的细节都留在该文，本文只记实现与验证。

**共同点：所有外部进程的输出都缺少上界。** 三处外部输入（Harness 的 stdout/stderr、npm/pnpm 的
stdout/stderr、登录 shell 的输出）此前都是"读多少留多少"，而它们的内容长度完全由外部决定。

**一、Harness 单行无上限（该文 §3.1）**

`BufRead::lines()` 会把一个 `String` 一直扩到换行符出现为止。dsh、node 或插件打印一条没有换行的
巨型 JSON / base64 / 回显请求体的 provider 错误时，这一行会同时进入环缓冲、脱敏函数和日志文件。
实测：一条 10 MiB 的行调用一次 `redact()` 约 **130 ms**，而代码对每行调了**两次**（环缓冲内部一次、
日志一次）。修复：`LINE_LIMIT_BYTES = 256 KiB`，读取时按字节截断、继续排空到换行符，行尾追加
`…[truncated: line exceeded 256 KiB]`。

**二、同一行重复脱敏（该文 §3.3）**

`ring.push(&line)` 内部调 `redact()`，紧接着 `log.write(&redact(&line))` 又调一次。修复：读取侧脱敏
一次，`Ring::push_redacted(&safe)` 与 `log.write(&safe)` 共用结果。

**注意（该文建议里没写到的坑）**：`parse_dsh_url` 必须继续用**原始行**解析。脱敏会把 `token=…` 改写成
`token=***`，从脱敏后的副本解析启动 URL 会丢掉鉴权参数。

**三、单条巨型日志突破轮转（该文 §3.4）**

轮转原先在写入**之前**检查，但只比较文件当前大小。实测：limit 1 MiB、当前 11 字节时写入一条 10 MiB
的行，文件直接变成 10,485,772 字节，要等**下一条**日志才轮转。修复：判定改为
`written + incoming > limit`。补一条该文未提的边界 —— **当前文件为空时不轮转**：那条日志总得写到
某个文件里，先轮转只会制造一个空备份再把它写进新文件。

**四、npm/pnpm 输出无上限（该文 §3.2）**

`drain()` 用 `read_to_end` 收集整条管道，而安装预算长达 300 秒。修复：`Captured` 有界缓冲，stdout
4 MiB / stderr 1 MiB，**仍然读空管道**（提前停读会让子进程阻塞在写满的管道上，正是原注释要避免的），
截断时在错误信息里标注。

实现与建议的差异：该文建议"固定大小尾部缓冲区"，这里改为**保留前部**。调用方只用 stdout 的 JSON
（截断即报错，不去解析半截文档）和 stderr 的**前几行**（npm 的错误摘要在最前面），环形尾部缓冲
没有用武之地。

**五、该文未列出的三项**

- **`map_while(Result::ok)` 遇非 UTF-8 会永久静默断流**（优先级不低于该文的 P2）。`lines()` 在非法
  UTF-8 上返回 `Err`，`map_while` 就此结束整个循环 —— 其后**所有**行都被丢弃，包括 `dsh web:` URL 行，
  而读取线程仍在正常排空管道，日志里没有任何迹象。实测：插件输出一个 `0xFF` 字节后，转发出来的行数
  为 0。修复：自实现 `read_line`（按字节读到换行、`from_utf8_lossy` 解码、单行截断、剥 `\r`）。
  这条与第一项共用同一个函数，是把它从 `lines()` 换掉的主要理由。
- **`Ring::tail()` 拼接无上限内容**：环缓冲只限行数（200）不限字节，巨型行会原样进入失败页。
  修复：增加 `RING_BYTES = 1 MiB` 字节预算。
- **登录 shell / npm / `ps` / `node` 子进程无超时**：`command -v npm`、`npm prefix -g`、`command -v node`、
  三处 `ps`、以及 `node dsh.js --version` 都用裸 `Command::output()`，它会**永久等待**。带 `-lc` 的几处
  跑的是 `$SHELL`，会 source 用户的 rc 文件 —— 一个等待网络挂载或密码输入的 rc 就能把启动线程永久
  挂住，而这几处正是启动关键路径。
  修复：新增 `process::stdout_within(command, timeout)`（只取 stdout 首行、超时 kill），七处调用点全部
  改走它。`identity.rs` 里超时按 `Parent::Live` 处理 —— 与它原有的"未知即保留"一致，不能因为 `ps`
  超时就把进程判成"父进程已消失"而放行信号。

  这里有个**必须区分的三分支**（`parent_from_ps`）：`None`（没跑成）→ `Parent::Live`；
  `Some("")`（跑了、没输出，即"没有这个进程"）→ `Parent::Gone`；有输出 → 看是不是会话管理器。
  把后两者合并会让崩溃残留**永远无法清理**（那正是最常见的一种），所以单测直接钉住这三条。
  顺带修掉 `shellenv::capture` 超时分支里的 `reader.join()`：孙进程会一直持有管道写端，join 就是当初
  那个预算要消除的无界等待，改为 `drop(reader)`。

**六、退出同步等待（该文 §3.5）：维持现状**

该文建议"接受现状或异步清理"，本轮选**接受现状**。同步 `shutdown()` 换来的是"退出后不留残余 Harness"，
比最多 5 秒的退出停顿更重要；异步化要新增一条退出路径上的状态机，收益与风险不成比例。

**验证**（装有 Rust 工具链的环境，该文 §8 的限制在本轮不适用）：

- `cargo test` **155 passed / 0 failed**，集成 6 项。本轮新增 12 项：单行截断后流继续、非 UTF-8 不断流、
  空行不断流、CRLF 剥 `\r`、Ring 字节预算、Ring 只存已脱敏内容、轮转不越界、空日志不轮转、
  有界管道三例（截断/完整/空）、`ps` 三种答复（超时/无输出/有父进程）的分类。
- `cargo fmt --check` 通过、`cargo clippy --all-targets` 0 warning、`cargo check --all-targets` 通过。
- 修复前的缺陷复现（用于确认测试真的能抓）：把 `Logger` 与 `redact` 原样抽出编译，实测轮转越界
  （10 MiB 行 → 10,485,772 字节文件）与重复脱敏的 130 ms/行开销；`map_while` 的断流用一个含 `0xFF`
  的输入实测转发行数为 0。

**遗留（本轮未做，如实说明）**：`locator::probe_node_within` 与 `probe_version_line` 的读线程仍可能
因孙进程持有管道而残留（它们已 `drop(reader)` 不阻塞，但线程本身不回收）；这两处只影响进程退出时的
短暂残留，不影响启动路径的正确性。

### 13.17 第五轮审查：24 项问题（2026-09-16）

触发：[`dsh-desktop-b0d5405-code-review.md`](./dsh-desktop-b0d5405-code-review.md)（针对 `b0d5405`）。
该文 24 项经核对**全部属实、无一项误报**，本轮全部处理；逐项状态与差异回写在该文 §9，
这里只记实现与验证。

**三个 P1**

- **日志脱敏下标错位（安全）**。`redact_bearer` / `redact_field` 在循环里把 `rest`/`search` 往后切，
  却用 `line[..at]` 判断"关键字是否在词首" —— `at` 是相对当前后缀的偏移，从第二次匹配起就错位。
  两个后果：错位处恰好是字母数字时**第二个凭据不脱敏**；`at` 落在多字节字符中间时 `line[..at]`
  **直接 panic**（实测 `错误：token 无效，请检查 token=xxx` 即崩）。修复：改用绝对偏移
  `start + at`。panic 落在 `forward_lines` 读取线程时管道被关闭、落在 `app_log` 时 `APP_LOGGER` 锁被
  毒化（此后每次 `app_log` 都 panic，退出流程再也停不掉 Harness），所以顺带把 `app_log` 改为**锁外
  脱敏 + 容忍毒化**。同时补上 `Basic`/`Digest`/`Token` 方案 —— 它们把凭据放在同一个位置，此前整条
  留在日志里。方案名只在字段名已证明是凭据时匹配，否则 `the token is expired` 会被改成 `the *** expired`。
- **Windows 打开外链可注入命令（安全）**。原实现 `cmd /C start "" <url>`：`Command::arg` 按 MSVC
  约定转义，而 `cmd.exe` 自己解析 `&`、`|`、`^`、`%`，WHATWG URL 序列化又不编码它们 ——
  `https://evil.example/a&calc.exe` 里的 `&` 就是命令分隔符。URL 来自模型输出与抓取页面，属攻击者可控。
  修复：FFI 调 `shell32!ShellExecuteW`，URL 作为一个字符串交给 shell 处理器，中间没有解释器。
- **影子前缀首次更新失败后无法回滚**。打包版首更时影子前缀还是空的，`commit` 发现目标不存在
  （`had_previous = false`）便没有产生备份；新树起不来时 `rollback` 因"回滚目录不存在"失败，记录被保留、
  坏树留在原地，而 `update-check.json` 已记为最新版 —— **此后每次启动都失败**。修复：`SwapRecord` 增加
  `had_previous`（`Option`，缺字段的旧记录按"有备份"处理，绝不因此删用户的安装），`rollback_with` 在
  `Some(false)` 时把失败树移到 `.failed` 并视为回滚成功，种子重新生效。配套 #8 的失败标记，避免下一轮
  立刻重装同一版本。

**P2 的主要几类**

- **外部进程的时间预算仍漏**。`stdout_within` 在子进程退出后无条件 `join` 读取线程，而管道会被子进程
  留下的**孙进程**持有（登录 shell 的 rc 里 `cmd &` 起的 agent/daemon 就继承 stdout）—— 实测 1 秒预算
  实际返回 4.01 秒，换成常驻进程则永久挂住。修复：新增 `process::output_within`，读线程**边读边发布**到
  共享缓冲，调用方在子进程退出后用**有界** `recv_timeout` 收尾。修复后同一输入 176 ms 返回，且已读内容
  不丢。`lsof`（并加 `-b`）、`ss`、`netstat`、`shellenv::capture`、两个 node 探测一并改走它，
  非 UTF-8 输出也不再清空整次环境导入。
- **更新流程提前停实例**。暂存写的是 `runtime/staging/…`，根本不碰在用的树，但停止步骤排在它前面：
  一检测到新版本就杀掉本可复用的会话，暂存失败时白停一次，用户在没有 Harness 的状态下等下载。修复：
  暂存移到停止之前，停止只发生在 3c/3d；3d 增加守卫 —— 外部实例被保留且其 CLI 树正是待替换的那棵时
  放弃切换（否则 Windows rename 失败、Unix 上运行中的实例会在下次 `require()` 崩）。
- **复用的旧实例没有看护**。复用分支只 `adopt` 就返回，而 `watch_harness` 需要 `Child::wait`；实例死掉后
  自动恢复不触发，页面看护只问"帧有没有动"（服务端死了页面照画），窗口停在再也连不上的页面 —— 正是
  自动恢复要解决的问题。修复：新增 `watch_reused` 轮询 pid，走同一套 `exit_action` 流程（拿不到退出码
  按 `clean = false`）。
- **profile 快照/模板复制不支持符号链接**。`DirEntry::file_type()` 不跟随链接，链接于是落进 `fs::copy`，
  而 `fs::copy` 跟随：指向目录的链接直接报错，指向文件的被复制成普通文件（`node_modules/.bin/*` 因此
  失效）。本机真实 profile 有 **163 个**这样的链接，所以开启 `auto_update_plugins` 时**每次启动都先停掉
  实例再更新失败**。修复：新增 `transaction::copy_entry`，链接按链接复制（Windows 无权限时退回复制），
  `restore_tree` 删旧链接时也不再跟随。实测 349 MB / 163 链接的真实 profile 快照成功。
- **`-lc` 与 `-lic` 不一致**。`find_launcher` 对**每个**候选都先查一次 node，即使 `judge` 根本不需要；
  而 `-lc` 不读 `.zshrc`（实测确认），nvm/fnm 用户在那里查不到。修复：node 查找改为惰性。
- **`stage-runtime.sh` 把 pnpm store 生成进模板**。`cache` 默认是相对路径，而 `make-profile-template.sh`
  先 `cd "$dest"` 再 `pnpm install --store-dir …`，pnpm 相对**项目目录**解析 —— store 落进
  `profile-template/.runtime-cache/`，既进便携包又在首启播种时复制进用户 profile（Makefile 用了
  `abspath` 所以不受影响，Windows 便携构建受影响）。修复：绝对路径 + 传入 tools 目录，
  `check-runtime-stage.sh` 增加断言。
- **`present_status_page` 先销毁窗口再重建**。`destroy` 是异步的，tauri-runtime-wry 处理 `Destroyed` 时
  若窗口表变空会发 `RunEvent::ExitRequested`，而本壳在该事件上直接 `shutdown()`、从不 prevent ——
  于是新 splash 与旧窗口的 `Destroyed` 抢时序，不利时"显示状态页"变成"直接退出"。修复：调换顺序，
  先建 splash 再销毁，与 `create_harness` 一致。

**其余（#4、#14–#24）**

`Uint8Array.fromBase64` 的 `/s/g`（删掉所有小写 `s`，导致解码抛错或**静默返回错误字节**）改为 `\s`；
`Math.sumPrecise` 的非有限值短路（`[Infinity]` 原本返回 `NaN`）、`Iterator#reduce` 无初值时下标从 1 起；
`plugin_skip_reason` 改为无条件调用（`NotDeclared` 此前永不记录）并修掉文案里填错的两个参数；
`takeover::Retry` 让"用户已取消"不再被重复追问；`HARNESS_URL` 让崩溃恢复的兜底 URL 带上端口；
`spawn_and_reap` 收掉外链子进程的僵尸；`output_within` 与 `taskkill` 统一设 `CREATE_NO_WINDOW`；
`netstat` 不再依赖会被本地化的 `LISTENING`；`parse_dsh_url` 的无前缀兜底只接受本次端口；
`terminate` 在组长已退出时仍清理整个进程组（实测确认 `kill(-pgid)` 在组长退出后仍可达子进程）；
`Cache::is_fresh` 的乘法改 `saturating_mul`；`SystemUpdates` 去掉与 `Config` 矛盾的 `#[default]`。

**验证**：`cargo test` **174 passed / 0 failed**（本轮新增 19 项）、`cargo fmt --check` 通过、
`cargo clippy --all-targets -- -D warnings` 在 **host 与 `x86_64-pc-windows-gnu` 两个目标上均 0 warning**、
`scripts/check-runtime-stage.sh` 自检通过。三个 JS 兼容块在真实 node 引擎里跑通（此前只有字符串断言，
这正是 `/s/g` 能发布出去的原因）。

**自查**：本轮改动自身引入的两个问题由新增测试当场抓出并在提交前修正 —— `Math.sumPrecise` 的第一版
修复只处理"输入非有限"，漏了"求和过程溢出"（`[1e308, 1e308]` 仍返回 `NaN`）。

**遗留（未做实机）**：#13 未在真实窗口连续触发渲染进程崩溃验证（机制已在依赖源码核实，调换顺序无行为
风险）；#2、#19 的 Windows 行为只经交叉编译与 lint，未实机点击构造过的链接。

### 13.18 第五轮审查的复核与补修（2026-09-16）

触发：对 §13.17 的复核，见 [`dsh-desktop-b0d5405-code-review.md`](./dsh-desktop-b0d5405-code-review.md) §10。
复核结论推翻了 §13.17 里的三条说法，§13.17 原文保留，以本节为准：

- "`takeover::Retry` 让「用户已取消」不再被重复追问"：**不成立**。当时 `confirm_takeover` 仍会弹出问题，`Retry::Declined` 只是在第二次拒绝后换了一句日志。
- "3d 增加守卫……放弃切换"：**实际不生效**。守卫检查的是新端口，而唯一会保留外部实例的 `UseOtherPort` 分支已经把 `port` 改成了新端口。
- "`-lc` 与 `-lic` 不一致……修复：node 查找改为惰性"：**只做了一半**。惰性查找只省下了登录 shell，查找本身仍然看不到 `.zshrc` 里加的 PATH。

**本轮实现**

- **重试不再重复询问。** `start()` 返回 `StartError { reason, declined }`。`take_over_handoff_and_start` 先用 `may_ask_after_failure(error, port, port_now)` 判断：用户在本次尝试里已经选过"取消"（`declined`），或选过"改用其它端口"（`port_now != port`），就直接报告失败。
- **3d 切换守卫。**
  - `UseOtherPort` 记下被保留实例的 pid，由 `swap_conflicts(target, target_exists, owned, command)` 判断冲突：
    - 目标树不存在（打包版首更）：不冲突；
    - 用户前缀：一律冲突，因为终端里的 `dsh` 经软链启动，命令行里看不出树的路径；
    - 影子前缀：看命令行是否包含树的路径（插件市场交接重放的正是本壳的命令行），读不到也算冲突。
  - 3c 之后端口又出现外部 Harness 的竞态也放弃切换。两种情况都**继续启动**，不再像之前那样 `return Ok(())` 后既不启动 Harness 也不给页面。
- **核心更新失败退避。**
  - `Cache.failures` 记录连续失败次数；`failure_window_secs` 为 5 × 4^(n−1) 分钟，封顶 24 小时。
  - `carried_failure` 在重试窗口过后仍保留标记，7 天后遗忘。否则次数永远停在 1，退避不会增长。
  - 暂存失败、提交失败、启动失败回滚、`recover_pending_swap` 回滚，这四处都写标记。
  - 刚换上的树首次启动使用 `STARTUP_TIMEOUT_FIRST`。
- **登录 shell 的 PATH 参与定位。**
  - 解析运行时之前先汇合 `-lic` 环境采集，导入的 `PATH` 经 `resolve_runtime` 传给 `locator::system_node` / `system_dsh`。
  - `locator::search` 先查 App 的 PATH，再查导入的 PATH；拿到导入的 PATH 时不再起 `-lc`，没导入时才用它兜底。
  - `path_lookup_in` 忽略相对条目。
  - 代价：环境采集（约 160 ms）从"与 npm 查询并行"改为在解析运行时之前汇合；换来的是有 nvm 的机器上省掉若干个 0.3–1 s 的登录 shell。
- **插件更新先快照再停实例**，快照失败也写失败标记。
- **修复自身引入的回归。**
  - `recover_exited` 开头判断 `EXITING`，否则正常退出会被当成崩溃。
  - `parse_netstat_listener` 按列判断 TCP、本地地址 `:{port}`、远端 `:0`，与语言无关，也不会误认出站连接。
  - 组长已退出时，`clean_orphaned_group` 只在 Unix 上对 `-pgid` 发信号，不再按 pid 回退，避免 pid 被复用后误杀。
  - `scheme_prefix_len` 原地比较字节（256 KiB 满字段行从 180 ms 降到约 2–3 ms）。
  - Windows 上 `copy_entry` 的回退路径，相对链接目标改为按链接所在目录解析。
  - `output_within` 记录是否读到 EOF；没读到或被截断时，用 `complete_lines` 丢掉残行，宽限改为 500 ms；`stdout_within` 遇到"截断后没有完整行"时返回 `None`。
  - `ShellExecuteW` 在专用线程里调用，并配对初始化 COM。
- **补上原审查漏掉的脱敏写法。** `locate_value` / `value_len` 识别 `key = value` 和 `"key": "value"`（含 `\"` 转义与 `'`）；带引号的值以配对的引号为界；没有闭合引号时退回无引号规则，并先跳过开头空白。
- **`stage-runtime.sh` 生成模板时，把自带 node 放到 PATH 前面**，与 Makefile 一致。

**验证**：

- `cargo test` **185 passed**（新增 11 项），集成 6 项；`cargo fmt --check` 通过；本机与 `x86_64-pc-windows-gnu` 的 `clippy -D warnings` 均为 0 warning；`check-runtime-stage-test.sh` 通过。
- 脱敏另做了 30 万条随机输入测试，0 次 panic。

**遗留**：Windows 实机验证（#2、#19、N5、N7），以及 #13 的实机验证，与 §13.17 相同。

### 13.19 绘制看护的第二版：把「没在画」和「没在跑」分开（2026-09-18）

**触发**：用户再次报告“模型响应渲染卡住，think 模式尤其容易卡住，退出应用重新启动后恢复正常”。
完整审查见 [`dsh-desktop-v0.4.4-freeze-review.md`](./dsh-desktop-v0.4.4-freeze-review.md)。

**§13.15 的哪一半没说对**：那一版把“停画”归因到 `background_throttling`（默认 `suspend`，本壳已设为 `disabled`），
并据此承诺“关掉节流后应不再复发”。实测**不成立**：0.4.4（含该设置，wry 0.55.1 也确实把 `Disabled` 写进了
`WKPreferences.inactiveSchedulingPolicy`，见 `wry-0.55.1/src/wkwebview/mod.rs:473-498`）上，窗口一失去焦点，
帧计数仍**连续数千次**不动，而探测始终有应答 —— 即 JS 在跑、一帧没画。那一版对“帧没动”的解释是对的，
对“为什么会停”和“停了怎么办”的判断是错的。

**§13.15 里真正失效的那一步**：`attended()`（可见 + 未最小化 + 有焦点）为假时走 `Unattended`，
分支里什么都不做。于是最常见的使用形态（窗口在旁、焦点在 IDE）下，停画**永远不会被恢复**，
只剩一行每 15 s 重复一次的日志 —— 3617 行，占全日志 81%。

**这一版的判据**：探测同时维护两个计数器，并在“帧没动”时用它们把两种停画分开。

| 帧 | `setTimeout(0)` 心跳 | 结论 | 动作 |
| --- | --- | --- | --- |
| 动了 | — | `Drawing` | 清零 streak、恢复标题 |
| 没动 | 在跳 | `Frozen(Suspended)`：任务队列活着，只是不被绘制 | **不重载**；标题提示“已暂停绘制：聚焦窗口或重新加载界面即可恢复”，日志每个连续段记一条 |
| 没动 | 没跳 | `Frozen(Stalled)`：主线程忙或挂了 | 连续 3 次 → 重载 |
| 无应答 | — | `Silent`：渲染进程没了 | 连续 3 次 → 重载 |
| 引擎数不了心跳 | — | `Frozen(Unknown)` | 归入 `Stalled` 一侧，由重载预算兜住 |

- **为什么不能只靠“帧没动”**：重载是唯一恢复手段，也是把同一段重内容再渲染一遍的手段；
  上游 [discussion #5023](https://github.com/deepseek-ai/deepseek-harness/discussions/5023) 记录的正是
  “一个巨大的 markdown 块每 delta 重解析整篇（O(n²)）→ 帧率 60→13 → 重载后又烧一遍”。
  实测日志里 5 次停画有 4 次在一个周期内自愈，说明其中相当一部分只是页面在忙。
- **`Stalled` 与 `Suspended` 的门槛不同**：前者连续 3 次（≥45 s）才动；后者只有在“用户回来之后仍然不画”时才并入前者。
  用户回来这件事由窗口 `Focused(true)` 事件推进：看护的等待从 `sleep` 改成 `recv_timeout`，
  事件到达就**立即**判定一次，而不是等下一个周期。
- **一次坏探测不再等于一次误重载**：`LIVENESS_MISSES` 2 → 3。
- **手动恢复**：菜单 View → 「重新加载界面」（⌘R），走与看护相同的 `navigate(current_url())`。
  页面是远程内容、没有 capability，自己给不出这个入口；没有它，一次停画的代价就是退出整个应用。
- **现场可复盘**：壳日志每条加本地时间戳（`[YYYY-MM-DD hh:mm:ss]`），`Unattended` 分支的每周期重复改为
  “每个连续段一条”。没有时间戳时，间歇性冻结无法与崩溃报告、Jetsam 事件对齐 —— 这正是 §13.15 的
  “真机触发待做”一直没做的原因。
- **死因文案**：渲染进程结束的日志不再写“多为内存压力”。本机唯一一份相关报告
  （`com.apple.WebKit.WebContent-2026-09-18-095206.ips`）是 JavaScriptCore 主线程断言
  （`CodeBlock::setOptimizationThresholdBasedOnCompilationResult`），且没有对应的 JetsamEvent。

**验证**：单测 185 → **192**（帧/心跳的四种组合与“未表态”归并、暂停不花重载预算、聚焦后仍不画并入重载、
一次坏探测只观察、连续三次才重载、探测脚本 ES5 且帧与心跳各自一次只排一个回调、
**探测输出与解析结构体是同一份线上格式** —— 该用例当场抓到 `timers_seen` 与 `timersSeen` 的大小写不一致，
那会让每次应答都解析失败、被读成“渲染进程没了”；菜单项与处理器共用同一个常量且有 ⌘R；
日志时间戳格式与“时钟不可用只丢时间戳”）；`cargo fmt --check` 与两端 `clippy -D warnings` 均为 0 warning。

**真机验证待做**：切到后台再切回，确认日志出现 `页面停止绘制：计时器仍在运行`、标题出现“已暂停绘制”、
且**没有**重载；以及 ⌘R 真的重新加载当前会话（会话不丢）。

**边界（如实说明）**：

- 心跳只能证明“任务队列还在跑”，不能证明“窗口可见”。macOS 的遮挡（occlusion）节流是否独立于
  `inactiveSchedulingPolicy` 仍未证实；这也是不把恢复押在“回到前台就好了”上的原因 ——
  用户回来之后仍不绘制，页面就并入 `Stalled` 的重载判定。
- think 阶段之所以最容易停：折叠的 Think 行仍把整段思考文本放进 DOM，每帧还要对整段文本做一次
  `trimEnd` + `lastIndexOf` + `replaceAll("**")`（`dsh-client-ui-chat` 的 `ReasoningRow`），
  流式发布要连等三个 `requestAnimationFrame`（`dsh-client-ui-conversation`），再叠加 §13.15 记录的
  第三方 client 插件（`dsh-dream-skin`、`dsh-better-sidebar`）都在主 React 树里。壳这一侧只能减少误重载、
  提供手动重载，根治要走上游。

