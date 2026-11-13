# dsh-desktop

DeepSeek Harness 的桌面壳（Tauri 2，MIT 许可）。它负责定位并启动 `dsh web`、捕获带 launch token 的启动 URL、
用系统 WebView 承载界面，退出时回收子进程；dsh 核心、插件市场与壳自己的更新也由它管理。
本仓库只是壳，不含 Harness 前端源码。

## 安装

### 下载

到 [Releases](https://github.com/tanuki-cat/dsh-desktop/releases) 按平台取包：

| 平台 | 自带运行时版（推荐） | 精简版（需自备 node/dsh） |
|---|---|---|
| macOS arm64 | `dsh-desktop_<v>_macos-arm64-bundled.app.zip` | `dsh-desktop_<v>_macos-arm64.app.zip` |
| macOS x64 | `dsh-desktop_<v>_macos-x64-bundled.app.zip` | `dsh-desktop_<v>_macos-x64.app.zip` |
| Linux x64 | `dsh-desktop_<v>_linux-x64-bundled.deb` | `dsh-desktop_<v>_linux-x64.deb` |
| Linux arm64 | `dsh-desktop_<v>_linux-arm64-bundled.deb` | `dsh-desktop_<v>_linux-arm64.deb` |
| Windows x64 | `dsh-desktop_<v>_windows-x64-portable.zip`（附同名 `.exe` 自解压包） | —— |

- **自带运行时版**（文件名带 `-bundled`）随包带 Node、dsh 与插件市场模板，目标机器无需预装任何东西。
- **精简版**只有壳，要求机器上已装 `node`（≥ 22.13）与 `dsh`（npm 包 `@deepseek-ai/dsh`），
  且 dsh 版本在已测试区间 `>= 0.1.5-rc.1, < 0.2.2` 内。
- Windows 只出免安装包，它始终自带运行时。

### macOS

```bash
unzip dsh-desktop_<v>_macos-<arch>-bundled.app.zip
mv "DSH Desktop.app" /Applications/
```

构建**未签名、未公证**，首次打开需右键 →「打开」；较新的 macOS 可能会要求到「系统设置 → 隐私与安全性」放行。
也可以直接去掉隔离标记：

```bash
xattr -dr com.apple.quarantine "/Applications/DSH Desktop.app"
```

- 最低系统：精简版 macOS 10.15；自带运行时版 **13.5**（随包 Node 24 的要求）。
- 原生窗口需要 Safari 16.4（macOS 13.3）以上的 WebKit，更旧的引擎会自动改用系统浏览器承载界面（见[缺陷](#缺陷)）。
- 开机自启：系统设置 → 通用 → 登录项 → 添加 `DSH Desktop.app`。

### Linux

```bash
sudo apt install ./dsh-desktop_<v>_linux-<arch>[-bundled].deb
```

WebKitGTK 不随包，系统需有 `webkit2gtk-4.1`、`gtk+-3.0`、`libsoup-3.0`；
从源码构建前可用 `make doctor` 检查并给出各发行版的安装命令。精简版另需 `node` 与 `dsh`。

### Windows

解压到**短路径**再双击。包内依赖树最深约 223 字符，路径太长时资源管理器会报 `0x80010135`，
此时改用同名的 `.exe` 自解压包，或 `tar -xf` 解压：

```
D:\dsh\DSH Desktop\dsh-desktop.exe
```

WebView 使用系统 WebView2（Windows 11 自带；Windows 10 可能需要另装 WebView2 Runtime）。

### 校验

每个 Release 附 `SHA256SUMS`：

```bash
shasum -a 256 -c SHA256SUMS   # Linux 用 sha256sum -c SHA256SUMS
```

Windows 可用 `Get-FileHash <文件> -Algorithm SHA256` 与清单中的值对照。

### 数据与配置

首次运行会在应用数据目录写入 `config.json`。目录按平台为：

| 平台 | 应用数据目录 |
|---|---|
| macOS | `~/Library/Application Support/com.deepseek.dsh.desktop/` |
| Linux | `~/.local/share/com.deepseek.dsh.desktop/` |
| Windows | `%APPDATA%\com.deepseek.dsh.desktop\` |

同目录还有：

| 文件 / 目录 | 内容 |
|---|---|
| `config.json` | 配置（手写 JSON，只写想改的字段即可） |
| `state.json` | 运行状态（pid、端口、cwd、启动时间），退出时删除 |
| `logs/harness.log` | 壳与 Harness 的日志，5 MiB × 4 份轮转 |
| `appearance.json`、`icons/` | 图标选择与自定义图标 |
| `app-update/` | 桌面应用自更新的下载、校验与备份 |

配置改动**下次启动生效**；文件整体无法解析时，本次运行使用默认值，且不会覆盖你的文件。

### 从源码构建

需要 Rust ≥ 1.77、Node 与 pnpm 12.3.4；Linux 先跑 `make doctor` 装系统依赖。

```bash
make check           # 类型检查（全部 targets，与 CI 口径一致）
make test            # 离线单测 + 脚本自检 + 真实 JS 引擎里的兼容层测试（需 PATH 上有 node）
make dev             # 运行 debug 版
make bundle          # 精简版安装包（macOS 出 .app，Linux 出 .deb）
make bundle-bundled  # 自带运行时版：先 staging Node/dsh（约 1.4 GB），再打包
```

## 功能

### 启动与进程监管

- 定位 `dsh`：依次查 `DSH_DESKTOP_DSH`、`config.json` 的 `dsh_path`、PATH、常见安装目录与登录 shell；
  每个候选都会校验是不是 `@deepseek-ai/dsh`，同名但不相干的 `dsh` 会被跳过。
- 固定端口（默认 `3080`）：启动 `dsh web --profile web --patch <overlay> --no-open --port N`，
  从 stdout 解析 launch token URL，交给系统 WebView。固定 authority 让 cookie 跨重启复用。
- 退出（关闭窗口、⌘Q、Dock 退出）向整个进程组发 SIGTERM，5 秒后 SIGKILL，并删除 `state.json`；
  启动超时或窗口创建失败也会先收尾再报错，不留占着端口的半启动实例。
- 状态页显示 dsh 版本与来源（`system` / `bundled` / `env`）以及实际监管的树路径。
- 自带运行时版在 profile 不存在时首启播种模板（含插件市场），之后交给插件市场自己更新；精简版永不播种。

### 实例复用与接管

- 上次启动的实例仍在服务 → 直接复用它，不重启。
- 端口被**外部** Harness 占用时弹面板（写明对方 pid、命令行与 workspace），可选：接管 /
  保留并用浏览器打开 / 保留并换端口 / 退出；120 秒无答复才按 `take_over_existing` 决定。
- 插件市场「立即重启」拉起的替代实例按命令行识别（本壳独有的 `--patch` overlay），不弹面板，直接停掉重起。
- 插件市场的恢复页占端口时：点名了插件就用浏览器打开它、不抢端口；没点名则请它释放后继续启动。
- 被强杀或崩溃后，下次启动读取 `state.json` 自愈：仍在服务的记录复用，其余清理后重起。

### 故障恢复

- Harness 意外退出 → 自动重启：干净退出等 8 秒交接（插件市场重启属于这一种），崩溃/被杀 2 秒后直接拉起；
  连续 3 次短命重启才停在终态状态页。
- 终态状态页有「重新启动 Harness」；此时再点 Dock 图标或从 Finder 重新打开，等价于按这个按钮。
- **绘制看护**：每 15 秒探测页面的动画帧、定时器心跳与 `document.visibilityState`。帧停但心跳在跑 =
  被 WebKit 暂停绘制，页面完好、**不重载**，标题提示露出窗口或用 ⌘R；两者都停或连续 3 次无应答才重载
  （最多 3 次，且会避开正在输入的时刻）。
- **渲染兜底**：窗口被遮挡时用定时器顶替动画帧，流式输出继续推进，切回来时内容已经追平。
- ⌘R /「视图 → 重新加载界面」拿到新文档，会话保留。
- 其他 WebKit 补偿：滚动锚定（展开思考块时正文不跳）、菜单弹层按住焦点（模型与思考强度点击生效）、
  空白弹窗交接（账户登录能自动打开浏览器）、页面错误与 `console.error` 回传日志。

### 更新

三类更新互相独立，各有缓存与失败退避：

1. **dsh 核心**：启动时查 npm registry（默认取 `latest`、`next`、`alpha` 中最高版本），有新版本先装进暂存目录，
   校验包名、版本与入口后原子切换；新版本起不来立即回滚。失败从 5 分钟起逐次 ×4 退避，最长 24 小时。
   `auto_update` 默认开；用户自己装的 CLI 默认只提示（`system_updates: notify`）。
2. **插件市场**：`auto_update_plugins` 默认关。开启后先快照 profile，再停实例、
   `dsh plugin --profile web add dshmarket@<版本>`；失败把快照放回。
3. **壳自己**：`app_update` 默认 `download`。Harness 窗口出现后由后台线程查 GitHub Releases，
   按平台、架构与是否自带运行时精确选资产，下载后与 `SHA256SUMS` 逐字比对，只有通过的包进 `app-update/ready/`。
   新版提示写在窗口标题；菜单可手动「检查桌面应用更新…」与「安装已下载的更新…」，下载完成的状态页也直接给安装按钮。
   `install` 在 macOS 上继续完成备份、替换与重启；Linux/Windows 停在已校验的包并给出手动命令。

### 外观

- 顶层「外观」菜单：默认 / 深海 / 极夜 / 晨雾 / 选择自定义图片… / 移除自定义图片。
  预置图标内嵌在二进制里，精简版与自带运行时版都有。
- 自定义 PNG 会先解码整理：展开调色板、灰度与 16 位图，非正方形按中心裁切，超过 1024 px 缩到 1024，
  短边小于 64 px 才拒绝；整理结果复制到 `icons/custom.png`，原文件移走或删除不影响。
- macOS 改的是 Dock 与 ⌘Tab 图标，Windows/Linux 改窗口图标；选择存在 `appearance.json`，每次启动重新应用。

### 环境变量与 API Key

- GUI 应用继承的是 launchd 环境，不读 `.zshrc`。壳默认导入一次登录 shell 环境
  （`$SHELL -lic env`，实测约 160 ms，超时 8 秒），因此 API Key 与 nvm/fnm 提供的 PATH 都可用。
- 导入过程会执行你的 shell 启动脚本；不想这样就把 `import_shell_env` 设为 `false`，
  改用 `config.json` 的 `env` 字段显式给出变量。
- 也可以在 Web GUI 的 Models 页面填写 Key（写进 `~/.dsh` 的凭证服务，与 shell 环境无关）。

### 安全

- Harness 窗口**零 capability**；导航限定当次 authority；外链与 `window.open` 只把 `http`/`https`
  交给系统浏览器，其他 scheme 记日志后丢弃。
- Harness 输出写日志前统一脱敏：`token`、`api_key`、`authorization`（含 `Bearer`）、
  `cookie`/`set-cookie`、`password`、`secret` 等。
- `config.json` 与 `state.json` 按仅当前用户可读写入（Unix 0600，Windows 显式 ACL）；
  本地状态页的 CSP 不含 `unsafe-inline`/`unsafe-eval`。

### 下载与附件

- 页面发起的下载落到 `~/Downloads`，重名自动加 `-1`/`-2` 后缀，不覆盖已有文件。
- 附件上传走 WebView 的原生文件选择器。

### 配置

`config.json` 的常用字段（都可省略）：

| 字段 | 默认 | 说明 |
|---|---|---|
| `port` | `3080` | 固定端口；authority 稳定才能复用 cookie 与接管上次实例 |
| `workspace` | `$HOME` | 传给 dsh 的工作目录，也就是 agent 的 workspace root |
| `dsh_path` | `null` | 记住的 `dsh` 启动器路径；不写也能从 PATH 与登录 shell 找到 |
| `dsh_home` | `null` | `null` 共用 `~/.dsh`；指向其他目录可隔离插件、设置与会话 |
| `take_over_existing` | `false` | 接管面板 120 秒无答复时的默认：`false` 用浏览器打开，`true` 终止并接管 |
| `auto_update` | `true` | 启动时检查并安装 dsh 新版本 |
| `auto_update_plugins` | `false` | 是否把 profile 里的 `dshmarket` 升到 registry 最新 |
| `update_tags` | `["latest","next","alpha"]` | 从这些 dist-tag 中取最高版本；只跟正式版就写 `["latest"]` |
| `update_check_interval_minutes` | `60` | 成功查询的缓存时长，`0` 表示每次启动都查 |
| `require_tested_dsh` | `true` | CLI 版本在已测试区间外时拒绝启动（同时约束自动安装） |
| `system_updates` | `"notify"` | 用户自己装的 CLI 有新版本时：只提示，或 `install` 就地升级 |
| `runtime` | `"auto"` | 运行时来源：`auto` / `bundled`（强制自带）/ `system` |
| `import_shell_env` | `true` | 启动时导入登录 shell 的环境变量 |
| `app_update` | `"download"` | 壳自己的更新策略：`off` / `notify` / `download` / `install` |
| `app_update_channel` | `"stable"` | 跟随 `stable` 或 `prerelease` 渠道 |
| `env` | `{}` | 追加或覆盖传给 harness 的环境变量，优先级最高，如 `{"DEEPSEEK_API_KEY": "sk-…"}` |
| `webkit_compat` / `render_fallback` / `keep_menu_focus` / `page_diagnostics` / `scroll_anchor` | `true` | 旧 WebView 兼容层、遮挡补帧、菜单焦点、页面错误回传、滚动锚定；排障时可逐项关闭 |

### 菜单

| 菜单项 | 位置 | 作用 |
|---|---|---|
| 重新加载界面 | 视图（⌘R） | 重新加载 Harness 当前 URL，会话保留 |
| 检查桌面应用更新… | 视图 | 忽略缓存，当场检查壳的新版本 |
| 安装已下载的更新… | 视图 | 安装 `app-update/ready/` 里已校验的包 |
| 外观 | 顶层 | 切换预置图标，或选择 / 移除自定义图片 |

## 缺陷

以下为已知问题与限制。

### 分发与平台

- **未签名、未公证**：macOS 首次打开需右键或 `xattr`；自更新只比对 `SHA256SUMS`，不验证代码签名。
- **平台验证不均**：Linux（x64 / arm64）的包由 CI 构建、尚未在真机跑过；Windows 只有免安装包。
  macOS arm64 与 Windows 免安装包做过实机验证。
- **自带运行时版体积大**：解压后约 1.4 GB / 9 万个文件，且 macOS 最低要求被 Node 24 抬到 13.5。

### 更新与版本

- **就地安装只有 macOS**：Linux 的 `.deb` 需要 root，Windows 无法覆盖运行中的 `.exe`，
  这两处只做到「下载并校验 + 给出安装命令」。
- **`app_update_channel = "prerelease"` 当前与 `stable` 等价**：发布流水线从不把 Release 标成 prerelease，
  预发布渠道实际没有内容。
- **自更新检查走未鉴权的 GitHub API**（每 IP 每小时 60 次）：共享出口 IP 上可能取不到，失败会按退避重试。
- **`ready/` 里的包不会在重启时自动安装**：它只是准备好，安装入口是菜单或状态页按钮。
- **区间外版本默认拒绝启动**：已测试区间为 `>= 0.1.5-rc.1, < 0.2.2`，正式版 `0.2.2` 尚未验证；
  把 `require_tested_dsh` 设为 `false` 可以继续，但状态页会标注「未测试版本」。

### WebView 与界面

- **旧引擎没有原生窗口**：可补的 API（`Iterator`、`Promise.try`、`Math.sumPrecise` 等）由兼容层注入，
  Safari 16.4（macOS 13.3）以上即可；补不了的 `class static block`（macOS ≤ 12）只能改用系统浏览器承载界面。
- **被遮挡时 WebKit 暂停绘制**：渲染兜底能让流式输出继续推进，但切回窗口可能短暂落后；
  两者都停或渲染进程被杀时最多自动重载 3 次，之后只能重启应用。

### 使用限制

- **没有设置界面**：所有配置都要手改 `config.json`，下次启动生效。
- **图标只改运行期**：Finder / 访达里看到的仍是包内的原图标；自定义图片按中心裁切，不能选裁切位置。
- **只有一个 workspace**：没有多 workspace 切换 UI，换 workspace 要改配置并重启。
- **精简版没有插件市场**：市场来自 profile，精简版不会播种 profile；
  需要自己执行 `dsh plugin --profile web add dshmarket`。
- **并发边界**：默认共用 `~/.dsh`，终端里另开同 profile 的 `dsh` 会与壳并发读写会话存储；
  需要严格隔离就把 `dsh_home` 指向别的目录。
- **实例复用依赖 30 天有效的 cookie**：清过应用数据或超过 30 天后，窗口会显示
  `dsh web authentication required`，退出应用再启动即可。
- **日志里保留路径**：脱敏只覆盖凭据，workspace 路径等仍会留在 `logs/` 中；需要更少留存请自行清理。

设计文档与实施记录见 `docs/`。