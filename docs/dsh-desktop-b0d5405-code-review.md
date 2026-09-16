# dsh-desktop 代码审查报告（b0d5405）

审查对象：<https://github.com/tanuki-cat/dsh-desktop>  
审查分支：`main`  
审查提交：`b0d5405`（`v0.4.3` 之后第 3 个提交，已推送到 `origin/main`）  
审查日期：2026-09-16  
上一轮审查：[`dsh-desktop-latest-code-review.md`](./dsh-desktop-latest-code-review.md)（针对 `44a347e`，其 5 项已在 `b0d5405` 修复）

本文是**审查结论 + 修复任务清单**。修复落地后，把结果回写到文末的「处理状态」，不要改写各条的原始结论。

## 1. 总体结论

审查范围包括 `src-tauri/src` 下的全部非测试代码、`src/index.html`、`scripts/` 和 `.github/workflows/`。上一轮已列出并修复的问题这里不再重复。

共发现 **3 个 P1、10 个 P2**（其中第 13 项需要实机确认），另有 **11 个 P3**。

- **P1-1、P1-2 是安全问题。**
  - P1-1：日志脱敏会漏掉同一行里的第二个凭据；行里有中文时还会 panic，进而让退出流程留下孤儿进程。
  - P1-2：Windows 下打开外链时存在命令注入。
- **P1-3 会让打包版在第一次核心更新失败后无法自行恢复**，之后每次启动都失败，直到用户手动删除文件。
- **P2 主要有三类：**
  - 外部进程的时间预算仍有漏洞；
  - 更新流程提前停掉实例，失败后会反复重试；
  - 符号链接和登录 shell 环境等边界情况没有处理。

## 2. 审查方法与限制

- 逐文件静态阅读，并对关键路径做了调用链追踪。
- **已复现**的条目，是把源码片段原样复制到临时目录，用 `rustc 1.98.1` 单独编译运行；或者用 `node`、`pnpm` 以及本机真实的 `~/.dsh/profiles/web` 数据验证。每条都注明了复现方式和输入，便于重跑。
- **推断**的条目会明确标注推断依据。
- 本轮**没有**运行 `cargo test`、`cargo clippy`，也没有做 Windows 实机验证。仓库声明的测试规模（155 个单测 + 6 个集成用例）来自上一轮文档，不是本轮的独立结果。

## 3. 问题汇总

| # | 优先级 | 问题 | 验证 |
|---|---|---|---|
| 1 | P1 | 日志脱敏下标错位：同一行第二个凭据不被脱敏，非 ASCII 行 panic | 已复现 |
| 2 | P1 | Windows `cmd /C start` 打开外链，URL 可注入命令 | 推断（Rust 文档 + URL 序列化已验证） |
| 3 | P1 | 影子前缀第一次更新失败后无法回滚，此后每次启动都失败 | 代码追踪 + 现有测试佐证 |
| 4 | P2 | 兼容层 `Uint8Array.fromBase64` 的正则会删除字母 `s` | 已复现 |
| 5 | P2 | `stdout_within` 等函数在子进程退出后无限期 join，时间预算失效 | 已复现 |
| 6 | P2 | `lsof` / `ss` / `netstat` 没有超时 | 代码阅读 |
| 7 | P2 | 核心更新在下载前就停掉了正在运行的实例 | 代码追踪 |
| 8 | P2 | 核心更新失败后没有失败标记，每次启动都重复下载、超时、回滚 | 代码追踪 |
| 9 | P2 | 复用的旧实例没有进程看护 | 代码追踪 |
| 10 | P2 | profile 快照 / 模板复制不支持符号链接 | 已复现（本机真实 profile） |
| 11 | P2 | 定位 node/dsh/npm 用 `-lc`，导入环境用 `-lic`，结果不一致 | 推断（zsh 启动文件规则） |
| 12 | P2 | `stage-runtime.sh` 把 pnpm store 生成到 profile 模板里 | 已验证（`pnpm store path`） |
| 13 | P2 | `present_status_page` 先销毁窗口再重建，可能触发退出 | **待实机确认**（依据 tauri-runtime-wry 源码） |
| 14 | P3 | 登录 shell 输出含非 UTF-8 字节时，整次环境导入为空 | 已复现（std 行为） |
| 15 | P3 | `PluginSkip::NotDeclared` 永远不会被记录，文案也有误 | 代码阅读 |
| 16 | P3 | 重启路径里，用户选了“取消”后会被再问一次 | 代码追踪 |
| 17 | P3 | 渲染进程崩溃恢复的兜底 URL 没带端口 | 代码阅读 |
| 18 | P3 | `open_external` 不回收子进程，积累僵尸进程 | 代码阅读 |
| 19 | P3 | Windows 下多个子进程没设 `CREATE_NO_WINDOW`；`netstat` 状态文字被本地化 | 代码阅读 |
| 20 | P3 | `parse_dsh_url` 兜底会接受任意回环 URL，且不校验端口 | 代码阅读 |
| 21 | P3 | `terminate` 在组长已退出时不清理同组残留进程 | 代码阅读 |
| 22 | P3 | `Cache::is_fresh` 的乘法可能溢出 | 代码阅读 |
| 23 | P3 | `Math.sumPrecise` 兼容实现遇 `Infinity` 返回 `NaN`；`Iterator#reduce` 下标少 1 | 前者已复现 |
| 24 | P3 | `SystemUpdates` 的 `#[default]` 和注释与实际默认值矛盾 | 代码阅读 |

## 4. P1 详细发现

### 4.1 P1：日志脱敏下标错位（#1）

相关代码：

