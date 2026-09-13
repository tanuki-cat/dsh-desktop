# 代码审查结论：自带运行时分支的 16 处问题与修法

> 审查日期：2026-09-13 ｜ 分支 `feat/bundled-runtime` ｜ 基准 commit `e0079a1`
> 审查范围：`src-tauri/src/*.rs`（约 3000 行）、`Makefile`、`scripts/*`、`.github/workflows/*`、`src-tauri/tauri.conf.json`
> 对照文档：[`design-task-feat-dsh-bundled-runtime.md`](./design-task-feat-dsh-bundled-runtime.md)、
> [`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md)
> 姊妹文档：[`design-task-fix-desktop-shell-audit.md`](./design-task-fix-desktop-shell-audit.md)
> （main 分支的审查；其中本文 P0-3 与那份文档的 §3 是同一个问题，两边都要改）
>
> 本文是**审查结论 + 修复任务清单**，不修改上面两份方案文档（它们记录的是当时的决策与实测，按文档生命周期规则保持原样）。
> 修复落地后，把结果回写到本文的"处理状态"列，方案文档只在结论被推翻时才另开 supersede 文档。

---

## 0. 结论摘要

代码与方案的主体是吻合的（运行时解析、播种、PATH 注入、更新落点、Windows 兼容都已落地），
问题集中在三类：**闸门没有真正生效**、**更新链路在自带运行时下的收尾不完整**、**方案里写了但没实现的兜底**。

| # | 级别 | 问题 | 位置 | 处理状态 |
|---|---|---|---|---|
| 1 | P0 | staging 校验的"必需内容"闸门完全失效（已实测复现） | `scripts/check-runtime-stage.sh:17-41` | 待修 |
| 2 | P0 | 自带运行时更新后本轮仍跑旧核心，并报一条误导日志；且每次启动重装一遍 | `src-tauri/src/lib.rs:805-830` | 待修 |
| 3 | P0 | 更新前停实例用了进程组终止，可能连带杀掉用户终端里的其它进程 | `src-tauri/src/lib.rs:356` | 待修 |
| 4 | P1 | §2.4 的四格矩阵有一格永远走不到，对应单测是假的绿 | `src-tauri/src/lib.rs:508` | 待修 |
| 5 | P1 | `DSH_DESKTOP_DSH` 排障开关会触发"自带运行时"的全套副作用 | `src-tauri/src/runtime.rs:154`、`lib.rs:386` | 待修 |
| 6 | P1 | 首启播种把 90s 首启超时吃成 30s；播种失败留下半棵 profile | `src-tauri/src/lib.rs:591-612`、`1064-1073` | 待修 |
| 7 | P1 | 播种不看运行时来源，`runtime: system` 的用户也会被播种 | `src-tauri/src/lib.rs:591` | 待修 |
| 8 | P1 | staging 缺文件数/体积闸门，目录残留会被静默打包 | `Makefile:211`、`scripts/stage-runtime.sh:60` | 待修 |
| 9 | P1 | 只拦悬空链接，不拦绝对链接 | `scripts/check-runtime-stage.sh:44` | 待修 |
| 10 | P2 | `minimumSystemVersion` 仍是 10.15，低于自带 node 的 `minos 11.0` | `src-tauri/tauri.conf.json:26` | 方案 §20.4 已记为待办 |
| 11 | P2 | 更新不传 `--cache`；`runtime/{prefix,tools,npm-cache}` 无人创建 | `src-tauri/src/update.rs:350`、`lib.rs:391` | 方案 §21 已记为待办 |
| 12 | P2 | 没有 `runtime.lock`，只信下载来的 `SHASUMS256.txt` | `Makefile:198-207`、`scripts/stage-runtime.sh:54-57` | 待修 |
| 13 | P2 | §2.3 的"回退 + last-known-good"完全没实现 | `src-tauri/src/lib.rs:1075-1083` | 待修 |
| 14 | P2 | 系统探测无条件执行，`bundled` / 环境变量覆盖时也白跑 | `src-tauri/src/lib.rs:508-536` | 待修 |
| 15 | P2 | `runtime: system` 仍被能力门槛拒掉，与 §2.4 的措辞有出入 | `src-tauri/src/lib.rs:519` | 待定（可能是有意为之，需文档化） |
| 16 | P3 | 发布门禁用 `make fmt` + `git diff`，会改工作区 | `.github/workflows/release.yml:109-110` | 待修 |

**环境备注**（不是代码问题）：本工作树的 `src-tauri/target/` 里缓存了旧路径
`/Users/wangzy/Applications/Scripts/dsh-desktop/…` 的 tauri 构建产物，`cargo test` 会在 build script 阶段失败
（`failed to read plugin permissions`）。`cargo clean -p dsh-desktop` 即可，但这说明 CI 之外这份工作树的测试没人跑过。

---

## 1. P0-1：staging 校验的"必需内容"闸门完全失效

**位置**：`scripts/check-runtime-stage.sh:17-41`

**根因**：`first_of()` 通过 `node_bin=$(first_of …)` 调用，命令替换会开一个**子 shell**，函数里的 `fail=1`
在子 shell 结束时一起消失，父 shell 的 `fail` 永远是 `0`。只有悬空链接与 quarantine 两项检查（在主 shell 里跑）
能真正让脚本失败。

**实测复现**（造一个只有 `node/bin/node` 的假 staging 目录）：

```text
必需内容：
  [缺]   fakert/node/lib/node_modules/npm/bin/npm-cli.js
  [缺]   fakert/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/package.json
  [缺]   fakert/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js
  [缺]   fakert/tools/bin/pnpm
  [缺]   fakert/profile-template/package.json
  [缺]   fakert/profile-template/node_modules/dshmarket/package.json
  [缺]   fakert/profile-template/pnpm-lock.yaml
  [缺]   fakert/THIRD-PARTY-NOTICES.md
  [ok]   无悬空符号链接
自带 node : v22.0.0
自带 dsh  : v22.0.0        ← $dsh_js 为空，实际打印的是 node --version
staging 校验通过            ← EXIT=0
```

**影响**：缺 dsh / 缺 pnpm / 缺插件市场模板 / 缺许可清单的 staging 会照常打包发布。
方案 §12 的验收项"故意删掉一个平台原生模块 → 构建必须报错"目前形同虚设。

**修法**：让 `first_of` 不经过命令替换 —— 用全局变量承接结果（`first_of_result`），或把缺失项写进一个
临时文件/计数变量，函数末尾直接 `exit 1`。同时修掉"`$dsh_js` 为空时把 `node --version` 当成 dsh 版本"的收尾。

**回归**：在 `scripts/` 下加一个自检用例（假目录 → 断言退出码非 0），或在脚本里加 `--self-test` 分支，
纳入 CI 的 staging 步骤之前执行。

---

## 2. P0-2：自带运行时更新后本轮仍跑旧核心

**位置**：`src-tauri/src/lib.rs:805-830`

**根因**：更新装进影子前缀（`app-data/runtime/prefix`）后，回读版本用的仍然是
`locator::version_of(&resolved.dsh_js)`，而 `resolved.dsh_js` 是**解析阶段就固定的 seed 路径**，
它的 `package.json` 当然没变，于是 `version == from` 成立，代码走进"装到别处了"的分支：

```rust
version = locator::version_of(&resolved.dsh_js).unwrap_or_else(|| to.clone());
if version == from {
    // 日志指向 npm 全局前缀 —— 但前缀其实是对的
    harness::app_log("update installed but the supervised CLI is still {from}; check npm global prefix");
} else { just_updated = true; … }
```

**影响**：

1. 日志误导排障方向（`check npm global prefix`，而前缀正确）；
2. 不置 `just_updated`、不重启实例 → **本轮启动继续跑旧 seed**，新版本要等下一次启动的版本仲裁才生效，
   与方案 §4"结合现有'更新后强制重启实例'的规则，更新在同一轮启动内完成"矛盾；
3. **每次启动都会完整重装一遍**：`update::check_cached` 的缓存只在"已安装版本变了"时失效，
   而这条路径上版本永远不变；缓存命中时代码仍然走 `Status::UpdateAvailable` 分支去装 ——
   缓存挡的是网络查询，不是安装。于是每次启动都下载/重建一棵 289 MB 的依赖树。

**修法**：

- 安装成功后按 `resolved.update_prefix(data_dir)` 用 `dsh_js_in()` 重新定位新树，回读**新树**的版本；
- 版本确实变了 → 把 `resolved.dsh_js` 切到新树、`just_updated = true`；
- 仍未变才记"装到别处了"的日志（这条日志对系统安装路径依然有价值，保留）；
- 顺带：`UpdateAvailable` 来自缓存时也应避免重复安装 —— 安装成功后把 `Cache.installed` 写成**新版本**，
  或在装完后立刻刷新缓存条目。

**回归**：新增单测覆盖"安装到影子前缀 → 解析出的 CLI 路径与版本都切到影子前缀"；
`dsh_js_in` 已是纯函数，可以用临时目录造两棵树验证。

---

## 3. P0-3：更新前停实例用了进程组终止

**位置**：`src-tauri/src/lib.rs:356`

`stop_instance_before_update()` 调用 `process::terminate(pid, TERMINATE_GRACE)`，
而 `process::terminate` 先发 `kill(-pid, …)`（整个进程组），失败才退回单 pid。

接管路径（`lib.rs:905`）对同一类目标用的是 `process::terminate_pid`，注释写明
"只对该 PID 发 SIGTERM（不用进程组，避免连带杀掉用户终端）"（壳方案 §13.1 第 2 条）。

**影响**：更新路径少了这层保护。端口上是用户从终端里起的外部 Harness 时，
`kill(-pid)` 可能把同一 job 里的其它进程一起 SIGTERM。

**修法**：`stop_instance_before_update()` 改用 `terminate_pid`；只有"确认是本应用上次启动的实例"
（`ours.is_some()`）才可以用进程组版本。

**回归**：策略函数 `may_stop_before_update` 已有单测矩阵，这里补一个"外部实例走单 pid 路径"的断言
（可把终止动作抽成参数或 trait 以便测试）。

---

## 4. P1-4：§2.4 的四格矩阵有一格永远走不到

**位置**：`src-tauri/src/lib.rs:508`、`535-536`

`locator::locate(None, None)` 的顺序是"先找 `dsh` 启动器，找不到直接 `Err`，再找 node"，
所以 `system_node` 与 `system_dsh` 同生共死：

```rust
let system = locator::locate(None, None).ok();
let system_node = accepted.and(system.as_ref().map(|loc| loc.node.clone()));
let system_dsh  = accepted.and(system.as_ref().map(|loc| loc.dsh_js.clone()));
```

**影响**：方案 §2.4 矩阵里"本机有 node、没有 dsh → node 用系统的、dsh 用自带的"这一格**在真实调用里不可达**。
`runtime.rs::decide` 的纯函数支持它，单测 `half_an_install_is_combined_with_the_bundled_half` 也在测它，
但上游永远喂不出这种输入 —— 测试是假的绿。功能上无害（自带 node 可用），但矩阵与验收都失真。

**修法**：把"找 node"与"找 dsh"拆成两次独立解析（`locator` 里已有 `path_lookup` / `login_shell_lookup`
两个可复用的私有入口），分别喂给 `Inputs::system_node` / `system_dsh`。

---

## 5. P1-5：`DSH_DESKTOP_DSH` 会触发"自带运行时"的全套副作用

**位置**：`src-tauri/src/runtime.rs:154-157`、`src-tauri/src/lib.rs:386-388`

```rust
// runtime.rs：只有 System 才是 Notify，Env 落进 Shadow
let updates = match dsh.origin { Origin::System => Updates::Notify, _ => Updates::Shadow };
// lib.rs：从更新策略反推"是不是自带"
fn bundled(&self) -> bool { !matches!(self.updates, runtime::Updates::Notify) }
```

用 `DSH_DESKTOP_DSH` 指向自己安装的 dsh（这是 README 承诺的**排障开关**）时：

- `updates = Shadow` → 更新往 `app-data/runtime/prefix` 装一棵完整依赖树，而那棵树永远不会被选中
  （显式覆盖排第一），配合 P0-2 就是每次启动白装一遍；
- `bundled() == true` → 子进程 PATH 前置自带 tools、`npm_config_prefix` / `PNPM_HOME` 被改写到 app-data。

**影响**：一个纯排障开关改变了用户的子进程环境与磁盘占用。

**修法**：`bundled()` 按 `decision.dsh.origin`（`Seed` / `Shadow`）判断，而不是从 `updates` 反推；
`Origin::Env` 的树按"非本壳所有"处理（`Notify`，或按 `update::install_prefix` 反推真实前缀）。

---

## 6. P1-6：首启播种吃掉了 90s 首启超时；失败会留下半棵 profile

**位置**：`src-tauri/src/lib.rs:591-612`（`seed_profile_template` / `copy_tree`）与 `1064-1073`（超时判定）

超时判定的依据是 `$DSH_HOME/profiles/web` 是否存在，而播种发生在它**之前**：

```rust
if let Some(note) = seed_profile_template(&config, resolved.seed.as_deref()) { … }   // 先播种
…
let timeout = if home.join("profiles").join("web").exists() { NEXT /*30s*/ } else { FIRST /*90s*/ };
```

**影响**：

1. 真正的首次启动拿到 30s 常态超时，而不是为它准备的 90s（冷启动实测 4–6s 通常够用，
   但首启正是"可能很慢"的那一次）；
2. `copy_tree` 中途失败（磁盘满、权限）会留下**半棵** `profiles/web`，下一次启动因为 `profile.exists()`
   既不补种也不修复，dsh 拿到一个残缺 profile。

**修法**：

- 超时判定移到播种**之前**计算（或用播种函数的返回值告诉调用方"这是首启"）；
- `copy_tree` 先写 `profiles/web.tmp`，完成后 `rename` 到位；失败则清理临时目录并记日志。

---

## 7. P1-7：播种不看运行时来源

**位置**：`src-tauri/src/lib.rs:591`

`seed_profile_template` 只要求"包里有 `profile-template` 且 `~/.dsh/profiles/web` 不存在"，
不判断本次是否真的采用了自带运行时。`runtime: system` 的用户也会被塞一份钉版本的 `dshmarket`。

**修法**：加一道 `resolved.bundled()`（修完 P1-5 之后语义才正确）的判据，或至少把播种行为写进 README
与状态页，让"用系统安装也会被播种"成为显式契约。

---

## 8. P1-8：staging 缺文件数/体积闸门，残留会被静默打包

**位置**：`Makefile:211`、`scripts/stage-runtime.sh:60`，配合 `src-tauri/tauri.conf.json:28`（`runtime/**/*` 常开）

两处都只 `rm -rf` 四个已知子目录（`node` / `dsh-prefix` / `tools` / `profile-template`），
没有整体清空，也没有方案 §5（v3.1 第 4 条）要求的**文件数/体积闸门**。
该条实测结论是："放进 20,000 个文件（78 MB）后 `make bundle` 正常成功，产物直接变大 —— 无任何告警。"

**修法**：

- staging 开头清空整个 `runtime/`（保留 `README.md` 这个非空 marker）；
- `check-runtime-stage.sh` 增加"文件数落在 25k±10%、解压体积 ≤ 550 MB（按平台给区间）"的断言；
- 对应方案 §12 的验收项"故意多放一个文件、或删掉一个平台原生模块 → 构建必须报错"。

---

## 9. P1-9：只拦悬空链接，不拦绝对链接

**位置**：`scripts/check-runtime-stage.sh:44`

`find -L "$root" -type l` 只能找出**悬空**链接。方案 §5 的实测结论是：资源复制会解引用符号链接，
**绝对链接会把宿主机文件原样复制进包**（实测把宿主 node 的 67 KB 启动器搬了进去，哈希一致），
因此"绝对链接与悬空链接都要拦下"。

**修法**：追加一次 `find "$root" -type l` + 读取链接目标，目标以 `/`（或 Windows 盘符）开头即失败并打印清单；
`node_modules/.bin/*` 这类相对链接放行。

---

## 10. P2 汇总

| 项 | 位置 | 说明与修法 |
|---|---|---|
| `minimumSystemVersion` = 10.15 | `tauri.conf.json:26` | 自带 node 22.23.2 是 `minos 11.0`（方案 §18.1 H2 实测）。声明 10.15 的结果是"能装、能开壳、一起 harness 就崩"。**自带运行时的构建**应提到 `11.0`；精简版构建保持 10.15，需要按构建类型区分配置 |
| 更新不传 `--cache` | `update.rs:350` | 方案 §4 要求 `--cache <app-data>/runtime/npm-cache` 复用依赖树缓存（§21 已记为未修）。同时 §6 的"缓存回收 / 重置运行时入口"也没有 |
| 运行时目录无人创建 | `lib.rs:391` | `update_prefix()` 只 `join` 路径，方案 §3 第 2 步要求 `create_dir_all(app-data/runtime/{prefix,tools,npm-cache})`（幂等）。目前靠 npm 自己建，`tools` 与 `npm-cache` 则完全没建 |
| 没有 `runtime.lock` | `Makefile:198-207`、`stage-runtime.sh:54-57` | 只校验**下载来的** `SHASUMS256.txt`，信任链止于 TLS（方案 §5 v3.1 第 6 条建议把期望 SHA256 钉进仓库）。修法：新增 `src-tauri/runtime.lock`，`runtime-fetch` 与它比对 |
| 没有"回退 + last-known-good" | `lib.rs:1075-1083` | 方案 §2.3 规则 2/3：被选中的树启动失败时**自动改用另一个候选重试一次**，并记录 last-known-good。目前 `abort_start` 直接报错，§12 的"回退演练"验收项无从通过 |
| 系统探测无条件执行 | `lib.rs:508-536` | `Preference::Bundled` 或已设 `DSH_DESKTOP_*` 时仍然跑 `locator::locate`（可能 `zsh -lc 'command -v dsh'`）+ `probe_node`（最长 5s），结果全被丢弃。修法：按 preference / env 短路 |
| `runtime: system` 仍被门槛拒 | `lib.rs:519` | 门槛在 `decide()` 之前生效，`system_node/dsh` 被置 `None`，所以强制系统在 Node < 22.13 时会掉进自带或错误页。这可能是有意为之（旧 node 本来也跑不起来），但与方案 §2.4"保留手动 `runtime: system` 覆盖"的措辞有出入，**需要二选一：放行并只警告，或把现行语义写进 README** |
| 发布门禁会改工作区 | `release.yml:109-110` | `make fmt` + `git diff --exit-code` 是"先改再查"，应改用 `cargo fmt --check`（或 `make fmt-check`） |

---

## 11. 建议的修复顺序

| 阶段 | 范围 | 理由 |
|---|---|---|
| A（半天） | P0-1、P0-2、P0-3 | 三条都在几十行以内，且都能补单测；P0-1 是"发布正确性"的前提，P0-2 是"更新功能是否真的工作"的前提 |
| B | P1-5、P1-4、P1-6、P1-7 | 运行时来源语义的一次性理顺：`bundled()` 按 origin 判断之后，播种与 PATH 注入的判据才正确 |
| C | P1-8、P1-9、P2 的 `runtime.lock` | 构建期闸门补齐，对应方案 §12 的三条 staging 验收项 |
| D | P2 的回退 / `--cache` / 目录创建 / `minimumSystemVersion` | 与签名公证、macOS 实机验收一起做（方案 §20.4 的剩余项） |

---

## 12. 验证方式

- **单测**：`make test`（当前工作树需先 `cargo clean -p dsh-desktop`，见 §0 环境备注）。
  新增用例建议：影子前缀安装后的路径/版本切换（P0-2）、外部实例按单 pid 终止（P0-3）、
  只装 node 不装 dsh 的矩阵格（P1-4）、`Origin::Env` 不被当作 bundled（P1-5）、
  播种失败不留半棵 profile（P1-6）。
- **脚本**：`sh scripts/check-runtime-stage.sh <假目录>` 必须退出码非 0（P0-1）；
  往 staging 里多放一个文件、删掉一个原生模块，`make runtime-stage` 必须报错（P1-8）；
  放一个绝对符号链接，必须报错（P1-9）。
- **实机**：`make bundle-bundled` 后用 `DSH_DESKTOP_RUNTIME_PREFERENCE=bundled` 跑一次完整启动，
  核对 `logs/harness.log` 里的 `runtime: … | updates: …`、`child PATH = …`、`spawn: …` 三行；
  再人为把影子前缀替换成旧版本，确认版本仲裁与（修好之后的）回退路径都按预期走。
- **CI**：`.github/workflows/windows-portable.yml` 的 staging 步骤会调用 `check-runtime-stage.sh`，
  P0-1 修好之后它才真正是一道门禁。

## 13. 验收标准

- [ ] 缺任一必需内容的 staging 目录会让 `check-runtime-stage.sh` 退出码非 0；
- [ ] 自带运行时下完成一次核心更新：**本轮**即用上新版本（日志出现 `dsh updated: A -> B` 且实例被重启），
      且下一次启动不再重复安装；
- [ ] 更新前停掉外部实例时只终止该 pid，不触碰它的进程组；
- [ ] 只装了 node（没装 dsh）的机器上，日志显示 `node … (system) + dsh … (bundled)`；
- [ ] 设置 `DSH_DESKTOP_DSH` 指向系统安装时，不写影子前缀、不改 `npm_config_prefix` / `PNPM_HOME`；
- [ ] 全新 `DSH_HOME` 的首启用 90s 超时；播种被中断后重启仍能得到完整 profile；
- [ ] staging 目录里多一个文件或少一个原生模块 → `make runtime-stage` 报错；
- [ ] 被选中的树起不来时，自动改用另一个候选重试一次并记日志（方案 §2.3 规则 2）。

---

## 14. 本次未覆盖的范围

- 未做联网测试（`make test-live`）与实机 GUI 验证；
- 未审查 `src/index.html`、图标脚本、`README.md` 正文的表述一致性（只核对了与本文相关的几处）；
- macOS 签名/公证链路（方案 §7）本轮没有产物可验；
- Linux 侧 staging 与 `.deb` / AppImage 产物未在 Linux 机器上跑过。
