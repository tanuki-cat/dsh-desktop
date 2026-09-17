# dsh-desktop 代码审查报告（v0.4.4 / 5d8c7fe）

审查对象：<https://github.com/tanuki-cat/dsh-desktop>  
审查分支：本地 `main`（工作区干净，仅 `?? .DS_Store`）  
审查提交：`5d8c7fe`（`chore(release): v0.4.4`）  
审查日期：2026-09-17  
上一轮审查：[`dsh-desktop-b0d5405-code-review.md`](./dsh-desktop-b0d5405-code-review.md)（针对 `b0d5405`，其 24 项与复核 R1–R5/N1–N7/E1–E3 已在 `64a2041`…`4fcd571` 修复）

本文是**审查结论 + 修复任务清单**。修复落地后，把结果回写到文末的「处理状态」，不要改写各条的原始结论。

## 1. 总体结论

审查范围：`src-tauri/src` 全部非测试代码、`src/index.html`、`scripts/`、`.github/workflows/`、`Makefile`、`README.md`，以及 `docs/` 中仍然有效的设计文档。

共发现 **2 个 P1、8 个 P2、12 个 P3**，另有 **2 项清理**。

- **A1 是功能损坏**：v0.4.1 第 7 项的修复在 `64a2041` 重构更新事务时，**故障路径**漏掉了「停止正在使用待替换 CLI 树的实例」。命中的话，一次注入失败会把应用推到只能退出的失败页。
- **A2 是发布闸门失效**：staging 闸门只判「存在」不判「可执行」，**本轮已实测 `chmod -x` 后闸门仍 exit 0**。
- **B1/B2/B3/B4 是同一类「状态与副作用不同步」**：三个看护线程缺世代栅栏、接管面板吞掉 IPC 失败、reduce 回归测试从未跑被测代码、失败页闩在开窗前置位。
- 本轮**没有发现行为倒退**：上一轮复核的 R1–R5、N1–N7、E1–E3 逐条在位。A1 是继承下来的缺口，B3 是测试从未生效。

## 2. 审查方法与限制

- 逐文件静态阅读 + 调用链追踪；可疑项用 `node`、假 staging 目录、依赖源码（`tauri 2.11.5` / `tauri-runtime-wry 2.11.4` / `wry 0.55.1`）交叉验证。
- 本机实跑的基线（macOS，rustc 1.98.1）：
  - `cargo test`：**185 passed / 0 failed**，集成 **6 passed**；
  - `cargo fmt --check` 通过；
  - `cargo clippy --all-targets -- -D warnings` **0 warning**；
  - `make test-scripts` 通过；`scripts/check-runtime-stage.sh src-tauri/runtime` 通过（31046 文件 / 519 MB）；
  - 已构建的 `DSH Desktop.app` 里的 runtime：**31064 个普通文件、0 个符号链接**（staging 里有 18 个链接，被打包解引用）。
- **未做**：Windows 实机、Linux 实机、签名/公证链路；跨平台结论只到交叉编译 + 源码级核对。

## 3. 问题汇总

| # | 级别 | 问题 | 验证 |
|---|---|---|---|
| A1 | **P1** | 更新故障路径漏了「停止正在用该树的实例」：3c 只认 state.json 命中的 pid，自动换端口时旧实例仍在服务被 rename 的树 | 代码追踪（`git log -S` 佐证） |
| A2 | **P1** | staging 闸门只查存在不查可执行，`chmod -x` 后仍放行 | **已实测** |
| B1 | P2 | 三个看护线程缺世代栅栏（或栅栏在副作用之后），旧看护会操作/销毁新窗口 | 代码追踪 + 依赖源码 |
| B2 | P2 | 接管面板 `invoke` 只包同步 `try/catch`，Promise rejection 被吞 | 代码阅读（同文件另一处有对照） |
| B3 | P2 | 「Iterator reduce 下标」回归测试注入空脚本，跑的是 node 原生实现 | **已实测** |
| B4 | P2 | 失败页闩在 `present_status_page` **之前**置位，开窗失败后永久沉默 | 代码阅读 |
| B5 | P2 | 闸门自检只覆盖 2/7 条失效路径，`loose` 还绕开文件数/体积阈值 | 代码阅读 + 实测 |
| B6 | P2 | preflight 不校验 staging 版本变量，跨平台可能静默带不同 dsh/Node | 代码阅读 |
| B7 | P2 | `identity.rs` 全文无 `cfg(unix)`，只用 `ps` | 代码阅读 |
| C1 | P2 | 更新前停止实例不在恢复路径上，脚本损坏时自启必失败并烧退避窗口 | 代码追踪 |
| C2 | P3 | 活动探测叠加在帧探测超时之后，每个失败周期多 5–10 s | 代码阅读 |
| C3 | P3 | 下载目录硬编码 `~/Downloads`；`home_dir()` 为 None 时落到 cwd | 代码阅读 |
| C4 | P3 | 重名兜底第 1001 次覆盖已有文件，且是 TOCTOU | 代码阅读 |
| C5 | P3 | 进度页带 detail 时被加 `.error`，spinner 被隐藏 | 代码阅读 |
| C6 | P3 | 提问把 splash 永久放大，选完不还原 | 代码阅读 |
| D1 | P3 | 改 staging 脚本不影响 CI 缓存 key | 代码阅读 |
| D2 | P3 | Windows 作业门禁比其它平台窄 | 代码阅读 |
| D3 | P3 | 重复发布不刷新 Release 正文 | 代码阅读 |
| D4 | P3 | THIRD-PARTY-NOTICES 的 UNKNOWN 不让任何流程失败 | 代码阅读 |
| D5 | P3 | `make windows` 的 win tarball 分支永不生效 | 代码阅读 |
| D6 | P3 | 脚本自检在 node 缺失时静默通过（3 个集成用例 0 断言） | 代码阅读 |
| D7 | P3 | 集成测试用固定临时目录名，并行/同机会互删 | 代码阅读 |
| E1 | 清理 | `locator::locate()` 死代码；`Config` 的 `#[serde(default)]` 死路径 | 代码阅读 |
| E2 | 清理 | `.DS_Store` 未进 `.gitignore` | `git status` |

