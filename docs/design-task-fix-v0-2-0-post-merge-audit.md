# 代码审查结论：v0.2.0 合并后（插件市场自动更新 + 全平台自带运行时 CI）

> 审查日期：2026-09-14 ｜ 分支 `main` ｜ 基准 commit `e91a668`（v0.2.0）
> 审查范围：`feat/bundled-runtime` 合并进 main 之后的新增部分 ——
> 插件市场自动更新（`9508e40`）、全平台自带运行时 CI（`922a604`、`768ef8a`）、
> 以及两份既有审查的修复在合并后是否完好
> 姊妹文档：[`design-task-fix-desktop-shell-audit.md`](./design-task-fix-desktop-shell-audit.md)（main 桌面壳审查，11 + N1–N6 + R1）、
> [`design-task-fix-bundled-runtime-audit.md`](./design-task-fix-bundled-runtime-audit.md)（自带运行时审查，16 项）
>
> 本文只记录**本轮新发现**与**合并完整性核验**；两份姊妹文档里已结案的条目不重复。

---

## 0. 结论摘要

合并本身是干净的：两轮审查的全部修复都在，`#[cfg(windows)] compile_error!` 也已按"Windows 成为正式产物"
正确移除。新增的 CI 比建议做得更严。问题集中在**新功能**上 —— 插件市场自动更新在 GUI 启动下必然失败，
并且每次启动都会先停一次 Harness。A1/A2 已在同一轮修掉并由单测 + 命令行复现守住，见 §10；
复核那次修复时又发现两处小项（A5/A6），见 §11 —— **A5 已按"短期失败标记"修掉（§11.3），A6 只记录备查**。

| # | 级别 | 问题 | 位置 | 处理状态 |
|---|---|---|---|---|
| A1 | P1 | 插件市场自动更新在 GUI 启动下必然失败（PATH 上没有 pnpm），且每次启动都先停一次 Harness、失败不记 attempted | `src-tauri/src/update.rs::install_plugin`、`lib.rs:1221` | **已修**（§10.1） |
| A2 | P2 | 首启播种后立刻联网装插件市场，与"离线首启"目标冲突 | `src-tauri/src/lib.rs:1075` 与 `1221` | **已修**（§10.2） |
| A3 | P3 | `system_updates: notify` 与 `auto_update_plugins` 策略不一致：前者承诺不动用户的安装，后者仍重写用户 profile | `src-tauri/src/lib.rs:1221` 一带 | **已修**（2026-09-15，见 §4 注） |
| A4 | P3 | `is_session_supervisor` 仍是子串匹配（上一轮已提出，未改） | `src-tauri/src/lib.rs::is_session_supervisor` | 可不改 |
| A5 | P3 | 插件安装的 `Err` 分支记 attempted，与 `carried_attempt` 叠加后把"瞬时失败"变成"永久不再尝试" | `src-tauri/src/lib.rs`（3b3 的 `Err` 分支）、`update.rs::carried_attempt` | **已修**（§11.3） |
| A6 | P3 | 复用分支现在也要等登录 shell 抓取（`ChildEnv` 前移的副作用） | `src-tauri/src/lib.rs`（3b4） | 记录备查（§11.2） |

**独立验证**：`cargo test` **71 passed / 0 failed**、`cargo clippy --all-targets -- -D warnings` **0 warning**、
`cargo fmt --check` 通过。

---

## 1. 合并完整性核验（结论：全部完好）

### 1.1 两轮修复的关键符号都在

| 来源 | 符号 | 位置 |
|---|---|---|
| 桌面壳 #1 | `may_open` | `window.rs` |
| 桌面壳 N1 | `self_heal_action` | `lib.rs` |
| 桌面壳 N2/R1 | `classify_parent` / `is_session_supervisor` | `lib.rs` |
| 桌面壳 #3 | `stop_mode` | `lib.rs` |
| 桌面壳 #4/N4 | `carried_attempt` | `update.rs` |
| 桌面壳 #5 | `validate_paths` | `harness.rs` |
| 桌面壳 #6 | `home_workspace` | `lib.rs` |
| 桌面壳 #7 | `dsh_path` | `lib.rs` / `locator.rs` |
| 桌面壳 N3 | `LoadSignals` | `window.rs` |
| 桌面壳 N6 | `FAILURE_SHOWN` | `window.rs` |
| 桌面壳 N5 | `Logger.written` | `harness.rs` |
| 桌面壳 #11 | `make fmt-check` | `Makefile` / `release.yml` |

