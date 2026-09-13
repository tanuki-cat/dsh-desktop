# dsh-desktop

DeepSeek Harness 的 Tauri 桌面壳：启动 `dsh web`、捕获启动 URL、用系统 WebView 承载 UI、退出时回收子进程，
并在每次启动前检查/安装 dsh 核心的新版本。

> 许可证：MIT（见 `LICENSE`）。
>
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
- 生命周期：退出即 SIGTERM 进程组（5s 后 SIGKILL）并删除 state.json —— 关闭窗口（`RunEvent::ExitRequested`）
  与 ⌘Q / Dock 退出（`RunEvent::Exit`）两条事件链都处理；复用的上次实例也登记，退出时一并停掉
- 启动失败即收尾：等待 URL 超时或窗口创建失败时，先停掉刚起的进程再报错，不留"假失败 + 端口被占"
- 看护线程：启动成功后继续 `wait` 子进程，Harness 意外退出会弹回状态页报错、清掉 state.json（应用不静默变死页面）
- 残留自愈：只有强杀/崩溃这类拿不到回调的场景才靠下次启动清理（state.json 0600 + 存活校验）
- 安全：Harness 窗口零 capability、导航限定当次 authority、外链与 `window.open` 交系统浏览器
- 下载：`on_download` 落盘到 `~/Downloads`，同名文件自动加 `-1`/`-2` 后缀（不静默覆盖）；
  附件上传由 wry 的 `runOpenPanel` 原生处理
- **dsh 核心升级**：每次启动查 registry（默认取 `latest`+`next` 最高版本），有新版就装并重启实例；
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
- 离线测试：`cargo test` **29 passed**（URL 解析含 LAN 后缀、token 脱敏、状态文件往返、locator 软链解析、
  6 个 semver 比较用例、Linux `ss` 输出解析、judge 判定、缓存新鲜度规则、缓存落盘往返、
  npm PATH 前缀、沉默对端探针超时、package.json 版本解析、部分/损坏 config.json 处理、
  进程组终止与僵尸进程识别、下载重名避让、URL 解析兜底、CLI 版本区间判定）。
- 权限：`config.json`（`env` 字段可能放 API Key）与 `state.json` 均为 0600；旧版本留下的 0644 文件会在下次启动时被就地收紧（实机已确认）。
- 更新链路的失败证据也来自实机日志：修复前每次 GUI 启动都记 `npm view 失败: env: node: No such file or directory`
  （见设计文档 §13.8）。
- 联网测试：`make test-live` → registry head = 0.1.5-rc.2 且不降级；**冷查询 1168–1217 ms → 缓存命中 0 ms**。
  另有一条专门复现 GUI 失败场景的回归测试（把进程 PATH 换成不含 node 的值、npm cache 指向临时目录）：
  先断言裸调 `npm` 会 exit 127，再断言走修复后的路径能正常查到 registry。

**待实机点击确认**：附件上传、下载、`window.open` 实际效果、macOS TCC 授权归因。

**平台**：构建入口只覆盖 macOS 与 Linux（其他平台在 Makefile 解析阶段直接报错）。Windows 分支代码保留但**未支持也未验证**：
那里的存活探测恒为真，`terminate` 会白等满 grace，要移植得先换成 `OpenProcess` + `GetExitCodeProcess`。

**未做**：签名与公证（对外分发必需）、多 workspace 切换 UI；
**自带运行时（打包 Node/dsh）已规划未实施**，见下方"后续实施计划"。

## 构建（Makefile，支持 macOS 与 Linux）

`make` 是统一构建入口；平台由 `uname -s` 自动判定（`Darwin` → `app`，`Linux` → `deb`）。