## 4. P1 详细发现

### 4.1 A1：更新故障路径漏了「停止正在使用待替换树的实例」

**现象**：一次核心更新在「提交后启动失败」的路径上回滚旧树并自动重启，但重启会**用一棵模块已残缺的旧树**去起，因而必然再失败；连续 3 次后停在失败页，用户只能退出应用。

**证据链**（全部在 `lib.rs::start()`）

1. `stop_instance_before_update()` 现在只存在于 `update_flow.rs:78`（插件分支）与 `:439`（定义）。`lib.rs` 里已经没有调用：

       $ git log --oneline -S "stop_instance_before_update" -- src-tauri/src/lib.rs
       64a2041 fix: 更新事务可回滚、失败后退避，启动流程不再误判或重复询问   <- 删除

   `64a2041` 把「先停实例」换成「先暂存」，注释写明 *"The stop happens in step 3c, once a verified tree exists"*。
2. **3c 只停三类实例**：自家 state.json 命中且端口相同（复用分支）、自家 state.json 命中但端口不同（`:1990`）、外部实例且用户同意接管（`:2056`）。故障路径下三类都不成立：
   - `staged_update` 非空 → 复用分支被 `Some(pid) if !just_updated && staged_update.is_none()`（`:1949`）排除；
   - 落到 `Some(pid)` 分支，注释写 *"it is our child, so its whole process group goes down together"*，而 `:1992` 的 `process::terminate(pid, …)` **只停 state.json 里那一个 pid**；更新期间被看护或插件流程重启出的第二个实例不在 state.json 里。
3. 3c 于是走 `ForeignAction::UseOtherPort`：`:2038` 把 `port` 换成新端口。`:2111` 的 `raced` 只看**新端口**，看不到旧端口上的残留（用户主动选「保留并换端口」那一条已由上一轮复核 R2 的 `swap_conflicts` 覆盖，**自动**换端口这条没有）。
4. `:2128` 的 `commit_core_update()` 在旧实例仍持有 `require` 的树上 rename：Unix 上进程还活着，下一次 lazy `require()` 得到 `MODULE_NOT_FOUND`；`:2186` 用新树 spawn，因端口被占拿不到 URL → `:2219` 回滚旧树。
5. `:2277` 失败页给「重新启动 Harness」按钮 → 再 `start()` → 3c 又落到 `Some(pid)`（或复用一个只加载了一半模块的旧实例）→ 必然再失败。

**影响**：应用进入只能退出的状态；同时白白消耗 `MAX_AUTO_RESTARTS = 3` 的自动重启预算。

**建议修法**（成本递增）

- 最小改动：在 `:2111` 的 `raced` 判定里把**旧端口**也算上（`probe(port_before) == Harness && !ours_on_port(...)` → `drop(staged)`）；
- 或者把「3c 之前的停止」纳入恢复路径，不依赖 3c 自己发现；
- 推荐：把判定抽成纯函数（例如 `swap_blocked(port_before, port_now, kept, raced_old, raced_new)`）并补矩阵测试 —— 现有 `swap_conflicts` 只覆盖了其中一半。

**复现方式**：给受管 `dsh` 打一个「切换后必失败」的桩（或把启动超时压到 1 s），触发一次核心更新，观察日志是否出现 `Harness 已退出，正在重新启动…` 以及随后的端口占用。

### 4.2 A2：staging 闸门只查「存在」不查「可执行」

