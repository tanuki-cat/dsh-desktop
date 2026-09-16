# dsh-desktop 最新代码审查报告

审查对象：<https://github.com/tanuki-cat/dsh-desktop>  
审查分支：`main`  
审查提交：`44a347e`  
审查日期：2026-09-16

## 1. 总体结论

当前 `main` 比 `v0.4.3` 多两个提交：

- `b2a79fd`：将接近 3000 行的 `lib.rs` 按职责拆分，并迁移测试。
- `44a347e`：修复端口探针面对持续发送数据的对端时无法按时结束的问题。

本次审查没有发现会普遍导致应用无法启动、数据损坏或 Harness 无法使用的确定性严重 Bug。最新端口探针修复方向正确，也没有发现明显回归。

目前最值得优先处理的是外部进程输出的内存边界。现有代码对 Harness 单行输出以及 npm/pnpm 的完整输出没有大小限制，异常情况下可能造成明显内存增长、CPU 占用升高甚至 OOM。

## 2. 问题汇总

| 优先级 | 问题 | 主要影响 | 建议 |
|---|---|---|---|
| P1 | Harness 单行输出没有长度限制 | 巨型单行可能造成高内存占用、卡顿或 OOM | 限制单行大小并截断 |
| P1/P2 | 更新命令完整收集 stdout/stderr | npm/pnpm 持续输出时可能积累数百 MB | 使用固定大小尾部缓冲区 |
| P2 | 同一日志行重复执行脱敏 | 高频或大日志时放大 CPU 与内存分配 | 每行只脱敏一次 |
| P2/P3 | 单条巨型日志可突破 5 MiB 轮转目标 | 日志文件可能远超设计大小 | 写入前计算预计大小 |
| P3 | 退出时同步等待 Harness 终止 | 应用退出可能停顿约 5 秒 | 接受现状或异步清理 |

## 3. 详细发现

### 3.1 P1：Harness 单行输出没有长度限制

相关代码：

