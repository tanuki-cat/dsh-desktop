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
> **分支策略**：自带运行时开发用的 `feat/bundled-runtime` 已合并回 `main`（2026-09-13），
> 后续开发直接在 `main` 上进行；该分支只作历史快照保留。

设计依据：[`docs/design-task-feat-dsh-tauri-desktop-shell.md`](docs/design-task-feat-dsh-tauri-desktop-shell.md)
（正文为设计，实施偏差与验证证据见其 §13.1–13.4）。

## 当前状态

**已实现**

- locator：解析 dsh 启动器、其真实 `lib/bin.js`（穿软链）、以及 node（GUI 启动没有 Homebrew PATH）
- 启动：launcher flag 顺序、显式 cwd（agent workspace）、`--patch` overlay 强制 `printUrl`、独立进程组
- 输出：持续读 stdout/stderr、token 脱敏写日志（5MB × 3 份轮转，写入前判定）、200 行环缓冲用于错误页
- URL：解析首个 token（对 `(LAN: ...)` 后缀健壮）、首启 90s / 常态 30s 超时
- 实例：固定端口；探测 → 复用自己上次的实例 / 接管外部 Harness / 占用时错误页
- 生命周期：退出即 SIGTERM 进程组（5s 后 SIGKILL）并删除 state.json —— 关闭窗口（`RunEvent::ExitRequested`）
  与 ⌘Q / Dock 退出（`RunEvent::Exit`）两条事件链都处理；复用的上次实例也登记，退出时一并停掉
- 启动失败即收尾：等待 URL 超时或窗口创建失败时，先停掉刚起的进程再报错，不留"假失败 + 端口被占"
- 看护线程：启动成功后继续 `wait` 子进程，Harness 意外退出会清掉 state.json、销毁已无意义的 Harness 窗口并弹回状态页报错（应用不静默变死页面）；状态页是终点，关闭它即退出应用
- 残留自愈：只有强杀/崩溃这类拿不到回调的场景才靠下次启动清理；state.json 0600。记录的 pid 仍监听记录的端口时才发信号；仍服务**本次端口**的记录会保留，让启动流程直接复用而不是重启；pid 已不拥有该端口时，只有"命令行含 `--profile web`/`dsh`，且父进程已消失或由 launchd / `systemd --user` 接管"的残留才会被清理（pid 复用、以及别人正在跑的会话都不动）
- 安全：Harness 窗口零 capability、导航限定当次 authority、外链与 `window.open` 只把 `http`/`https` 交系统浏览器（`file:`、自定义 scheme 记 `external scheme blocked` 后丢弃）
- 下载：`on_download` 落盘到 `~/Downloads`，同名文件自动加 `-1`/`-2` 后缀（不静默覆盖）；
  附件上传由 wry 的 `runOpenPanel` 原生处理
- **升级**：每次启动查 registry（默认取 `latest`+`next` 最高版本）—— dsh 核心有新版就装并重启实例；
  profile 里的插件市场 `dshmarket` 走同一套机制（自己的缓存窗口，装完同样重启让新插件生效）；
  npm 子进程显式带上 node 所在目录的 PATH（否则 GUI 启动下 `#!/usr/bin/env node` 必然 exit 127）
- 打包：`pnpm tauri build --bundles app` → `DSH Desktop.app`

**验证**

- 实机（2026-09-13，macOS）：接管外部 Harness → 自启并拿到 token URL → WebView 内 token→cookie 成功，
  会话列表/文件卡片/输入框正常渲染；`state.json` 与端口监听者一致；日志 0 行明文 token。
- 崩溃提示实机复现（2026-09-13，macOS）：启动后 `kill -9` 掉 harness 进程，日志记
  `Harness pid 94329 exited unexpectedly (code None)`，状态页重新弹出报错、`state.json` 被清除，
  应用自身保持运行（不再是一张死页面）。