- [`harness.rs#L141-L168`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/harness.rs#L141-L168)（`redact_bearer`）
- [`harness.rs#L179-L217`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/harness.rs#L179-L217)（`redact_field`）
- [`harness.rs#L720-L724`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/harness.rs#L720-L724)（`app_log`）
- [`lib.rs#L432-L441`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L432-L441)（`shutdown`）

**问题。** 两个函数都在循环里把 `rest` / `search` 不断往后切，`at` 是相对当前 `search` 的偏移。但判断“关键字是否位于单词开头”时写的是：

```rust
let starts_a_word = at == 0
    || !line[..at]            // ← 从原始行开头取，而不是从 rest 开头取
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric());
```

从第二次匹配开始，检查的字符就错位了。这会造成两种后果：

1. **漏脱敏。** 错位处的字符恰好是字母或数字时，真正的第二个凭据会被当成“不是单词开头”而跳过。
2. **panic。** 原始行在 `at` 处不是字符边界（行首有中文等多字节字符）时，`line[..at]` 直接 panic。而且 `starts_a_word` 在判断分隔符之前就计算了，所以任何第二次匹配都可能触发。

**复现（已执行）：** 把 `harness.rs` 第 92–218 行原样编译后调用 `redact`。

| 输入 | 输出 |
|---|---|
| `token=aaa token=bbb` | `token=*** token=bbb` |
| `DEEPSEEK_API_KEY=sk-first OPENAI_API_KEY=sk-second` | `DEEPSEEK_API_KEY=*** OPENAI_API_KEY=sk-second` |
| `x Bearer aaa, y Bearer bbb` | `x ***, y Bearer bbb` |
| `错误：token 无效，请检查 token=xxx` | panic：`end byte index 20 is not a char boundary; it is inside '效'` |
| `中tokenXYtoken=1` | panic：`end byte index 2 is not a char boundary; it is inside '中'` |
| `Authorization: Basic dXNlcjpwYXNz` | `Authorization: *** dXNlcjpwYXNz`（附带问题：只有 `Bearer` 会整体脱敏，`Basic` / `Digest` 等方案的凭据会残留） |

**影响。**

- **凭据泄露：** 环境变量转储、回显请求头的 provider 错误里，同一行常出现多个凭据，第二个及之后的会原样写进 `harness.log`，也会出现在失败页上。
- **panic 发生在 `forward_lines` 读取线程：** 线程退出，`ChildStdout` 被 drop，管道读端关闭。dsh 之后写 stdout 会得到 EPIPE，Node 上可能表现为未处理的 `error` 事件，导致进程退出。启动阶段如果两个读取线程都死掉，`wait_for_url` 会报“进程已退出”。
- **panic 发生在 `app_log`：** 此时正持有 `APP_LOGGER` 锁，锁会被毒化，之后**每一次** `app_log` 都会 panic。`shutdown()` 恰好是先调 `app_log("stopping Harness …")`、再 `terminate`，panic 发生在事件循环回调里，所以退出时 Harness 不会被停掉，而且应用可能直接 abort。
- dsh 面向中文用户，行首是中文的日志很常见，因此这类输入很现实。

**修复建议。**

- 用绝对偏移：`rest` 始终是 `line` 的后缀，所以

  ```rust
  let absolute = line.len() - rest.len() + at;
  let starts_a_word = absolute == 0 || !line[..absolute].chars().next_back().is_some_and(char::is_alphanumeric);
  ```

  `search` 是 `rest` 的 ASCII 小写，两者字节结构相同，`absolute` 一定是字符边界。
- 两个函数都要改。
- 顺带建议：
  - `redact_bearer` 扩展到 `Basic`、`Digest`、`Token` 等常见方案；
  - `app_log` 先在锁外完成 `redact`，再加锁写入；
  - `APP_LOGGER.lock()` 改为容忍毒化（`unwrap_or_else(PoisonError::into_inner)`），避免一次 panic 让所有日志和退出流程连锁失败。

**验收。**

- 新增单测覆盖：同一关键字出现两次、`Bearer` 出现两次、行首为中文或 emoji 且后面有多个匹配。上表的输入全部脱敏且不 panic。
- 模糊测试（可选）：任意 UTF-8 输入下 `redact` 都不 panic。

### 4.2 P1：Windows 打开外链存在命令注入（#2）

相关代码：[`window.rs#L1651-L1678`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L1651-L1678)

```rust
#[cfg(target_os = "windows")]
let program = "cmd";
...
command.args(["/C", "start", ""]);
let _ = command.arg(url.as_str()).spawn();
```

**问题。**

- Rust 的 `Command::arg` 在 Windows 上只处理 MSVC 约定的引号，**不会**替 `cmd.exe` 转义 `&`、`|`、`^`、`%` 等元字符。Rust 标准库文档明确提醒，直接调用 `cmd.exe` 时参数不安全。
- 另一方面，WHATWG URL 序列化不会编码 path/query 里的 `&`、`|`、`%`。用 `node` 验证：`https://evil.example/a&calc.exe`、`https://evil.example/x?q=1&y=%USERPROFILE%|whoami` 都原样保留（url crate 实现的是同一规范）。
- 这些 URL 来自 `on_navigation` / `on_new_window`，也就是模型输出和抓取网页里的链接，属于攻击者可控输入；`may_open` 只检查了协议头。

**影响。** Windows 用户点击一个构造过的链接，就可能执行任意命令，或把环境变量（`%USERPROFILE%` 等）拼进 URL 发给外部站点。Windows 是受支持的目标平台（便携版）。

**修复建议。**

- 不经过 `cmd.exe`，任选其一：
  - 参照 `process.rs` 里现有 `win` 模块的最小绑定风格，直接 FFI 调用 `shell32!ShellExecuteW(NULL, "open", url, NULL, NULL, SW_SHOWNORMAL)`；
  - 引入 `tauri-plugin-opener`；
  - 调用 `explorer.exe <url>` 或 `rundll32 url.dll,FileProtocolHandler <url>`（不经过 cmd 解析，但行为不如 `ShellExecuteW` 可控）。
- 同时给这个子进程加上 `CREATE_NO_WINDOW`（参见 #19）。

**验收。**

- Windows 实机点击 `https://example.com/a&calc.exe`，浏览器打开该地址，不启动计算器。
- 代码层面，Windows 分支不再出现 `cmd`。

### 4.3 P1：影子前缀第一次更新失败后无法回滚（#3）

相关代码：

- [`transaction.rs#L161-L187`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/transaction.rs#L161-L187)（`commit`：`had_previous == false` 时不产生备份）
- [`transaction.rs#L194-L212`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/transaction.rs#L194-L212)（`rollback`：备份不存在时返回 `Err`）
- [`update_flow.rs#L252-L303`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/update_flow.rs#L252-L303)（先写 swap 记录，再 `commit`；回滚失败则保留记录）
- [`update_flow.rs#L343-L366`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/update_flow.rs#L343-L366)（`recover_pending_swap`）
- [`runtime.rs#L227-L258`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/runtime.rs#L227-L258)（`pick_bundled`：影子版本更高时总是选影子）

**问题链。**

1. 打包版第一次做核心更新时，运行的是 app 内只读的种子，更新目标是 `app-data/runtime/prefix`，这时里面还没有 CLI。
2. `commit_core_update` 先写 `update-swap.json`，其中 `backup` 指向 `runtime/rollback/<to>`；`commit` 发现目标不存在，`had_previous = false`，于是**没有任何东西被移到 backup**。
3. 新树没能打印启动 URL（崩溃或超时）时，`roll_back_core_update` 调用 `rollback`，因“回滚目录不存在”失败（现有测试 `a_rollback_puts_the_previous_tree_back_and_keeps_the_failed_one` 也断言了这一点）。结果是记录保留、坏树留在原地。
4. 下一次启动：
   - `recover_pending_swap` 同样失败，记录继续保留；
   - `resolve_runtime` 里影子版本高于种子，`pick_bundled` 选中这棵坏树；
   - `update-check.json` 的 `installed` 已经变成新版本，检查结果是 `UpToDate`，不会触发重装；
   - 于是启动失败。此后每次启动都是这样。

**影响。** 打包版（最常见的发行形态）第一次核心更新如果坏了，用户就无法自行恢复，只能手动删除 `runtime/prefix` 和 `update-swap.json`。更新事务存在的意义恰恰是应对这种情况。

**修复建议。**

- `SwapRecord` 增加 `#[serde(default)] had_previous: Option<bool>`，由 `commit_core_update` 根据目标目录是否存在来填写。
- `rollback` 或其调用方遇到 `had_previous == Some(false)` 时，把目标目录移到 `.failed`（或直接删除），然后视为回滚成功、清掉记录。这样影子目录中不再有 CLI，种子重新生效。
- 旧记录（字段缺失，即 `None`）保持现有行为。系统前缀的更新一定有旧树，不应在备份缺失时删掉用户的安装。
- 回滚成功后配合 #8 写入失败标记，避免下一次启动立刻重装同一版本。

**验收。**

- 单测：`commit` 到不存在的目标，随后 `rollback`（记录 `had_previous = false`）成功，目标目录不再存在，swap 记录被清除。
- 单测：`runtime::decide` 在影子目录缺失时选种子（已有同类用例，可复用）。
- 手工：伪造一个起不来的影子版本，启动一次看到回滚，第二次启动正常进入种子版本。

## 5. P2 详细发现

### 5.1 兼容层 `Uint8Array.fromBase64` 删除字母 s（#4）

相关代码：[`window.rs#L730-L743`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L730-L743)

```js
var normalized = String(value).replace(/s/g, "").replace(/-/g, "+").replace(/_/g, "/");
```

这里本意是去掉空白（`/\s/g`），实际写成了 `/s/g`，会删除所有小写字母 `s`。原始字节已确认是 `/s/g`，Rust 原始字符串不会吞掉反斜杠。

**复现（已执行，node）：**

- `aGVsbG8=`（即 "hello"）→ 规范化为 `aGVbG8=` → `atob` 抛 `InvalidCharacterError`；
- 删除后长度仍合法时，则**静默返回错误字节**。

**影响。** 只在装了兼容层的旧 WebKit（Safari < 18.2）上生效。一旦生效，所有走 `Uint8Array.fromBase64` 的功能（附件、PDF 等二进制数据）都会报错或数据损坏。

**修复。** 改为 `/\s/g`。`window/tests.rs` 已经有兼容脚本相关测试，建议补一个用例：用 JS 引擎，或至少用字符串断言，确保脚本里不含 `replace(/s/g`。

### 5.2 子进程退出后无限期 join，时间预算失效（#5）

相关代码：

- [`process.rs#L74-L88`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/process.rs#L74-L88)（`stdout_within`：`Ok(Some(_)) => reader.join()`）
- [`shellenv.rs#L123-L141`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/shellenv.rs#L123-L141)（`capture`：同样在退出后 `reader.join()`）
- [`locator.rs#L80-L111`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/locator.rs#L80-L111)、[`locator.rs#L352-L369`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/locator.rs#L352-L369)

**问题。** 这些函数只在超时分支里 drop 读取线程，注释也写明了孙进程会一直持有管道。但在子进程**已退出**的分支里，它们无条件 `join`。登录 shell 很快退出，而 rc 文件拉起的后台进程（agent、daemon、`cmd &!` 等）继承了 stdout，管道就不会关闭，`join` 会一直等到那个后台进程退出。

**复现（已执行）：** 把 `stdout_within` 原样编译后运行：

```rust
stdout_within(Command::new("/bin/sh").args(["-c", "sleep 4 & echo /usr/local/bin/npm"]), Duration::from_secs(1))
// 输出：budget 1s, returned Some("/usr/local/bin/npm") after 4.022179541s
```

把 `sleep 4` 换成常驻进程，就会**永久**卡住。

**影响。** 调用方全部在启动线程上：`command -v npm/node/dsh`、`npm prefix -g`、`ps` 身份检查、登录 shell 环境导入。应用会停在 splash 页，没有任何超时兜底。

**修复建议。** 参照 `update.rs` 里的 `drain` / `collected`：读取线程通过 channel 回传结果，调用方在子进程退出后用**剩余 deadline**（再加一个短暂宽限）做 `recv_timeout`，超时就按已读内容或 `None` 处理。建议抽出一个通用的 `output_within(command, timeout, limit) -> Option<(ExitStatus, Vec<u8>)>`，让 `stdout_within`、`shellenv::capture`、`probe_node_within`、`probe_version_line` 以及 #6 共用。读取时同样限制字节数。

**验收。** 单测：`sh -c 'sleep 30 & echo x'` 在 1 秒预算下，约 1 秒内返回（`Some("x")` 或 `None` 都可以，但必须按时返回）。Unix 平台的用例用 `#[cfg(unix)]` 守卫。

### 5.3 `lsof` / `ss` / `netstat` 没有超时（#6）

相关代码：[`harness.rs#L424-L466`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/harness.rs#L424-L466)

`listener_pid` 的三个实现都直接调用 `.output()`。`b0d5405` 给 `ps` 等调用加了预算，但漏了这里。`lsof` 会遍历所有进程的文件描述符，遇到无响应的网络文件系统时会阻塞（不带 `-b` 时尤其明显）。

调用点都在启动或恢复路径上：自愈（`start` 第 1 步）、端口检测（3c）、更新前停止实例、`wait_for_handoff` 轮询，以及看门狗恢复。

**修复。**

- `lsof -t` 和 `ss -H` 只需要第一行，可以直接改用 `stdout_within`（在 #5 修复之后）。
- `netstat -ano` 需要完整输出，改用 #5 提出的通用函数。
- 可以考虑给 `lsof` 加上 `-b`，避免阻塞在内核调用上。

### 5.4 核心更新在下载前就停掉了正在运行的实例（#7）

相关代码：[`lib.rs#L1551-L1599`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1551-L1599)

**问题。** 现在的顺序是 `stop_instance_before_update` → `stage_core_update`（最长 300 秒的 `npm install`）。但暂存安装写的是 `runtime/staging/...`，**不碰**正在使用的那棵树；真正替换发生在 3d 的 `commit_core_update`。而且 3c 的端口检测在 `staged_update.is_some()` 时本来就会终止自己的实例（`lib.rs#L1808-L1814`）。`lib.rs#L1554` 的注释（“npm rewrites the CLI tree in place”）是暂存机制出现之前的说法。

**影响。**

- 本来可以复用的会话，一检测到新版本就被杀掉。
- 暂存失败（断网、registry 异常）时，实例被白白停掉，启动流程只能重新拉起一个。
- 用户在没有 Harness 的状态下等待下载。
- 对打包版的第一次更新来说，运行中的种子根本不会被触碰，停机完全没有必要。

**修复建议。** 核心更新去掉暂存前的停止步骤，改由 3c 和 3d 负责。需要保留一条约束：**如果 3c 里用户选择保留外部实例**（`UseBrowser` / `UseOtherPort`），而这个外部实例恰好运行在 `target` 这棵树上（系统安装且 `system_updates=install` 的情况），3d 就不能提交，应放弃这次暂存。否则 Windows 上 rename 会失败，Unix 上运行中的实例会在下一次延迟 `require()` 时崩溃。插件更新（pnpm 原地改写 profile）仍然需要先停实例。

**验收。** 暂存失败时，日志里没有“stopped Harness pid … before updating”，复用分支照常生效。暂存成功时，只在 3c/3d 停一次实例。

### 5.5 核心更新失败后反复重试（#8）

相关代码：

- [`lib.rs#L1490-L1515`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1490-L1515)（只看 `attempted`，不看 `failed_recently`）
- [`lib.rs#L1976-L2007`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1976-L2007)（回滚后没有任何标记）
- [`update.rs#L942-L957`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/update.rs#L942-L957)（只有插件有 `mark_*_failed`）

**问题。** 新版本起不来而被回滚后，`update-check.json` 里仍然是 `installed = from`、`latest = to`，没有失败标记。于是下一次启动（缓存窗口内直接读缓存，窗口外重新查询）还会得到 `UpdateAvailable`，再次下载约 290 MB、再次提交、再次等待超时、再次回滚。每次启动都是这样。

另外，刚切换上的新树首次启动用的是 `STARTUP_TIMEOUT_NEXT`（30 秒）。新版本首次启动可能要做迁移或编译缓存，容易被误判为失败。`first_launch` 只反映 profile 是否存在，与这个问题无关。

**修复建议。**

- 为核心更新增加 `mark_attempt_failed(data_dir, to)`（复用 `mark_failed_in`）。暂存失败、提交失败、启动失败（回滚）时都写入这个标记；`lib.rs` 的核心分支像插件分支一样检查 `failed_recently`。
- 如果启动失败属于确定性问题，可以考虑对同一版本使用更长的退避时间（例如失败两次后，只有等 registry 发布新版本才再试）。
- `core_swapped` 为真时使用 `STARTUP_TIMEOUT_FIRST`。

### 5.6 复用的旧实例没有进程看护（#9）

相关代码：[`lib.rs#L1780-L1806`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1780-L1806)、[`lib.rs#L2060-L2067`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L2060-L2067)

复用分支只调用了 `adopt` 和 `create_harness` 就返回；`watch_harness` 只在自己 spawn 的分支里启动（它需要 `Child::wait`）。复用的实例退出后：

- 自动恢复不会触发；
- 页面看护只检查页面是否在绘制，服务端死了页面仍然在画，所以也察觉不到；
- 窗口停留在一个再也连不上的页面，这正是 v0.4.0 自动恢复功能要解决的问题。

**修复建议。** 为复用的 pid 启动一个轮询看护：每隔几秒 `process::is_alive(pid)`，发现死亡后走与 `watch_harness` 相同的 `exit_action` 流程，拿不到退出码时按 `clean = false` 处理。可以把 `watch_harness` 里“等待结束”的部分抽象成参数，两种情况共用后续逻辑。

### 5.7 profile 快照和模板复制不支持符号链接（#10）

相关代码：

- [`transaction.rs#L303-L325`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/transaction.rs#L303-L325)、[`transaction.rs#L331-L382`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/transaction.rs#L331-L382)
- [`lib.rs#L898-L910`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L898-L910)（`copy_tree_into`，模板播种）
- [`update_flow.rs#L62-L78`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/update_flow.rs#L62-L78)（先停实例，再快照）

**问题。** `DirEntry::file_type()` 不跟随符号链接，所以链接会进入 `fs::copy` 分支，而 `fs::copy` 会跟随链接：

- 链接指向**目录**时，直接报错；
- 链接指向**文件**时，会被复制成普通文件。`node_modules/.bin/*` 被拷成普通文件后，其中按相对路径 `require` 的模块会解析到 `.bin/` 下，因而失效。

**复现（已执行）。**

- 本机真实 profile `~/.dsh/profiles/web` 里有 **163** 个符号链接，均位于 `.dsh-module-fallback/node_modules/` 下，指向 `node_modules/<pkg>` 目录（绝对路径）。
- 用 `transaction::copy_tree` 原样复制一个“目录 + 指向它的链接”，得到 `InvalidInput: the source path is neither a regular file nor a symlink to a regular file`。
- `node_modules/.bin/` 下的条目也都是指向文件的链接（如 `csv2json -> ../d3-dsv/bin/dsv2json.js`）。

**影响（开启 `auto_update_plugins` 时）。** `update_market_plugin` 先停掉实例，然后快照失败、直接返回，并且**不写失败标记**。于是只要有新版本的 dshmarket，每次启动都会先杀掉可复用的实例，再更新失败，插件市场永远无法自动更新。`move_path` 跨卷时的回退路径和模板播种也用了同样的复制逻辑。

另外，快照是整个 `node_modules` 的完整副本（本机约 349 MB），并且保留两代。在不支持 clonefile 的文件系统（如 Windows NTFS）上，这会带来明显的耗时和空间开销。

**修复建议。**

- 复制时先判断 `file_type().is_symlink()`：读取 `read_link` 后原样重建链接（Unix 用 `std::os::unix::fs::symlink`；Windows 创建链接需要权限，可以用目录联接或退回到复制目标内容，并记录日志）。恢复时同样按链接处理。
- 把快照移到 `stop_instance_before_update` 之前（快照本来就跳过了 `data` 和 `.dsh-market` 这类实时写入的目录）；快照失败时写入 `mark_plugin_attempt_failed`。
- 可选：评估是否只快照 `package.json`、lockfile 和 `node_modules` 的元数据，或者在 macOS 上显式使用 clonefile。

**验收。** 单测：包含目录链接和文件链接的树，经过快照再恢复后，链接仍然是链接，且指向不变。在本机真实 profile 上快照成功。

### 5.8 登录 shell 查找与环境导入不一致（#11）

相关代码：

- [`locator.rs#L489-L509`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/locator.rs#L489-L509)、[`update.rs#L534-L552`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/update.rs#L534-L552)：`$SHELL -lc 'command -v …'`
- [`shellenv.rs#L95-L105`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/shellenv.rs#L95-L105)：`$SHELL -lic env`（优先）
- [`locator.rs#L373-L402`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/locator.rs#L373-L402)：`find_launcher` 对每个候选都预先调用 `find_node_without_env`

**问题（推断）。**

- zsh 的 `-l`（非交互）只读 `.zshenv`、`.zprofile`、`.zlogin`，不读 `.zshrc`。而 nvm、fnm 等工具的安装脚本默认把初始化写进 `.zshrc`。这些用户 `-lc 'command -v node/dsh'` 查不到结果，但 `-lic env` 导入的 PATH 里有，子进程最终拿到的 PATH 里也有。本机 node 来自 Homebrew（写在 `.zprofile`），因此没能直接复现。
- 后果：
  - 非打包版会报“找不到 dsh”；
  - 打包版会悄悄忽略用户自己的安装，改用自带版本。

**性能。** 从 Finder 启动的应用，PATH 里通常没有 node，所以 `find_launcher` 会对**每个**候选都起一次登录 shell 查 node，即使这个候选随后因 manifest 已经确认、根本用不到这个 node。再加上 `system_node()`、`login_shell_lookup("dsh")` 和 `login_shell_npm()`，一次启动最多会串行起 N+3 个登录 shell。装了 nvm 时每个要 0.3–1 秒。

**修复建议。**

- 解析运行时之前先 join 已经在并行采集的 `-lic` 环境（它有 8 秒预算，正常约 160 ms），用导入的 PATH 做 `path_lookup`；`-lc` 查找只作为最后兜底。
- `find_launcher` 只在 `judge` 真正需要 `--version` 探测时才去查 node（把 node 查找移进闭包，做成惰性的）。

### 5.9 `stage-runtime.sh` 把 pnpm store 生成到了模板里（#12）

相关代码：[`scripts/stage-runtime.sh#L95-L97`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/scripts/stage-runtime.sh#L95-L97)；对照 [`Makefile#L266-L268`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/Makefile#L266-L268)

```sh
PNPM_STORE_DIR="$cache/pnpm-store" sh "$here/make-profile-template.sh" \
  "$out/profile-template" "$market_version"
```

**问题。**

1. `cache` 默认是相对路径 `.runtime-cache`，而 `make-profile-template.sh` 会先 `cd "$dest"` 再执行 `pnpm install --store-dir …`，pnpm 相对**项目目录**解析这个路径。
   - 已验证：在临时项目里执行 `pnpm store path --store-dir .runtime-cache/pnpm-store`，输出为 `<项目目录>/.runtime-cache/pnpm-store/v11`。
   - 因此 store 会落进 `src-tauri/runtime/profile-template/.runtime-cache/pnpm-store`：既被打进 Windows 便携包，首次启动播种时又会被复制进用户的 `~/.dsh/profiles/web`；CI 缓存的 `.runtime-cache/pnpm-store` 也始终是空的。
   - Makefile 用了 `$(abspath …)`，所以 macOS 发行包不受影响。
   - `windows-portable.yml` 调用 `stage-runtime.sh` 时没有设置 `CACHE_DIR`，因此受影响。
2. 没有传第 3 个参数（刚安装好的 `tools/bin`），模板用的是系统 pnpm。CI 里两者版本恰好都是 12.3.4，本地构建则可能不一致。
3. 这一步也没有像 Makefile 那样把自带 node 放到 PATH 前面。

**修复。**

- 使用绝对路径 `PNPM_STORE_DIR="$(cd "$cache" && pwd)/pnpm-store"`。
- 传入 tools 目录：Unix 用 `$out/tools/bin`；Windows 上 npm 把 shim 放在前缀根目录，传 `$out/tools`，并确认 `make-profile-template.sh` 的 `-x "$tools_bin/pnpm"` 在 Git Bash 下成立。
- `check-runtime-stage.sh` 增加断言：`profile-template` 下不得出现 `.runtime-cache` 或 `pnpm-store`。

**验收。** Windows 便携包中不存在 `runtime/profile-template/.runtime-cache`；两条构建路径产出的模板目录结构一致。

### 5.10 待实机确认：`present_status_page` 先销毁窗口再重建（#13）

相关代码：[`window.rs#L914-L925`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L914-L925)；依赖 `tauri-runtime-wry 2.11.4`（`src/lib.rs` 第 4310–4323 行、第 4371–4372 行、第 4469–4475 行）

**依据。** tauri-runtime-wry 在处理 `TaoWindowEvent::Destroyed` 时，从窗口表中移除该窗口；如果表变为空，就回调 `RunEvent::ExitRequested { code: None }`，未被 prevent 则 `ControlFlow::Exit`。本应用在 `ExitRequested` 上直接调用 `shutdown()`（`lib.rs#L494`），从不 prevent。

`present_status_page` 从后台线程先发 `destroy()`，再 `create_splash()`，两条消息都是异步投递到主线程的。能不能安全，取决于新 splash 被插入窗口表是否早于旧窗口的 `Destroyed` 事件。macOS 上 tao 通过 `close_async` 把关闭操作排进 GCD 主队列，与用户事件通道之间的先后顺序没有保证。

**可能的影响。** 如果时序不利，Harness 意外退出时，看门狗调用 `show_progress` 会让应用直接退出，并调用 `shutdown()`，而不是显示“正在重新启动”。`watch_first_load` 的失败页、`show_failure` 和接管询问也走同一个函数。

**建议。**

- 调换顺序：先确保 splash 存在（必要时 `create_splash`），再销毁 Harness 窗口，与 `create_harness` 里“先建新窗、再销毁 splash”的顺序保持一致。
- 实机验证：窗口打开状态下连续 20 次 `kill -9 <harness pid>`，应用每次都应停留在进度页并完成恢复。macOS 和 Windows 各测一轮。

## 6. P3 发现

| # | 位置 | 问题与建议 |
|---|---|---|
| 14 | [`shellenv.rs#L117-L121`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/shellenv.rs#L117-L121) | `read_to_string` 遇到非 UTF-8 时返回 `Err`，并且**不写入任何内容**（已复现：`b"DEEPSEEK_API_KEY=sk\nLANG=\xff\n"` 读出空串）。只要任一环境变量含非法字节，整次导入就为空，API Key 随之丢失，而这正是导入要解决的问题。改用 `read_to_end` 加 `from_utf8_lossy`（与 `stdout_within` 一致），并限制读取字节数。可顺带考虑用 `env -0` 处理值里含换行的变量（需确认 macOS 10.15 的 `env` 是否支持）。 |
| 15 | [`lib.rs#L1665-L1676`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1665-L1676)、[`lib.rs#L1188-L1193`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1188-L1193) | `plugin_skip_reason` 只在 `if declared` 内部调用，因此 `NotDeclared` 永远不会被记录，与注释里的“Reported rather than silently skipped”不符。它的文案中两个 `{}` 填的都是 `dshmarket`（应为 profile 名 `web`），`（` 后面还混进了一串空格。 |
| 16 | [`lib.rs#L1049-L1085`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1049-L1085) | 重启和恢复路径里，`start()` 已经就外部实例询问过；用户选择“取消”后返回 `Err`，`Err` 分支的 `confirm_takeover` 会**再问一次**同一个问题。选择“改用端口”后，如果新端口启动失败，也会就原端口再问一次。建议 `start()` 返回可区分的错误类型（“用户已拒绝”），这类错误不进入重试。另外，重试只对端口类失败有意义，`require_tested_dsh` 这类失败不应让用户先杀掉别人的实例。 |
| 17 | [`window.rs#L1318-L1330`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L1318-L1330) | 兜底 URL `http://127.0.0.1/` 没带端口，会被导航拦截判为非同源，并用系统浏览器打开 80 端口。`recover_terminated_webview` 应该拿到当前 Harness 的端口或启动 URL（可以在 `create_harness` 时存进静态变量）。这个处理函数也没有次数限制，在内存压力下可能反复“崩溃 → 重载”。 |
| 18 | [`window.rs#L1674-L1677`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L1674-L1677) | `spawn()` 之后不 `wait`，每次打开外链都会留下一个僵尸进程，直到应用退出，会占用用户的进程数配额。建议起一个线程 `wait`，或在 #2 修复后用不产生子进程的 API。 |
| 19 | [`harness.rs#L385-L440`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/harness.rs#L385-L440)、[`process.rs#L219-L233`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/process.rs#L219-L233)、[`locator.rs#L64`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/locator.rs#L64) | 发行版的 exe 是 `windows_subsystem = "windows"`，调用 `powershell`、`netstat`、`taskkill`、`node -e`、`cmd` 时都没有设 `CREATE_NO_WINDOW`，每次都会闪出控制台窗口。`netstat` 的 `LISTENING` 在德语等系统上会被本地化，导致 `listener_pid` 始终为 `None`；可改用 `Get-NetTCPConnection -State Listen -LocalPort N`。 |
| 20 | [`harness.rs#L227-L245`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/harness.rs#L227-L245) | 没有 `dsh web:` 前缀时，会接受任意一行里第一个带 query 的 `http://127.0.0.1…`。启动期间插件或 MCP 打印的本地地址可能被当成入口，`state.json` 也会记下错误的端口。建议优先采用带前缀的行，并校验 `url.port()` 等于请求的端口（或者至少在不相等时记录日志）。 |
| 21 | [`process.rs#L102-L117`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/process.rs#L102-L117) | 组长已退出时，`is_alive` 返回 false，函数直接返回，同一进程组里残留的 worker、MCP 子进程不会收到信号。看门狗在 Harness 意外退出后也不清理这个组，重启后旧的子进程会继续占用资源。建议对自己创建的进程组，无论组长是否存活都发送一次 `kill(-pgid, SIGTERM)`。 |
| 22 | [`update.rs#L713-L727`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/update.rs#L713-L727) | `ttl_minutes * 60` 在 `update_check_interval_minutes` 取极大值时溢出：debug 构建会 panic，release 构建会回绕。改用 `saturating_mul`。 |
| 23 | [`window.rs#L710-L726`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L710-L726)、[`window.rs#L609-L625`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/window.rs#L609-L625) | `Math.sumPrecise` 兼容实现：`[Infinity]` 和 `[1e308, 1e308]` 都返回 `NaN`（已用 node 复现），规范要求返回 `Infinity`。应在补偿求和前先处理非有限值。`Iterator.prototype.reduce` 没有初始值时，规范的计数器从 1 开始，这里从 0 开始。 |
| 24 | [`runtime.rs#L76-L87`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/runtime.rs#L76-L87)、[`lib.rs#L1521-L1524`](https://github.com/tanuki-cat/dsh-desktop/blob/b0d5405/src-tauri/src/lib.rs#L1521-L1524) | `SystemUpdates` 的 `#[default]` 是 `Install`，文档注释也说它是默认值；但 `Config` 实际使用 `default_system_updates()`，即 `Notify`（README 也写的是 `notify`）。`lib.rs#L1521` 的注释写的是“默认就地升级”。建议统一为 `Notify`，并修正注释。 |

## 7. 建议修复顺序

1. **#1、#2**：安全问题，改动小，可以一起提交。
2. **#3，并同时完成 #8**：更新事务的正确性。#3 修好后，如果没有失败标记，会立刻陷入 #8 的重试循环。
3. **#5，再做 #6、#14**：共用同一个有界子进程读取函数。
4. **#7、#9**：会话可用性。
5. **#10、#12**：打包与插件更新。
6. **#13**：先实机验证，再决定是否改动（即使不能复现，调换顺序的成本也很低）。
7. **#4、#11 以及其余 P3。**

## 8. 验证建议

- 每一项修复都附带回归测试，以本文“复现”中的输入为准。
- #1、#5、#10、#14 可以写成纯单元测试（Unix 平台的用例用 `#[cfg(unix)]`）。
- #2、#12、#13、#19 需要 Windows 或 macOS 实机，或者 CI 产物检查。
- 完成后运行 `cargo test`、`cargo fmt --check`、`cargo clippy --all-targets`，并同步 README 里的测试计数。

## 9. 处理状态

**本轮 24 项全部处理完毕（2026-09-16）**，各项原始结论未改动。核对时 24 项全部复现属实，
无一项误报。逐项说明见下表；实现细节与验证见
[`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md) §13.17。

> **复核更正（2026-09-16）**：对上述修复的复核（§10）发现，#7、#8、#11、#16 只完成一部分或实际未生效，
> #10 缺少一处配套改动；#1、#5、#6/#19、#21 的修复各自引入了新问题。下表的「状态」已按复核结果更正，
> 原说明保留；更正原因与后续处理见 §10。

| # | 状态 | 说明 |
|---|---|---|
| 1 | **已修** | `redact_bearer` / `redact_field` 改用**绝对偏移**（`start + at`），下标错位与 panic 同时消失。顺带：`Basic`/`Digest`/`Token` 方案也脱敏；`app_log` 在锁外脱敏且容忍锁毒化 |
| 2 | **已修** | Windows 分支改为 FFI 调 `ShellExecuteW`，不再经过 `cmd.exe`。`Command::arg` 的转义约定与 `cmd` 的解析规则不同，这正是注入的成因 |
| 3 | **已修** | `SwapRecord` 增加 `had_previous`（`Option`，旧记录按"有备份"处理）；`rollback_with` 在 `Some(false)` 时把失败树移到 `.failed` 并视为回滚成功 |
| 4 | **已修** | 空白正则修正为 `\s`（原本是 `/s/g`，会删掉所有小写字母 `s`） |
| 5 | **已修** | 新增 `process::output_within`：读线程经 channel 回传并**边读边发布**，调用方用有界 `recv_timeout` 而非 `join`。实测 `sh -c "sleep 30 & echo x"` 由 4.0 s 变为 176 ms |
| 6 | **已修** | `lsof`（并加 `-b`）、`ss`、`netstat` 全部改走带预算的读取 |
| 7 | **部分 → 见 §10 R2** | 暂存移到停止之前；停止只发生在 3c/3d。3d 增加守卫：外部实例被保留且其 CLI 树正是待替换的那棵时放弃切换 |
| 8 | **部分 → 见 §10 R3** | 新增 `update::mark_core_attempt_failed`；核心分支像插件分支一样检查 `failed_recently` |
| 9 | **已修** | 新增 `watch_reused`：轮询 pid，死亡后走与 `watch_harness` 相同的恢复流程（拿不到退出码按 `clean = false`） |
| 10 | **部分 → 见 §10 R5** | 新增 `transaction::copy_entry`：链接按链接复制（Unix 用 `symlink`，Windows 无权限时退回复制）；`restore_tree` 删旧链接时不再跟随。实测**本机真实 profile（349 MB / 163 个链接）快照成功** |
| 11 | **部分 → 见 §10 R4** | `find_launcher` 的 node 查找改为惰性（`get_or_insert_with`），不再对每个候选都起一次登录 shell |
| 12 | **已修** | store 路径改为绝对（`cd "$cache" && pwd`），并传入 tools 目录；`check-runtime-stage.sh` 增加"模板内不得有 pnpm store"断言 |
| 13 | **已修** | `present_status_page` 调换顺序：先建 splash 再销毁 Harness 窗口，与 `create_harness` 一致。**未做实机验证**（见下） |
| 14 | **已修** | `shellenv::capture` 改走 `output_within`：`read_to_end` + `from_utf8_lossy`，非 UTF-8 不再清空整次导入 |
| 15 | **已修** | `plugin_skip_reason` 改为无条件调用（`NotDeclared` 此前永不记录）；文案的两个 `{}` 改为 profile 名（新增 `PROFILE_NAME` 常量）并去掉多余空格 |
| 16 | **未生效 → 见 §10 R1** | 新增 `takeover::Retry`（`TakeOver`/`Declined`/`Refused`），用户选"取消"后重试不再追问 |
| 17 | **已修** | 新增 `HARNESS_URL`，兜底 URL 改为上次加载的地址（原本是不带端口的根路径，会被判为非同源并交给浏览器打开 80 端口） |
| 18 | **已修** | 新增 `spawn_and_reap`：起线程 `wait`，不再留僵尸 |
| 19 | **已修** | `output_within` 与 `taskkill` 统一设 `CREATE_NO_WINDOW`；`netstat` 不再依赖 `LISTENING` 这个会被本地化的词 |
| 20 | **已修** | `parse_dsh_url` 增加 `expected_port`：无前缀兜底只接受本次启动的端口，带前缀的行仍按原样采用 |
| 21 | **已修** | `terminate` 在组长已退出时仍向进程组发信号并等待。已实测确认 `kill(-pgid)` 在组长退出后仍可达子进程 |
| 22 | **已修** | `ttl_minutes.saturating_mul(60)` |
| 23 | **已修** | `Math.sumPrecise` 非有限值短路；`Iterator#reduce` 无初值时下标从 1 起 |
| 24 | **已修** | 去掉 `#[default]`（`Config` 用的是 `default_system_updates()` → `Notify`），注释同步 |

### 9.1 修复过程中新发现的问题（自查）

本轮改动自身引入、由新增测试当场抓出，已在提交前修正：

- **#23 的第一版修复不完整**：只在"输入值非有限"时短路，漏了"求和过程中溢出"。`[1e308, 1e308]`
  仍返回 `NaN`（应为 `Infinity`）。改为在 `sum + value` 溢出时短路，并继续把剩余值加完。
- **#13 的顺序调整有个前提**：先建 splash 再销毁窗口；若 splash 创建失败则保留原 Harness 窗口并
  返回失败，调用方按失败处理——行为正确，但值得记录。

### 9.2 与本文建议的差异

- **#5**：本文建议"用剩余 deadline 加短暂宽限做 `recv_timeout`"。实现改为**边读边发布**到共享缓冲，
  再在子进程退出后用固定短宽限收尾：这样即使宽限用尽，已经读到的内容也不会丢；而按 deadline 等待
  在"子进程立刻退出但孙进程持有管道"时仍要等满预算。
- **#12**：本文建议"Unix 传 `$out/tools/bin`，Windows 传 `$out/tools`"。实现改为先探测 `bin/pnpm`
  是否可执行、否则退回前缀根，两种布局都覆盖。
- **#21**：本文建议"无论组长是否存活都发一次 `kill(-pgid, SIGTERM)`"。实现保留了原有的
  "等宽限期再 SIGKILL"结构，只在组长已死时用固定短等待替代轮询（否则 `is_alive` 立即返回 false，
  宽限期会被整个跳过，子进程来不及退出）。

### 9.3 验证

在装有 Rust 工具链的环境执行（本文 §2 提到的限制在本轮已不适用）：

- `cargo test`：**174 passed / 0 failed**（本轮新增 19 项），集成 6 项；
- `cargo fmt --check` 通过；
- `cargo clippy --all-targets -- -D warnings` 在 **host 与 `x86_64-pc-windows-gnu` 两个目标上均 0 warning**；
- `cargo check --target x86_64-pc-windows-gnu --all-targets` 通过（Windows 专用分支无法在本机运行，
  但能编译并被 lint 覆盖）；
- `scripts/check-runtime-stage.sh` 自检通过。

新增用例覆盖本文各条的复现输入：同一行多个凭据、中文/emoji 前缀不 panic、散文不被误脱敏、
`Basic`/`Digest` 方案、首更无备份可回滚、旧记录不误删、子进程留下持有管道的孙进程时按时返回、
输出上限与截断标记、非 UTF-8 无损解码、符号链接快照与恢复、真实 profile 快照、组长已退出仍清理进程组、
`fromBase64`/`sumPrecise`/`reduce` 三个 JS 兼容块在真实 node 引擎里跑通。

### 9.4 遗留

- **#13 未做实机验证**：本文原就标注"待实机确认"，本轮按建议调换了顺序，但**没有**在真实窗口上
  连续触发渲染进程崩溃来验证。机制依据（tauri-runtime-wry 的 `Destroyed` → `ExitRequested`）
  已在依赖源码中核实，调换顺序本身无行为风险。
- **#2 未做 Windows 实机点击验证**：代码层面 Windows 分支已不含 `cmd`，`ShellExecuteW` 的签名与
  返回值判据经过交叉编译与 lint 检查，但未在 Windows 上实际点击构造过的链接。
- **#19 的控制台闪烁**未在 Windows 实机确认（只保证标志已设置）。

## 10. 复核（2026-09-16）

对 §9 所列修复做了一次独立复核：逐项阅读未提交的 diff，重新执行本文 §4–§6 的复现，并补测若干边界。

- 自动检查全部通过：`cargo test`（174 + 6）、`cargo fmt --check`，以及本机与 `x86_64-pc-windows-gnu` 两个目标上的 `cargo clippy --all-targets -- -D warnings`。
- 原复现输入均已修复：#1 的 5 个输入全部脱敏且不 panic，另用 20 万条随机输入模糊测试也没有 panic；#4、#5、#14 由新增测试覆盖。
- 但下列问题说明「24 项全部完成」不成立。

### 10.1 遗留：修复不完整或未生效

| # | 对应原条目 | 问题 |
|---|---|---|
| R1 | #16 | **仍会重复询问。** `confirm_takeover` 内部仍调用 `resolve_foreign_action`，也就是会再弹一次问题；`Retry::Declined` 只是在第二次拒绝之后换了一句日志。`start()` 返回的仍是 `String`，重试分支无法知道用户在 `start()` 里已经拒绝过。 |
| R2 | #7 | **3d 守卫检查错了端口。** 唯一会保留外部实例、又继续走到 3d 的分支是 `UseOtherPort`，而它已经把 `port` 改成了新端口（`lib.rs:1952`）；守卫去探测新端口，永远是 `Closed`，于是照常切换。`system_updates: install` 时，CLI 树会在用户选择保留的实例底下被替换——以前是先问再暂存，这属于回归。另外，守卫一旦触发，就会 `return Ok(())`：既不启动 Harness，也不给任何页面。 |
| R3 | #8 | **只做了一半。** 失败标记固定只压 5 分钟，之后每次启动仍然会重新下载、切换、超时；换上新树后的首次启动仍按 30 秒超时（`lib.rs:2085`）；进程被杀后由 `recover_pending_swap` 回滚的路径不写失败标记。 |
| R4 | #11 | **只做了惰性查找。** `-lc` 查找与 `-lic` 导入环境不一致这一主要问题没有处理，nvm/fnm 用户仍然查不到自己安装的 node/dsh。 |
| R5 | #10 | **快照仍在停止实例之后。** 快照失败时不写失败标记，下次启动还会先停实例、再失败。 |

### 10.2 修复引入的新问题

| # | 级别 | 问题 |
|---|---|---|
| N1 | P2 | **正常退出被当作崩溃。** 重构 `watch_harness` 时删掉了 `child.wait()` 之后的 `EXITING` 判断，`recover_exited` 里也没有这项判断。每次正常退出都会记录 "exited unexpectedly"，并弹出重启进度页、发起一次重启，与退出流程抢跑。 |
| N2 | P2（Windows） | **`netstat` 可能返回错误的 pid。** 为了规避本地化而删掉 `LISTENING` 判断后，只要一行里含 `:{port} ` 就算命中，出站连接也会被当成监听者。按修改后的循环逻辑、用典型 `netstat -ano` 输出实测：`TCP 10.0.0.5:50123 93.184.216.34:3080 ESTABLISHED 4242` 排在监听行之前，结果返回 4242（真正的监听者是 7777）。 |
| N3 | P3 | **组长已退出后仍可能误杀。** `terminate` 在组长已退出时仍用原 pid 发信号：Windows 上是 `taskkill /T` 再 `/F`，而 Windows 会很快复用 pid；Unix 上 `kill_signal` 在 `kill(-pgid)` 失败后会退回 `kill(pid)`。两者都可能打到无关进程。 |
| N4 | P3 | **脱敏最坏情况变成平方复杂度。** `scheme_prefix_len` 每命中一次，就把整行剩余部分转一次小写。实测一行 256 KiB、全是 `token=a ` 的输入，一次 `redact` 要 180 ms；普通文本不到 1 ms。它的注释写"方案名随凭据一起删掉"，实际输出里方案名是保留的。 |
| N5 | P3（Windows） | **相对链接的目标解析错误。** 无权限创建链接时，`copy_entry` 回退为复制，但用 `canonicalize(target)` 解析相对目标，结果按进程的当前目录解析，而不是按链接所在目录。 |
| N6 | P3 | **输出截断没有标记。** `output_within` 在子进程退出后只再等固定 150 ms。机器负载高、读线程没被调度到时，输出会被截断，但 `truncated` 仍为 false；环境导入可能把最后一个变量（例如 API Key）截断后导入。容量上限截断时，也同样会导入被截断的最后一行。 |
| N7 | P3（Windows） | **缺少 COM 初始化。** `ShellExecuteW` 也会在启动线程等后台线程上调用，这些线程没有初始化 COM，不符合 MSDN 的要求。 |

### 10.3 原审查漏掉的既有问题

| # | 级别 | 问题 |
|---|---|---|
| E1 | P2 | **JSON 形式的凭据不脱敏。** 实测 `{"api_key":"sk-json","token":"t-json"}` 原样写进日志：关键字后面紧跟的是 `"`，不是 `=` 或 `:`。`token: "abc"` 这种带引号的值也会漏掉：值以 `"` 开头，被当作空值。provider 的错误回显以 JSON 为主。 |
| E2 | P3 | **等号两侧有空格时不脱敏。** 例如 `password = hunter2`。 |
| E3 | P3 | **`stage-runtime.sh` 没有把自带 node 放到 PATH 前面**（原 #12 第 3 点）。模板一步调用的 pnpm 是 `#!/usr/bin/env node` 脚本，会用到机器上碰巧存在的 node。 |

### 10.4 本轮修复计划与状态

§10.1–§10.3 的问题已全部处理（2026-09-16），各条的原始描述未改。实现细节见
[`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md) §13.18。

| # | 修法 | 状态 |
|---|---|---|
| R1 | `start()` 改为返回 `StartError { reason, declined }`。重试分支先用纯函数 `may_ask_after_failure` 判断：本次尝试里用户已经选过"取消"，或选过"保留它，改用其它端口"（表现为启动端口已经变了），就直接报告失败、不再询问 | **已修** |
| R2 | `UseOtherPort` 分支记下被保留实例的 pid。3d 用纯函数 `swap_conflicts` 判断冲突：待替换的树不存在则不冲突；非本壳的前缀一律算冲突；本壳前缀看命令行，读不到也算冲突。冲突时丢弃暂存、**继续启动**。3c 之后端口又出现外部 Harness 的竞态同样处理，不再 `return Ok(())` | **已修** |
| R3 | `Cache.failures` 记录同一版本连续失败的次数，`failure_window_secs` 从 5 分钟起每次乘 4，最长 24 小时。标记在重试窗口过后仍然保留（否则次数永远只有 1），7 天后遗忘。换树后首次启动用 90 秒超时；`recover_pending_swap` 回滚成功后也写标记。额外补上：暂存失败、提交失败也写标记（README 原本就这样描述，但代码只在启动失败时才写） | **已修** |
| R4 | 解析运行时之前先汇合 `-lic` 环境采集，`resolve_runtime` → `system_node` / `system_dsh` 接收导入的 PATH。locator 的 `search` 先查 App 的 PATH，再查导入的 PATH；拿到导入的 PATH 时不再额外起 `-lc` 登录 shell，只有没导入时才用它兜底。PATH 中的相对条目不再参与查找 | **已修** |
| R5 | `update_market_plugin` 先做快照、再停实例；快照失败时写 `mark_plugin_attempt_failed` | **已修** |
| N1 | `recover_exited` 开头判断 `EXITING`，两种看护路径都覆盖到 | **已修** |
| N2 | 新增纯函数 `parse_netstat_listener`：协议为 TCP、本地地址以 `:{port}` 结尾、远端地址以 `:0` 结尾，才算监听行 | **已修** |
| N3 | 组长已退出时调用 `clean_orphaned_group`：Unix 上只对 `-pgid` 先发 SIGTERM、200 ms 后发 SIGKILL，不再按 pid 回退；Windows 上什么也不做 | **已修** |
| N4 | `scheme_prefix_len` 改为原地比较开头的字节（`eq_ignore_ascii_case`）。实测同一 256 KiB 满字段行从 180 ms 降到约 2–3 ms。注释改为如实描述：保留方案名、替换其后的凭据 | **已修** |
| N5 | `copy_entry` 把链接目标与链接所在目录拼接后再交给 Windows 回退路径使用；创建的链接本身仍按原样写入 | **已修**（仅交叉编译 + lint，未在 Windows 实机验证） |
| N6 | `output_within` 记录读线程是否读到 EOF；没读到 EOF 或触发容量截断时，用 `complete_lines` 丢掉最后一行不完整的内容，并置 `truncated`。宽限从 150 ms 改为 500 ms。`stdout_within` 遇到"截断后没有完整行"时返回 `None`，不再返回 `Some("")`（后者会被当成"没有这个进程"）。环境导入不完整时记一条日志（只记变量名，不记值） | **已修** |
| N7 | `ShellExecuteW` 放到专用线程里调用，前后配对执行 `CoInitializeEx(APARTMENTTHREADED \| DISABLE_OLE1DDE)` / `CoUninitialize` | **已修**（仅交叉编译 + lint，未在 Windows 实机验证） |
| E1/E2 | 新增 `locate_value` / `value_len`：识别 `KEY=value`、`key: value`、`key = value`、`"key": "value"`（含 `\"` 转义和 Python 的 `'`）；带引号的值以配对的引号为界（跳过 `\"`）。没有闭合引号时退回无引号规则，并先跳过开头的空白 | **已修** |
| E3 | 模板一步把自带 node 所在目录加到 PATH 前面 | **已修** |

### 10.5 验证

- `cargo test`：**185 passed / 0 failed**（本轮新增 11 项），另有 6 个集成用例通过；`scripts/tests/check-runtime-stage-test.sh` 通过。
- `cargo fmt --check` 通过；`cargo clippy --all-targets -- -D warnings` 在本机与 `x86_64-pc-windows-gnu` 两个目标上均为 0 warning。
- 新增用例：
  - `netstat`：出站连接排在监听行之前、德语状态词、IPv6 监听、UDP 同端口、只有连接没有监听；
  - 脱敏：JSON、转义 JSON、Python dict、带引号的值、`key = value`、截断在引号内的行、没有闭合引号的值、保留结构、散文与计数不被改写、1 MiB 满字段行在 2 秒内完成；
  - 进程输出：残行被丢弃、唯一一行被截断时返回 `None`、`complete_lines`；
  - 失败退避：窗口递增、封顶、换版本重新计数，标记在窗口过后仍保留、7 天后遗忘，`recover_pending_swap` 写标记；
  - 重试不重复询问的判定矩阵、`swap_conflicts` 矩阵、导入的 PATH 参与查找且不再起登录 shell、PATH 中的相对条目被忽略。
- 独立复测：
  - 脱敏在 30 万条随机输入（含引号、反斜杠、制表符、多字节字符）下 0 次 panic；
  - `lsof -b -nP -t -iTCP:<port> -sTCP:LISTEN` 在 macOS 上能正确返回监听进程的 pid。

### 10.6 遗留

- **仍需 Windows 实机验证：** #2、#19、N5、N7；#13 仍需在真实窗口里连续触发崩溃来验证。原因同 §9.4。
- **脱敏不保证幂等（已知，可接受）：** 引号不闭合、又夹杂转义引号的少数输入，再脱敏一遍还会替换更多内容。每一遍都只增不减，不会漏掉凭据。随机测试 30 万条中出现 1 例。
- **`key = value` 会作用于散文：** 例如 `the token = abc` 里的 `abc` 也会被替换。这是有意的取舍：配置转储正是这种写法，漏掉凭据的代价高于改写一个词。