`#[cfg(windows)] compile_error!` 已移除 —— 合并后 Windows 是正式产物，这条拒绝构建的护栏正确退场。

### 1.2 自带运行时审查里风险最高的三项，抽查为真修

- **P0-1 staging 闸门**（实跑）：只放一个 `node/bin/node` 的假目录 → 退出码 **1**，输出
  `文件数 1 超出闸门：staging 不完整，或混进了不属于本平台的残留` + `staging 校验失败`；
  `check-runtime-stage.sh --self-test` → 退出码 **0**。首轮那个"缺 8 项仍然校验通过"的形态不再出现。
- **P0-2 更新收尾**：`installed_cli(prefix, supervised, fallback)` 按安装前缀重新定位新树并回读它的版本，
  版本确实变了才 `resolved.dsh_js = installed_path` + `just_updated = true`；`update::install` 也传了
  `--cache <app-data>/runtime/npm-cache`（P2-11）。
- **§10 的打包陷阱**：`tauri.conf.json` 里既没有 `resources` 也没有 `11.0`，两者只在
  `bundle-bundled` 时由 `BUNDLED_CONFIG_JSON` 经 `--config` 注入 ——
  普通 `make bundle` 不可能再把残留 staging 静默打进包。

### 1.3 新 CI 比建议更严（无需改动）

- 每个平台出"精简版 + 自带运行时版"两种产物，自带版按 **TARGET** 平台 staging（arm64 runner 上交叉编译
  x64 时不会误装 arm64 的 node）；
- 两道产物断言：macOS 检查 `Contents/Resources/runtime/node/bin/node` 存在，Linux 按
  "bundled 体积 > 精简版 × 3" 拦住 resources 没生效的情况；
- staging 缓存只缓存**输入**（Node 包 + npm/pnpm store），key 带 `matrix.suffix` 与
  `hashFiles(runtime.lock, Makefile, scripts/*)`，且 `runtime-fetch` 仍按 `src-tauri/runtime.lock` 校验 SHA256 ——
  缓存命中只省下载，不参与信任链。

---

## 2. A1（P1）：插件市场自动更新在 GUI 启动下必然失败

**位置**：`src-tauri/src/update.rs::install_plugin`、调用点 `src-tauri/src/lib.rs:1221`（步骤 3b3）

`install_plugin` 自己拼子进程 PATH：

```rust
.env("PATH", npm_path(node, std::env::var_os("PATH").as_deref()))
```

即 **node 所在目录 + 应用自己的 PATH**。而 `dsh plugin add` 是转发给 **pnpm** 的，pnpm 不在这条 PATH 上：

| 本该提供 pnpm 的来源 | 是否在 `install_plugin` 的 PATH 里 |
|---|---|
| 随包分发的 `<seed>/tools/bin`（自带运行时的既定落点，自带方案 §4） | ❌ |
| 可写落点 `app-data/runtime/tools/bin` | ❌ |
| 登录 shell 的 PATH | ❌ —— 它在 `start()` 第 3d 步（`lib.rs:1419`）才 join，而插件检查在 3b3（`lib.rs:1221`），跑在它前面 |
| GUI 继承的 launchd PATH | ❌ 不含 Homebrew |

**实测**（仓库中已 staging 的自带 node + 自带 dsh，隔离 `DSH_HOME`，PATH 只给 `node/bin:/usr/bin:/bin`）：

```text
dsh: initialized profile web at …/dshhome/profiles/web
dsh: pnpm not found on PATH — install pnpm to manage profile plugins
```

**影响**不止"功能不生效"：

1. 只要 registry 上有更新的 dshmarket，每次启动都会先走 `stop_instance_before_update`
   —— **把 `SelfHeal::Keep` 刚保住的实例停掉**（姊妹文档 N1 修复的收益被这一步抵消），然后安装失败；
2. 失败走 `Err` 分支，**不调用 `mark_plugin_attempt_ineffective`**，缓存因此不抑制，下次启动原样重来。
   核心更新那条路径有这层保护（`update installed but the supervised CLI is still …` → 记 attempted），
   插件这条没有。