- 退出路径实机复现（2026-09-13，macOS）：⌘Q 等价的 Apple Event 退出后，日志出现
  `stopping Harness pid 92619` / `Harness stopped`，3080 端口释放、`state.json` 删除；
  修复前只处理红点关闭（`ExitRequested`），⌘Q 走的是 `RunEvent::Exit`，清理从未执行。
- 插件市场自动更新实机验证（2026-09-13，macOS，构建的 `.app`）：启动后日志依次出现
  `plugin update available: dshmarket 1.45.1 -> 1.46.1, installing`、`plugin updated: dshmarket 1.45.1 -> 1.46.1`，
  随后 harness 重启并正常服务；profile 的依赖范围被改写为 `^1.46.1`、`node_modules` 内实装 1.46.1，
  且 `plugin-check.json` 与核心的 `update-check.json` 各自独立（时间戳与内容互不影响）。
- 离线测试：`cargo test` **71 passed**（URL 解析含 LAN 后缀、token 脱敏、状态文件往返、locator 软链解析与
  `DSH_DESKTOP_DSH` 优先级、6 个 semver 比较用例、Linux `ss` 输出解析、judge 判定、缓存新鲜度规则、
  缓存落盘往返与旧缓存文件兼容、"装到别处 → 同窗口与跨窗口都不重复安装"、npm PATH 前缀、沉默对端探针超时、
  package.json 版本解析、部分/损坏 config.json 处理、workspace / dsh_path 回退不改文件、HOME 缺失不回落 `/`、
  进程组终止与僵尸进程识别、外链 scheme 白名单、页面加载重试判定矩阵、自愈三分支与
  "Keep ⇒ 进程组终止"的组合断言、孤儿残留的 `ps` 判定与父进程分类（launchd / `systemd --user` / 父进程已消失 / 活着的会话）、
  更新前停止模式、
  日志运行中轮转与备份份数、日志句柄共享、计数器不提前轮转、spawn 路径校验点名、
  下载重名避让、URL 解析兜底、CLI 版本区间判定；合并自带运行时后另有：运行时决策矩阵与能力门槛、
  影子前缀与版本仲裁、`Origin::Env` 不算自带、播种与首启超时、更新后切树回读、
  插件市场 profile 判定与插件缓存隔离）。
- 权限：`config.json`（`env` 字段可能放 API Key）与 `state.json` 均为 0600；旧版本留下的 0644 文件会在下次启动时被就地收紧（实机已确认）。
- 更新链路的失败证据也来自实机日志：修复前每次 GUI 启动都记 `npm view 失败: env: node: No such file or directory`
  （见设计文档 §13.8）。
- 联网测试：`make test-live` → registry head = 0.1.5-rc.2 且不降级；**冷查询 1168–1217 ms → 缓存命中 0 ms**。
  另有一条专门复现 GUI 失败场景的回归测试（把进程 PATH 换成不含 node 的值、npm cache 指向临时目录）：
  先断言裸调 `npm` 会 exit 127，再断言走修复后的路径能正常查到 registry。

**待实机点击确认**：附件上传、下载、`window.open` 实际效果、macOS TCC 授权归因。

**平台**：`make` 的构建入口只覆盖 macOS 与 Linux（其他平台在解析阶段直接报错）。Windows 走
`.github/workflows/windows-portable.yml` 的原生 staging + 免安装包（已实机验证，见下一节），
`process.rs` 的存活探测用的是 `OpenProcess` + `GetExitCodeProcess`。本机可用
`cargo clippy --target x86_64-pc-windows-gnu` 对 `cfg(windows)` 代码做回归（只检查类型与 lint，不能运行 PE）。

**未做**：签名与公证（对外分发必需）、多 workspace 切换 UI、"回退 + last-known-good"（方案 §2.3 规则 2）。
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
| `make test` | 离线单元测试（当前 **71** 个） | 不联网 |
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

### 已知坑