| 目标 | 作用 | 备注 |
|---|---|---|
| `make` / `make help` | 列出全部目标 | 默认目标 |
| `make doctor` | 检查工具链与平台依赖 | Linux 用 `pkg-config` 查 `webkit2gtk-4.1` / `gtk+-3.0` / `libsoup-3.0` 并给出 Debian/Fedora/Arch 安装命令；macOS 提示装 Xcode CLT |
| `make check` | `cargo check --all-targets` | 与 CI 口径一致 |
| `make fmt` / `make clippy` | 格式化 / lint（`clippy -D warnings`） | |
| `make test` | 离线单元测试（当前 **20** 个） | 不联网 |
| `make test-live` | 联网集成测试（全部） | 自动设 `DSH_DESKTOP_LIVE_TESTS=1`：查真实 registry、对比冷/热缓存耗时，并在「PATH 里没有 node」的模拟 GUI 环境下验证 npm 仍可运行 |
| `make dev` | 运行 debug 版 | 等价 `cargo run --manifest-path src-tauri/Cargo.toml` |
| `make build` | 编译 release 可执行文件 | 产物 `src-tauri/target/release/dsh-desktop` |
| `make bundle` | 打包安装包 | macOS → `.../bundle/macos/DSH Desktop.app`；Linux → `.../bundle/deb`；依赖 `node-deps`（自动 `pnpm install` 拉 `@tauri-apps/cli`） |
| `make run` | 构建后启动 | macOS 用 `open` 打开打包产物；Linux 直接跑 release 二进制 |
| `make icons` | 由 `icon.png` 重生成 `icon.icns` | 仅 macOS（`sips` + `iconutil`）；Linux 打包直接用 png |
| `make icon-art` | 由 `src-tauri/icons/make_icon.py` 重绘 `icon.png` | 自绘小黑鲸，需 `python3` + Pillow；改设计改脚本，不要手改 png |
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
| `workspace` | `$HOME` | 传给 dsh 的工作目录 = agent 的 workspace root |
| `dsh_home` | `null` | `null` 表示共用 `~/.dsh`（插件/设置/会话全保留）；指向别的目录则隔离 |
| `take_over_existing` | `true` | 端口被外部 Harness 占用时，停止它并接管；`false` 则改用系统浏览器打开 |
| `auto_update` | `true` | 启动时检查并安装 dsh 新版本 |
| `update_tags` | `["latest","next"]` | 取其中最高版本；只跟正式版就写 `["latest"]` |
| `update_check_interval_minutes` | `60` | 一次成功的查询结果缓存多久（0 = 每次启动都查）。查询实测约 1.2–1.9 s，缓存命中 0 ms |
| `import_shell_env` | `true` | 启动时导入登录 shell 的环境变量（见下节）。`false` 则只用 App 自身环境 |
| `require_tested_dsh` | `false` | CLI 版本落在已测试区间外时是否拒绝启动。默认只告警并继续（状态页标注「未测试版本」） |
| `env` | `{}` | 显式追加/覆盖传给 harness 的环境变量，优先级最高，如 `{"DEEPSEEK_API_KEY": "sk-…"}` |

只要写你想改的字段即可：`port` / `workspace` 缺失会取默认值，其余字段本就有默认值。
若文件整体无法解析，本次运行使用默认配置并记一条日志，**但不会覆盖你的文件**（只有文件不存在时才会写入）。

状态与日志：`<app-data>/state.json`（0600）、`<app-data>/logs/harness.log`（脱敏，5MB 轮转）。
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
| 端口被别的程序占用 | 错误页，提示改 `config.json` 的端口 |
| `take_over_existing: false` 且是外部 Harness | 不接管：用系统浏览器打开并给出说明 |
| 关闭窗口（红点 / ⌘W） | `AppHandle::exit(0)` → `RunEvent::ExitRequested` → SIGTERM 进程组 → 删状态文件 |
| ⌘Q / Dock 退出 / `quit app` | tao `application_will_terminate` → `RunEvent::Exit` → 同样的清理（幂等） |
| 退出发生在 Harness 启动途中 | `EXITING` 标志：刚起来的子进程立即停掉，不会漏成孤儿 |
| 复用上次实例后退出 | 该实例已登记，退出时照样 SIGTERM（旧行为不登记 → 留孤儿） |
| 强杀（SIGKILL）/ 崩溃 | 拿不到任何回调，只能靠下次启动自愈清理 |
| 启动等待 URL 超时 / 窗口创建失败 | 停掉刚起的子进程 → 状态页报错（不会留下占着端口的半启动实例） |
| 启动成功后 Harness 意外退出 | 看护线程发现退出 → 重新弹出状态页显示退出码与最近输出、清 state.json |
| 有新版 dsh 但端口上是外部实例且不允许接管 | 跳过本次更新（不重写别人正在用的树），实例继续服务，日志记 `update deferred` |
| 下载同名文件 | 自动改名 `name-1.ext`，不覆盖已有文件 |
| 关闭状态页（启动失败时） | 直接退出应用（此时没有 Harness 窗口，不会留下无窗口进程）；应用自己移除该窗口走 `destroy`，不触发这条 |
| 壳被强杀后再次启动 | 读取 state.json 自愈清理残留进程 |