- [`harness.rs#L585-L599`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/harness.rs#L585-L599)
- [`harness.rs#L31-L50`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/harness.rs#L31-L50)

Harness 的 stdout 和 stderr 使用以下方式逐行读取：

```rust
for line in BufReader::new(reader).lines().map_while(Result::ok) {
    ring.push(&line);
    log.write(&redact(&line));
    if let Some(url) = parse_dsh_url(&line) {
        let _ = tx.send(url);
    }
}
```

Ring 虽然只保存最近 200 行，但没有限制每一行的长度。如果 dsh、Node 或插件输出一条没有换行的巨大 JSON、模型响应或错误对象：

1. `BufRead::lines()` 会持续扩大 `String`；
2. Ring 会保存完整内容；
3. 脱敏函数会多次扫描并复制字符串；
4. 日志系统会尝试一次性写入完整内容。

这可能导致明显卡顿、瞬时高内存，极端情况下可能造成 OOM。

建议：

- 将单行上限设置为 256 KiB 或 1 MiB；
- 超过上限后继续排空到下一个换行符，但不再扩展内存；
- 日志中追加截断提示，例如：

```text
...[truncated: original line exceeded 256 KiB]
```

### 3.2 P1/P2：更新命令 stdout/stderr 可无限占用内存

相关代码：

- [`update.rs#L99-L117`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/update.rs#L99-L117)
- [`update.rs#L23-L33`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/update.rs#L23-L33)

更新命令的 stdout 和 stderr 分别交给后台线程，最终执行：

```rust
let mut buffer = Vec::new();
let _ = pipe.read_to_end(&mut buffer);
```

安装操作允许运行最长 300 秒，但其输出没有大小限制。npm/pnpm 生命周期脚本、异常重试或插件安装脚本如果持续输出，内存可能在 5 分钟内增长到数百 MB。

而调用方通常只需要：

- stdout 中的少量 JSON；
- stderr 的前几行或最后几行错误信息。

因此没有必要保留全部输出。

建议：

- stdout/stderr 分别限制为 1–4 MiB；
- 使用固定大小的尾部缓冲区保存最后一部分诊断信息；
- 超限后继续读取并丢弃，避免子进程因管道填满而阻塞；
- 在错误信息中标记输出已截断。

### 3.3 P2：同一日志行重复执行脱敏

相关代码：

- [`harness.rs#L53-L180`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/harness.rs#L53-L180)
- [`harness.rs#L591-L597`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/harness.rs#L591-L597)

当前执行流程是：

```rust
ring.push(&line);          // Ring::push 内部调用 redact()
log.write(&redact(&line)); // 再次调用 redact()
```

一次 `redact()` 又会针对多个敏感字段反复搜索和构造字符串。正常日志量下影响有限，但遇到高频日志或超长单行时，会显著放大 CPU 消耗和内存分配。

建议只脱敏一次：

```rust
let safe = redact(&line);
ring.push_redacted(&safe);
log.write(&safe);
```

这项优化应与单行长度限制一起完成。

### 3.4 P2/P3：单条巨型日志可以突破轮转大小

相关代码：

- [`harness.rs#L666-L703`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/harness.rs#L666-L703)

当前轮转发生在写入之前，但只检查文件当前是否已经超过限制：

```rust
self.rotate_if_oversized(&mut guard);
writeln!(file, "{line}");
```

例如当前日志为 1 MiB，下一条日志为 100 MiB，这次写入会直接把文件扩大到约 101 MiB，直到下一条日志到来时才会轮转。

建议：

- 写入前检查 `written + incoming_size > limit`；
- 如果当前文件非空，则先轮转；
- 同时实施单行大小限制，否则新文件本身仍可能远超 5 MiB。

### 3.5 P3：退出过程可能同步阻塞事件线程

相关代码：

- [`lib.rs#L432-L440`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/lib.rs#L432-L440)
- [`lib.rs#L493-L505`](https://github.com/tanuki-cat/dsh-desktop/blob/44a347e/src-tauri/src/lib.rs#L493-L505)

`RunEvent::ExitRequested` 和 `RunEvent::Exit` 直接调用同步 `shutdown()`。进程终止等待上限为 5 秒，因此 Harness 响应较慢时可能出现：

- 点击退出后窗口短暂无响应；
- macOS 显示应用仍在退出；
- 关机或注销过程略微延长。

该实现的优点是能够尽量避免遗留 Harness 子进程，因此不建议为了更快退出而简单移除等待。如果优化，可考虑异步发出终止信号，并设置一个较短的最终强制清理窗口。

## 4. 最新提交专项审查

最新提交 `44a347e` 修复的是：端口探针原本只设置每次 socket 读取的超时，如果对端持续在超时窗口内发送少量数据，读取就可以一直成功，导致探针整体永不返回。

新实现增加了：

- 整体 deadline；
- 每次读写前根据剩余时间重新设置超时；
- 64 KiB 响应缓存上限；
- 非 401 响应的提前判定；
- 沉默对端和持续发送对端的回归测试。

结论：该修复逻辑正确，没有发现明显的新回归。响应缓存最多只会比 64 KiB 上限多出一个读取块，影响可以忽略。

## 5. 已做得较好的部分

与此前的 v0.4.1 相比，当前实现已经有明显改善：

- 大型 `lib.rs` 已按照运行时、进程、更新、窗口、接管等职责拆分；
- Harness 崩溃和插件市场自重启具备恢复预算；
- 更新采用 staging、验证、提交及回滚机制；
- WebView 卡帧和渲染进程退出有看护与恢复逻辑；
- 端口接管同时验证 HTTP 特征与监听进程身份；
- Harness 日志具有凭据脱敏和轮转；
- npm 查询、安装及 shell 环境读取均有超时保护；
- Windows 与 Unix 的进程终止路径分别处理。

## 6. 建议修复顺序

1. 为 Harness 输出实现单行大小限制。
2. 为 npm/pnpm stdout 和 stderr 实现有界缓冲区。
3. 将 Harness 每行日志改为只脱敏一次。
4. 将日志轮转判断改为包含即将写入的字节数。
5. 最后再评估退出过程是否需要异步化。

前三项可以放在一个改动中完成，因为它们都属于外部输出的有界处理。

## 7. 验证建议

建议补充以下测试：

- Harness 输出一条 10 MiB 且没有换行的数据，进程内存保持在预期范围；
- npm 模拟进程连续输出超过缓冲区上限，调用仍可正常结束；
- 截断后的日志不泄露 API Key、Cookie 或 Authorization；
- 单条日志大于轮转阈值时，不产生远超 5 MiB 的日志文件；
- stdout 与 stderr 同时高频输出时，不发生管道阻塞；
- Windows 和 macOS 上退出时，Harness 进程均被清理。

## 8. 审查限制

本次完成了源码、提交差异和关键生命周期路径的静态审查。检查环境未安装 Rust/Cargo，因此未能在本地重新执行 `cargo test`、`cargo clippy` 或构建产物。

仓库文档声明当前测试规模为 142 个离线单元测试及 6 个集成测试；这属于仓库提供的信息，不等同于本次环境中的独立运行结果。

---

## 9. 处理状态（2026-09-16）

本文的 5 项发现经代码核对**全部属实**，已在本轮全部修复；核对过程中另发现 3 个本文未列出的相关问题，
其中 1 个优先级不低于本文的 P2，一并修复。逐项说明见下表，实现细节与验证见
[`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md) §13.16。

| 本文条目 | 状态 | 修复要点 |
|---|---|---|
| §3.1 Harness 单行输出没有长度限制 | **已修** | `LINE_LIMIT_BYTES = 256 KiB`，超限继续排空到换行但不扩张内存，行尾追加截断提示 |
| §3.2 更新命令完整收集 stdout/stderr | **已修** | `Captured` 有界缓冲：stdout 4 MiB / stderr 1 MiB，仍读空管道避免子进程阻塞，截断在错误信息中标注 |
| §3.3 同一日志行重复执行脱敏 | **已修** | 读取侧脱敏一次，`Ring::push_redacted` 与日志共用同一份结果 |
| §3.4 单条巨型日志可突破轮转大小 | **已修** | 轮转判定改为 `written + incoming > limit`；空文件不轮转（否则只是白挪备份） |
| §3.5 退出时同步等待 Harness 终止 | **维持现状** | 按本文建议保留同步等待：它换来的"不留残余 Harness"比 5 秒退出停顿更重要，未做异步化 |

### 9.1 本轮额外修复（本文未列出）

| 问题 | 优先级 | 修复要点 |
|---|---|---|
| `map_while(Result::ok)` 遇非 UTF-8 字节会**永久静默断流** | 不低于 P2 | 换成自实现的 `read_line`：按字节读到换行、`from_utf8_lossy` 解码、单行截断。此前插件输出一个原始字节就会丢掉其后**所有**行（含 `dsh web:` URL 行），日志里没有任何原因 |
| `Ring::tail()` 拼接无上限内容 | P3 | Ring 增加 `RING_BYTES = 1 MiB` 字节预算，与 200 行上限同时生效，失败页不会被巨型行撑爆 |
| 登录 shell / npm / `ps` 子进程无超时 | P2 | 新增 `process::stdout_within`，`command -v npm`、`npm prefix -g`、`command -v node`、两处 `ps` 全部加预算；卡住的 rc 文件此前能把启动线程永久挂住 |

### 9.2 与本文建议的差异

- §3.2 建议的"尾部缓冲区"改为**保留前部**：调用方只用 stdout 的 JSON（截断即报错，不解析半截文档）
  和 stderr 的**前几行**（npm 的错误摘要在最前面），因此不需要环形尾部缓冲。
- §3.3 建议的改法需要一处修正：`parse_dsh_url` 必须继续用**原始行**解析，否则 `token=…` 已被脱敏成
  `token=***`，启动 URL 的鉴权参数会丢失。实现里两者分开取用。
- §3.4 的实现补了一条本文未提的边界：当前文件为空时不轮转，否则一条超限日志会先制造一个空备份、
  再把这条日志写进新文件，白挪一次备份。

### 9.3 验证

在装有 Rust 工具链的环境中执行（本文 §8 提到的检查限制在本轮已不适用）：

- `cargo test`：**155 passed / 0 failed**（本轮新增 12 项：单行截断、非 UTF-8 不断流、空行不断流、
  CRLF、Ring 字节预算、Ring 只存已脱敏内容、轮转不越界、空日志不轮转、有界管道三例、
  `ps` 三种答复的分类），另有 6 个集成用例；
- `cargo fmt --check` 通过；`cargo clippy --all-targets` 0 warning；`cargo check --all-targets` 通过。

本文 §8 提到的"仓库文档声明 142 个离线单测"在本次修复前已过期：`44a347e` 新增探针回归测试后为 143，
本轮为 155。README 中的计数已同步更新。