- **`bundle.resources` 只由 `make bundle-bundled` 声明**（`--config` 注入），基座 `tauri.conf.json` 里没有它：
  常开这条 glob 会把残留的 staging（实测 520 MB）静默打进普通 `make bundle` 的包 —— 合并前两边各自吃过一次这个亏。
  因此 `make bundle` 与 `src-tauri/runtime/` 无关，自带运行时版必须走 `make bundle-bundled`；
  该目标要求 `src-tauri/runtime/` 里至少有一个文件，`README.md` 就是那个 marker（`.gitignore` 只忽略 payload）。
- `make bundle` / `make run` 需要 `pnpm`；只跑 `make dev` / `check` / `test` 不需要。
- Linux 首次构建前请先 `make doctor` 装齐系统依赖；即便将来采用自带 Node/dsh 的发行方式，
  **WebKitGTK 仍来自系统**（见后续实施计划）。
- `make test-live` 与首次 `make bundle` 需要网络（查 registry / 拉 Tauri CLI）。

### 自带运行时（已并入 main）

已完成并可实机复现，并已**并入 `main`**（2026-09-13 合并 `feat/bundled-runtime`）：

- `make runtime-fetch / runtime-stage / runtime-clean / bundle-bundled` 四个目标全部跑通；
- staging 自带校验（`scripts/check-runtime-stage.sh`，`--self-test` 会先用假目录验证闸门本身有效）：必需文件、**悬空符号链接**（会让 `tauri build` 失败）、**绝对符号链接**（会被解引用，把宿主机文件复制进包）、**quarantine 属性**（会带进 .app）、**文件数/体积闸门**（多放 2 万个文件这类残留以前会被静默打包）；
- profile 模板由真 pnpm 生成（含插件市场 dshmarket，见方案 §2.5）；
- 实测：`env -i PATH=/usr/bin:/bin` 下自带 node 跑自带 dsh → `0.1.5-rc.2`；
  全新 DSH_HOME + 模板播种 → `dsh web` **6 秒**出 URL、stderr 干净、`.dsh-market` 出现；
- 实测：`make bundle-bundled` 产出 598 MB 的 `.app`，包内 node 可直接执行、31,090 个文件；
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
- 用系统安装时：**默认仍然就地升级**（`system_updates: install`，与自带运行时之前的行为一致），
  想自己管升级就写 `system_updates: notify`（只提示，不动用户的全局前缀）；
- **node 与 dsh 分开解析**：系统装了 node 但没装 dsh 时，可以「系统 node + 自带 dsh」混用（方案 §2.4 的那一格以前不可达）；
- **只有本壳自己的树才算「自带」**：seed（包内）与影子前缀（`app-data/runtime/prefix`）会得到 PATH 前置、
  `npm_config_prefix`／`PNPM_HOME` 注入与首启播种；`DSH_DESKTOP_DSH`／`DSH_DESKTOP_RUNTIME` 指向的树按**用户的**处理 ——
  不动它的子进程环境、不往里装更新（排障开关不再带来副作用）；
- **能力门槛仍然管 `runtime: system`**：node 缺 `module.stripTypeScriptTypes`、或有自带运行时时架构不一致，
  系统安装会被拒（错误页说明原因）。这是有意为之（旧 node 本来就跑不起 dsh），与方案 §2.4「保留手动覆盖」的措辞
  有出入，以本条为准；
