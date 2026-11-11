# 桌面壳自更新与应用图标（issue #2）

对应 [tanuki-cat/dsh-desktop#2](https://github.com/tanuki-cat/dsh-desktop/issues/2) 提出的两项需求：
**桌面应用本体的自动更新**与**可切换的应用图标**。

> 范围说明：本壳已经有两套"更新"——随包 dsh 核心走 npm registry（`update.rs` / `update_flow.rs`），
> 插件市场走 profile。本文件说的"更新"是**第三个对象**：`DSH Desktop` 这个可执行文件/安装包本身。
> 三者互不影响，缓存、失败退避、日志前缀都各自独立。

## 1. 目标与非目标

### 目标

1. 启动后在**不阻塞启动流程**的前提下检查桌面应用是否有新版本（GitHub Releases）。
2. 有新版时用壳自己已有的方式让用户看得见（窗口标题 + 菜单），而不是要靠用户自己去仓库看。
3. 一次点击即可**下载 → 校验 → 安装**；校验失败、安装失败都必须留下可回滚的结果。
4. 允许用户在几套预置图标之间切换，也允许选一张自己的 PNG；选择持久化，重启后仍生效。

### 非目标（明确不做）

- **不引入 `tauri-plugin-updater`**。官方 updater 需要 `latest.json` + minisign 私钥，
  而本仓库未签名、产物是 `.app.zip` / `.deb` / 免安装 `.zip`，与官方 updater 期望的
  `.app.tar.gz` + `.sig` / NSIS 形状不一致；引入后在没有私钥的情况下会把现有发布流水线卡住。
- **不在 Windows / Linux 上替换正在运行的安装**：Linux 的 `.deb` 需要 root，Windows 无法覆盖
  正在执行的 `.exe`（与 `windows-portable.yml` 的免安装形态绑定）。这两种平台做到
  "完成校验、把包交给用户"为止，并在日志与标题里说清楚。
- 不因为这项功能改动既有打包形状（`bundle.resources` 只在自带运行时版声明）。

## 2. 应用自更新

### 2.1 数据来源

`GET https://api.github.com/repos/tanuki-cat/dsh-desktop/releases?per_page=20`：

- 一次取列表，两个渠道都从同一份数据里选：`stable` 丢掉 `draft` 与 `prerelease`，
  `prerelease` 只丢 `draft`，然后各取版本号最大的一条。这么选而不是分别打 `/latest`，
  是因为预发布渠道没有对应的端点，而两次请求会引入两个时间点的两套答案。
- 资产名已经是稳定契约（`release.yml` / `windows-portable.yml` 生成）：
  `dsh-desktop_<version>_<os>-<arch>[-bundled].app.zip | .deb`，
  Windows 例外：只有一种 `dsh-desktop_<version>_windows-x64-portable.zip`（便携包始终自带运行时，
  所以名字里没有 `-bundled`）。2026-11-11 实测的 v0.4.15 Release 逐字核对过这 10 个资产名。
- 校验依据：Release 里的 `SHA256SUMS` 资产（现有流水线已经合并各平台清单）。

### 2.2 选择哪个资产

`AssetTarget { os, arch, bundled }`：

| 维度 | 取值 |
|---|---|
| os | `macos` / `linux` / `windows`（编译期 `cfg`） |
| arch | `aarch64` → `arm64`，`x86_64` → `x64`（编译期 `cfg`） |
| bundled | 运行时资源目录里能发现 `runtime/node`（`seed_root_for` 的同一判据）→ `-bundled` |

选不到完全匹配的资产时**不降级猜测**：宁可报"这个版本没有适合本机的包"，也不把 x64 包装进 arm64。

### 2.3 校验与落盘

1. 下载资产与 `SHA256SUMS` 到 `<app-data>/app-update/downloads/`。
2. `SHA256SUMS` 里必须存在该资产名对应的行，哈希用 `sha2` 计算后逐字比较。
3. **只有校验通过的包会从 `downloads/` 移到 `ready/`**；未通过的包当场删除——删除是校验
   函数自己的步骤，不是每个调用方都要记得的一句。`ready/` 只保留最新一个包（下次下载成功时
   清掉更早的），同名且哈希仍然匹配的包直接复用，不重复下载数百 MB。

### 2.4 安装（仅 macOS）

1. `ditto -x -k <zip> <staging>` 解包，要求在解包结果里找到**唯一**的 `*.app`。
2. 校验这个 `*.app`：
   - `Contents/Info.plist` 的 `CFBundleIdentifier` == `com.deepseek.dsh.desktop`；
   - `CFBundleShortVersionString` == 目标版本（避免"装上去的还是旧版本"）。
3. 交换：当前 `.app` 改名为 `<app-data>/app-update/backup/<被替换的版本>.app`（从当前包的
   `Info.plist` 读；读不出才退化成新版本号——目录里装的是旧版本，用新版本号命名会骗回滚的人），
   新 `.app` 移入原路径；第二步失败就把备份改回去。只保留刚生成的这一份备份。
4. 清除新包上的 `com.apple.quarantine`（构建未签名，用户手动安装也要走这一步），并记日志。
5. `open -n <path>` 启动新实例后**立刻**销毁单实例 socket（`tauri_plugin_single_instance::destroy`），
   再退出本进程——退出路径复用既有的 `shutdown()`，Harness 会被正常回收。
   删 socket 必须紧跟 `open` 之后：新实例一旦连上旧进程的 socket 就会通知一下再 `exit(0)`，而旧进程
   同时也在退出，结果是两个都没了；`RunEvent::Exit` 里删又太晚（那已经是整个事件循环之后）。
   反过来先删再 `open` 也不行——启动失败时 socket 已经没了，这个活着的进程就不再是单实例。

失败一律停在"已验证的包还在 `ready/`"，并把原因写进日志与标题，绝不留半个安装。

### 2.5 非 macOS 与「已下载」状态

下载并校验后停在 `ready/`，不自动安装。菜单「安装已下载的更新…」是这一段状态的出口：
macOS 上它完成就地安装并重启，其他平台给出手动命令（Linux `sudo apt install ./<file>.deb`，
Windows 解压替换）。这是平台限制，不是没做完。

这条路存在的原因：`download` 策略只会把包放进 `ready/`，而启动路径从不读它；没有这个入口时
那份包永远装不上，标题里的「已就绪」也会是空头支票。

安装前会再比一次版本：`ready/` 里的包可能已经落后于正在运行的版本（用户手动升级过，或之后的
启动用 `install` 装过），那时装上去是降级，所以直接拒绝。

### 2.6 触发时机与节奏

- 检查在 **Harness 窗口出现之后**的后台线程里跑，绝不影响启动。
- 结果写 `app-update-check.json`：包含 `checked_at` / `installed` / `latest` / `channel` /
  `flavour` / `failed*`，复用 `update::Cache::is_fresh` 的同一套新鲜度语义（间隔来自
  `update_check_interval_minutes`）。
- 失败按 `update::failure_window_secs` 退避，避免把网络问题变成每次启动的阻塞。
- 菜单两项：「检查桌面应用更新…」强制忽略缓存；「安装已下载的更新…」消费 `ready/` 里那份包。
  检查与安装分开，是因为替换用户正在使用的应用该由用户决定，而不是「他点了检查」顺带完成。
- 中间状态写到状态页/标题；状态页已经有一张**终端**页在显示时只写日志（见 §6.3），
  否则会把失败原因和重启按钮一起顶掉。

### 2.7 配置

`config.json` 新增（都有 `serde(default)`，旧文件照常可用）：

| 键 | 值 | 默认 | 含义 |
|---|---|---|---|
| `app_update` | `off` / `notify` / `download` / `install` | `notify` | 检查到之后做到哪一步 |
| `app_update_channel` | `stable` / `prerelease` | `stable` | 跟随的发布渠道 |

`off` 不做**自动**检查（菜单那一项仍然工作——那是用户明确的请求，不是启动路径的决定）；
`notify` 只提示；`download` 自动下载 + 校验到 `ready/`，由菜单完成安装；
`install` 在 macOS 上继续安装并重启（其他平台等价于 `download`）。

## 3. 应用图标

### 3.1 形态

- 预置集合：`default`（原图标）+ 3 个配色变体，均以 PNG 字节**内嵌**进二进制
  （`include_bytes!`），不新增 `bundle.resources`，精简版与自带运行时版表现一致。
- 自定义：用户挑一张 PNG，壳负责**解码并整理成图标**（调色板/灰度/16 位由解码器展开；非正方形
  按中心裁成正方形；超过 1024 px 用面积平均缩到 1024），把整理后的图写入
  `<app-data>/icons/custom.png`；不引用用户原路径（原文件被移动/删除不应让图标失效），
  也不把原图尺寸带进每次启动。只有短边小于 64 px 才拒绝——那点像素没有细节可以救。

### 3.2 选择从哪里点

菜单（三个平台都是**顶层**「外观」子菜单，追加在默认菜单之后）：

```
外观 ▸ 默认                  ✓
       深海
       极夜
       晨雾
       ────────────
       选择自定义图片…
       移除自定义图片
```

点「选择自定义图片…」按平台走两条路：

| 平台 | 手段 |
|---|---|
| macOS | `NSOpenPanel`（`objc2-app-kit`，菜单处理器已在主线程） |
| Windows | PowerShell 的 `System.Windows.Forms.OpenFileDialog` |
| Linux | `zenity`，没有就用 `kdialog` |

Windows/Linux 上两个选择器都不可用时回落到状态页：页面里有一个
`<input type="file" accept="image/png">`，用户选完由页面读成 data URL、经
`dsh-desktop:icon-picked` 事件交回 Rust（状态窗口是唯一持有 core capability 的窗口，
不需要新插件或新 capability）。回落时页面会先用 Rust 侧的 `picker_script` 打开这个 input；
它和状态脚本一样带等待重试，因为刚创建的状态窗口可能还没解析到自己的脚本块。
macOS 不走这条回落：`NSOpenPanel` 一定可用，失败就是真的失败，写日志。

### 3.3 应用到哪里

| 平台 | 手段 | 覆盖范围 |
|---|---|---|
| macOS | `NSApplication.setApplicationIconImage`（主线程） | Dock / ⌘Tab / 强制退出窗口 |
| Windows / Linux | `WebviewWindow::set_icon` | 现有窗口的任务栏图标；下次启动时对新窗口同样生效 |

macOS 的 `setApplicationIconImage` 在运行期生效且不落盘；重启后由启动流程按持久化的选择重新应用。
这也意味着改名后的"重启生效"不需要动 `.app` 包里的 `icns`（那需要重新签名，本仓库未签名，不做）。

### 3.4 持久化

`<app-data>/appearance.json`：`{ "icon": "default" | "deep" | "night" | "dawn" | "custom" }`。
不写 `config.json`——那是用户手写的文件，与 `state.json` / `update-check.json` 同属运行状态。

## 4. 影响范围

| 文件 | 改动 |
|---|---|
| `src-tauri/src/app_update.rs` | 新增：检查、资产选择、下载校验、策略编排 |
| `src-tauri/src/app_update/macos.rs` | 新增：`ditto` 解包、bundle 校验、交换、重启 |
| `src-tauri/src/app_update/tests.rs` | 新增：24 条离线用例 |
| `src-tauri/src/icon/tests.rs` | 新增：18 条离线用例 |
| `src-tauri/tests/app_update_live.rs` | 新增：3 条联网用例（`DSH_DESKTOP_LIVE_TESTS=1`） |
| `Makefile` | `make icon-presets`，并让 `make icon-art` 一并重生成预设 |
| `src-tauri/src/icon.rs` | 新增：PNG 解码、预置图标、选择与持久化、应用 |
| `src-tauri/src/lib.rs` | 配置项、菜单（检查/安装两项）、启动后线程、安装后的重启路径 |
| `src-tauri/src/window.rs` | 标题组合新增"应用有新版"一路（与 dsh 核心提示并存）、状态页回落 |
| `src-tauri/src/process.rs` | `run_within`：把"没装这个工具"与"超时"分开（下载失败诊断） |
| `src-tauri/src/window/tests.rs` | 新增 5 条离线用例（标题三态、提示文案、picker 脚本、终端页不被顶掉） |
| `src-tauri/src/process/tests.rs` | 新增 2 条离线用例（spawn 失败 vs 超时） |
| `src-tauri/tests/webkit_compat_shim.rs` | 新增 1 条：picker 脚本在真引擎里等页面就绪 |
| `src/index.html` | 选择自定义图片的交互块 |
| `src-tauri/icons/presets/*.png` | 新增：内嵌预置图标（由脚本生成，不手改） |
| `scripts/make-icon-presets.py` | 新增：从 `icon.png` 派生配色变体（纯标准库，不需要 Pillow） |
| `src-tauri/Cargo.toml` | `sha2`、`png` |

## 5. 验证方式

- 单测（离线）：版本比较、资产选择矩阵（os/arch/bundled 不匹配即拒）、`SHA256SUMS` 解析、
  校验失败即删、已校验包的复用、备份命名、安装前的降级拒绝、重启与单实例 socket 的顺序、
  PNG 解码（灰度/16 位展开、超大文件头拒绝、非 PNG 拒绝）、中心裁切、超限缩放、数据 URL 上限、
  预乘 alpha、存储的是整理后的图标、图标选择往返、标题组合三态。
- 真引擎（node）：picker 脚本等页面定义入口后再调用（`tests/webkit_compat_shim.rs`）。
- `make check` / `make clippy` / `make test`。
- `cargo clippy --target x86_64-pc-windows-gnu` 覆盖 `cfg(windows)` 分支。
- 联网（`make test-live`）：`tests/app_update_live.rs` 三条 —— 真实 Release 列表可用且
  能被选出一个带资产的版本，真实 `SHA256SUMS` 可下载、可解析、含本机构建对应的条目，
  以及一条完整下载 → 校验 → 落 `ready/` 的链路。
- 真实资产抽查：手动下载一个平台包并与清单比对 SHA256（结果见 §6.1）。

## 6. 实施结果（2026-11-11）

与上面方案的偏差，以及实施时才知道的事实：

- **没有新增 HTTP 依赖**。`ureq`/`reqwest` 都会被拉进来才能发请求，而这两条路都用系统工具：
  macOS/Linux 用 `curl`（`--proto =https --proto-redir =https`，禁止降级），Windows 用
  PowerShell 的 `Invoke-WebRequest`（`-TimeoutSec`、`-NoProfile`）。两条路都走
  `process::run_within`，因此有进程级超时，与仓库里 `taskkill`/`netstat`/登录 shell 的处理方式一致。
  失败原因也因此看得见：curl 用 `--stderr -` 把错误并进 stdout，PowerShell 用 try/catch 写 stdout。
- **哈希是流式的**（`sha256_file`）：自带运行时包是数百 MB，一次读进内存会是这个进程最大的一次分配。
- **Windows 的资产名不带 `-bundled`**：便携包永远自带运行时，方案里的统一模板在这里是错的，
  单测改为逐字钉住真实 Release 的 5 种名字（2026-11-11 用 `releases?per_page=20` 核对过）。
- **标题有两个版本槽位**（`PENDING_UPDATE` 给 dsh 核心，`PENDING_APP_UPDATE` 给应用）。
  两者可以同时成立；应用那条优先，因为它才是这个窗口能直接处理的（菜单里有入口）。
- **菜单放在 View 下**（「检查桌面应用更新…」与「安装已下载的更新…」），不是应用菜单：
  它们与「重新加载界面」是同一类——都作用于壳自己。图标是顶层「外观」子菜单。
- **检查与安装分成两个菜单项**：`notify` 下点「检查」只得到链接与开关提示，不会因为"用户点了"
  就下载；下载（`download`）与安装（macOS）各自是独立的一步，替换正在运行的应用由用户按下
  「安装已下载的更新…」决定。
- **安装后的重启**用 `open -n` + `app.exit(0)`：退出仍然走既有的 `RunEvent::Exit` → `shutdown()`，
  Harness 子进程被正常回收；不引入 `tauri-plugin-process` 的 relaunch（它会绕过这套清理）。
- **图标没有走 `iced`/`image` crate**：`png` 足够，且已经在依赖图里（Tauri 的菜单与图标代码用）。
  解码只接受 8 位 RGB/RGBA；调色板或低位深会明确报错让人另存，而不是交出一个平台会读错的缓冲区。

### 6.1 验证证据

| 项目 | 结果 |
|---|---|
| `make fmt-check` / `make clippy`（all targets，`-D warnings`） | 通过 |
| `cargo clippy --target x86_64-pc-windows-gnu --all-targets -D warnings` | 首轮通过（覆盖 `cfg(windows)` 的 PowerShell 路径与 picker）；第二轮未能在本机重跑（沙箱不允许写 `~/.cargo/registry`，`jiff-tzdb` 解包失败），由 `windows-portable.yml` 的 clippy + test 兜住 |
| `make test` | **304 passed**（相对 `255` 基线新增 49 条：资产命名/渠道/无资产/校验和与复用/失败退避/plist/bundle 根/备份命名/重启顺序/PNG 解码/预乘 alpha/数据 URL 上限/图标往返/标题三态/提示文案/picker 脚本/终端页不被顶掉/spawn 失败与超时） |
| `cargo test --test webkit_compat_shim` | 5 passed（新增 1 条：picker 脚本在 node 里等页面定义入口后再调用） |
| `DSH_DESKTOP_LIVE_TESTS=1 cargo test --test app_update_live` | 3 passed：真实 Release 头 `v0.4.15`、11 个资产、`SHA256SUMS` 可解析且含本机构建对应的条目；第三条走完整下载链路，真实下载 7.86 MB 的 `linux-x64.deb` 并通过校验后落到 `ready/` |
| 真实资产校验 | `dsh-desktop_0.4.15_linux-arm64.deb` 实下 7.8 MB，SHA256 `496ed82f…` 与清单逐字相同 |
| `ditto -x -k` 往返 | 对真实 `DSH Desktop.app` 打包再解包，`CFBundleShortVersionString` 仍是 0.4.15 |
| 第二轮修复后的联网复核（2026-11-11 10:29） | 三条联网用例未能重跑：上一轮跑完后这个 IP 的未鉴权 GitHub API 配额已用尽（`x-ratelimit-remaining: 0`，403）。改用**同一套 curl 参数**直连 Release 资产复核：`SHA256SUMS` 10 行可解析，`dsh-desktop_0.4.15_linux-x64.deb` 7.86 MB 下载后 SHA256 与清单逐字相同。配额恢复后 `make test-live` 可重跑 |

已实机演练的部分（2026-11-11，本机 macOS，`cargo build --release` 后直接跑二进制）：
应用能起来、菜单能构建（`Builder::build` 阶段就构建菜单，早于 path resolver 被 manage ——
第一版在这里 panic 过，现已改成读进程内缓存）、后台更新检查真实跑通（日志
`桌面应用已是最新: 0.4.15（release 头 0.4.15）`）、Harness 正常启动并在退出时被回收。

**待实机点击确认**（需要打包后的 `.app` 与下一次发版）：菜单三项的点击效果、macOS 上
`app_update: install` 的完整链路（下载 → 装 → 重启后版本号变化），以及 Dock 图标切换后
⌘Tab 与强制退出窗口是否同步。

### 6.2 已知限制

- 就地安装只有 macOS 有。Linux 的 `.deb` 需要 root，Windows 无法覆盖正在运行的 exe，
  这两处只做到"已验证的包落盘 + 告诉用户怎么装"。
- 图标改的是运行期图标，没有改包里的 `.icns`/`.ico`。Finder 里看到的仍然是原始图标；
  改 bundle 内资源会破坏签名（本仓库未签名，但也不该让用户的安装变成半签名状态）。
- 自定义图标是**中心裁切**，没有让用户选裁切位置：需要精确构图时请自己先裁好再选。
- `app-update/backup/` 只保留**刚替换下来的那一份**（更早的备份会被清掉）：再往前已经没有
  任何决策能用到它，而一份自带运行时的备份与一次完整安装相当。
- Release 列表走的是未鉴权的 GitHub API（每 IP 每小时 60 次）。共享出口 IP 上取不到时，
  检查会失败并按退避重试——现在日志里能看到 `curl: (56) … 403` 这种原因，而不是只有退出码。
- `app_update_channel = "prerelease"` 在当前流水线下与 `stable` 等价：`release.yml` 用
  `gh release create` 发布，从不加 `--prerelease`/`--draft`。要真的启用预发布渠道，得先在流水线
  里把预发布标出来。
- 标题上的"已下载并校验"只是提示，重启**不会**安装它；安装入口是菜单「安装已下载的更新…」。

### 6.3 第二轮：审查修复

对第一轮实现做了一次针对性的审查，下面每一条都是当时确认的问题，修复与对应测试一起落地。

| 问题 | 后果 | 处理 |
|---|---|---|
| 重启与单实例插件的顺序反了（`open -n` → `sleep 600ms` → `exit`） | 新实例在旧进程还活着时连上它的 socket，通知后自己 `exit(0)`；旧进程随即退出，**两个都没了** | `open -n` 之后立刻 `destroy` 掉 socket，再退出；去掉 600 ms 延时。测试钉住「启动在前、释放 socket 在后」 |
| `app_update: download` 在 macOS 上是死路，标题却写「重启即可安装」 | 包永远装不上，还会每个周期重下几百 MB | 新增菜单「安装已下载的更新…」消费 `ready/`；`ready/` 里哈希仍匹配的包直接复用；文案改成「已下载并校验（菜单：…）」，不再承诺重启 |
| `manual_install_hint` 的 else 分支给 macOS 也打印 `sudo apt install` | macOS 上唯一的一句话指向不存在的命令 | 增加 macOS 分支（解压替换 `.app`，或改用 `install` 策略）；测试按平台断言 |
| 下载失败的原因永远为空（`output_within` 丢弃 stderr），缺工具被报成「超时」 | 网络问题无法诊断 | curl 加 `--stderr -`（≥ 7.63），PowerShell 用 try/catch 把异常写到 stdout；`process::run_within` 把 `Spawn` 与 `Timeout` 分开（`output_within` 行为不变）。两条新用例 + 一次真实 403 现场验证 |
| `picker_script` 是一次性 eval，页面未解析完就丢 | Windows/Linux 的图标回落路径静默无效 | 采用 `status_script` 的同一套等待重试（40 × 25 ms），并在 node 里跑真引擎用例 |
| 两条新增用例没有触到被测代码（校验失败删除由测试自己做；两条提示文案是测试里复制的字面量） | 通过但不证明任何事 | 抽出 `check_digest` / `available_notice_text` / `ready_notice_text` 后再断言；删除逻辑由被测函数完成 |
| `announce_update` 用不带应用槽位的组合 | 宣布 dsh 核心更新时会把应用更新提示从标题里抹掉 | 删掉只传一个槽位的 `compose_title`，所有调用点都显式给两个槽位 |
| `show_message` 会覆盖已 latch 的终端失败页，并清掉重试标记 | 失败原因和重启按钮一起消失 | 终端页在场时只写日志；用例钉住这个守卫 |
| 备份目录按**新**版本号命名 | `0.4.16.app` 里装的是 0.4.15，回滚时容易拿错 | 从当前包 `Info.plist` 读被替换的版本；读不出才退化成新版本号。备份只留最新一份 |
| Dock 位图按非预乘 alpha 写入 | 自定义 PNG 的半透明边缘偏亮，出现白边 | 新增 `premultiply_rgba` 并用于 `NSBitmapImageRep` |
| 失败路径残留 `backup/.staging`；状态页数据 URL 无上限；临时文件名固定 | 磁盘残留、无上限分配、并发覆盖 | `install` 无论成败都清 staging；`decode_data_url` 先过 `check_data_url_size`；临时文件名带纳秒。 |
| `ready/` 里的包可能已经比正在运行的版本旧（手动升级过、或之后的启动用 `install` 装过） | 点「安装已下载的更新…」会把应用**降级** | `install_package` 先比版本，不比当前新就拒绝并说明原因；读不出版本仍交给 bundle 自检 |
| 自定义 PNG「不生效」：2026-11-11 实机三次选择全部被拒（2886×1338 / 1920×1080 不是正方形，1254 超过 1024），**macOS 上只在日志里写一行，界面上什么都没有** | 用户看到的是菜单点了没反应 | 解码器展开调色板/灰度/16 位；非正方形按中心裁切、超限用面积平均缩到 1024（短边 <64 才拒绝）；存的是整理后的图标；失败在**所有平台**弹「图标没有更新 + 原因」；「没有文件选择器」与「这张图不能当图标」拆成两条不同的处理 |

文档同步：测试数量、`off` 与菜单的关系、`app_update: download` 的出口、macOS「外观」菜单的
实际位置、`ready/` 的清理时机、证据里引用的日志原文，都改成了与实现一致的说法。