**位置**：`scripts/check-runtime-stage.sh` 的 `first_of()`（`:33-44`，只用 `[ -e ]`）、node / pnpm 候选（`:47-51`）、判定（`:56-59`）。

**复现**（本轮实测，临时目录，已删除）：

       --- baseline (executable)         --- exit=0 (pass)
       --- after chmod -x node and pnpm --- exit=0 (PASS -> not caught)

同一个脚本的另外四项（悬空链接、绝对链接、模板内 store、quarantine）都能正确报错，只有「可执行」这一项没有断言。

**影响**：闸门「通过」、发布照常，用户拿到「能开壳、一拉 harness 就失败」的包 —— 正是这道闸门存在的目的。tauri 复制资源保留 mode（README 自述；本轮也在已构建的 .app 上核对过文件数与链接数）。

**建议修法**：必需内容检查里，node / pnpm 候选改判可执行性（Unix 用 `[ -x ]`；Windows 判 `.exe`/`.cmd` 存在且非目录），并断言最终取到的候选可用；同时在 `scripts/tests/check-runtime-stage-test.sh` 里加一条「去掉 `+x` 必须失败」的反例。

## 5. P2 详细发现

### 5.1 B1：看护与重建缺世代栅栏

- `window.rs:1396-1490`（`watch_page_liveness`）：`generation != GENERATION.load(...)` 的检查在 `:1486`，而 `:1472` 的 `window.navigate(url)`、`:1455`/`:1480` 的 `set_title` 都在它**之前**。窗口重建后旧线程醒来，`app.get_webview_window(HARNESS)` 拿到的是**新窗口**，却带着旧循环的 `misses`/`reloads`/`frames`。`Silent` 不受 `attended()` 保护（`:1067-1068`），新窗口初始化期间一次 `Silent` 就可能触发 `navigate`（换过端口时还会被 `on_navigation` 判为非同源丢给浏览器）或 `Report`（把「请重启应用」写到健康窗口的标题上）。
- `window.rs:1584-1632`（`watch_first_load`）：**完全没有**栅栏。该线程最长存活 `3 × 20s + 退避 ≈ 62s`；期间窗口被替换后 `window_alive` 对**新窗口**返回 true，而 `port_serving(旧 port)` 为 false → `LoadOutcome::Report`（`:1617`）→ `show_failure` → `present_status_page` **销毁正在工作的新窗口**。
- `window.rs:1214-1216` 与 `:1287`（`create_harness`）：先 `destroy()` 再 `build()` 同一 label。`destroy` 是异步的（`tauri-runtime-wry` 只 `send_event(Destroy)`），label 唯一性在 `tauri-2.11.5/src/manager/window.rs:71` 判定，摘除发生在 `src/manager/mod.rs:653` 的 `on_window_close`（收到 `Destroyed` 时）。两种交错都有害：晚到则摘掉的是**新窗口**（`get_webview_window(HARNESS)` 永久为 None，看护自行退出、崩溃恢复放弃、状态页再也无法销毁它）；早到则 `build()` 返回 `WindowLabelAlreadyExists` → `create_harness` 失败 → `abort_start` 杀掉刚起来的 Harness。`present_status_page` 的注释（`:933-940`）已经写明这个异步性并为此调过顺序，这里是同 label 的 destroy→create，却没有对应处理。

**建议修法**：把栅栏前置到 `get_webview_window` 之后、任何 probe / `set_title` / `navigate` 之前（循环末尾那次保留无害）；给 `watch_first_load` 也传 generation；以纯函数（例如 `may_act(generation, current) -> bool`）抽出以便单测；`create_harness` 在 `destroy()` 后做有界等待（轮询 label 消失，例如 2 s，超时记日志并按失败返回），或改为对已存在窗口 `navigate`。

**待实机确认**：命中哪一种交错取决于 tao 的 `Destroyed` 投递时机，建议连续重启 20 次并检查日志。

### 5.2 B2：接管面板吞掉 IPC 失败，可能执行与用户选择相反的动作

**位置**：`src/index.html:179-192`。`window.__TAURI_INTERNALS__.invoke(...)` 返回 Promise，此处只包了同步 `try/catch`，rejection 不会被捕获（同文件 `:215-220` 的重启按钮明确用了 `.catch(restore)`，说明作者知道它是异步的）。按钮文字会停在「已选择：…」。

**影响**：Rust 侧 `wait_for_choice`（`window.rs:340`，超时 `CHOICE_TIMEOUT = 120s`）收不到答案 → `ask_choice` 返回 `None` → 调用方按 `config.json` 处理。默认 `take_over_existing: true` 时，用户点「保留它，用系统浏览器打开」反而会被**接管**，对方正在跑的 agent 会话被终止；用户还会盯着「已选择」等满 120 秒。