- 探测短路：`runtime: bundled` 或两个 `DSH_DESKTOP_*` 都已指定时，跳过系统运行时探测（省掉一次登录 shell + 最长 5 秒的探测）；
- 首启超时：判定发生在播种**之前**，所以带模板播种的首启仍然是 90 秒预算；播种失败不会留下半棵 profile（先写 `.tmp` 再改名）；
- 想看效果：`DSH_DESKTOP_RUNTIME_PREFERENCE=bundled|system|auto` 可覆盖 `config.json` 的 `runtime`。

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
以及 stdout 里的启动 URL 行。区间外的版本会在启动日志里记一条 warning，状态页显示「未测试版本」；
把 `require_tested_dsh` 设成 `true` 可改为直接拒绝启动。当前测试区间：`>= 0.1.5-rc.1, < 0.2.0`（常量在 `update.rs`）。

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
| `take_over_existing` | `true` | 端口被外部 Harness 占用时，停止它并接管；`false` 则改用系统浏览器打开 |
| `auto_update` | `true` | 启动时检查并安装 dsh 新版本 |
| `auto_update_plugins` | `true` | 同时也把 profile 里的插件市场（`dshmarket`）更新到 registry 上的最新版；它会改写你 profile 的 `package.json`/锁文件，所以单独一个开关 |
| `update_tags` | `["latest","next"]` | 取其中最高版本；只跟正式版就写 `["latest"]` |
| `update_check_interval_minutes` | `60` | 一次成功的查询结果缓存多久（0 = 每次启动都查）。查询实测约 1.2–1.9 s，缓存命中 0 ms |
| `import_shell_env` | `true` | 启动时导入登录 shell 的环境变量（见下节）。`false` 则只用 App 自身环境 |
| `require_tested_dsh` | `false` | CLI 版本落在已测试区间外时是否拒绝启动。默认只告警并继续（状态页标注「未测试版本」） |
| `runtime` | `"auto"` | 运行时来源：`auto` 用系统已装的（通过门槛时），否则用自带；`bundled` 强制自带；`system` 保持旧行为（开发用） |
| `env` | `{}` | 显式追加/覆盖传给 harness 的环境变量，优先级最高，如 `{"DEEPSEEK_API_KEY": "sk-…"}` |

只要写你想改的字段即可：`port` / `workspace` 缺失会取默认值，其余字段本就有默认值。
若文件整体无法解析，本次运行使用默认配置并记一条日志，**但不会覆盖你的文件**（只有文件不存在时才会写入）。

状态与日志：`<app-data>/state.json`（0600）、`<app-data>/logs/harness.log`（脱敏；写入前判定 5MB × 3 份轮转，即 `harness.log` + `.1`/`.2`/`.3`）。
macOS 的 app data 目录为 `~/Library/Application Support/com.deepseek.dsh.desktop/`。

## 环境变量（API Key 等）

**问题**：macOS 的 GUI 应用继承的是 launchd 的环境，**不会读取 `.zshrc`/`.zprofile`**。
所以在终端里 `export DEEPSEEK_API_KEY=…` 对双击启动的 `.app` 无效，harness 会报
`no API key for provider "deepseek-official"`。

**做法**：启动时执行一次登录 shell 的环境导入（`$SHELL -lic env`，超时 8 s，失败则回退 `-lc` 或跳过），
把结果合并进 harness 子进程的环境：

- 实测开销 **约 160 ms**（在版本读取/更新检查的同一时间段内并行执行，基本不占启动关键路径），
  取到约 46 个变量，输出 0 行噪音；
- **只把变量名写进日志**（值可能是密钥，永不落盘）；
- 保留 App 自己管理的变量：`HOME`/`TMPDIR`/`SHELL`/`SHLVL`/`PWD`/`_`/`DSH_*` 等不导入；
- `PATH` 走合并策略：**自带 node 目录 → `/opt/homebrew/bin` → `/usr/local/bin` → shell 的 PATH → App 的 PATH**（去重、只保留绝对路径），
  这样 agent 执行的 `git`/`node`/`python` 与你的终端一致，而壳自己用的 node 仍优先。

想关掉：`config.json` 里设 `"import_shell_env": false`；想强制某几个值：用 `env` 字段（最后应用，优先级最高）。
另一种等价做法是在 Web GUI 的 Models 页面里填写 API Key —— 那会写进 `~/.dsh` 的凭证服务，与 shell 环境无关。

## 行为