**修法**（按收益排序）：

- 把第 3d 步的子进程环境组装**提到更新步骤之前**，让 `install_plugin` 与 npm 调用复用同一份 merged PATH
  （含 seed / app-data 的 `tools/bin`、登录 shell PATH）以及 `npm_config_prefix` / `PNPM_HOME`；
  这同时让核心更新的 npm 调用也拿到登录 shell 的 PATH；
- 安装前先探一次 pnpm 是否可解析，不可解析就跳过并记日志 —— **尤其不要停实例**；
- `Err` 分支也记一次 attempted（或记一个"本窗口内已失败"的标记），避免每次启动重复整套动作。

**回归**：`install_plugin` 的 PATH 组装抽成纯函数并加单测（断言 seed 的 `tools/bin` 在其中）；
再补一个"pnpm 不可解析时不触发 `stop_instance_before_update`"的策略级用例。

**已按此修掉（2026-09-14）**：修法与验证见 §10.1／§10.4。补充一处实测细节：`dsh plugin` 是
`spawnSync("pnpm", …, { shell: false })`，**相对** PATH 项解析不到 —— 复现时把 `tools/bin` 加进 PATH
必须用绝对路径（§7 的命令已按此更正），组装出的 PATH 本来就是绝对路径。

---

## 3. A2（P2）：首启播种后立刻联网装插件市场

**位置**：`src-tauri/src/lib.rs:1075`（`seed_profile_template`）与 `1221`（步骤 3b3）

干净机器首次启动的顺序是：播种模板（内含固定版本的 dshmarket）→ 紧接着查 registry →
有新版就装。于是首启会产生一次静默下载，自带运行时方案 §1／§4 的"首启不下载"承诺不再成立
（断网时只是失败并记日志，不致命）。

**修法**：`seed_profile_template()` 已经知道自己有没有播种，把这个信息返回给调用方，
**本轮跳过插件检查**（下次启动再更新）。顺带让"首启流量"这条验收项重新成立。

**已按此修掉（2026-09-14）**：见 §10.2。核心 `dsh` 的更新检查未动（不属本轮范围）。

---

## 4. A3（P3）：两个更新开关的策略不一致

`system_updates: notify` 的语义是"这是用户自己管的安装，壳不要写它"。但
`auto_update_plugins`（默认 true）不看运行时来源，照样重写用户 `~/.dsh/profiles/web` 的
`package.json` 与 lockfile。

两条路可选，需要产品侧拍板：用同一个偏好收口（系统运行时 + `notify` 时也不动 profile），
或保留现状但在 README 写明这条例外。

> **2026-09-15 已修（取第一条路）**：`auto_update_plugins` 默认改为 **`false`**，于是「不动用户安装」这条
> 承诺默认成立，两个开关不再矛盾；想保持 profile 自动更新就显式写 `true`。
> 该改动随 v0.4.1 代码审查的修复一并落地，见 `docs/dsh-desktop-v0.4.1-code-review.md` 第 2 项。

---

## 5. A4（P3）：`is_session_supervisor` 仍是子串匹配

```rust
command.contains("launchd") || command.contains("systemd") || command.contains("init")
```

`contains("init")` 会命中 `python init_worker.py`、`/home/u/init-scripts/run.sh` 这类命令行。
要真造成误杀需同时满足：state.json 里的 pid 被复用、复用者的命令行含 `--profile web` + `dsh`、
且其父进程命令行恰好含 `init`/`systemd` —— 实际不可能，可以不改。
要收紧就取命令行第一个 token 的 basename 比对集合（`launchd` / `systemd` / `init` / `dumb-init`）。

---

## 6. 建议顺序（2026-09-14：1、2 已完成，见 §10）

1. ~~**A1** —— 一个刚上线的功能 100% 不工作，且每次启动多停一次 Harness；~~ ✅ 已修
2. ~~**A2** —— 与 A1 同一块代码，一起改成本最低；~~ ✅ 已修
3. **A3 / A4** —— 定策略与可选收紧（未动：A3 需要产品侧决定，A4 可保留）。

---

## 7. 验证方式