**建议修法**：`invoke(...).catch(...)` 里写失败文案并恢复按钮态；补一个「`invoke` 返回 rejected Promise」的用例（现有 `CHOICE_DRIVER` 的 `invoke` 永远 resolve）。

### 5.3 B3：「Iterator reduce 下标」回归测试从未执行被测代码

**位置**：`src-tauri/src/window/tests.rs:275-298`，关键行 `:284`：

       compat_script(&["Iterator.prototype.reduce".to_string()])

`compat_script` 是**全等**匹配（`window.rs:499-508`），`COMPAT_BLOCKS` 的键是 `"Iterator"`（`:485`），两者不相等 → **一个块都不注入**，`script` 是空串。

**已实测**：`node -e` 得到 `native reduce indices: 1,2`（node v22.23.2），与本机断言一致，所以断言恒成立；在没有原生实现的旧 node 上则 `!output.status.success()` → `:293-296` 直接 `return`（注释写成「引擎已经有这个 API，无需检查」）。

**影响**：`b0d5405` 报告 §9.1 记录的「`Iterator#reduce` 无初值时下标从 1 起」这条修复**没有任何测试真正覆盖**；注入空脚本时该用例必定通过。上一轮 §9 表格第 23 项因而失实。

**建议修法**：改为 `compat_script(&["Iterator".to_string()])`，并加前置断言证明垫片确实被注入、被测引擎确实缺少原生实现（probe 里先输出 `typeof [].values().reduce`，为 `"undefined"` 才断言，否则显式跳过并打印原因）。

### 5.4 B4：失败页闩在开窗之前置位

**位置**：`window.rs:883-894`（`show_failure`）与 `:901-912`（`show_notice`）。`FAILURE_SHOWN.swap(true, …)` 在 `present_status_page(app)` **之前**执行。若 `present_status_page` 返回 false（splash 建不出来，`:941-946`），闩已为 true 而 `RETRY_OFFERED` 仍为 false。

**影响**：此后所有终态页被 `:884-889` 静默丢弃（日志只说 `failure page already shown`），用户既看不到原因也拿不到重启入口。附带：`:948` 的 `harness_window.destroy()` 错误被 `let _ =` 吞掉，返回 true 会让调用方以为「旧窗口一定没了」。

**建议修法**：`present_status_page` 失败时复位闩，或在闩之前先确保有窗口。

### 5.5 B5：闸门自检只覆盖 2 条失效路径

**位置**：`scripts/tests/check-runtime-stage-test.sh`（全文 57 行）。只造了「缺必需内容」（`:19-27`）与「绝对链接」（`:44-50`）两个反例。**没有反例**的断言：悬空链接（`check-runtime-stage.sh:62-69`）、模板内 pnpm store（`:93-100`）、quarantine（`:103-111`）、文件数闸门（`:119-122`）、体积闸门（`:123-126`）。

加重因素：自检用 `loose`（`:17`）把文件数/体积阈值设成 `0 / 99999999 / 99999`，连「干净目录通过」也不经过这两项；`files`/`mb` 的计算若被改坏，自检与 `make test` 都不会发现。

**影响**：虚假安全感 —— 脚本开头写着「闸门失效是无声的」，README 也据此宣称「自检会先用假目录验证闸门本身有效」，与实测覆盖范围不符（本轮已证实可执行位这一项确实漏网，见 A2）。

**建议修法**：为上述 5 项各造一个反例，保留一个「干净但闸门未放宽」的正例；`make test` 已依赖它。

### 5.6 B6：preflight 不校验 staging 的版本变量

**位置**：`release.yml:34-48` 只比 `tauri.conf.json` / `Cargo.toml` / `package.json`；staging 版本分散在 `Makefile:191-194`、`scripts/stage-runtime.sh:15-18`、`windows-portable.yml:18-27`（`workflow_dispatch`）与 `:41-55`（`workflow_call`，同一组默认值写了两遍）。

当前四处一致（22.23.2 / 0.1.5-rc.2 / 12.3.4 / 1.46.1），但**没有任何门禁**：`release.yml:235-236` 调 windows-portable 时只传 `build_ref`。

**影响**：升级 dsh/Node 时改了 Makefile 却漏改 workflow 默认值，同一 tag 下 Windows 便携包与 macOS/Linux 包会带不同版本的 dsh/Node，而 Release 正文与 README 仍宣称单一版本。

**建议修法**：preflight 一并 grep 出四处版本并两两比对；更彻底的做法是删掉 workflow 默认值，改为从 Makefile 解析，只保留显式覆盖。

### 5.7 B7：identity 模块无平台条件编译，Windows 上走不到却没有编译覆盖

