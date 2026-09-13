# 代码审查结论：main 分支对照桌面壳方案的 11 处问题与修法

> 审查日期：2026-09-13 ｜ 分支 `main` ｜ 基准 commit `c1fbb18`
> 审查范围：`src-tauri/src/*.rs`（8 个文件、约 2600 行）、`Makefile`、`src-tauri/tauri.conf.json`、
> `.github/workflows/*`
> 对照文档：[`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md)
> 姊妹文档：[`design-task-fix-bundled-runtime-audit.md`](./design-task-fix-bundled-runtime-audit.md)
> （`feat/bundled-runtime` 分支的审查；其中 P0-3 与本文 §2 是同一个问题，两边都要改）
>
> 本文是**审查结论 + 修复任务清单**，不修改方案文档本身（它记录的是当时的决策与实测，按文档生命周期规则保持原样）。
> 修复落地后把结果回写到"处理状态"列。2026-09-13 已完成修复，逐项结果与新增测试见 §16；
> 方案文档中确属"实现与设计不符"的两处事实性描述（§5 看护方式、§4 步骤 8/9 的落地范围）已就地加注修正。
>
> **复核（2026-09-13，commit `d379428` + `7905ae8`）**：11 项修复逐条核验通过，测试与 lint 结果已独立复跑确认；
> 复核中新发现 6 项（其中 1 项是首轮审查漏掉的既有缺陷，由本次 #3 的改动暴露）——见 **§17**，均**未修**。

---

## 0. 结论摘要

方案 §1–§13 描述的主体功能在 main 上都已落地（locator、启动参数顺序、URL 解析、探测/接管、
进程组生命周期、退出清理、看护线程、日志脱敏、单实例、更新检查）。问题分三类：
**进程终止的身份校验不足**、**更新链路会重复执行**、**方案写了但没实现的交互**。

| # | 级别 | 问题 | 位置 | 处理状态 |
|---|---|---|---|---|
| 1 | P1（安全） | 外链交给系统打开时没有 scheme 白名单，`file:` 等任意 scheme 可被页面触发 | `src-tauri/src/window.rs:70-88`、`168-180` | **已修**：`may_open` 只放行 http/https |
| 2 | P1 | 自愈清理不校验身份，可能 SIGTERM 到 pid 复用后的**无关进程组** | `src-tauri/src/lib.rs:317-327` | **已修**：`should_self_heal` 要求 pid 仍监听记录的端口 |
| 3 | P1 | 更新前停实例用进程组终止，与接管路径的保护相反 | `src-tauri/src/lib.rs:236` | **已修**：外部实例走 `terminate_pid`（`stop_mode`） |
| 4 | P1 | 缓存的 `UpdateAvailable` 每次启动都会重跑 `npm install` | `src-tauri/src/lib.rs:356-389`、`update.rs:432-473` | **已修**：`Cache.attempted` 抑制同一窗口内的重复安装 |
| 5 | P1 | `workspace` 不校验，失败信息没有上下文 | `src-tauri/src/lib.rs:562`、`harness.rs:241` | **已修**：`Config::repair` + `validate_paths` + `spawn:` 日志 |
| 6 | P2 | 没有 HOME 时 `default_workspace()` 回落到 `/` | `src-tauri/src/lib.rs:65-69` | **已修**：回落 `temp_dir()` 并记日志 |
| 7 | P2 | 方案 §4 步骤 3 的"记忆路径"与"选择 dsh 路径…"未实现，`remembered` 是死参数 | `src-tauri/src/locator.rs:16-27`、`68-80` | **已修**（产品决策：中间方案）：新增 `config.json` 的 `dsh_path`，不做选择器 |
| 8 | P2 | 方案 §4 步骤 8/9 的"窗口加载失败重试 token URL"未实现 | `src-tauri/src/window.rs:60-123` | **部分实现**（产品决策）：退避重试同一 URL，不重启 harness，见 §16 |
| 9 | P2 | 日志轮转只有 1 个备份，且只在启动时判定一次（方案 §8 写的是 5MB × 3 份） | `src-tauri/src/harness.rs:346-363` | **已修**：写入前惰性判定，滚动 `.1`→`.3` |
| 10 | P3 | `bundle.resources` 在 main 上是死配置，且会把残留的 staging 静默打包 | `src-tauri/tauri.conf.json:28` | **已修**：main 上删除该配置，合并分支时再加回 |
| 11 | P3 | 发布门禁 `make fmt` + `git diff` 会改工作区；Windows 分支代码仍在但不可用 | `.github/workflows/release.yml:109-110`、`process.rs:135-145` | **已修**：新增 `make fmt-check`；`#[cfg(windows)] compile_error!` |

**文档债（非代码问题）**：方案 §5 写的"启动后每 30s 校验一次子进程存活"已被 `watch_harness`
的阻塞 `wait` 取代（§13.8 明确"没有任何后台轮询或定时器"，这是更好的做法），但 §5 的正文没同步。

---

## 1. P1（安全）：外链没有 scheme 白名单

**位置**：`src-tauri/src/window.rs:70-88`（`on_navigation` / `on_new_window`）、`168-180`（`open_external`）

```rust
.on_navigation(move |target| {
    let same_origin = target.scheme() == "http"
        && target.host_str() == Some("127.0.0.1")
        && target.port() == Some(port);
    if !same_origin {
        open_external(target.as_str());   // ← 任意 scheme 都递给 open / xdg-open
    }
    same_origin
})
```

**问题**：导航围栏正确地拦住了 WebView 内的跨源导航，但随后把 URL **原样交给操作系统**，
`open_external` 也不做任何过滤。Harness 页面渲染的是 agent 产出的内容（含模型生成的链接、抓取到的网页），
所以 `window.open("file:///System/Applications/Calculator.app")` 这类调用会让 macOS 直接启动该应用；
`file:` 之外，其它 App 注册的自定义 URI scheme 同样是现成的攻击面。

方案 §7 的原话是"其他 **https** 链接在 Rust 侧用 opener 打开系统浏览器"，实现放宽成了"其他一切"。

**修法**：在 `open_external` 入口按 scheme 白名单过滤（`http` / `https`，按需加 `mailto`），
其余记一条 `external scheme blocked: …` 日志后丢弃。

**回归**：`open_external` 抽出一个纯函数 `may_open(target: &Url) -> bool` 并加单测
（`https` 放行、`file` / `smb` / 自定义 scheme 拒绝）。

---

## 2. P1：自愈清理会 SIGTERM 到 pid 复用后的无关进程组

**位置**：`src-tauri/src/lib.rs:317-327`

```rust
if let Some(state) = process::read_state(data_dir) {
    if process::is_alive(state.pid) {
        process::terminate(state.pid, TERMINATE_GRACE);   // ← kill(-pid, SIGTERM)：整个进程组
    }
    process::clear_state(data_dir);
}
```

**问题**：方案 §4 步骤 2 写的是"读状态文件，若上次的 pid 仍存活**且命令行匹配** → SIGTERM 清理"，
实现只做了 `is_alive`。`state.json` 会跨重启留存（只有正常退出与看护线程会清），
重启后低位 pid 被复用是常态；`process::terminate` 又是先 `kill(-pid)` 打整个进程组。
于是最坏情况是：上次崩溃残留的 state.json + 重启 + pid 复用 → 桌面壳启动时把一个无关进程组整组 SIGTERM。

同一个 `state.json` 在"复用/接管"分支（`lib.rs:450-452`、`221-223`）里是拿 `listener_pid(port)`
交叉验证过的，唯独自愈这条没有。

**修法**（任选其一或叠加）：

- 用 `state.port` 的监听者比对：`listener_pid(state.port) == Some(state.pid)` 才动手（与现有分支一致，最省事）；
- 校验进程身份：`ps -p <pid> -o command=` 里含 `dsh` 且含 `--profile web`；
- 至少把 `started_at` 与系统启动时间比较，开机之前的记录一律只清文件不发信号。

**回归**：抽一个纯函数 `should_self_heal(state, listener_pid, alive) -> bool` 并加单测矩阵。

---

## 3. P1：更新前停实例用了进程组终止

**位置**：`src-tauri/src/lib.rs:236`（`stop_instance_before_update`）

`stop_instance_before_update` 用的是 `process::terminate`（先 `kill(-pid, …)`），
而接管路径 `lib.rs:481` 特意用 `process::terminate_pid`，注释写明
"只对该 PID 发 SIGTERM（不用进程组，避免连带杀掉用户终端）"（方案 §13.1 第 2 条）。

**影响**：端口上是用户从终端起的外部 Harness 时，更新路径会把它所在的整个进程组 SIGTERM
（同一条管道/脚本里的其它进程一起遭殃）。

**修法**：外部实例一律走 `terminate_pid`；只有确认是本应用上次启动的实例（`ours.is_some()`）
才可以用进程组版本。

> 同样的问题在 `feat/bundled-runtime` 上也在（见姊妹文档 P0-3），修的时候一并处理，避免两边分叉。

---

## 4. P1：缓存的 `UpdateAvailable` 每次启动都会重跑 `npm install`

**位置**：`src-tauri/src/lib.rs:356-389`、`src-tauri/src/update.rs:432-473`

`check_cached` 的缓存只挡**网络查询**，不挡**安装**：命中缓存时返回的 `status` 仍然是
`UpdateAvailable`，调用方照样走"停实例 → `npm install -g`"。而 `Cache::is_fresh` 只在
"已安装版本变了"时失效：

```rust
if self.installed != installed { return false; }
```

**后果**：方案 §13.11 已经识别过"npm 把包装到别处（自定义 prefix、pnpm/yarn/volta 布局，
或 `install_prefix()` 返回 None）"的情形 —— 那时版本永远不变，于是

1. **每一次启动**都会先 `stop_instance_before_update`（杀掉正在跑的 Harness，包括用户自己起的），
2. 再完整重建一棵约 289 MB 的依赖树，
3. 然后记一条 `update installed but the supervised CLI is still X`，什么也没变。

§13.11 写的"此后每个缓存周期都会重装一遍"低估了频率 —— 缓存窗口内也照装不误。

**修法**：安装完成后按结果刷新缓存 —— 版本没变时把这次的 `latest` 记成"已尝试且无效"
（例如把 `Cache.installed` 写成当前版本 + 增加一个 `attempted: Option<String>` 字段），
命中时只提示不再安装；`Cache` 结构改动要保持 serde 向后兼容（新字段带 `#[serde(default)]`）。

**回归**：单测覆盖"安装后版本未变 → 下次同一缓存窗口内不再触发安装"。

---

## 5. P1：`workspace` 不校验，失败信息没有上下文

**位置**：`src-tauri/src/lib.rs:562`（`SpawnOptions.workspace`）、`src-tauri/src/harness.rs:241`（`spawn`）

`config.json` 里的 `workspace` 直接进 `Command::current_dir()`。目录不存在、或被写成相对路径时，
用户在错误页上只看到：

```text
启动进程失败: No such file or directory (os error 2)
```

既没说是哪个路径，也没说是哪一项配置 —— 而 main 上 `harness::spawn` 也不记
`spawn: <完整命令行>` 与 `警告: workspace … 不是已存在的目录` 这两行日志
（`feat/bundled-runtime` 已经补上了）。

**修法**：把分支上那套搬过来 ——

- `Config` 加一个 `repair()`：`workspace` 不是"绝对且存在的目录"就回落到默认值并记日志，
  **不改写用户的文件**（与现有"config.json 无法解析时不覆盖"的原则一致）；
- `spawn` 前置校验 node / dsh_js / workspace 必须是绝对路径，错误信息里点名是哪一项；
- 启动时记一行 `app data dir = … | workspace = …`。

---

## 6. P2：没有 HOME 时 `default_workspace()` 回落到 `/`

**位置**：`src-tauri/src/lib.rs:65-69`

```rust
std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
```

把文件系统根目录当 agent workspace 比直接失败更糟（agent 的 `glob` / `grep` 会从 `/` 开始扫）。
改为 `std::env::temp_dir()`，并在日志里说明为什么没用 `$HOME`。

---

## 7. P2：方案 §4 步骤 3 的"记忆路径 / 选择 dsh 路径…"未实现

**位置**：`src-tauri/src/locator.rs:16-27`、`68-80`；调用点 `src-tauri/src/lib.rs:346`

方案 §4 步骤 3 的顺序是 `DSH_DESKTOP_DSH` → **记忆路径** → PATH → 常见目录 → login shell，
失败时给"错误页 + 『选择 dsh 路径…』"。实现里：

- `locate(None, …)` —— `remembered` 参数**永远是 None**，是个死参数（`find_launcher` 里的分支不可达）；
- 失败只返回一句纯文本错误，没有任何选择路径的交互。

**处理建议**（二选一，需要产品侧拍板）：

- 实现：找不到时在状态页给一个"选择 dsh 路径"的按钮（要给 splash 加 dialog 权限），选中的路径写进
  `config.json` 的新字段 `dsh_path`，`locate` 从那里取 `remembered`；
- 或者降级：删掉 `remembered` 死参数，把"用 `DSH_DESKTOP_DSH` 指定路径"作为唯一手段写进错误页文案，
  并在方案 §4 标注该交互已取消。

---

## 8. P2：方案 §4 步骤 8/9 的"窗口加载失败重试"未实现

**位置**：`src-tauri/src/window.rs:60-123`

方案基于"token URL 可重复访问"的实测结论，要求：加载失败 → 重试同一 token URL；
连续失败 → 重启 harness 取新 URL。实现里 `create_harness` 建完窗口就返回，
没有任何加载结果的观测点 —— 首次导航失败（偶发的端口竞争、WebView 初始化抖动）时，
用户看到的是一个空白窗口，只能自己重启应用。

**修法**：监听 `PageLoadEvent`（Tauri 2 的 `on_page_load`）或在窗口上跑一次轻量探测，
失败则按方案退避重试同一 URL，连续 N 次后走"重启 harness 取新 URL"。

---

## 9. P2：日志轮转与方案不符

**位置**：`src-tauri/src/harness.rs:346-363`（`Logger::open`）

- 方案 §8 与实施记录写的是"单文件 5MB × **3 份**"，实现只保留 **1 个备份**（`harness.log.1`，
  `rename` 时直接覆盖上一个）；
- 轮转**只在 `Logger::open` 时判定一次**，而 `open` 只发生在启动阶段（`init_app_log` 与 `spawn`）。
  一次长时间运行的会话里日志会无上限增长，5 MB 这个门槛形同虚设。

**修法**：`Logger::write` 里按写入量做惰性检查（每 N 行或按累计字节），轮转时滚动 `.1` → `.2` → `.3`；
或明确把方案改成"1 个备份"，两边取齐。

---

## 10. P3：`bundle.resources` 在 main 上是死配置，而且是个陷阱

**位置**：`src-tauri/tauri.conf.json:28`

```json
"resources": ["runtime/**/*"]
```

main 上既没有 staging 目标（Makefile 里没有 `runtime-*`），也没有读种子的代码（没有 `runtime.rs`），
所以这条配置的全部作用就是往 `.app` 里塞一个没人用的 `Contents/Resources/runtime/README.md`。

**真正的风险**：payload 目录 `src-tauri/runtime/*` 是 gitignore 的。
开发者切到 `feat/bundled-runtime` 跑过 `make runtime-stage` 再切回 main，那 520 MB 会原地留着
（**审查时这个工作树就是这个状态：`du -sh src-tauri/runtime` = 520M**），
而 main 上 `make bundle` 会把它们**静默打进 `.app`**，没有任何提示 ——
正是自带运行时方案 §5（v3.1 第 4 条）实测过的"staging 残留会被静默打包"。

**修法**：main 上删掉这条 `resources`（等分支合并时再加回来），
或在 `bundle` 目标前加一道闸门：main 上 `src-tauri/runtime/` 除 `README.md` 外必须为空。

---

## 11. P3：其它

- **发布门禁会改工作区**（`.github/workflows/release.yml:109-110`）：`make fmt` + `git diff --exit-code`
  是"先改再查"，应改用 `cargo fmt --check`（或加一个 `make fmt-check` 目标）。
- **Windows 分支代码仍在但不可用**（`src-tauri/src/process.rs:135-145`）：`kill_signal(None) => true`
  （存活探测恒真 ⇒ `terminate` 必然耗满整个 grace 再报失败）、`window.rs:168-180` 的
  `open_external` 用 `cmd /C start` 拼远程 URL。README 已声明只支持 macOS/Linux，
  Makefile 在非 macOS/Linux 上直接 `$(error …)`。建议在 `lib.rs` 顶部加
  `#[cfg(windows)] compile_error!("Windows 支持见 feat/bundled-runtime 分支")`，
  避免有人误建出一个行为不对的包。

---

## 12. 建议的修复顺序

| 阶段 | 范围 | 理由 |
|---|---|---|
| A（半天） | §1 scheme 白名单、§2 自愈身份校验、§3 进程组终止 | 三处都在 20 行以内，都能补单测；前两条是"会伤到用户机器上其它东西"的类别 |
| B | §4 重复安装、§5 workspace 校验与日志 | 都在更新/启动的关键路径上，直接决定"失败时能不能自己定位" |
| C | §9 日志轮转、§10 resources 闸门、§11 | 收尾，与发布流程一起验证 |
| D | §7 / §8 两项交互 | 需要先定产品取舍（实现 or 从方案里划掉），不阻塞前三阶段 |

修 §3 时同步修 `feat/bundled-runtime`（姊妹文档 P0-3），避免两边分叉。

---

## 13. 验证方式

- **单测**：`make test`（当前工作树需先 `cargo clean -p dsh-desktop`，`src-tauri/target/` 里缓存了
  旧路径 `/Users/wangzy/Applications/Scripts/dsh-desktop/…` 的 tauri 构建产物，
  build script 会报 `failed to read plugin permissions`）。
  新增用例建议：`may_open` 的 scheme 白名单、`should_self_heal` 的判定矩阵、
  "安装后版本未变 → 不再重复安装"的缓存行为、`repair()` 对坏 workspace 的回落。
- **实机**：
  - 伪造一份 `state.json`（pid 填一个当前存在的无关进程）→ 启动 → 断言该进程**没有**被终止；
  - 在终端里 `dsh web` 占住 3080 → 触发更新 → 断言只有该 pid 收到 SIGTERM；
  - 在 Harness 页面的控制台执行 `window.open("file:///Applications")` → 断言不会打开访达，
    日志里有 `external scheme blocked`；
  - 把 `config.json` 的 `workspace` 改成不存在的目录 → 错误页应点名该路径。
- **构建**：main 上 `src-tauri/runtime/` 留有 payload 时，`make bundle` 必须报错或产物里不含它。

## 14. 验收标准

> 勾选项为 2026-09-13 修复后的状态；每条都注明覆盖它的单测（矩阵见 §16.3）。
> 需要真实 GUI / 真实进程的断言在最后一行单独列出，本轮未做。

- [x] 非 `http`/`https` 的外链不会被交给系统打开，且有日志（`only_web_schemes_leave_the_shell`）；
- [x] `state.json` 记录的 pid 与端口监听者对不上时，自愈只清文件、不发信号（`self_heal_signals_only_the_pid_that_still_owns_the_port`）；
- [x] 停止外部 Harness（接管与更新两条路径）都只终止该 pid，不触碰它的进程组（`external_instances_are_never_stopped_by_process_group`；接管路径原本就用 `terminate_pid`）；
- [x] "npm 装到别处"的机器上，连续启动两次只会尝试安装一次（`an_ineffective_install_is_not_retried_inside_the_window`）；
- [x] `workspace` 不可用时，错误页与日志都点名具体路径与配置项（`a_bad_workspace_is_repaired_in_memory_only`、`spawn_paths_are_named_in_the_error`）；
- [x] 日志达到 5 MB 后在**运行中**就轮转，且备份份数与方案一致（`rotates_while_running_and_keeps_three_backups`）；
- [x] main 上带残留 staging 时不会产出一个 500 MB 的 `.app`（`bundle.resources` 已删，不再存在打包路径）；
- [ ] 实机断言（伪造 state.json 不误杀、`dsh web` 占端口时只有该 pid 收到 SIGTERM、控制台 `window.open("file:///…")` 被拦、坏 workspace 的错误页文案、带 payload 时 `make bundle` 产物）尚未执行。

---

## 15. 本次未覆盖的范围

- 未做联网测试（`make test-live`）与实机 GUI 验证，全部结论来自代码与方案对照；
- 未审查 `src/index.html`、图标脚本与 README 正文的表述一致性（只核对了与本文相关的几处）；
- macOS 签名/公证（方案 §10 的最后一行，仍为 ❌）本轮无产物可验；
- Linux 侧未在 Linux 机器上实跑（`listener_pid` 的 `ss` 回退分支同样只有单测覆盖）。

---

## 16. 修复记录（2026-09-13）

§1–§11 全部落地，§7/§8 按产品决策走中间/部分实现。新增 14 个单测；
`make fmt-check`、`make clippy`（`-D warnings`）、`make test` 全绿：**43 passed / 0 failed**。

### 16.1 逐项改动

| # | 改动 | 位置 |
|---|---|---|
| 1 | 新增纯函数 `may_open(&Url) -> bool`（只放行 `http`/`https`）；`open_external` 先解析 URL，非白名单 scheme 记 `external scheme blocked: <scheme> (<target>)` 后丢弃，无法解析的记 `external link dropped` | `window.rs` |
| 2 | 新增纯函数 `should_self_heal(&HarnessState, listener, alive)`：仅 `alive && listener == Some(state.pid)` 时发信号，否则记日志只清 state.json | `lib.rs` |
| 3 | 新增 `StopMode`：`ours.is_some()` → 进程组 `terminate`，外部实例 → `terminate_pid`；状态页与日志写明用了哪种 | `lib.rs` |
| 4 | `Cache` 增加 `attempted: Option<String>`（`#[serde(default)]`，旧缓存文件可读）；`Checked.attempted` 在缓存命中且 `attempted == to` 时为真，调用方只记日志不安装；安装后版本未变则 `mark_attempt_ineffective` 回写 | `update.rs`、`lib.rs` |
| 5 | `Config::repair()`：`workspace` 不是"绝对且存在的目录"→ 本次回落到默认值并记日志，**不改写文件**；`dsh_path` 不是绝对且存在的文件 → 本次忽略。`harness::validate_paths` 在 spawn 前点名 node / dsh_js / workspace，`spawn:` 与 `app data dir = … | workspace = …` 各记一行 | `lib.rs`、`harness.rs` |
| 6 | `home_workspace(home)`：`HOME` 缺失或空白 → `temp_dir()` 并记日志，永不回落 `/` | `lib.rs` |
| 7 | `config.json` 新增 `dsh_path`，接到 `locator::locate` 的 `remembered`；找不到 dsh 的错误文案点名该字段 | `lib.rs`、`locator.rs` |
| 8 | `on_page_load` 记录 `Finished`；看护线程没等到且端口不再服务时退避重试同一 URL（最多 3 次 × 20 s），仍失败给状态页 | `window.rs` |
| 9 | 日志在**每次写入前**判定体积，超过 5 MB 滚动 `.1`→`.2`→`.3`；`Logger::open` 改为每个路径一个共享实例，避免多句柄在轮转后继续写被改名的旧文件 | `harness.rs` |
| 10 | 删除 main 的 `bundle.resources`（main 没有 staging 目标与运行时解析，留着只会把残留 payload 静默打进 `.app`） | `tauri.conf.json` |
| 11 | 新增 `make fmt-check`，发布门禁改用它；`lib.rs` 顶部加 `#[cfg(windows)] compile_error!`，`windows-portable.yml` 注释同步 | `Makefile`、`release.yml`、`lib.rs`、`windows-portable.yml` |

### 16.2 产品决策与实现偏差

- **§7（中间方案）**：`config.json` 新增 `dsh_path` 作为"记忆路径"，位于 `DSH_DESKTOP_DSH` 之后、PATH 之前。
  未引入 `tauri-plugin-dialog`，不做"选择 dsh 路径…"按钮；错误页文案与 README 都指向该字段。
- **§8（部分实现）**：实现了"重试同一 token URL"，**未实现**"连续失败 → 重启 harness 取新 URL"
  （需要把 `start()` 的定位/更新/探测/启动拆成可重入流程，风险大于收益）。
  为避免误判，只有重试时刻端口已不再以 Harness 身份应答才重试；端口健康而只是加载事件迟到时只记日志、不动窗口。
- **§9**：按方案 §8 的"5 MB × 3 份"实现，而不是反向把方案改成 1 份。
- **§10**：采用"main 上删掉这条配置"，未在 Makefile 加闸门（分支合法需要 payload，共用 Makefile 无法区分分支）。

### 16.3 新增单元测试

| 文件 | 用例 | 覆盖 |
|---|---|---|
| `window.rs` | `only_web_schemes_leave_the_shell` | https/http 放行；file / smb / 自定义 / javascript / data / mailto 拒绝 |
| `window.rs` | `load_retry_stops_on_a_live_port_or_a_closed_window` | 重试判定矩阵：可重试 / 端口健康不打扰 / 窗口已关 / 次数用尽报错 |
| `lib.rs` | `self_heal_signals_only_the_pid_that_still_owns_the_port` | 自愈身份校验矩阵 |
| `lib.rs` | `external_instances_are_never_stopped_by_process_group` | `stop_mode` 判定 |
| `lib.rs` | `a_missing_home_never_becomes_the_filesystem_root` | `HOME` 缺失/空白 → temp_dir |
| `lib.rs` | `a_bad_workspace_is_repaired_in_memory_only` | 不存在/相对路径回落，且文件不被改写 |
| `lib.rs` | `a_bad_dsh_path_is_ignored_in_memory_only` | 无效 `dsh_path` 忽略、有效则保留 |
| `harness.rs` | `spawn_paths_are_named_in_the_error` | node / dsh_js / workspace 出错时点名，workspace 错误含 `config.json` |
| `harness.rs` | `rotates_while_running_and_keeps_three_backups` | 运行中轮转、只保留 3 份、live 文件有界 |
| `harness.rs` | `an_oversized_file_rotates_before_the_next_line` | 上次遗留的超大文件在下次写入前轮转 |
| `harness.rs` | `open_shares_one_logger_per_path` | 同一路径只一个句柄（轮转后不会写旧文件） |
| `update.rs` | `an_ineffective_install_is_not_retried_inside_the_window` | 同窗口内不重复安装；新版本不误抑制 |
| `update.rs` | `cache_files_without_the_attempt_field_still_parse` | 旧缓存文件（无 `attempted`）向后兼容 |
| `locator.rs` | `the_environment_override_wins_over_the_remembered_path` | `DSH_DESKTOP_DSH` 优先于 `dsh_path`，其后才是 PATH |

### 16.4 验证命令与结果

```text
cargo clean --manifest-path src-tauri/Cargo.toml   # 必须先清：target/ 里缓存着旧工作区路径
make fmt-check   # cargo fmt --check，通过
make clippy      # clippy --all-targets -D warnings，通过
make test        # 43 passed; 0 failed（联网用例在未设 DSH_DESKTOP_LIVE_TESTS 时自动跳过）
make test-live   # 3 passed（registry head=0.1.5-rc.2；冷查询 1179 ms / 缓存命中 0 ms；
                 #  PATH 无 node 时裸 npm exit 127、修复后的路径仍可查到 registry）
```

> §13 提到的旧路径缓存这次确实会挡住构建：`failed to read plugin permissions: … Applications/Scripts/dsh-desktop/…`。
> 只清 `-p dsh-desktop` 不够（报错文件在 `tauri` 的 build 输出里）；本次先清 `tauri` / `tauri-build` /
> `dsh-desktop` 三个包，输出显示连带删掉 4.9 GB（等于全量重建）后才通过。换工作区路径后直接 `cargo clean` 最省事。

---

## 17. 复核结论（2026-09-13，对 `d379428` + `7905ae8`）

独立复跑验证：`cargo test` **43 passed / 0 failed**（另含 3 个联网用例通过）、
`cargo clippy --all-targets -- -D warnings` **0 warning** —— 与 §16.4 的记录一致。

### 17.1 11 项修复的核验结论

| # | 核验 |
|---|---|
| 1 | `may_open` 只放行 http/https；`open_external` 先 `Url::parse` 再判，解析失败也记日志 —— 正确 |
| 2 | `should_self_heal` 判据与测试矩阵正确 —— 正确（副作用见 §17.2 的 N1 / N2） |
| 3 | `stop_mode` 判定正确 —— 正确，但生产路径走不到 `ProcessGroup`（见 N1） |
| 4 | `attempted` 字段带 `#[serde(default)]`、`match` 守卫顺序正确、旧缓存兼容用例到位 —— 正确（残留见 N4） |
| 5 | `repair()` 只改内存不写文件、`validate_paths` 点名配置项、`spawn:` 与 `app data dir` 日志 —— 正确 |
| 6 | `home_workspace` 回落 `temp_dir()` —— 正确 |
| 7 | `dsh_path` 位于 `DSH_DESKTOP_DSH` 之后、PATH 之前，优先级有单测 —— 正确 |
| 8 | 按 §16.2 的取舍部分实现 —— 可用，但重试条件的方向存疑（见 N3） |
| 9 | 运行中判定 + 3 份轮转 —— 正确；**并顺带修掉了首轮审查没发现的真问题**：`init_app_log` 与 `spawn` 原先各开一个文件句柄，轮转后其中一个会继续写被改名的旧文件，`LOGGERS` 注册表按路径共享句柄解决了它 |
| 10 | `resources` 已删，`runtime/README.md` 区分了 main 与分支 —— 正确 |
| 11 | `fmt-check` + `#[cfg(windows)] compile_error!` —— 正确 |

方案文档的三处改动是**加注 + 日期**（"2026-09-13 修正/落地"），保留原决策文字、未改写成相反结论，
符合文档生命周期规则。

### 17.2 新发现（均未修）

| # | 级别 | 问题 | 位置 | 性质 | 处理（2026-09-13） |
|---|---|---|---|---|---|
| N1 | P1 | `ours` 恒为 `None`：复用分支与 `StopMode::ProcessGroup` 都是死代码 | `lib.rs:443` / `315` / `587` | 既有缺陷（首轮漏掉），被 #3 暴露 | **已修**：`SelfHeal` 三分支，能复用的记录保留；`ours` 非空时的重启走进程组 |
| N2 | P2 | 不监听端口的残留不再被清理 | `lib.rs:427-443` | #2 的副作用 | **已修**：`ps -p <pid> -o ppid=,command=` 命中"父进程为 1 且命令行含 `--profile web`/`dsh`"才发信号 |
| N3 | P2 | 重试条件与最可能的故障模式相反 | `window.rs` `load_outcome` | #8 的取舍方向 | **已修**：改以 `PageLoadEvent::Started` 为判据，并保留"端口健康时不报错页"的护栏 |
| N4 | P2 | 缓存窗口过期后仍会重试一次无效安装 | `update.rs::check_cached` | #4 的残留 | **已修**：`carried_attempt` 跨窗口保留标记，只有新版本才重开 |
| N5 | P3 | 日志每写一行多一次 `metadata()` syscall | `harness.rs::rotate_if_oversized` | #9 的代价 | **已修**：`Logger.written` 字节计数器，仅 open 时 seed 一次 stat |
| N6 | P3 | 两处错误页文案"关闭本窗口即退出应用"与实际不符 | `window.rs:24-30` 与两条 `show_failure` | 既有 + 新增同款 | **已修**：`show_failure` 先 `destroy()` Harness 窗口，文案自洽 |

#### N1（P1）：`ours` 恒为 `None`，"复用上次实例"是死代码

`start()` 步骤 1 在自愈之后**无条件** `process::clear_state(data_dir)`（`lib.rs:443`），
而两处"是不是自己上次启动的实例"的判断都靠 `read_state`：

- `stop_instance_before_update`（`lib.rs:315`）
- 探测/接管分支（`lib.rs:587`）

状态文件在同一次启动的前几十毫秒已经被删掉，所以两处的 `ours` 恒为 `None`。后果：

1. README 的「端口上是本应用上次启动的实例（state.json 对得上且存活）→ 直接复用」与方案 §13.1/§13.2
   描述的**复用分支永远不会执行**；`lib.rs:599` 的 `adopt()` 是死代码；每次崩溃后重启都必然重新拉起
   harness，而不是复用仍然健康的实例；
2. 本次新增的 `stop_mode(ours.is_some())` 因此恒返回 `PidOnly` —— `ProcessGroup` 分支只有单测覆盖，
   等于 #3 的修复在生产里只跑了一半（好在方向是安全的那一半）。

`git log -S "process::clear_state(data_dir);"` 显示这个无条件清理从初始提交 `88820f0` 就在，
即方案 §13.2 那句"下次启动走复用分支"是**推断而非实测**。

**修法**：步骤 1 区分三种情况，而不是一律删文件 ——

- 记录失效（pid 已死、或 pid 不再拥有记录的端口）→ 只删文件；
- 记录有效且**发了信号**（自愈成功）→ 删文件；
- 记录有效、仍在服务、且端口就是本次要用的端口 → **保留文件**，跳过终止，交给 §3c 的复用分支决定
  （复用 or 因 `just_updated` 重启）。

**回归**：把上面三分支抽成纯函数（例如 `self_heal_action(state, listener, alive, port) -> Action`），
补一个覆盖"有效且在服务 ⇒ Keep"的用例；并加一条断言确保 `stop_mode` 在该路径下能返回 `ProcessGroup`。

#### N2（P2）：不监听端口的残留不再被清理

新判据要求 `listener == Some(state.pid)`。一个卡在启动中、尚未 `bind` 端口的残留 harness 现在：
自愈不杀它（不是监听者），后续探测也看不见它（`probe` = `Closed` ⇒ 直接新起一个），于是永久成为孤儿。
窗口很小（harness 很早 bind），但这是 #2 换来的确定性代价。

**修法**：给自愈加一条次要判据 —— 进程命令行含 `--profile web` 且父进程已消失（macOS/Linux 可用
`ps -p <pid> -o ppid=,command=`），命中则同样按"我们的残留"处理。

#### N3（P2）：重试条件与最可能的故障模式相反

`load_outcome` 只在"端口已不再以 Harness 身份应答"时重试。但本文 §8 描述的空白窗口场景
（WebView 初始化抖动、首次导航失败）通常伴随**端口是健康的** —— 这时代码走 `Settled`，
空白窗口原样留着，等于没覆盖它要解决的那个场景。反过来，端口真的死了时重新导航同一个 URL 也不会成功，
而且 `watch_harness` 会同时弹出"Harness 已退出"，两个线程抢同一个状态页（后写的覆盖先写的）。

**修法**：改用 `PageLoadEvent::Started` 作为区分信号 ——

- 20 s 内连 `Started` 都没到 ⇒ 导航根本没开始，无论端口是否健康都值得重新 `navigate`；
- `Started` 到了但 `Finished` 没到 ⇒ 确实在加载中，保持现在的"不打扰"；
- 端口不服务且 `watch_harness` 已经报过错 ⇒ 不重复弹状态页（两条路径共用一个"已报告"标志）。

#### N4（P2）：缓存窗口过期后仍会重试一次无效安装

`check_cached` 在走网络查询时把 `attempted` 重置为 `None`（"新查询开新窗口"），
所以 npm 布局不对的机器每隔 `update_check_interval_minutes`（默认 60 分钟）仍会完整来一遍：
停掉正在跑的 harness → 重建约 289 MB 依赖树 → 发现版本没变。

**修法**：新查询时若 `installed` 与 `latest` 都与旧条目相同，则**保留** `attempted`；
只有 `latest` 变化（真的有新版本）才清空。

#### N5（P3）：日志每写一行多一次 syscall

`rotate_if_oversized` 在**每次 `write` 前**都 `std::fs::metadata(&self.path)`，
把方案 §13.6 认可的"每行一次 syscall"变成两次；harness 流式输出时是双倍。

**修法**：在 `Logger` 里累计已写字节数，只有计数越过 `limit` 时才 `metadata()` 复核并轮转
（进程重启后由首次写入时的一次 stat 兜底，`an_oversized_file_rotates_before_the_next_line` 用例继续有效）。

#### N6（P3）：错误页文案与实际行为不符

`create_splash` 的 `CloseRequested` 只在 **HARNESS 窗口不存在**时 `exit(0)`（`window.rs:24-30`），
而两条会弹状态页的失败路径触发时，harness 窗口都还在：

- `lib.rs::watch_harness`（Harness 意外退出，既有）；
- `window.rs::watch_first_load` 的 `LoadOutcome::Report`（本次新增，沿用了同样的文案）。

两处都写着"关闭本窗口即退出应用"，实际关掉状态页什么也不会发生，用户面对的仍是那个死窗口。

**修法**：这两条路径在 `show_failure` 之前 `destroy()` harness 窗口（此时页面已无意义），
文案即自洽；或给状态页加一个"失败态"标志，处于该状态时关闭即 `exit(0)`。

### 17.3 顺带（非缺陷）

- `pnpm-lock.yaml` 有 101 行**未提交**改动：新版 pnpm 把 `packageManager` 自举成
  `packageManagerDependencies` + `@pnpm/exe.*` 写进了锁文件。不影响 CI（发布门禁已不再 `git diff --exit-code`），
  但每次本地 `pnpm install` 都会重现 —— 建议一次性提交或明确决定忽略。
- `mailto:` 目前也被 `may_open` 拦下（只记日志）。方案 §7 原话只提 https，所以实现合规；
  但 agent 输出里的邮件链接会静默失效，值得确认是否要放行。

### 17.4 建议顺序

1. **N1** —— 它同时让 #3 的修复在生产里真正生效，且恢复 README/方案承诺的复用行为；
2. **N6 + N3** —— 一起改：失败路径的窗口归属与文案是同一件事；
3. **N4 / N2 / N5** —— 收尾，各自独立。

> 上述顺序已执行完毕，记录见 §17.5。