| 场景 | 行为 |
|---|---|
| 端口无监听 | 定位 dsh/node → 启动 → 等 URL → 打开窗口 |
| 端口上是本应用上次启动的实例（state.json 对得上且存活） | 直接复用（cookie 对同一 authority 仍有效，实测跨重启有效） |
| 端口上是**别人**启动的 Harness（CLI / Automator） | **接管**：401 特征确认身份 → 对该 PID 发 SIGTERM → 等端口释放 → 自启拿新 token |
| 启动前发现新版 dsh | 先升级 CLI（splash 显示进度），随后重启实例跑新版本 |
| 启动前发现新版插件市场 | 先停实例 → `dsh plugin --profile web add dshmarket@<版本>` → 重启实例（`auto_update_plugins: false` 可关） |
| 端口被别的程序占用 | 错误页，提示改 `config.json` 的端口 |
| `take_over_existing: false` 且是外部 Harness | 不接管：用系统浏览器打开并给出说明 |
| 关闭窗口（红点 / ⌘W） | `AppHandle::exit(0)` → `RunEvent::ExitRequested` → SIGTERM 进程组 → 删状态文件 |
| ⌘Q / Dock 退出 / `quit app` | tao `application_will_terminate` → `RunEvent::Exit` → 同样的清理（幂等） |
| 退出发生在 Harness 启动途中 | `EXITING` 标志：刚起来的子进程立即停掉，不会漏成孤儿 |
| 复用上次实例后退出 | 该实例已登记，退出时照样 SIGTERM（旧行为不登记 → 留孤儿） |
| 强杀（SIGKILL）/ 崩溃 | 拿不到任何回调；下次启动先自愈：仍在服务本次端口的记录保留下来交给复用分支，其余情况清掉残留与记录 |
| 启动等待 URL 超时 / 窗口创建失败 | 停掉刚起的子进程 → 状态页报错（不会留下占着端口的半启动实例） |
| 启动成功后 Harness 意外退出 | 看护线程发现退出 → 清 state.json → 销毁 Harness 窗口 → 状态页显示退出码与最近输出（关闭状态页即退出应用） |
| 窗口首次加载失败 | 20 s 内没有收到"加载开始"事件（导航根本没起来，无论端口是否健康）→ 退避重试同一 token URL，最多 3 次；已经开始加载则一律不打扰。次数用尽且端口也不再服务才弹状态页；端口仍健康时只记日志，不在事件不投递的环境里给正常应用弹错误页 |
| 页面里的非 http/https 链接（`file:`、自定义 scheme…） | 不交给系统：日志记 `external scheme blocked: <scheme> (...)` 后丢弃 |
| 有新版 dsh 但端口上是外部实例且不允许接管 | 跳过本次更新（不重写别人正在用的树），实例继续服务，日志记 `update deferred` |
| 下载同名文件 | 自动改名 `name-1.ext`，不覆盖已有文件 |
| 关闭状态页（启动失败时） | 直接退出应用（此时没有 Harness 窗口，不会留下无窗口进程）；应用自己移除该窗口走 `destroy`，不触发这条 |
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
1. 需要联网时：`npm view @deepseek-ai/dsh dist-tags --json` → 在 `update_tags` 里取版本号最高者
   （fetch 超时压到 **8 s**，离线时快速回退而不是等 npm 默认的 25 s）；
2. 与已安装版本做 semver 比较（`rc.2 > rc.1 > alpha.2`，正式版高于同号预发布版），**从不降级**；
3. **先把正在使用这棵 CLI 树的实例停掉**：npm 是原地重写依赖树，而 node 按需懒加载模块 ——
   树被换走后运行中的 harness 下一次 `require()` 会直接 `MODULE_NOT_FOUND`（实测）。所以先 SIGTERM 停实例、
   等端口释放，再安装；不能停时（外部实例且 `take_over_existing=false`、或拿不到 PID）**跳过本次更新**并记日志，
   实例不受任何影响；
4. 有新版则 `npm install -g --no-fund --no-audit --cache <app-data>/runtime/npm-cache @deepseek-ai/dsh@<解析出的具体版本>`；
   npm 取自 node 同目录，且 npm 全局前缀 ≠ CLI 实际位置时自动带 `--prefix`；`runtime/{prefix,tools,npm-cache}` 在启动时
   幂等创建；
5. 所有 npm 子进程都在 PATH 最前面插入 npm 所在目录：npm 是 `#!/usr/bin/env node` 脚本，
   而 GUI 启动的壳只有 launchd 的 PATH（不含 node），不这样处理会直接 `exit 127`（详见设计文档 §13.8）；