**位置**：`src-tauri/src/identity.rs:70-89`（`stdout_within(Command::new("ps") …)`）、`:122-126`（`classify_parent` 同样只用 `ps`）；全文除 `:159` 的 `#[cfg(test)]` 外**没有任何 cfg**。

该模块被 `lib.rs:1511` 的自愈分支调用，而 Windows 上 `ps` 不存在：`stdout_within` 返回 `None` → `looks_like_our_orphan` 记日志并返回 false → `SelfHeal::Clear`。行为是安全的（保守：不误杀），但这段代码在 Windows 上**永远走不到**，也不会被 `cargo clippy --target x86_64-pc-windows-gnu` 约束（`harness.rs:533-548` 的同类 `ps` 调用就有 `#[cfg(unix)]`/`#[cfg(windows)]` 分区，是可对照的写法）。

**建议修法**：给 `ps` 分支加 `#[cfg(unix)]`，并为 Windows 提供显式的「不做判定」实现，让两端都在编译面上成立。

### 5.8 C1：更新前停止实例不在恢复路径上

**位置**：`lib.rs:1918-1924`（`require_tested_dsh` 为 true 时 `return Err`）与 `:2007`（`resolve_foreign_action`）。

插件更新（`update_flow.rs:78`）与核心暂存（`lib.rs:1754-1784`）都发生在版本闸门**之前**并各自停过实例，而闸门一旦拒绝就 `return`。此时没有任何东西停止正在服务的实例；随后的自启（在被拒绝的版本允许启动时，或下一次启动时）拿不到端口，失败还会被 `mark_core_attempt_failed` 算进退避窗口。

**建议修法**：把版本闸门提到 3b 之前，避免为一次注定不启动的启动做完整安装；或至少在拒绝时不再把这次拒绝写成版本失败。

## 6. P3 与清理项

| # | 位置 | 问题与建议 |
|---|---|---|
| C2 | `window.rs:1410`/`:1417`、`:1173-1181` | 帧探测超时 5 s 之后才做活动探测（再 5 s），而 `Silent` 页面上 `page_is_busy` 几乎必然是 false。建议仅在 `state != Silent` 时探测，或把两次探测合并到同一次 eval |
| C3 | `window.rs:1653-1658` | `~/Downloads` 硬编码：Linux 的 XDG 与 Windows 的「已知文件夹」失效；`home_dir()` 返回 None 时退到 `PathBuf::from(".")`（进程 cwd）。建议用平台下载目录 API，取不到再退 `home_dir()/Downloads`；None 时拒绝下载并记日志 |
| C4 | `window.rs:1674-1684` | 1000 个候选耗尽后 `return dir.join(name)`，而 `name` 正是已知存在的那个文件 → 静默覆盖；且 `exists()` 检查是 TOCTOU。建议用尽后返回带时间戳/随机后缀的名字，或返回 false 拒绝下载 |
| C5 | `index.html:141` + `:54` | `toggle("error", Boolean(pending[1]))` 用的是 detail 而不是第三槽的终态标志，而 `lib.rs:2172-2185` 等大量进行中状态都带 detail → spinner 被隐藏。改为 `Boolean(pending[2])` |
| C6 | `window.rs:823-829` | `size_for_question` 只放大不还原；选「浏览器打开」或「取消」后 splash 留在 520×560。建议终态或清空提问时设回 `SPLASH_SIZE` |
| D1 | `release.yml:127` | cache key 的 `hashFiles` 未包含 `scripts/check-runtime-stage.sh`。补进 `hashFiles` 列表 |
| D2 | `windows-portable.yml:126-129` | 只跑 `cargo test --lib`；其它平台是 `make fmt-check` + `clippy` + `test`。建议 Windows 也跑 `make fmt-check` 与 `cargo clippy --all-targets -- -D warnings` |
| D3 | `release.yml:364-367` | 已存在的 Release 只 `--clobber` 文件，不刷新正文。补 `gh release edit "$TAG" --notes-file notes.md` |
| D4 | `write_third_party_notices.py:104-105` | `UNKNOWN` 只 `print` 计数并 `return 0`。建议 `unknown > 0` 时 `return 1`。当前实际为 0，属预防性 |
| D5 | `Makefile:166-171` vs `:207-210` | win tarball 分支以 `findstring windows,$(TARGET)` 判定，而 `windows:` 用 `WINDOWS_TARGET`、从不设 `TARGET` → 永不生效。改为同时看两者，或在目标里显式传 `TARGET=$(WINDOWS_TARGET)` |
| D6 | `webkit_compat_shim.rs:64-68`、`:107-110` | node 不在 PATH 时 `eprintln!` 后返回空串，调用方只 `println!` 不校验 → 3 个用例 0 断言仍报 ok。改为与 `update_live.rs` 一致：显式 require 或断言输出非空 |
| D7 | `window/tests.rs:706`、`webkit_compat_shim.rs:69/111` | 固定 `temp_dir()/dsh-desktop-<name>`，又各自 `remove_dir_all` → 同进程并行或同 runner 多 job 并发时互删。改用进程 id / 原子计数器后缀 |
| E1 | `locator.rs:18-30` | `locate()` 只被自己的测试调用（`lib.rs` 里只剩一句注释提到它）。若确认无未来用途，连同测试一起删；`Config` 的 `#[serde(default)]`（`lib.rs:73-138`）也是死路径 —— 结构体从不由 `Deserialize` 构造，默认值全部来自 `:299-315` 的显式字面量 |
| E2 | `.gitignore` | 加 `.DS_Store`（仓库里已有未跟踪副本） |

