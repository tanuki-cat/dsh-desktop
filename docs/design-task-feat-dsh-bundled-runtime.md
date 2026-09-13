# 桌面壳自带 Node 与 dsh 核心的发行方案

> 目标：在**没有预装 Node.js 和 DeepSeek Harness** 的机器上，双击即用。
> 上游设计：[`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md)（其 §22 已把本方案列为 V2）。
>
> **v2（review 后修订）**：补齐回退、版本仲裁、pnpm、按平台 staging 等 11 项（§15 修订记录）；下载本文所指的"离线首启"
> 仍是**待断网实测**的设计目标，不是已验证结论。
> 本文所有数字与机制均为本机实测（macOS 14/15 arm64，`@deepseek-ai/dsh` 0.1.5-rc.2）。

---

## 0. 结论摘要

方案可行为 **"只读 seed + 可写影子前缀"**：Node 与 dsh 只读地放在 `.app` 里作为种子，
**首个 dsh 核心更新落在 app-data 的可写前缀**中，之后的解析顺序优先用后者。
关键依据（全部实测）：

| 事实 | 实测结果 |
|---|---|
| 官方 Node 发行版**自带 npm** | `node-v22.23.2-darwin-arm64.tar.gz` 48 MB → 解压 **187 MB**，`bin/` 含 `corepack node npm npx`，npm 10.9.8 |
| 只靠自带 node 能跑 harness | `env -i PATH=/usr/bin:/bin` + 自带 node + 复制的 dsh 树 → **成功输出 `dsh web: http://127.0.0.1:…/?token=…`**，stderr 为空 |
| dsh 安装树体积 | **289 MB / 25,412 文件**（dsh 包本身只有 10 个文件，体积来自 72 个依赖），其中 **12 个原生 `.node`** |
| `npm install -g --prefix P` 产物布局 | `P/lib/node_modules/<pkg>`（实测），即更新目标前缀的布局与现有定位逻辑一致 |
| `bundle.resources` | glob **相对 `src-tauri/`** 解析，产物落到 `Contents/Resources/runtime/…`；**匹配为空会让 `tauri build` 直接失败** |
| harness **不写自己的安装树** | 用真实安装树 + 全新 `DSH_HOME` 启动后，安装树被修改文件数 = **0**；`profiles/`、`storages/`、`.dsh-module-fallback` 全在 `DSH_HOME` ⇒ **只读 seed 成立** |

体积现实：解压后 **187 + 289 ≈ 475 MB**（`.app` 约 490 MB），压缩分发量级 150–250 MB。
这是本方案最大的代价，§7 给出取舍。

---

## 1. 目标 / 非目标

**目标**

- 目标机器不需要 Node、不需要 npm、不需要预装 dsh；
- 首次启动**不依赖首启下载**（目标：离线可用；**待断网实测**，见 §12）；
- dsh 核心仍可自动更新（现有更新检查保留），且更新不破坏代码签名；
- macOS 与 Linux 优先，Windows 留到后续阶段。

**非目标**

- 不用自带运行时代替系统 WebView（macOS 用 WKWebView、Linux 仍需 WebKitGTK，见 §9）；
- 不打包 marketplace 第三方插件（由用户按需安装，走 profile 的 pnpm）；
- 不修改 Harness 本体。

---

## 2. 目录布局

### 2.1 `.app` 内（只读 seed）

```text
DSH Desktop.app/Contents/Resources/runtime/
├── node/                     # 官方 Node 发行版（平台相关），含 bin/node、bin/npm、lib/node_modules/npm
│   └── LICENSE               # Node 许可（MIT）
├── dsh-prefix/               # 按 npm 全局前缀布局的 dsh 安装树
│   └── lib/node_modules/@deepseek-ai/dsh/{lib,node_modules,package.json,LICENSE}
└── THIRD-PARTY-NOTICES.md    # Node / npm / dsh 及依赖的许可汇总
```

### 2.2 app-data 内（可写）

```text
~/Library/Application Support/com.deepseek.dsh.desktop/    (Linux: ~/.local/share/…)
├── config.json               # 现有配置
├── runtime/
│   ├── prefix/               # dsh 更新落点（npm --prefix 目标）
│   └── npm-cache/            # 更新用 npm 缓存，避免重复下载 289MB 依赖树
├── update-check.json         # 现有更新检查缓存（§13.6）
└── logs/harness.log
```

### 2.3 解析顺序（改动点）

| 目标 | 顺序 |
|---|---|
| node | `DSH_DESKTOP_NODE` → `Resources/runtime/node/bin/node` → 系统 PATH → login shell |
| dsh | `app-data/runtime/prefix/…` 与 `Resources/runtime/dsh-prefix/…` **取版本更高者** → `DSH_DESKTOP_DSH` → 系统 PATH → login shell |

规则细化（review 后补，避免三类失效）：

1. **版本仲裁**：候选（影子前缀 / seed）必须 `lib/bin.js` 存在且能读到版本号；**取版本号更高者**，
   而不是固定优先级 —— 否则 `.app` 升级带来更新的 seed 后，仍会跑 app-data 里的旧核心。
2. **回退**：若被选中的树在启动阶段失败（spawn 出错或等不到 URL），**自动改用另一个候选重试一次**，
   并把失败写入日志；两个都失败才报错。
3. **last-known-good**：每次成功启动后记录所用树与版本，供下次诊断与回退参考。

即：**自带优先，系统兜底**；开发机上仍可用系统安装（配置可强制 `runtime: system`）。

---

## 3. 首次启动流程

1. 解析运行时（§2.3），日志记录来源（`runtime: bundled / system`）与版本；
2. 确保 `app-data/runtime/{prefix,npm-cache}` 存在（`create_dir_all`，幂等）；
3. 更新检查（现有逻辑）→ 需要更新时走 §4；
4. 用 `Resources/runtime/node/bin/node` 启动 `<dsh 树>/lib/bin.js --profile web --patch <overlay> --no-open --port <固定端口>`；
5. 其余（探测/接管/窗口/退出回收）不变。

**不复制 seed**：直接跑只读 seed，省掉首启 475 MB 拷贝（~10–30 s）与双份磁盘占用；
只有"更新"才在 app-data 落地。

---

## 4. dsh 核心更新流程（与现有实现的差异）

现有实现（`update.rs`）已经在用 `dirname(node)/npm` 解析 npm，并把 `install_prefix(dsh_js)` 交给 `npm install -g --prefix`。
自带运行时下只需两处调整：

1. **更新目标前缀**改为 `app-data/runtime/prefix`（不再从 `Resources` 里的 dsh 路径推导 —— 那是只读的，且写入会破坏签名）；
2. 追加 `--cache <app-data>/runtime/npm-cache`，让 289 MB 依赖树在多次更新之间复用缓存。

解析顺序（§2.3）会让更新后的版本自动生效；结合现有"更新后强制重启实例"的规则，更新在同一轮启动内完成。

**更新失败与回滚**（review 后补）：

- npm 安装到**临时目录** `app-data/runtime/prefix.new`，成功后原子替换 `prefix`（`rename`），
  避免半成品前缀破坏下次启动；
- 若替换后启动失败 → 依 §2.3 的"回退"用 seed 启动，并保留坏掉的前缀供诊断；
- 记录 `last-known-good`（树 + 版本 + 时间），提供"恢复上次可用版本"的路径。

**第三方插件（pnpm）**（review 后补，干净机器上的真实缺口）：

- 官方 Node 发行版只带 `corepack` 与 `npm`，**没有 pnpm**，而 `dsh plugin add …` 是转发给 pnpm 的；
- 因此 staging 时要把 pnpm 一并装进 `app-data/runtime/prefix`（`npm i -g pnpm`），并把该前缀的 `bin`
  注入 harness 子进程的 `PATH`（与 §5 的 PATH 策略一致）；
- 不这样做时，自带发行版的 marketplace 插件将无法安装 —— 必须在文档里显式声明。

---

## 5. 构建流程（Makefile 增补）

```bash
make runtime-fetch     # 按 uname/arch 下载官方 Node，并校验 SHASUMS256.txt
make runtime-stage     # 组装 src-tauri/runtime/{node,dsh-prefix,THIRD-PARTY-NOTICES.md}
make bundle            # 打包（依赖 runtime-stage）；resources 已配置
make runtime-clean     # 清理 staging（约 475MB）
```

要点：

- `runtime-fetch` 必须**校验官方 SHA256**，不校验的下载不应进入发布产物；
- `runtime-stage` 的 dsh 树来源：**正确配方是 `npm install --prefix <stage>/dsh-prefix @deepseek-ai/dsh@<版本>`**
  （开发机也可直接复制本地已安装树）。注意：dsh 自身发布的 tarball 只有 10 个文件，289 MB 全部来自它拉取的
  72 个依赖 —— 所以不存在"`npm pack` 后再装依赖"的捷径（v1 表述有误，已修正）；
- **staging 必须按平台各自执行**：树里含 12 个平台相关的原生模块（`pty`/`conpty`/`koffi`/`sharp-darwin-*`/`system`），
  macOS 上 stage 的树不能用于 Linux，反之亦然；CI 需 per-platform runner（macOS arm64/x64、Linux x64/arm64），
  不建议依赖 `npm install --os/--cpu` 跨平台产原生二进制；
- **子进程 PATH 策略**（review 后补）：启动 harness 时把 `Resources/runtime/node/bin` 与
  `app-data/runtime/prefix/bin` 前置进 PATH，使插件安装、MCP server、agent 执行的 `node`/`npm`/`pnpm`
  与壳内运行时一致；该决策要写进日志与诊断页，避免"为什么我的命令用的不是登录 shell 的 node"变成暗坑；
- `tauri.conf.json` 已加 `"resources": ["runtime/**/*"]`，且 `src-tauri/runtime/README.md` 作为**非空保证**存在
  （glob 匹配为空会导致 `tauri build` 失败，已实测）；
- `make clean` 不动 staging，`make runtime-clean` 才删（避免每次改代码都重下 48MB）。

---

## 6. 体积预算与取舍

| 组成 | 解压 | 压缩（估算/实测） |
|---|---|---|
| Node（darwin-arm64） | 187 MB | 48 MB |
| dsh 树（含 12 个原生模块） | 289 MB | 数十 MB（tarball 本身很小，依赖树按需下载） |
| Tauri 壳 | 11 MB | 5 MB |
| **合计** | **≈ 490 MB** | **≈ 150–250 MB** |

**构建期磁盘**（review 后补）：staging 475 MB + `target/…/bundle` 内再复制一份 ≈ 475 MB，
再加 debug/release 构建产物 ⇒ 峰值约 **1.5 GB**；CI 需要相应磁盘配额，`make runtime-clean` 用于回收。

可选削减（按推荐度）：

1. **不做手工裁剪**（推荐）：289 MB 里任何依赖都可能被某个 profile/插件在运行时按需加载，删文件的风险远大于收益；
2. 若接受首启联网，可只带 Node、dsh 树改为首启从 registry 拉取（`.app` 降到 ~200 MB）——**但违背"离线首启"目标**，仅作为备选；
3. 未来可用 `npm install --omit=dev --ignore-scripts` + 按平台过滤可选依赖做 CI 侧瘦身，需要逐平台验证启动。

---

## 7. 签名与公证（macOS）

- `.app` 内所有 Mach-O 都必须签名且同一 Team ID：**Node 二进制 + 12 个原生模块**
  （`pty.node`、`conpty.node`、`koffi.node`、`sharp-darwin-*.node`、`system.node` 等）；
- Node 官方二进制不是我们的签名，需要 `codesign --force --options runtime --timestamp` 重新签；
- 顺序：`runtime-stage` → 对 `runtime/` 里的 Mach-O 逐个签名 → `tauri build`（签 app 壳）→ 公证 + `stapler`；
- 验证命令（必须纳入 CI 与验收）：
  `codesign --verify --deep --strict -vvv "DSH Desktop.app"`、
  `spctl -a -vvv "DSH Desktop.app"`、`xcrun stapler validate "DSH Desktop.app"`；
- **复制保真**：`tauri build` 把 `runtime/` 复制进 bundle 时必须保留 mode/xattr 与已有签名，
  构建后要抽验（`codesign -dv` 对 node 与 2~3 个 `.node` 抽查）；任何"复制后再改文件"都会让封装签名失效；
- **签名后置校验必须紧跟 bundle 步骤**：`codesign --verify --deep --strict` → `spctl -a -vvv` →
  `xcrun stapler validate`，任一失败即视为构建失败（CI 门禁）；
- **不要在签名的 bundle 内写入**——这正是 §2.2/§4 把更新落到 app-data 的原因。

---

## 8. 平台矩阵

| 平台 | Node 发行版 | 产物 | 备注 |
|---|---|---|---|
| macOS arm64 | `darwin-arm64`（48 MB） | `.app` / dmg | 先做 |
| macOS x64 | `darwin-x64`（49 MB） | 同上 | 与 arm64 分别发布，或做 universal（两套 node + 按 `uname -m` 选） |
| Linux x64 | `linux-x64`（30 MB, tar.xz） | `.deb` / AppImage | AppImage 适合"免安装"，但仍依赖系统 WebKitGTK |
| Linux arm64 | `linux-arm64`（29 MB） | 同上 | — |
| Windows | `win-x64` | msi/nsis | 后续；需另做 Job Object 与 `taskkill` 之外的清理路径 |

---

## 9. Linux 的残余依赖（必须写明）

自带 Node 与 dsh **不能**消除 Linux 上的图形栈依赖：WebKitGTK / GTK3 / libsoup 仍需系统提供
（AppImage 也很难可靠内嵌）。因此 Linux 上的说法应是"**不需要 Node 与 dsh**"，而不是"零依赖"。
`make doctor` 已经会检查这三项并给出各发行版的安装命令。

---

## 10. 许可与合规

- Node.js：MIT（含 npm 为 Artistic-2.0），随发行版带 `LICENSE`；
- `@deepseek-ai/dsh`：BSD-3-Clause；
- 需在 `THIRD-PARTY-NOTICES.md` 汇总 Node / npm / dsh 及其依赖的许可证；
- marketplace 第三方插件**不随包分发**，由用户安装，责任与许可归其作者。

---

## 11. 代码改动清单

| 文件 | 改动 | 估计 |
|---|---|---|
| `runtime.rs`（新） | 解析 seed 路径、确保 app-data 目录、**候选探测 + 版本仲裁 + 回退**、报告来源与版本 | ~200 行 + 测试 |
| `runtime.rs`（新） | `last-known-good` 记录与诊断输出 | 含上行 |
| staging（Makefile） | 把 **pnpm** 一并装进前缀；子进程 PATH 注入清单 | ~15 行 |
| `locator.rs` | 接受 `BundledRoots`，按 §2.3 顺序解析，记录来源 | ~40 行 |
| `update.rs` | `install_prefix` 支持 app-data 目标；追加 `--cache`；**装到 `prefix.new` 后原子替换**；`engines.node` 兼容性检查 | ~50 行 |
| `lib.rs` | 取 `app.path().resource_dir()` 并注入；splash 文案区分自带/系统 | ~25 行 |
| `tauri.conf.json` | `bundle.resources`（已完成）；签名配置 | 少量 |
| `Makefile` | `runtime-fetch / runtime-stage / runtime-clean`，`bundle` 依赖 chain | ~60 行 |

---

## 12. 验收标准

- [ ] 在**没有任何 Node/npm/dsh** 的干净 macOS 虚拟机里双击运行 → 出现窗口并能开始对话；
- [ ] 断网首启可用（seed 完整、无首启下载）；
- [ ] `env -i PATH=/usr/bin:/bin` 冒烟测试：自带 node 能启动 harness 并输出 URL（**原型已通过**）；
- [ ] 有新版 dsh 时，更新落到 `app-data/runtime/prefix`，重启实例后跑新版本；`Resources` 未被写入；
- [ ] `codesign --verify --deep --strict` / `spctl -a -vvv` / `stapler validate` 全部通过；
- [ ] 体积不超过阈值（解压 ≤ 500 MB，分发包 ≤ 250 MB）；
- [ ] 系统已装 node/dsh 的开发机上，仍能通过配置切回系统运行时（用于开发与排障）；
- [ ] **断网首启实测**（拔网/禁网后首启，不使用任何网络）；
- [ ] **插件安装可用**：干净机器上能装 marketplace 插件（验证 pnpm 已随包/staging 就位）；
- [ ] **回退演练**：人为破坏 app-data 前缀（删 `lib/bin.js`）后仍能用 seed 启动；
- [ ] **版本仲裁**：把 app-data 前缀替换成旧版本时，启动应选用 seed 里更高的版本；
- [ ] **更新中断演练**：更新过程被 kill 后，下次启动仍可用（`prefix.new` 原子替换生效）。

---

## 13. 风险与未决问题

1. **体积**是主要代价（≈490 MB 安装、150–250 MB 分发）；需产品侧确认可接受。
2. **公证下的原生模块**：12 个 `.node` 每个都要签，任何一个遗漏都会导致启动被 Gatekeeper 拦。
3. **npm 更新时的磁盘峰值**：更新会在 `app-data/runtime/prefix` 重建依赖树，瞬时可能占用约 2× 289 MB。
4. **备选路线未验证**：首启下载式安装（体积小但有网络依赖）本次只做纸面评估。
5. **Windows 平台**未设计（Job Object、路径、签名流程都不同）。
6. **pnpm 缺失**：不自带 pnpm 时，自带发行版无法安装 marketplace 插件（已列入 P0-3 修正）。
7. **版本仲裁实现错**：仲裁规则写错会让"更新了 .app 却跑旧核心"或反之；需专门测试覆盖（§12）。
8. **构建期磁盘峰值**：约 1.5 GB（staging + bundle 复制 + 构建产物），CI 配额不足会表现为构建失败。
9. **跨平台 staging**：原生模块绑定平台，任何"在 macOS 上产出 Linux 包"的捷径都不成立。

---

## 14. 分阶段实施

| 阶段 | 范围 | 产出 |
|---|---|---|
| P1 | macOS arm64：`runtime-fetch/stage` + 解析顺序 + 更新落点 | 可在无 node 的 Mac 上双击运行的 `.app`（未签名） |
| P2 | 签名 + 公证 + 干净虚拟机验收 | 可分发（本机与受信任渠道） |
| P3 | Linux x64/arm64（deb + AppImage） | 两条命令产出的安装包 |
| P4 | macOS universal / Windows / `.app` 自更新（Tauri updater） | 完整发行矩阵 |

---

## 15. 修订记录（v2，review 后）

| # | 级别 | 问题 | 处理 |
|---|---|---|---|
| 1 | P0 | 影子前缀损坏后无回退，seed 可用却起不来 | §2.3 增加"回退 + last-known-good"，§4 增加 `prefix.new` 原子替换 |
| 2 | P0 | seed 与影子前缀无版本仲裁，.app 升级后可能跑旧核心 | §2.3 改为"取版本更高者" |
| 3 | P0 | 干净机器没有 pnpm，marketplace 插件无法安装 | §4 增加 pnpm staging 与 PATH 注入，§12 增加验收 |
| 4 | P0 | 原生模块必须按平台 staging；v1 的 `npm pack` 配方有误 | §5 改为 `npm install --prefix`，明确 per-platform runner |
| 5 | P1 | 子进程 PATH 策略未定义 | §5 明确"前置 bundled node/bin 与 prefix/bin"，并写入诊断 |
| 6 | P1 | 未检查 harness 对 Node 版本的要求 | §11 `update.rs` 增加 `engines.node` 兼容性检查 |
| 7 | P1 | 资源复制保真与签名后校验未写 | §7 增加复制保真抽验、签名后置校验作为 CI 门禁 |
| 8 | P1 | 构建期磁盘峰值未计入 | §6 增加约 1.5 GB 的构建期预算 |
| 9 | P2 | "离线首启"表述像已验证 | §1 标注为待断网实测；§12 增加该验收项 |
| 10 | P2 | 校验强度说明不清 | §5/`runtime-fetch` 明确只校验 `SHASUMS256.txt`（不含 gpg 签名） |
| 11 | P2 | AppImage 只读挂载与 strip 风险 | §9/§8 标注为待实测（只读挂载与设计兼容） |

新增的已核实事实（本轮实测）：**harness 对自己的安装树 0 写入**，证据见 §0 表格 —— 这是"只读 seed"成立的前提。