6. 安装后**从安装前缀回读** CLI 路径与版本：自带运行时的更新落在影子前缀，回读 seed 会让版本看起来没变，
   于是本轮继续跑旧核心、日志还指向 npm 前缀；版本变了就把受管 CLI 切到新树并在本轮重启实例；
   只有**确实没变**（npm 装到了别处）才记那条日志，并把这次尝试写进缓存（`attempted`），
   **同一缓存窗口内不再重复安装**（否则每次启动都会先停掉 Harness 再重建一棵约 289 MB 的依赖树）；
7. 刚更新过 → 强制重启实例（否则复用旧进程仍跑旧二进制）。

结果写入日志：`dsh is up to date` / `update available: A -> B` / `dsh updated: A -> B` / `update failed, keeping vA` /
`update A was already attempted and changed nothing; not installing again`。

### 插件市场（`dshmarket`）的自动更新

核心之外，这个壳还会保持 profile 里的插件市场为最新 —— 它是新机器唯一的插件安装入口：

- 同一个 registry 检查（`update_tags`、`update_check_interval_minutes`），答案缓存在**自己的**
  `<app-data>/plugin-check.json`，与核心的 `update-check.json` 互不干扰；
- 只在该 profile 的 `package.json` **声明了** `dshmarket` 时才动手（你自己删掉的插件不会被装回来），
  比较的是 `node_modules/dshmarket/package.json` 里的**实际安装版本**，不是范围；
- 有新版时走与核心相同的顺序：**先停实例**（pnpm 原地重写 profile 的 `node_modules`，运行中的
  harness 下次 lazy require 会崩）→ `dsh plugin --profile web add dshmarket@<版本>`（CLI 自己转发 pnpm，
  属于你的 profile 文件会被改写）→ 回读版本 → 本轮重启实例让新插件生效；
- 装了但版本没变（pnpm 写到了别处）只记日志，并把这次尝试写进缓存，同一版本不重复装；
- 不想要就设 `"auto_update_plugins": false`（核心的 `auto_update` 不受影响）。

**排障**：手工修好 npm 全局前缀（或换安装方式）后，壳在出现更高版本前不会再尝试安装 —— 日志里那句
`already attempted and changed nothing` 是唯一线索，删掉 `<app-data>/update-check.json` 即可复位。

## 启动耗时与性能

这份壳是进程编排 + I/O，**空闲时 CPU ≈ 0，没有常驻轮询或定时器**（唯一的定时行为是窗口首次加载看护：最多 3 次、每次等 20 s，加载成功或放弃后线程立即结束）。启动路径上各环节实测（macOS，2026-09-13）：

| 环节 | 实测 | 说明 |
|---|---|---|
| 定位 dsh/node | < 5 ms | 沿 PATH 逐目录做文件存在性检查（19 个目录的等价 shell 循环实测 4 ms）；只有找不到时才兜底起登录 shell（~160 ms） |
| 版本读取 | **~1 ms** | 读 CLI 所属 `package.json` 的 `version`；读不到才回退 `node dsh.js --version`（约 80 ms） |
| 更新检查（缓存命中） | **0 ms** | 60 分钟内沿用 `<app-data>/update-check.json`；冷查询约 1.2–1.9 s |
| 登录 shell 环境抓取 | ~160 ms | 与上面两步并行执行，join 后才组装子进程环境 |
| 端口探针 | ~12 ms | 建连/读/写各 600 ms 上限，不会挂在沉默的对端上 |
| 启动 harness 到拿到 URL | 1–4 s | 由 `dsh web` 自身决定，首启（初始化 profile）更久，超时 90 s / 30 s |

日志写入是每行一次 `write(2)`（无缓冲）：单次会话通常只有几十行，有意不做缓冲，以免崩溃时丢日志。
轮转由内存字节计数判定，不额外 `stat`；计数在打开日志时以文件实际大小 seed，所以上次遗留的超大文件仍会在下次写入前轮转。

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