## 7. 已确认无问题的部分（避免重复怀疑）

- `bundle.resources` **只**由 `bundle-bundled` 通过 `--config` 注入（`Makefile:225-230/277-278`），基座 `tauri.conf.json` 里没有该键；实测精简包 3 MB vs 自带运行时 .app 598 MB。半 staging 不会被普通 `make bundle` 打进包。
- `runtime-fetch` / `stage-runtime.sh` 每次都重新下载 `SHASUMS256.txt` 并与 `runtime.lock` 做 `grep` + `cmp` + `shasum -c`；CI 缓存命中只省下载，不改信任链。
- 交叉编译的目标架构正确（逐平台核对 `darwin-x64` / `darwin-arm64` / `linux-arm64`）。
- 无 `pull_request_target` / `workflow_run`，`github.ref_name` 只经 `env` 后加引号使用；`permissions: contents: write` 为最小必要权限。
- 外链 scheme 白名单（`may_open` 只放 http/https，`Url::parse` 归一化 scheme 大小写）、`on_navigation` / `on_new_window` 围栏、`NewWindowResponse::Deny` 均正确；Windows 走 `ShellExecuteW` + 专用线程 + COM 配对初始化，不经 `cmd`，无注入面。
- 兼容层逐块自守卫、ES5 纪律、`Uint8Array.fromBase64` 的空白正则、`Math.sumPrecise` 的非有限值短路、`findLast`/`findLastIndex`/`Object.hasOwn`/`Symbol.dispose` 均正确；`webkit_compat_shim.rs` 的 driver 会先**剥掉真实 API** 再跑，是有效覆盖（与 B3 那条空跑测试形成对比）。
- CSP 未被绕过：`script-src self` 禁止 `new Function`，页面用 `<script>` 元素做语法探针并在同一元素里放标志位，Rust 侧测试绑定两者。
- `present_status_page` 的「先建 splash 再 destroy harness」顺序与其注释一致（依赖侧 `Destroyed → 窗口表空 → ExitRequested` 链路已核对）。
- 上一轮复核的 R1–R5、N1–N7、E1–E3 逐条在位，未发现回退。

## 8. 待实机验证（本轮未下结论）

1. **相对符号链接逃逸**：`check-runtime-stage.sh:74-81` 只判绝对链接。本轮实测 `ln -s ../../../outside/secret.txt <staging>/node/bin/escape`（目标在 staging 之外且存在）时闸门 **exit 0**，即检查确实放过它；但打包解引用后是否真把宿主机文件带进 .app，需要在有真实 staging 的机器上确认（tauri 资源复制是解引用复制：已构建的 .app 里 18 个链接变成 0 个，支持这一判断）。
2. **`Silent` 不看 `attended`**：`eval_with_callback` 走事件循环，主线程被同步阻塞 >5 s（`LIVENESS_TIMEOUT`）时探测提交不出去 → `Silent`，两个周期后重载，可能丢掉未提交的输入。`busy` 只在 15 s 内有输入时救场。
3. **最小化/完全遮挡**时 WebKit/WebView2 是否停帧或停答；若停答则会耗尽重载预算并把健康页面标成「请重启应用」。
4. **`create_harness` 同 label 竞态**实际命中「新窗口从表里消失」还是「WindowLabelAlreadyExists」（见 B1）。

## 9. 与文档/README 的事实偏差

| 位置 | 偏差 |
|---|---|
| `docs/design-task-feat-update-transaction.md:89` | 仍写「`stop_instance_before_update()` 仍在最前」，已被 `64a2041` 改掉（见 A1）。按设计文档生命周期规则，应新建 supersede 说明或补一段修正记录，不原地改写结论 |
| `README.md:227` | 称自检「验证闸门本身有效」，实测只覆盖 2/7 条（见 B5） |
| `README.md:231` | 「31,090 个文件」为历史实测数字，当前 staging 为 31,046 |
| `Makefile:207` | 注释称 `make windows` 会切换 win tarball，实际条件永不成立（见 D5） |
| `docs/dsh-desktop-b0d5405-code-review.md` §9 第 23 项 | 标为「已修」，但 `Iterator#reduce` 那条修复没有生效的测试（见 B3） |