- **单测**：`make test`（当前 71 passed）。新增用例建议见 §2／§3 的"回归"。
- **命令行复现 A1**（本轮实测所用）：

  ```bash
  R=$PWD/src-tauri/runtime
  env -i HOME=$HOME DSH_HOME=/tmp/dsh-probe PATH=$R/node/bin:/usr/bin:/bin \
    $R/node/bin/node $R/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js \
    plugin --profile web --help
  # => dsh: pnpm not found on PATH — install pnpm to manage profile plugins（exit 127）
  ```

  修好之后，同一条命令把 `$R/tools/bin` 加进 PATH 应输出 pnpm 的帮助。注意要用**绝对路径**：
  `dsh` 内部是 `spawnSync("pnpm", …, { shell: false })`，相对 PATH 项解析不到（实测仍然报
  `pnpm not found`）。
- **实机**：干净 `DSH_HOME` 首启 → 日志里不应出现插件安装（A2）；
  有新版 dshmarket 时启动 → 不应出现"停实例 → 安装失败"的循环（A1）。
- **闸门**：`sh scripts/check-runtime-stage.sh --self-test` 退出码 0；
  对缺内容的目录退出码非 0（本轮已实测）。

## 8. 验收标准

- [ ] 自带运行时的干净机器上，插件市场能真正升级（日志出现 `plugin updated: dshmarket A -> B`）——
  命令行等价路径已实测通过（§10.4），只差 GUI 实机；
- [x] pnpm 不可解析时跳过插件更新，且**不停止**正在运行的 Harness（策略用例覆盖，且判定发生在
  `stop_instance_before_update` 之前）；
- [x] 插件安装失败后，同一缓存窗口内不再重复尝试（`Err` 分支现在也写 attempted）；
- [ ] 干净机器首启不产生插件市场的网络安装 —— `SeedOutcome.seeded` 那道门已由单测守住，GUI 实机待做；
- [x] 两份姊妹文档里已结案的条目在后续提交中不回退（`make test` 75 passed）。

## 9. 本次未覆盖的范围

- 未做实机 GUI 验证与联网测试（`make test-live`）；
- 未审查 macOS 签名/公证链路（仍未做）与 Linux 侧的实机安装；
- 未逐行复核自带运行时审查 16 项中其余 12 项（本轮只抽查了风险最高的三项，见 §1.2）；
- ~~未验证 v0.2.0 的实际发布产物（CI 只做了静态审查）。~~ **2026-09-14 已补**：v0.2.0 已发布，
  10/10 资产的 GitHub 服务端 sha256 与 `SHA256SUMS` 逐条一致，真实下载校验通过；CI 侧 Linux staging
  （31072 文件 / 562 MB）、macOS x64 交叉 staging 用的是 `darwin-x64` 的 node、两道产物断言与
  staging 缓存命中也都在那次真实发布里跑过。

---

## 10. 修复记录（2026-09-14，`main`）

A1 与 A2 一起修（同一块代码），A3／A4 未动。

### 10.1 A1：让插件安装跑在与 Harness 同一份环境上

| 项 | 改法 |
|---|---|
| 环境组装提前 | 原先 3d 的"登录 shell 环境导入 + PATH／工具前缀组装"整块上移到 **3b4**（`lib.rs::start`，核心更新之后、插件更新之前），产物是 `ChildEnv { path, vars }`；`install_plugin` 与 harness 子进程从此共用同一份 PATH，不再出现"检查用一套工具链、安装用另一套" |
| 纯函数化 | `tool_path_prefix(node, seed, bundled, data_dir)` 只产出前缀（node 目录 → `<app-data>/runtime/tools{,/bin}` → `<seed>/tools{,/bin}` → Homebrew／System32），`ChildEnv::assemble(config, node, seed, bundled, data_dir, imported)` 负责 `shellenv::merge_path`、`npm_config_prefix`／`PNPM_HOME` 与 `config.env` 覆盖 |
| `install_plugin` 不再读进程环境 | 新增 `path: &OsStr` 参数，内部改成 `npm_path(node, Some(path))`；调用点传 `OsStr::new(&child.path)` |
| 安装前先解析 pnpm | 新增 `update::find_pnpm(path)`（按平台取 `pnpm` / `pnpm.cmd` / `pnpm.exe` / `pnpm.bat`，且必须是**可执行文件**：Unix 看 exec 位）。配合 `plugin_skip_reason(installed, pnpm)`：缺 pnpm 时只记 `PATH 上没有 pnpm，跳过插件市场更新（不停止正在运行的 Harness）`，**不进入 `stop_instance_before_update`**，也省掉一次 registry 查询 |
| 失败也记 attempted | `Err` 分支补 `update::mark_plugin_attempt_ineffective(data_dir, &to)`，与"装了但 profile 仍加载旧版"分支一致，同一缓存窗口内不再重放整套动作 |

