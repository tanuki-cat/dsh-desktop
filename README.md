# dsh-desktop

DeepSeek Harness 的 Tauri 桌面壳：启动 `dsh web`、捕获启动 URL、用系统 WebView 承载 UI、退出时回收子进程，
并在每次启动前检查/安装 dsh 核心的新版本。

> 当前版本仍**要求系统已装 `node` 与 `dsh`**。让目标机器无需预装这两者的发行方案
> （自带 Node + dsh 核心）已规划完成，见 [后续实施计划](#后续实施计划自带运行时无需预装-nodedsh)。

设计依据：[`docs/design-task-feat-dsh-tauri-desktop-shell.md`](docs/design-task-feat-dsh-tauri-desktop-shell.md)
（正文为设计，实施偏差与验证证据见其 §13.1–13.4）。

## 当前状态

**已实现**

- locator：解析 dsh 启动器、其真实 `lib/bin.js`（穿软链）、以及 node（GUI 启动没有 Homebrew PATH）
- 启动：launcher flag 顺序、显式 cwd（agent workspace）、`--patch` overlay 强制 `printUrl`、独立进程组
- 输出：持续读 stdout/stderr、token 脱敏写日志（5MB 轮转）、200 行环缓冲用于错误页
- URL：解析首个 token（对 `(LAN: ...)` 后缀健壮）、首启 90s / 常态 30s 超时
- 实例：固定端口；探测 → 复用自己上次的实例 / 接管外部 Harness / 占用时错误页
- 生命周期：关闭窗口即退出并 SIGTERM 进程组（5s 后 SIGKILL）、state.json（0600）+ 崩溃残留自愈
- 安全：Harness 窗口零 capability、导航限定当次 authority、外链与 `window.open` 交系统浏览器
- 下载：`on_download` 落盘到 `~/Downloads`；附件上传由 wry 的 `runOpenPanel` 原生处理
- **dsh 核心升级**：每次启动查 registry（默认取 `latest`+`next` 最高版本），有新版就装并重启实例
- 打包：`pnpm tauri build --bundles app` → `DSH Desktop.app`

**验证**

- 实机（2026-09-13，macOS）：接管外部 Harness → 自启并拿到 token URL → WebView 内 token→cookie 成功，
  会话列表/文件卡片/输入框正常渲染；`state.json` 与端口监听者一致；日志 0 行明文 token。
- 离线测试：`cargo test` **16 passed**（URL 解析含 LAN 后缀、token 脱敏、状态文件往返、locator 软链解析、
  6 个 semver 比较用例、Linux `ss` 输出解析、judge 判定、缓存新鲜度规则、缓存落盘往返）。
- 联网测试：`DSH_DESKTOP_LIVE_TESTS=1 cargo test --test update_live` →
  registry head = 0.1.5-rc.2 且不降级；**冷查询 1168 ms → 缓存命中 0 ms**。

**待实机点击确认**：附件上传、下载、`window.open` 实际效果、macOS TCC 授权归因。

**未做**：签名与公证（对外分发必需）、Windows Job Object（当前 `taskkill /T /F`）、多 workspace 切换 UI；
**自带运行时（打包 Node/dsh）已规划未实施**，见下方"后续实施计划"。

## 构建（Makefile，支持 macOS 与 Linux）

`make` 是统一构建入口；平台由 `uname -s` 自动判定（`Darwin` → `app`，`Linux` → `deb`）。

| 目标 | 作用 | 备注 |
|---|---|---|
| `make` / `make help` | 列出全部目标 | 默认目标 |
| `make doctor` | 检查工具链与平台依赖 | Linux 用 `pkg-config` 查 `webkit2gtk-4.1` / `gtk+-3.0` / `libsoup-3.0` 并给出 Debian/Fedora/Arch 安装命令；macOS 提示装 Xcode CLT |
| `make check` | `cargo check --all-targets` | 与 CI 口径一致 |
| `make fmt` / `make clippy` | 格式化 / lint（`clippy -D warnings`） | |
| `make test` | 离线单元测试（当前 **16** 个） | 不联网 |
| `make test-live` | 联网集成测试 | 自动设 `DSH_DESKTOP_LIVE_TESTS=1`，查真实 registry 并对比冷/热缓存耗时 |
| `make dev` | 运行 debug 版 | 等价 `cargo run --manifest-path src-tauri/Cargo.toml` |
| `make build` | 编译 release 可执行文件 | 产物 `src-tauri/target/release/dsh-desktop` |
| `make bundle` | 打包安装包 | macOS → `.../bundle/macos/DSH Desktop.app`；Linux → `.../bundle/deb`；依赖 `node-deps`（自动 `pnpm install` 拉 `@tauri-apps/cli`） |
| `make run` | 构建后启动 | macOS 用 `open` 打开打包产物；Linux 直接跑 release 二进制 |
| `make icons` | 由 `icon.png` 重生成 `icon.icns` | 仅 macOS（`sips` + `iconutil`）；Linux 打包直接用 png |
| `make clean` | 清理构建产物 | `cargo clean` |
| `make distclean` | 连依赖缓存一起清理 | 额外删 `node_modules`、`.pnpm-store`、`.cargo-home`、`src-tauri/gen` |

可覆盖变量：`CARGO=`、`PNPM=`、`BUNDLE_TARGETS=`（如 `make bundle BUNDLE_TARGETS=appimage`）、
`CARGO_HOME=`、`PNPM_STORE=`——后两个留空即用工具自身默认，便于沙箱/CI 把缓存重定向到工作区。

### 已知坑

- **`src-tauri/runtime/` 不能为空**：`tauri.conf.json` 的 `bundle.resources = ["runtime/**/*"]` 相对
  `src-tauri/` 解析，匹配为空会让 `make bundle` 直接失败
  （`glob pattern runtime/**/* path not found or didn't match any files`）。仓库里的
  `src-tauri/runtime/README.md` 就是为此保留的标记文件：`.gitignore` 配置为**只忽略 payload、保留它**
  （若本项目纳入版本库，请确保该文件被提交）。误删后重新创建同名文件即可（内容不限，说明用途即可）。
- `make bundle` / `make run` 需要 `pnpm`；只跑 `make dev` / `check` / `test` 不需要。
- Linux 首次构建前请先 `make doctor` 装齐系统依赖；即便将来采用自带 Node/dsh 的发行方式，
  **WebKitGTK 仍来自系统**（见后续实施计划）。
- `make test-live` 与首次 `make bundle` 需要网络（查 registry / 拉 Tauri CLI）。

### 规划中的目标（自带运行时，尚未实现）

`runtime-fetch`（下载官方 Node 并按 `SHASUMS256.txt` 校验）、`runtime-stage`（组装 `src-tauri/runtime/`：
Node + dsh 树 + pnpm + 许可文件）、`runtime-clean`（回收约 475 MB staging）——见
[后续实施计划](#后续实施计划自带运行时无需预装-nodedsh) 与方案文档 §5。

运行时（当前版本）需要系统中已有 `dsh` 与 `node`。

## 配置（`config.json`）

首次运行写入应用数据目录，字段与默认值：

| 字段 | 默认 | 说明 |
|---|---|---|
| `port` | `3080` | 固定端口。authority 稳定才能让 cookie 跨重启复用、也才能接管上次实例 |
| `workspace` | `$HOME` | 传给 dsh 的工作目录 = agent 的 workspace root |
| `dsh_home` | `null` | `null` 表示共用 `~/.dsh`（插件/设置/会话全保留）；指向别的目录则隔离 |
| `take_over_existing` | `true` | 端口被外部 Harness 占用时，停止它并接管；`false` 则改用系统浏览器打开 |
| `auto_update` | `true` | 启动时检查并安装 dsh 新版本 |
| `update_tags` | `["latest","next"]` | 取其中最高版本；只跟正式版就写 `["latest"]` |
| `update_check_interval_minutes` | `60` | 一次成功的查询结果缓存多久（0 = 每次启动都查）。查询实测约 1.2–1.9 s，缓存命中 0 ms |

状态与日志：`<app-data>/state.json`（0600）、`<app-data>/logs/harness.log`（脱敏，5MB 轮转）。
macOS 的 app data 目录为 `~/Library/Application Support/com.deepseek.dsh.desktop/`。

## 行为

| 场景 | 行为 |
|---|---|
| 端口无监听 | 定位 dsh/node → 启动 → 等 URL → 打开窗口 |
| 端口上是本应用上次启动的实例（state.json 对得上且存活） | 直接复用（cookie 对同一 authority 仍有效，实测跨重启有效） |
| 端口上是**别人**启动的 Harness（CLI / Automator） | **接管**：401 特征确认身份 → 对该 PID 发 SIGTERM → 等端口释放 → 自启拿新 token |
| 启动前发现新版 dsh | 先升级 CLI（splash 显示进度），随后重启实例跑新版本 |
| 端口被别的程序占用 | 错误页，提示改 `config.json` 的端口 |
| `take_over_existing: false` 且是外部 Harness | 不接管：用系统浏览器打开并给出说明 |
| 关闭窗口 / Cmd+Q | SIGTERM 进程组 → 5s → SIGKILL，并清理状态文件 |
| 壳被强杀后再次启动 | 读取 state.json 自愈清理残留进程 |

> 为什么必须接管：启动 URL 里的 launch token 是 `randomBytes(32)` 且只存在于那个进程内存中，外部无法取得；
> 不重启就永远拿不到会话（无 cookie 访问一定是 401）。

## dsh 核心升级

0. **先看缓存**：`<app-data>/update-check.json` 里的结论在 `update_check_interval_minutes`（默认 60）内且已安装版本未变 → 直接沿用，不联网（实测 0 ms vs 1.2 s）。查询失败也会缓存，但只缓存 5 分钟，离线时不至于每次启动都干等；
1. 需要联网时：`npm view @deepseek-ai/dsh dist-tags --json` → 在 `update_tags` 里取版本号最高者
   （fetch 超时压到 **8 s**，离线时快速回退而不是等 npm 默认的 25 s）；
2. 与已安装版本做 semver 比较（`rc.2 > rc.1 > alpha.2`，正式版高于同号预发布版），**从不降级**；
3. 有新版则 `npm install -g --no-fund --no-audit @deepseek-ai/dsh@<解析出的具体版本>`；
   npm 取自 node 同目录，且 npm 全局前缀 ≠ CLI 实际位置时自动带 `--prefix`；
4. 刚更新过 → 强制重启实例（否则复用旧进程仍跑旧二进制）。

结果写入日志：`dsh is up to date` / `update available: A -> B` / `dsh updated: A -> B` / `update failed, keeping vA`。

## 打包成 .app

```bash
pnpm install --store-dir=./.pnpm-store
pnpm tauri build --bundles app     # 产物：src-tauri/target/release/bundle/macos/DSH Desktop.app
```

图标来自 `src-tauri/icons/icon.icns`（由 `icon.png` 用 `sips`+`iconutil` 生成）。当前**未签名**：
本机可运行；分发给别人需要 Developer ID 签名 + 公证（`codesign` / `notarytool`）。

## 后续实施计划：自带运行时（无需预装 Node/dsh）

目标：在**没有 Node、没有 dsh** 的机器上双击即用，且首次启动不依赖网络下载。

完整方案：[`docs/design-task-feat-dsh-bundled-runtime.md`](docs/design-task-feat-dsh-bundled-runtime.md)（v2，含实测数据与修订记录）。

已经用原型验证过的关键结论（方案据此成立）：

| 结论 | 实测 |
|---|---|
| 官方 Node 发行版**自带 npm** | 48 MB → 解压 187 MB，含 `node/npm/npx/corepack` |
| 只用自带 node 能启动 harness | `env -i PATH=/usr/bin:/bin` 下成功输出 `dsh web:` URL |
| harness **不写自己的安装树** | 安装树被修改文件数 = **0** ⇒ 只读 seed 成立 |
| dsh 树可整体搬迁 | 复制到任意 prefix 后照常启动 |
| 镜像 `bundle.resources` | 已在本仓库配置并实测（空 glob 会让构建失败） |

代价与做法（摘要）：

- 解压后约 **490 MB**（Node 187 + dsh 树 289 + 壳 11），分发包约 150–250 MB；
- **只读 seed + 可写影子前缀**：seed 放在 `Contents/Resources/runtime/`，dsh 核心更新落到
  `app-data/runtime/prefix`（不写签名的 bundle）；
- 实施前必须先落 4 项 P0：**回退/last-known-good、seed 与前缀的版本仲裁、随包 pnpm（否则插件装不了）、
  按平台 staging（12 个原生模块）**；
- 分阶段：P1 macOS arm64（无 node 可跑）→ P2 签名公证 → P3 Linux x64/arm64 → P4 universal/Windows/自更新。

## 开机自启

系统设置 → 通用 → 登录项 → 添加 `DSH Desktop.app` 即可（不需要额外代码）。