## 10. 建议修复顺序

1. **A1**（P1，功能损坏）
2. **A2 + B5**（P1/P2，发布闸门，两条一起改）
3. **B1**（三个看护的世代栅栏一起改）
4. **B2**（一行 `.catch` + 一个 rejected-Promise 用例）
5. **B3**（测试空跑，掩盖真实回归）
6. **B4 / B6 / B7 / C1**
7. **C2–C6**
8. **D1–D7**
9. **E1 / E2**
10. 最后同步第 9 节的文档偏差。

## 11. 处理状态

**已全部处理（2026-09-17）**，各项原始结论未改动。修复过程中的两处偏差在 §11.2 说明。

| # | 状态 | 修法 |
|---|---|---|
| A1 | **已修** | 新增纯函数 `swap_blocked(kept_conflict, raced, raced_original)`，并补上第三个输入：`start()` 记下 `configured_port`（在提问搬走 `port` 之前），3d 切换前探测**原端口**，仍有 Harness 时放弃本次切换。测试 `only_a_clear_field_lets_the_staged_tree_be_committed` 覆盖八格 |
| A2 | **已修** | `check-runtime-stage.sh` 新增 `first_exec_of()`：node / pnpm 判 `[ -x ]`（Windows 判 `.exe`/`.cmd` 且非目录）。实测 `chmod -x` 后闸门由 exit 0 变为 exit 1 |
| B1 | **已修** | 栅栏前置到 `get_webview_window` 之后、任何副作用之前（`may_act(generation, current)`，纯函数 + 测试）；`watch_first_load` 也接收 generation；`create_harness` 在 `destroy()` 后新增 `wait_for_label_release()`（2 s 有界等待，超时返回 `WindowLabelAlreadyExists` 而不是建在同一 label 上） |
| B2 | **已修** | `index.html` 的接管按钮改为 `invoke(...).catch(failed)`（同步抛错走原 `catch`）。新增集成用例 `a_choice_that_never_reaches_rust_says_so`（driver 让 `invoke` 返回 rejected promise）；删掉 `.catch` 后该用例确实失败 |
| B3 | **已修** | 键改为 `"Iterator"`，并在注入前删除 `globalThis.Iterator` 与 `%IteratorPrototype%.reduce`，注入后先断言 `Iterator` 真的被装回。**变异测试验证**：删掉垫片里的 `index = 1` 后该用例报 `left: "0,1"` |
| B4 | **已修** | `show_failure` / `show_notice` 改用 `FAILURE_SHOWN.load()`，闩只在 `present_status_page` 成功之后置位；失败时显式复位 |
| B5 | **已修** | 自检为每条闸门各造一个反例（可执行位、绝对链接、悬空链接、模板内 store、文件数、体积、quarantine），并保留「干净目录必须通过」的正例。**反向验证**：把可执行判定改回旧行为后自检立刻失败 |
| B6 | **已修** | `release.yml` 的 preflight 新增 staging 版本一致性校验（Makefile / stage-runtime.sh / windows-portable.yml 的 `NODE_VERSION` 与 `DSH_VERSION`）。**本地演练**：改掉 Makefile 的 `DSH_VERSION` 后 preflight 报错退出 1，还原后通过 |
| B7 | **已修** | `identity.rs` 把 `ps` 调用拆为 `ps_identity()`（`#[cfg(unix)]`）与 `parent_command()`（Windows 返回 `None` → `Parent::Live`），`PS_TIMEOUT` / `Duration` 也随之分区。复核后修正了原判断：枚举与纯函数保留在两端编译，只有 `ps` 调用本身是 Unix 专属 |
| C1 | **已修** | 版本闸门（3b2）**前移**到更新检查（3b）之前：会被拒绝启动的版本不再触发一次 290 MB 的暂存，插件分支也不会先停实例再放弃 |
| C2 | **已修** | 活动探测只在 `PageState::Frozen` 时进行（`Silent` 的页面答不出 JSON，问它只是白等 5 s） |
| C3 | **已修** | `downloads_dir()` 改为 `Option`：先看 `XDG_DOWNLOAD_DIR`，再看 `USERPROFILE/Downloads` 与 `home_dir()/Downloads`，全无时拒绝下载并记日志（不再落到进程 cwd） |
| C4 | **已修** | 1000 个候选用尽后返回带时间戳的名字，不再返回已知存在的原名。测试 `an_exhausted_name_search_never_reuses_a_taken_file` 造满 1000 个同名文件后断言新路径不存在 |
| C5 | **已修** | `index.html` 的 `.error` 改用第三槽的终态标志（`pending[2]`），进度页不再隐藏 spinner |
| C6 | **已修** | 新增 `size_after_question()`：提问答复后把窗口缩回 `SPLASH_SIZE` |
| D1 | **已修** | 两个 workflow 的 cache key `hashFiles` 均加入 `scripts/check-runtime-stage.sh` |
| D2 | **已修** | Windows 作业的门禁改为 `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets` |
| D3 | **已修** | 已存在的 Release 改为 `gh release edit --notes-file notes.md`，正文不再停留在首次创建时的版本 |
| D4 | **已修** | `write_third_party_notices.py` 在有 `UNKNOWN` 时返回 1（当前实际为 0） |
| D5 | **已修** | 判据改为 `$(TARGET)` 含 windows **或** `$(MAKECMDGOALS)` 含 windows（`WINDOWS_TARGET` 有 `?=` 默认值，不能用它判定）；`NODE_ARCH` 也按同一 triple 选择。四种调用方式实测：默认 darwin-arm64、`TARGET=x86_64-apple-darwin` → darwin-x64、`TARGET=…windows-gnu` 与 `make windows` → win-x64 |
| D6 | **已修** | 集成测试的 `require_node()` 在缺 node 时 panic（不再返回空串）；四个调用点补 `assert!(!stdout.is_empty())`；`window/tests.rs::run_shim` 同样改为 panic / 断言退出码 |
| D7 | **已修** | 新增 `crate::test_dir()`（名字带进程 id）与集成测试内的同名 helper，43 处固定路径全部替换 |
| E1 | **已修（更正）** | 复核推翻了本文原判断的一半：`locate()` 确实只被测试调用（保留并补注释说明它仍是候选顺序与软链解析的唯一入口），但 `Config` 的 `#[serde(default)]` **不是**死路径 —— `Config::load` 会反序列化用户的 `config.json`，`{"port": 4321}` 这类部分配置正依赖它 |
| E2 | **已修** | `.gitignore` 增加 `.DS_Store` |