顺带的行为变化：`child PATH = …` 这行日志提前到更新检查之前（原来在 harness spawn 前）；
登录 shell 的抓取仍然与版本读取／registry 查询并行，因此没有增加启动延迟。

### 10.2 A2：播种过的那一轮不查插件市场

`seed_profile_template` 改为返回 `SeedOutcome { seeded, note }`，`start()` 记下 `seeded_this_run`：
插件块拆成两支 —— 播种过的一轮只记 `刚播种 profile 模板，本轮不检查插件市场（下次启动再查）`，
其余情况照旧。核心 `dsh` 的更新检查未动（不属本轮范围）。

### 10.3 新增测试

`make test` → **75 passed**（71 + 4）：

| 用例 | 覆盖 |
|---|---|
| `update::tests::find_pnpm_takes_the_first_executable_match_on_path` | 可执行位、同名目录、空 PATH、无 PATH |
| `tests::a_missing_pnpm_skips_the_plugin_step_before_anything_is_stopped` | 策略三分支（缺 pnpm／未安装／两者齐备）与日志文案 |
| `tests::the_child_path_carries_the_bundled_tools_that_hold_pnpm` | 自带的 `tools/bin` 在 PATH 上、`PNPM_HOME`／`npm_config_prefix` 指向可写前缀、PATH 变量与 `child.path` 一致；系统运行时不被塞入我们的工具链 |
| `tests::seeding_reports_itself_so_the_first_launch_stays_offline` | 首启 `seeded = true` 且模板落盘、第二次为 `false`（A2 那道门就靠它） |

### 10.4 验证

- `make fmt-check` 通过、`make clippy` 0 warning、`make test` **75 passed**；
- **命令行端到端**（用仓库里已 staging 的自带运行时，PATH 按 `ChildEnv::assemble` 的顺序
  node/bin → tools/bin → tools，全部绝对路径）：

  ```bash
  R=$PWD/src-tauri/runtime
  env -i HOME=$HOME DSH_HOME=/tmp/dsh-a1-home PATH=$R/node/bin:$R/tools/bin:$R/tools:/usr/bin:/bin \
    $R/node/bin/node $R/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js \
    plugin --profile web add dshmarket@1.46.1
  # => + dshmarket 1.46.1 / Done in 4.3s using pnpm v12.3.4
  ```

  同一条命令去掉 `tools/bin` 仍是 `pnpm not found on PATH`（exit 127）—— 也就是修复前 Finder 启动会落进的
  那条路；
- **仍未做**：GUI 实机验证（需要一次"registry 上有新版 + Finder 启动"的实跑），以及 A3 的策略决定。

---

## 11. 复核 A1/A2 修复时的新发现（2026-09-14）

对 `efe93fb` 的复核结论：A1/A2 的修法正确、结构上排除了"探到了却装不了"的不一致，
独立复跑 `cargo test` **75 passed**、`clippy` 0 warning、`fmt --check` 通过，
并用仓库里已 staging 的自带运行时实测确认了"加上 `tools/bin` 后 `dsh plugin` 能解析到 pnpm"。
以下两项是复核中新看到的，都不影响上述结论。

### 11.1 A5（P3）：`Err` 分支记 attempted，会把瞬时失败变成永久不再尝试

**位置**：`src-tauri/src/lib.rs` 步骤 3b3 的 `Err` 分支、`src-tauri/src/update.rs::carried_attempt`

A1 的修法之一是"插件安装失败也记 attempted，同一缓存窗口内不重复尝试"。但
`carried_attempt` 的规则是"registry 头版本不变就把 `attempted` 一直带下去"（那是 N4 为
"装了但版本没变"设计的语义）。两者叠加后：

> 一次网络抖动导致的 `dsh plugin add` 失败 ⇒ 插件市场**在 dshmarket 发布下一个版本之前都不再更新**。