> 并发边界：壳只保证**自己这个端口**上不会同时跑两个实例（接管 + single-instance 插件）。`dsh_home` 为 `null` 时
> 共用 `~/.dsh`，你在终端另开一个同 profile 的 `dsh` 仍会与壳并发读写同一份会话存储 —— 需要严格隔离就把 `dsh_home`
> 指向别的目录（代价是 marketplace 插件、设置与会话不再共享）。
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
4. 有新版则 `npm install -g --no-fund --no-audit @deepseek-ai/dsh@<解析出的具体版本>`；
   npm 取自 node 同目录，且 npm 全局前缀 ≠ CLI 实际位置时自动带 `--prefix`；
5. 所有 npm 子进程都在 PATH 最前面插入 npm 所在目录：npm 是 `#!/usr/bin/env node` 脚本，
   而 GUI 启动的壳只有 launchd 的 PATH（不含 node），不这样处理会直接 `exit 127`（详见设计文档 §13.8）；
6. 安装后**回读一次 CLI 版本**：版本没变说明 npm 装到了别的前缀（自定义 prefix、pnpm/yarn 布局），
   此时只记日志、不谎报成功、也不为一次无效更新重启实例；
7. 刚更新过 → 强制重启实例（否则复用旧进程仍跑旧二进制）。

结果写入日志：`dsh is up to date` / `update available: A -> B` / `dsh updated: A -> B` / `update failed, keeping vA`。

## 启动耗时与性能

这份壳是进程编排 + I/O，**没有后台轮询或定时器，空闲时 CPU ≈ 0**。启动路径上各环节实测（macOS，2026-09-13）：

| 环节 | 实测 | 说明 |
|---|---|---|
| 定位 dsh/node | < 5 ms | 沿 PATH 逐目录做文件存在性检查（19 个目录的等价 shell 循环实测 4 ms）；只有找不到时才兜底起登录 shell（~160 ms） |
| 版本读取 | **~1 ms** | 读 CLI 所属 `package.json` 的 `version`；读不到才回退 `node dsh.js --version`（约 80 ms） |
| 更新检查（缓存命中） | **0 ms** | 60 分钟内沿用 `<app-data>/update-check.json`；冷查询约 1.2–1.9 s |
| 登录 shell 环境抓取 | ~160 ms | 与上面两步并行执行，join 后才组装子进程环境 |
| 端口探针 | ~12 ms | 建连/读/写各 600 ms 上限，不会挂在沉默的对端上 |
| 启动 harness 到拿到 URL | 1–4 s | 由 `dsh web` 自身决定，首启（初始化 profile）更久，超时 90 s / 30 s |

日志写入是每行一次 `write(2)`（无缓冲）：单次会话通常只有几十行，有意不做缓冲，以免崩溃时丢日志。

## 打包成 .app
```bash
pnpm install --store-dir=./.pnpm-store
pnpm tauri build --bundles app     # 产物：src-tauri/target/release/bundle/macos/DSH Desktop.app
```

图标来自 `src-tauri/icons/icon.icns`（由 `icon.png` 用 `sips`+`iconutil` 生成），
`icon.png` 由 `src-tauri/icons/make_icon.py` 自绘生成（矢量路径 + 4× 超采样，非官方素材）。当前**未签名**：
本机可运行；分发给别人需要 Developer ID 签名 + 公证（`codesign` / `notarytool`）。

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
- **只读 seed + 可写影子前缀**：seed 放在 `Contents/Resources/runtime/`，dsh 核心更新落到
  `app-data/runtime/prefix`（不写签名的 bundle）；
- 实施前必须先落 4 项 P0：**回退/last-known-good、seed 与前缀的版本仲裁、pnpm 随包分发（否则离线装不了插件）、
  按平台 staging（12 个原生模块）**；另有两项在 v3 实测中升级为 P0：**签名保留 entitlements**、
  **staging 校验悬空符号链接**；
- 首启更新策略待定：默认 `auto_update` 会在首启就下载整棵依赖树，与"离线首启"的宣传冲突（方案 §4 给了两个选项）；
- 分阶段：P1 macOS arm64（无 node 可跑）→ P2 签名公证 → P3 Linux x64/arm64 → P4 universal/Windows/自更新。

## 开机自启

系统设置 → 通用 → 登录项 → 添加 `DSH Desktop.app` 即可（不需要额外代码）。