### 11.1 顺带修正的文档偏差

- `README.md`：测试计数 185 → 188、集成 6 → 7、app 内文件数 31,090 → 31,064；闸门清单补上「可执行位」与「模板内 pnpm store」，并写明自检对每条闸门都有反例；同时说明集成测试缺 node 时会失败而不是跳过。
- `docs/design-task-feat-update-transaction.md` §2.5：按设计文档生命周期规则**保留原文**，追加一段 2026-09-17 的补正并交叉引用本文 A1。
- `docs/dsh-desktop-b0d5405-code-review.md`：追加 §11，记下第 23 项（`Iterator#reduce`）当时并未被测试覆盖这一事实。
- `Makefile` 的 win tarball 注释改为与实现一致（见 D5）。

### 11.2 与本文建议的差异

- **A1** 采用了「把原端口竞态并入 3d 守卫」的最小方案（本文建议里的第一条），没有改成「恢复 3c 之前停实例」：后者会重新引入 v0.4.1 第 7 项修掉的那次无谓停止。
- **B7** 的实现比本文建议更保守：Windows 上 `Parent` 枚举与 `parent_from_ps` 仍然编译（避免 `dead_code`），只有 `ps` 调用被 `cfg` 掉并返回 `None`，语义上等价于原来「Windows 上永远走不到」的行为。
- **E1** 只保留 `locate()` 并加注释，没有删除：删除会同时删掉三个用例的断言入口，收益低于损失。

### 11.3 验证

在装有 Rust 1.98.1 与 node 22.23.2 的 macOS 上执行：

- `cargo test`：**188 passed / 0 failed**（本轮新增 3 项：`only_a_clear_field_lets_the_staged_tree_be_committed`、`a_watcher_stops_acting_once_its_window_was_rebuilt`、`an_exhausted_name_search_never_reuses_a_taken_file`），集成 **7 passed**（新增 `a_choice_that_never_reaches_rust_says_so`）；
- `cargo fmt --check` 通过；
- `cargo clippy --all-targets -- -D warnings` 在 host 与 `x86_64-pc-windows-gnu` 两个目标上均 0 warning；
- `make test-scripts` 通过；`scripts/check-runtime-stage.sh src-tauri/runtime` 通过（31046 文件 / 519 MB）；
- 反向验证（每条都实际跑过，改回旧行为后确实失败）：可执行位闸门、`.catch` 缺失、垫片的 `index = 1`、preflight 的版本漂移。

### 11.4 遗留

- **仍需实机验证**：相对符号链接逃逸（§8.1）、`Silent` 与主线程阻塞（§8.2）、最小化/遮挡时的判定（§8.3）、`create_harness` 同 label 竞态实际命中哪种交错（§8.4）。这四项都要真实窗口，本机静态验证无法覆盖。
- **Windows / Linux 实机**：本轮所有 `cfg(windows)` 改动只到交叉编译 + clippy。