对比核心 `dsh` 的更新路径：它只在"装完了但被监管的 CLI 版本没变"时记 attempted，`Err` 不记 ——
因为 `Err` 多半是可重试的。插件这条现在比核心更激进。

**判断**：这是 A1 修复的合理副作用，但可以更贴切 —— "环境坏了"里最常见的那种（PATH 上没有 pnpm）
已经被前置的 `find_pnpm` 探测挡住，不会再走到安装；`Err` 剩下的基本是网络、registry、pnpm 自身的
瞬时故障。

**修法（二选一）**：

- 让 `Err` 沿用核心路径的语义（不记 attempted），把"别反复停实例"完全交给已经存在的 pnpm 探测；
- 或区分两种标记：`attempted`（装了但无效，长期抑制）与一个带短 TTL 的失败标记
  （例如复用 `FAILED_RETRY_MINUTES` 的 5 分钟窗口）。

**顺带**：用户侧的复位手段是删 `<app-data>/plugin-check.json`，而 README 的排障目前只写了核心的
`update-check.json` —— 无论选哪条修法，这一句都该补上。

**已按第二条修掉（2026-09-14）**：见 §11.3；顺带那一句也一并补进 README 了。

### 11.2 A6（P3，记录备查）：复用分支现在也要等登录 shell 抓取

**位置**：`src-tauri/src/lib.rs` 步骤 3b4

`ChildEnv::assemble` 前移之后，登录 shell 捕获线程在 3b4 被**无条件** join；而"复用本应用上次启动的
Harness"会在其后的检测步骤直接返回 —— 这条路径以前不需要等这个结果。

实测成本约 160 ms，上限 8 s（`shellenv::CAPTURE_TIMEOUT` 到期即 kill），可以接受。
记在这里只是为了：以后排查"复用为什么比预期慢"时，不必再从头找这条依赖。

若要优化，可以把 join 推迟到"确定要 spawn"之后，但那会让插件安装重新拿不到登录 shell 的 PATH ——
除非把插件步骤也挪到检测之后。不建议为这 160 ms 动这块顺序。

### 11.3 A5 的修复（2026-09-14）

按 §11.1 的第二个方案做：**区分"装了但没生效"与"安装失败"**。

| 项 | 改法 |
|---|---|
| 缓存结构 | `Cache` 增加 `failed: Option<String>` 与 `failed_at: u64`（都带 `#[serde(default)]`，旧文件照常解析）。长存的 `attempted` 语义不变：环境坏了、重试也没用 |
| 判定 | `Cache::failed_recently(now, status)`：同一个版本 **且** `now - failed_at < FAILED_RETRY_MINUTES`（沿用既有的 5 分钟常量）。`Checked` 新增 `failed_recently: bool`，与 `attempted` 并列，调用点两个守卫各写各的日志 |
| 跨查询续期 | 新增 `carried_failure(previous, latest, now)`：与 `carried_attempt` 一样只在同一版本上续，但**窗口一到就丢**，所以一次瞬时失败不会把某个版本钉到下一次发版 |
| 标记函数 | `mark_plugin_attempt_failed`（写入 `failed`/`failed_at`），替换 3b3 `Err` 分支里原来的 `mark_plugin_attempt_ineffective`；"装了但版本没变"那条分支仍然用长存标记 |
| 日志 | 新增 `plugin dshmarket <ver> failed to install last time; not retrying for 5 minutes`；原 `already attempted and changed nothing` 文案与语义都不变 |

新增两个用例，`make test` **77 passed**（75 + 2）：

| 用例 | 覆盖 |
|---|---|
| `update::tests::a_failed_install_only_suppresses_its_short_window` | 刚失败 → `failed_recently` 为真且 `attempted` 仍为假；窗口过后同一个缓存答案**重新允许安装**；`mark_plugin_attempt_failed` 只写失败标记，不碰长存标记 |
| `update::tests::a_failed_attempt_is_carried_only_inside_its_window` | `carried_failure` 的三个边界：同版本窗口内续期、窗口到期丢弃、换版本丢弃 |

顺带补的文档：README 的排障段现在写明两个缓存文件各管各的（核心 `update-check.json`、插件
`plugin-check.json`），并说明失败只抑制 5 分钟、会自动重试。A6 依 §11.2 的结论不动。
