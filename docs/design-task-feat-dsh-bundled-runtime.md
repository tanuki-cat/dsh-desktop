# 桌面壳自带 Node 与 dsh 核心的发行方案

> **2026-09-13 更新**：本方案已实施并**合并回 `main`**（`Merge branch 'feat/bundled-runtime' into main`）。
> 下面这段"分支策略"是合并前的记录，保留原样；后续开发与发布直接在 `main` 上进行。
>
> <details><summary>合并前的记录</summary>
>
> **分支策略（2026-09-13 起）**：本方案实施在长期分支 `feat/bundled-runtime` 上，**暂不合并到 `main`**。
> 该分支当前的对外用途是**构建与发布 Windows 免安装版**（`.github/workflows/windows-portable.yml`，
> 由 `main` 手动触发、构建 `build_ref` 指向的分支代码；在分支上打 tag 则由 `release.yml` 的
> `windows-portable` 任务一并发布）。macOS/Linux 的自带运行时继续在该分支上验证，
> `main` 保持“需要预装 node 与 dsh”的现状，只保留调度用的 workflow。合并时机由后续决定，
> 合入前不要把这里的改动同步回 `main`（`release.yml` 会因此多出一个构建不出有效运行时的 Windows 任务）。
>
> </details>

> 目标：在**没有预装 Node.js 和 DeepSeek Harness** 的机器上，双击即用。
> 上游设计：[`design-task-feat-dsh-tauri-desktop-shell.md`](./design-task-feat-dsh-tauri-desktop-shell.md)（其 §22 已把本方案列为 V2）。
>
> **v3.3（随包附带插件市场）**：全新 DSH_HOME 的 profile 是空的、没有安装入口，必须附带 dshmarket
> —— 实测 6.5 MB 纯 JS 模板 + 首启播种（§2.5）。
>
> **v3.2（与壳方案做兼容性对齐后）**：两个硬冲突（`install_prefix` 会把更新写进只读 bundle、
> `minimumSystemVersion` 低于 Node 的实际门槛）与两处语义冲突（环境变量覆盖顺序、PATH 语义），见 §18。
>
> **v3.1（第三轮 review 后）**：补上"更新时不能动正在服务的树"（node 懒加载，实测 MODULE_NOT_FOUND）、
> 子进程全局安装落点、staging 的垃圾/xattr 闸门、npm cache 回收等 9 项，见 §17。
>
> **v3（第二轮 review 后修订）**：把纸面风险改成实测结论 —— 签名必须保留 Node 的 JIT entitlements（否则 node 直接崩溃）、
> 资源复制会解引用符号链接、悬空链接会让构建失败、`rename` 无法覆盖非空目录、pnpm 不能装在被整体替换的前缀里等 12 项
> （§16）。"离线首启"仍是**待断网实测**的目标，不是已验证结论。
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
- 除**插件市场本身**外不打包第三方插件：其余插件仍由用户按需安装（走 profile 的 pnpm）；
  市场必须随包附带，理由见 §2.5（否则干净机器没有任何安装入口）；
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
├── tools/                    # 随包分发的 pnpm 前缀（离线装插件的前提，见 §4）
│   └── bin/pnpm
└── THIRD-PARTY-NOTICES.md    # Node / npm / dsh 及依赖的许可汇总
```

### 2.2 app-data 内（可写）

```text
~/Library/Application Support/com.deepseek.dsh.desktop/    (Linux: ~/.local/share/…)
├── config.json               # 现有配置
├── runtime/
│   ├── prefix/               # dsh 更新落点（npm --prefix 目标），更新时会被整体替换
│   ├── tools/                # pnpm 的**更新落点**（可选；随包已带一份，不随 dsh 更新被替换）
│   └── npm-cache/            # 更新用 npm 缓存，避免重复下载 289MB 依赖树
├── update-check.json         # 现有更新检查缓存（§13.6）
└── logs/harness.log
```

### 2.3 解析顺序（改动点）

| 目标 | 顺序 |
|---|---|
| node | `DSH_DESKTOP_NODE` → `Resources/runtime/node/bin/node` → 系统 PATH → login shell |
| dsh | `DSH_DESKTOP_DSH` → （`app-data/runtime/prefix/…` 与 `Resources/runtime/dsh-prefix/…` **取版本更高者**）→ 系统 PATH → login shell |

规则细化（review 后补，避免三类失效）：

1. **版本仲裁**：候选（影子前缀 / seed）必须 `lib/bin.js` 存在且能读到版本号；**取版本号更高者**，
   而不是固定优先级 —— 否则 `.app` 升级带来更新的 seed 后，仍会跑 app-data 里的旧核心。
2. **回退**：若被选中的树在启动阶段失败（spawn 出错或等不到 URL），**自动改用另一个候选重试一次**，
   并把失败写入日志；两个都失败才报错。
3. **last-known-good**：每次成功启动后记录所用树与版本，供下次诊断与回退参考。

即：**自带优先，系统兜底**；开发机上仍可用系统安装（配置可强制 `runtime: system`）。

排序原则（与壳方案对齐，v3.2 明确）：**显式覆盖永远第一**。壳方案 §3 与 README 都承诺
`DSH_DESKTOP_DSH` / `DSH_DESKTOP_NODE` 是"指定用哪一个"的排障开关；
若自带候选排在它前面，这个开关就会静默失效。

### 2.4 混合运行时：系统只装了一半怎么办（v3 补充，含实测）

提议的默认策略（按"本机已装什么"决定每一半的来源）：

| 本机 node | 本机 dsh | 采用 |
|---|---|---|
| 有 | 有 | 两者都用系统的（尊重用户既有安装） |
| 有 | 无 | node 用系统的，dsh 用自带的 |
| 无 | 有 | node 用自带的，dsh 用系统的 |
| 无 | 无 | 两者都用自带的 |

**可行性结论：可以，而且比预想的安全。** 原以为"混搭"会撞原生模块 ABI，实测本机 dsh 树里的原生模块
**全部是 N-API**（`nm -u` 检查：napi 符号 23–88 个、V8 符号 0 个），N-API 跨 Node 大版本 ABI 稳定，
所以"系统的 node + 自带的 dsh 树"（或反之）不会因为 ABI 直接崩。真正的门槛只剩两条：

1. **架构必须一致**：系统 node 可能是 x64（Rosetta）而自带预编译产物是 arm64，混用会加载失败 ⇒
   比较 `process.arch` 与自带运行时一致才允许混搭。**v3.4 修正**：这条只在该构建**确实带运行时**
   （seed 的 node 与 dsh 都在）时才能拒绝；没有自带候选时降级为警告并继续用系统安装 ——
   x64 node + x64 dsh 树本身自洽，拒绝它只是把“能跑但慢”换成错误页；
2. **能力门槛，而不是版本号**：`@deepseek-ai/dsh` 的 `package.json` **没有 `engines.node` 字段**（实测），
   所以"按 engines 判断"落空。可用的硬指标是能力探测：dsh 的 `@deepseek-ai/dsh-code-runtime-worker-thread`
   依赖 `module.stripTypeScriptTypes()`（Node ≥ 22.13；日志里那条 `ExperimentalWarning: stripTypeScriptTypes` 就是它），
   因此对候选 node 跑一次 `node -e "typeof require('module').stripTypeScriptTypes === 'function'"` 即可判定
   能否使用；探测失败则该半回退到自带运行时。

**必须一起定的两条规则**（否则这套策略会自我矛盾）：

- **更新语义**：若采用系统 dsh，而更新仍落到自带的影子前缀，那条"更高版本优先"的规则会让更新后的核心
  反过来盖掉用户自己的安装。原建议是"使用系统 dsh 时不做自动安装，只提示有新版"；**实现时改成可配置**
  （v3.4，见 §21）：`config.json` 的 `system_updates` 默认 `install`，即沿用自带运行时之前"有新版就装进
  用户自己的前缀"的既有行为 —— 老用户静默失去自动升级比一次 npm 全局写入更糟；想只提示就写 `notify`。
  装到哪个前缀仍由 `update::install_prefix(dsh_js)` 从 CLI 位置反推（与旧实现一致）；自带运行时不受该开关
  影响（始终写影子前缀）。"GUI 偷偷改我环境"的顾虑交给这个显式开关，而不是替用户默认关掉；

  > **2026-09-15 修正（默认值反转）**：`system_updates` 默认改为 **`notify`**。上面这条决策的权衡是
  > 「老用户静默失去自动升级比一次 npm 全局写入更糟」；反转的理由是：`npm install -g` 改的是**机器上
  > 其它工具也在用**的全局前缀，而桌面壳无法知道谁还在用它。想保留原来的行为就显式写 `install`。
  > 自带/影子运行时的自动更新不受影响 —— 那棵树是本壳自己的。
  >
  > 同轮一并收敛：`auto_update_plugins` 默认 `false`（profile 是用户数据；这也消解了
  > `design-task-fix-v0-2-0-post-merge-audit.md` 的 A3「两个更新开关策略不一致」），
  > `update_tags` 默认 `["latest"]`。见 `docs/dsh-desktop-v0.4.1-code-review.md` 第 2 项。
- **决策要稳定且可见**：系统运行时依赖登录 shell 探测，rc 文件改动或导入失败都会让结果在两次启动之间翻转。
  因此：探测一次后把结论与版本写进配置/诊断，并保留手动 `runtime: system | bundled | auto` 覆盖；
  启动日志与状态页显示 `runtime: system (node 22.23.2 / dsh 0.1.5-rc.2)` 这类信息。

**代价**：支持矩阵从 1 种变成 4 种组合。建议把（系统+系统）与（自带+自带）列为受支持组合并纳入验收，
两种混搭标注为"尽力而为"（有上面两道门槛 + 失败回退），并在 README 写明。

### 2.5 随包附带插件市场 dshmarket（v3.3 新增，含实测）

**问题**：dsh 在全新 `DSH_HOME` 上生成的 profile 是**完全空的**（实测）：

```json
{ "name": "dsh-profile-web", "private": true, "dependencies": {},
  "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"], "patchReload": "live" } } }
```

而 `dsh plugin --profile <name> <args…>` 其实是**对 pnpm 的透传**（实测：帮助输出就是 pnpm 自己的），
也就是说干净机器上既没有安装入口、也没有任何插件 —— 这正是"必须随包附带市场"的原因。

**做法：profile 模板 + 首次播种**（已实测可行）

1. staging 时在临时目录用**真 pnpm** 生成模板：
   `package.json`（`dependencies: { dshmarket: "<pin>" }`，bundles 追加 `dshmarket`）、
   与 dsh 生成一致的 `pnpm-workspace.yaml`，然后 `pnpm install`；
2. 把整个 `profiles/web` 目录（`package.json` / `pnpm-lock.yaml` / `pnpm-workspace.yaml` / `node_modules`）
   放进 `.app` 的 `Resources/runtime/profile-template/`；
3. 首次启动、且 `$DSH_HOME/profiles/web` **不存在**时，把模板复制过去，再启动 harness。

**实测数据**：

| 项 | 结果 |
|---|---|
| 模板体积 | **6.5 MB**（`dshmarket` + `js-yaml` + `argparse` + `undici`，全部纯 JS，**无原生模块**） |
| 平台相关性 | 与 dsh 树不同，模板是**纯 JS ⇒ 一套通用**，不必按平台各做一份 |
| 播种后 pnpm 是否认它 | 认：`dsh plugin --profile web list` → `dshmarket@1.45.1`（手抄的 node_modules 不行 —— 缺 pnpm 的元数据） |
| 播种后能否启动 | 能：`dsh web` **4 秒**输出启动 URL，stderr 为空，无模块解析错误 |
| 市场是否真的加载 | 是：profile 目录里出现了只有 dshmarket 才会创建的 `.dsh-market/` |

**边界与策略**：

- **只在缺失时播种**：用户已有 `profiles/web`（例如共用 `~/.dsh` 的老用户）时一个字节都不动；
  这类用户本来就能自行 `dsh plugin add dshmarket`。可选：给一个"安装插件市场"的显式入口；
- **市场自身可升级**：它只是普通依赖，用户/市场界面升级后落在 DSH_HOME，与 seed 模板无关（模板只在首次播种）；
- **安装其他插件仍需网络与 pnpm**：随包解决的是"没有入口"和"离线可用"，不是"离线装任意插件"；
- **许可**：`dshmarket` 为 MIT（其依赖 js-yaml/argparse/undici 也需一并收录进 `THIRD-PARTY-NOTICES.md`，见 §10）；
- **供应链**：模板在 staging 时由 npm registry 安装并固定版本，锁文件随包分发，便于审计。

---

## 3. 首次启动流程

1. 解析运行时（§2.3），日志记录来源（`runtime: bundled / system`）与版本；
2. 确保 `app-data/runtime/{prefix,tools,npm-cache}` 存在（`create_dir_all`，幂等）；
2b. **首次播种 profile 模板**（§2.5）：若 `$DSH_HOME/profiles/web` 不存在，把 `Resources/runtime/profile-template/`
   复制过去（内含插件市场 dshmarket）。存在则完全不动；
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

**首启的更新策略（review 后补）**：自带 seed 若低于 registry head，而 `auto_update` 默认 true，
第一次启动就会下载并安装整棵依赖树（约 289 MB / 数十 MB 压缩流量）。建议：

- 首启只**提示**有新版、把自动安装推迟到用户确认或第二次启动之后；
- 或在 `config.json` 增加 `auto_update_first_launch`（默认 false），把"首启不下载"变成显式契约；
- 无论选哪种，验收里要写清"首启流量预期"，避免"离线可用"的宣传被一次静默更新打破。

**更新失败与回滚**（review 后补）：

- npm 安装到**临时目录** `app-data/runtime/prefix.new`，成功后替换 `prefix`：
  **`rename` 不能覆盖非空目录**（实测 `ENOTEMPTY: Directory not empty`），所以是三步：
  `rename(prefix → prefix.old)` → `rename(prefix.new → prefix)` → 删除 `prefix.old`。
  两步之间崩溃会短暂没有前缀，正好由 §2.3 的回退路径兜底（这是回退必须存在的原因之一）；
- **顺序必须是"先停实例、再动目录"**（v3.1 修正，实测）：node 按需懒加载模块，
  树一旦被 rename 走，运行中的 harness 下一次 `require()` 就会 `MODULE_NOT_FOUND`
  （实测：进程启动后删掉树，1.5 s 后的 `require` 直接失败）。所以正确顺序是
  **装到 `prefix.new` → 停掉正在跑该树的实例 → 两次 rename → 启动新实例 → 确认起来后再删 `prefix.old`**；
  反过来（先换后停）会让活跃会话在更新中途崩掉，正是"更新期间损坏会话"的来源。
  这条同样适用于今天的实现：现存代码在更新检查之后才处理实例，若端口上有正在跑的实例（外部 CLI，或强杀后残留），
  npm 会一边重写它的树一边让它继续服务；实施时要把"停实例"提到 `npm install` 之前。
- 若替换后启动失败 → 依 §2.3 的"回退"用 seed 启动，并保留坏掉的前缀供诊断；
- 记录 `last-known-good`（树 + 版本 + 时间），提供"恢复上次可用版本"的路径。

**第三方插件（pnpm）**（review 后补，干净机器上的真实缺口）：

- 官方 Node 发行版只带 `corepack` 与 `npm`，**没有 pnpm**，而 `dsh plugin add …` 是转发给 pnpm 的；
- **pnpm 必须随包分发（seed 里的 `Resources/runtime/tools`）**：装到 app-data 意味着干净机器首启仍要联网，
  与"离线首启"目标直接冲突（v2 把装 pnpm 写成 staging 动作却指向 app-data，是第二处内部矛盾）；
- `app-data/runtime/tools` 只作为**可选更新落点**：解析时优先它、没有就用 seed；
- **不能装进 `prefix`**：该前缀在每次 dsh 更新时被整体替换（见上），pnpm 会在第一次核心更新后消失；
- PATH 注入顺序：`app-data/runtime/tools/bin` → `Resources/runtime/tools/bin` → `Resources/runtime/node/bin`；
- **同时要把全局安装落点指到可写目录**：把只读的 `Resources/runtime/node/bin` 前置进 PATH 后，agent 或用户在 harness 里跑
  `npm i -g <pkg>` 会试图写进 .app（只读）而失败。实测 npm 尊重 `npm_config_prefix`（`npm_config_prefix=/tmp/x npm prefix -g` → `/tmp/x`），
  所以子进程要带 `npm_config_prefix=app-data/runtime/tools`（同 `PNPM_HOME`），
  让全局安装落在应用数据目录里 —— 可写、可清理、也不污染用户的全局前缀；
- 同时把 pnpm 的 store/config 也钉在 app-data（`pnpm_config_store_dir`、`PNPM_HOME`），
  避免它往 `~/Library/pnpm` 或用户主目录乱写，"卸载"时能一次清干净；
- 不这样做时，自带发行版的 marketplace 插件将无法安装 —— 必须在文档里显式声明。

---

## 5. 构建流程（Makefile 增补）

```bash
make runtime-fetch     # 按 uname/arch 下载官方 Node，并校验 SHASUMS256.txt
make runtime-stage     # 组装 src-tauri/runtime/{node,dsh-prefix,tools,profile-template,THIRD-PARTY-NOTICES.md}
make plugin-template   # 可选：单独重建 profile 模板（固定 dshmarket 版本，真 pnpm install）
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
- **子进程 PATH 策略**（review 后补，v3.2 按来源细化）：启动 harness 时把 `app-data/runtime/tools/bin`、
  `Resources/runtime/tools/bin`、`Resources/runtime/node/bin` 与
  `app-data/runtime/prefix/bin` 前置进 PATH，使插件安装、MCP server、agent 执行的 `node`/`npm`/`pnpm`
  与壳内运行时一致；该决策要写进日志与诊断页，避免"为什么我的命令用的不是登录 shell 的 node"变成暗坑；
  **但语义要按运行时来源区分（v3.2，与壳方案对齐）**：壳方案与 README 的承诺是"agent 执行的
  `git`/`node`/`python` 与你的终端一致"。自带运行时下必然要打破它（否则自带 node 形同虚设），所以：
  选到**自带**运行时 → 自带 `node`/`npm`/`pnpm` 前置（并如实写日志"runtime: bundled"）；
  选到**系统**运行时（auto 且门槛通过，或配置强制）→ 保持用户 PATH 优先，只把工具目录追加在后面，
  **不要**改变用户终端里 `node` 的解析结果。
- `tauri.conf.json` 已加 `"resources": ["runtime/**/*"]`，且 `src-tauri/runtime/README.md` 作为**非空保证**存在
  （glob 匹配为空会导致 `tauri build` 失败，已实测）；
- **staging 必须校验符号链接**（实测教训）：`tauri build` 遇到**悬空链接直接失败** ——
  `failed to bundle project: resource path ... does not exist`，make 退出码 2。
  所以 `runtime-stage` 结尾要跑一次 `find -L <stage> -type l`：非空即失败并打印清单；
  依赖树里的 `node_modules/.bin/*` 是相对链接（正常），绝对链接与悬空链接都要拦下；
- **资源复制会解引用符号链接**（实测）：相对链接 `bin/npm -> ../lib/node_modules/npm/bin/npm-cli.js` 会变成
  同内容、同 mode 的普通文件；绝对链接会把**宿主文件**原样复制进包（实测把宿主 node 的 67 KB 启动器搬了进去，
  哈希一致）。因此 staging 里不要出现指向宿主机的绝对链接，且要知道 Node 官方发行版的 `bin/npm`、`npx`、
  `corepack` 都是 `#!/usr/bin/env node` 脚本 —— 解引用后仍然要求 PATH 上有 node，
  这正是 PATH 注入的硬性理由；
- **`THIRD-PARTY-NOTICES.md` 应自动生成**：72 个依赖手写许可清单必然过期，
  建议 staging 时用 `license-checker --json`（或等价工具）汇总，并把 Node/npm 自带的 LICENSE 一并收集；
- **构建耗时不是问题**（实测）：20,000 个文件的资源复制 + 打包共 **3.8 s**（sys 3.2 s），
  折算 25,412 文件的 dsh 树约 4–5 s；瓶颈仍是磁盘占用（§6）而不是时间；
- **profile 模板必须由真 pnpm 生成**（v3.3，见 §2.5）：手抄 `node_modules` 会缺 pnpm 元数据，
  `dsh plugin list` 认不出来；模板随包分发，只在首次播种时使用；升级 `dshmarket` 固定版本后要重新生成；
- **staging 目标目录必须先清空**（v3.1）：`src-tauri/runtime/` 里任何残留都会被静默打包 —— 实测放进 20,000 个文件的探针，
  `make bundle` 全程没有任何提示，产物直接多了 78 MB。`runtime-stage` 开头 `rm -rf` 目标（保留 marker 语义即可），
  并在 `bundle` 前加一道闸门：文件数/体积落在预期区间（如 25k±10%、解压 ≤ 550 MB）否则报错；
- **必须清掉扩展属性**（v3.1，实测）：Tauri 的资源复制会**保留 xattr**，带 `com.apple.quarantine` 的源文件在 bundle 里
  依旧是 quarantine 状态。构建机上的下载物（node tarball、npm 缓存里的包）一旦带上隔离属性，就会被打进 .app 并触发 Gatekeeper。
  所以 staging 结尾要 `xattr -cr <stage>`，并断言 `xattr -r <stage> | grep -c quarantine` 为 0；
- **staging 的 dsh 树必须用 npm 装，不能用 pnpm 复制**（v3.1）：pnpm 的 `node_modules` 是指向全局 store 的硬链接/相对链接，
  直接 stage 会得到悬空链接（构建直接失败）或不自包含的树；
- **校验值应钉在仓库里**（v3.1）：只校验下载来的 `SHASUMS256.txt` 意味着信任链条止于 TLS —— 分发服务器被替换时，校验值会被一起替换。
  建议把期望的 Node SHA256 写进仓库（如 `src-tauri/runtime.lock`），`runtime-fetch` 与它比对，而不是只信下载到的清单；
- `make clean` 不动 staging，`make runtime-clean` 才删（避免每次改代码都重下 48MB）。

---

## 6. 体积预算与取舍

| 组成 | 解压 | 压缩（估算/实测） |
|---|---|---|
| Node（darwin-arm64） | 187 MB | 48 MB |
| dsh 树（含 12 个原生模块） | 289 MB | 数十 MB（tarball 本身很小，依赖树按需下载） |
| Tauri 壳 | 11 MB | 5 MB |
| **合计** | **≈ 490 MB** | **≈ 150–250 MB** |

**运行期磁盘还会长**（v3.1）：`app-data/runtime/npm-cache` 随每次核心更新累积（数百 MB～GB 级），
方案原先没写回收策略。建议：更新成功后按阈值清理（或 `npm cache verify`），并提供"重置运行时"入口
（删 `runtime/` 即回到 seed 状态）。

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
- Node 官方二进制不是我们的签名，需要重新签 —— 但**必须保留它的 entitlements**：
  官方 node（v22.23.2 darwin-arm64）带 hardened runtime 与
  `com.apple.security.cs.allow-jit`、`allow-unsigned-executable-memory`、
  `allow-dyld-environment-variables`、`disable-executable-page-protection`（TeamIdentifier=HX7739G8FX）；
  **实测：只写 `codesign --force --options runtime -s - node` 会丢掉这些 entitlements，node 一启动就是
  `Trace/BPT trap: 5`（exit=133）**；加上 `--preserve-metadata=entitlements` 后 `node -e` 正常（exit=0）。
  正确命令：`codesign --force --options runtime --timestamp --preserve-metadata=entitlements -s …`，
  或显式提供 entitlements plist —— 二者选一但必须做，并把 `node -e "console.log(1)"` 纳入签名后冒烟；
  12 个 `.node` 原生模块同理（多为库，但同样不要改动它们的 entitlements）；
- 顺序：`runtime-stage` → 对 `runtime/` 里的 Mach-O 逐个签名 → `tauri build`（签 app 壳）→ 公证 + `stapler`；
- 验证命令（必须纳入 CI 与验收）：
  `codesign --verify --deep --strict -vvv "DSH Desktop.app"`、
  `spctl -a -vvv "DSH Desktop.app"`、`xcrun stapler validate "DSH Desktop.app"`；
- **复制保真实测结论（v3 补）**：

  | 属性 | 结果 |
  |---|---|
  | 可执行位（mode） | **保留**：755 的脚本复制后仍 755、可直接执行 |
  | 代码签名 | **保留**：ad-hoc 签过的 Mach-O 复制后 `codesign --verify` 仍 valid（内容逐字节一致） |
  | 符号链接 | **丢失**：被解引用成实体文件（详见 §5） |

  构建后仍要抽验（`codesign -dv` 对 node 与 2~3 个 `.node` 抽查）；任何"复制后再改文件"都会让封装签名失效；
- **签名后置校验必须紧跟 bundle 步骤**：`codesign --verify --deep --strict` → `spctl -a -vvv` →
  `xcrun stapler validate`，任一失败即视为构建失败（CI 门禁）；
- **不要在签名的 bundle 内写入**——这正是 §2.2/§4 把更新落到 app-data 的原因。
  附带好处：未公证的 .app 从 DMG 直接运行时 macOS 会启用 **App Translocation**（从随机只读路径启动），
  本方案因为从不写 bundle 而天然不受影响；验收可直接在"下载后双击"的场景下做。

---

## 8. 平台矩阵

| 平台 | Node 发行版 | 产物 | 备注 |
|---|---|---|---|
| macOS arm64 | `darwin-arm64`（48 MB） | `.app` / dmg | 先做；**最低系统版本 11.0**（实测 node 22.23.2 的 `LC_BUILD_VERSION minos = 11.0`） |
| macOS x64 | `darwin-x64`（49 MB） | 同上 | 同上，x64 也是 `minos 11.0`；与 arm64 分别发布，或做 universal（两套 node + 按 `uname -m` 选） |
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
- **随包附带插件市场** `dshmarket`（MIT，`github.com/dsh-market/dsh-market`）及其运行时依赖
  `js-yaml`（MIT）/ `argparse`（Python-2.0）/ `undici`（MIT）—— 见 §2.5；
- 其余 marketplace 第三方插件**不随包分发**，由用户安装，责任与许可归其作者。

---

## 11. 代码改动清单

| 文件 | 改动 | 估计 |
|---|---|---|
| `runtime.rs`（新） | 解析 seed 路径、确保 app-data 目录、**候选探测 + 版本仲裁 + 回退**、报告来源与版本 | ~200 行 + 测试 |
| `runtime.rs`（新） | `last-known-good` 记录与诊断输出 | 含上行 |
| staging（Makefile） | 把 **pnpm** 一并装进前缀；子进程 PATH 注入清单 | ~15 行 |
| `locator.rs` | 接受 `BundledRoots`，按 §2.3 顺序解析，记录来源 | ~40 行 |
| `update.rs` | `install_prefix` 支持 app-data 目标；追加 `--cache`；**装到 `prefix.new` 后三步替换**；安装后回读版本核对（已实现） | ~50 行 |
| `lib.rs` | 取 `app.path().resource_dir()` 并注入；splash 文案区分自带/系统 | ~25 行 |
| `lib.rs` + `update.rs` | **更新前先停掉正在使用目标树的实例**（顺序修正，§4）；子环境注入 `npm_config_prefix` / `PNPM_HOME` | ~30 行 |
| `tauri.conf.json` | `bundle.resources`（已完成）；签名配置 | 少量 |
| `Makefile` | `runtime-fetch / runtime-stage / runtime-clean`，`bundle` 依赖 chain；**staging 结尾做悬空链接校验**；自动生成 `THIRD-PARTY-NOTICES.md` | ~80 行 |
| staging 签名脚本 | 逐个 Mach-O 重签，**带 `--preserve-metadata=entitlements`**；签名后跑 `node -e` 冒烟 | ~25 行 |
| `runtime.rs`（新） | pnpm 用独立的 `runtime/tools` 前缀（不随 dsh 更新替换），PATH 注入两份 | 含上行 |
| `runtime.rs`（新） | **首次播种 profile 模板**（仅当 `$DSH_HOME/profiles/web` 不存在）：复制 `Resources/runtime/profile-template/` | ~25 行 |
| staging（Makefile） | 生成 profile 模板：固定 `dshmarket` 版本 → 真 `pnpm install` → 拷进 `runtime/profile-template/` | ~15 行 |

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
- [ ] **市场可用**：干净机器首启后，profile 里出现 `.dsh-market/`（证明 dshmarket 已加载）、市场界面能打开；
- [ ] **插件安装可用**：从市场里装一个第三方插件（验证 pnpm 随包就位 + 联网安装链路）；
- [ ] **已有 profile 不被改动**：把测试机 `$DSH_HOME/profiles/web` 换成用户自己的，启动后该目录字节不变；
- [ ] **回退演练**：人为破坏 app-data 前缀（删 `lib/bin.js`）后仍能用 seed 启动；
- [ ] **版本仲裁**：把 app-data 前缀替换成旧版本时，启动应选用 seed 里更高的版本；
- [ ] **更新中断演练**：更新过程被 kill 后，下次启动仍可用（`prefix → prefix.old → prefix` 三步替换生效）；
- [ ] **签名后冒烟**：签名完成、打包之后，直接执行 bundle 内的 `node -e "console.log(1)"` 退出码为 0
      （v3 实测：漏掉 entitlements 时这里是 `Trace/BPT trap: 5`）；
- [ ] **pnpm 存活**：完成一次 dsh 核心更新（整棵前缀被替换）后，marketplace 插件仍能安装；
- [ ] **悬空链接**：staging 里人为放一个悬空链接 → `runtime-stage` 必须报错退出，而不是等到 `tauri build` 才失败；
- [ ] **首启流量**：干净机器首启的网络流量符合策略（默认不应静默下载整棵依赖树）；
- [ ] **App Translocation**：从"下载的 DMG"直接双击运行（不拖进 /Applications）也能正常启动；
- [ ] **带实例更新演练**：端口上有正在跑的实例时触发更新 → 先停实例再安装，活跃回合不被中途打断、会话文件不损坏；
- [ ] **xattr 门禁**：staging 后 `xattr -r` 无 quarantine；打包后抽验 bundle 内 node 的 xattr；
- [ ] **staging 闸门**：故意多放一个文件、或删掉一个平台原生模块 → 构建必须报错而不是照发；
- [ ] **工具落点**：在 harness 里执行 `npm i -g` → 落在 `app-data/runtime/tools`，既不写 .app，也不动用户全局前缀。

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
10. **签名丢 entitlements**（v3 实测，已给出修法）：漏掉 `--preserve-metadata=entitlements` 的 node 在签名后必然崩溃，
    而且**只在真机运行签名产物时才发现** —— 必须把 `node -e` 冒烟纳入 CI，不能只看 `codesign --verify` 通过；
11. **资源复制的隐藏依赖**（v3 实测）：符号链接被解引用，绝对链接会把宿主文件搬进包 ⇒ 产物不再完全由 staging 决定，
    staging 要禁止绝对链接；悬空链接会让构建直接失败；
12. **发行后 Node 只能随 .app 升级**：seed 只读 ⇒ Node 安全补丁要发新版桌面壳；文档需给出 Node 版本矩阵与升级节奏，
    诊断页/日志要显示 bundled 的 node 与 dsh 版本（目前只显示来源）；node 能力探测不通过时应阻止更新并提示"需要更新桌面壳"（`engines.node` 不存在，见 §2.4）；
13. **app-data 沉淀**：更新过的前缀（289 MB+）会长期留在应用数据目录，需要"重置运行时"入口或文档化的清理方式。

---

## 14. 分阶段实施

| 阶段 | 范围 | 产出 |
|---|---|---|
| P0（半天） | **先做失败快验证**：staging 里放一个签名过的 Mach-O + 一个符号链接，跑通 `tauri build` →
  解引用行为、mode/签名保真、悬空链接报错（本轮已实测，实施时按结论搭 CI 门禁即可） | 已具备，无需再验证 |
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

## 16. 修订记录（v3，第二轮 review 后）

本轮把上一版留作"待实测"的几项直接做了实验（macOS 14/15 arm64，Node v22.23.2-darwin-arm64，Tauri 2.11.5），
并把结论写回正文。**其中前两项会让产物直接不可用**：

| # | 级别 | 问题 | 处理 |
|---|---|---|---|
| 1 | P0 | 按 v2 字面执行 `codesign --force --options runtime` 会丢 Node 的 JIT entitlements，**实测 node 启动即 `Trace/BPT trap: 5`（exit=133）** | §7 改为必须 `--preserve-metadata=entitlements`（或显式 plist），并加 `node -e` 冒烟验收 |
| 2 | P0 | pnpm 位置有两处矛盾：装在被整体替换的 `prefix` 里（第一次核心更新后消失）、且落点在 app-data（干净机器首启仍要联网，与"离线首启"冲突） | §2.1/§2.2 让 pnpm **随包分发**到 `Resources/runtime/tools`，app-data 的 `tools` 只作可选更新落点；§4 明确职责与 PATH 顺序 |
| 3 | P0 | 悬空符号链接会让 `tauri build` 直接失败（实测 `resource path … does not exist`） | §5 要求 `runtime-stage` 做 `find -L -type l` 校验；§12 加验收 |
| 4 | P1 | `rename` 不能覆盖非空目录（实测 ENOTEMPTY），"原子替换"不成立 | §4 改为三步替换并说明中间态由回退兜底 |
| 5 | P1 | 资源复制会解引用符号链接，绝对链接把宿主文件搬进包 | §5 记录实测行为与禁令；§13 增风险项 |
| 6 | P1 | 复制保真只有"必须保留"的要求，没有结论 | §7 给出实测表：mode 保留、签名保留、符号链接丢失 |
| 7 | P1 | 首启可能静默下载整棵依赖树（`auto_update` 默认 true） | §4 增首启更新策略；§12 增"首启流量"验收 |
| 8 | P2 | Node 只能随 .app 升级、`engines.node` 不满足时无动作定义 | §13 增风险项，明确"阻止更新 + 提示更新桌面壳" |
| 9 | P2 | `THIRD-PARTY-NOTICES.md` 手写必然过期 | §5 改为 staging 时自动生成 |
| 10 | P2 | 构建耗时未知（只有磁盘预算） | §5 补实测：20k 文件复制 + 打包 3.8 s，25k 约 4–5 s |
| 11 | P3 | 卸载/清理路径未写 | §13 增 app-data 沉淀风险与"重置运行时"建议 |
| 12 | P3 | App Translocation 未评估 | §7 说明"不写 bundle"天然免疫，并加入验收 |
| 13 | P1 | v2 计划"按 `engines.node` 检查兼容性"**落空**：`@deepseek-ai/dsh` 的 package.json 没有该字段（实测） | §2.4 改为**能力探测**（`module.stripTypeScriptTypes`，Node ≥ 22.13），§11/§13 同步修正 |
| 14 | 新增 | "本机装了一半怎么办"没有策略 | §2.4 给出混合运行时决策矩阵 + 可行性实测（原生模块**全是 N-API**，跨大版本 ABI 稳定）+ 架构/能力两道门槛 |
| 15 | P0 | 采用系统 dsh 时，影子前缀的更新会被"更高版本优先"反过来盖掉用户自己的安装 | §2.4 明确：**用系统运行时时不自动安装**（只提示），桌面壳永不写用户的全局前缀 |

**本轮实测明细**（可复现）：

- 官方 node entitlements：`codesign -d --entitlements -` → allow-jit / allow-unsigned-executable-memory /
  allow-dyld-environment-variables / disable-executable-page-protection，flags `0x10000(runtime)`；
- 重签对照：去掉 entitlements → `node -e` 退出 133（Trace/BPT trap: 5）；保留 → 退出 0；
- 资源复制探针：755 脚本 → 仍 755 且可执行；相对链接 → 同内容普通文件；绝对链接 → 复制宿主目标（哈希一致）；
  悬空链接 → `tauri build` 失败；ad-hoc 签名的 Mach-O → 复制后 `codesign --verify` 仍 valid；
- 20,000 文件（78 MB）资源复制 + 打包：3.845 s（sys 3.185 s）；
- `rename(dir_a, dir_b)`（b 非空）→ `OSError: Directory not empty`。

## 17. 修订记录（v3.1，第三轮 review 后）

这轮专门找"方案没写到、但真机上会咬人"的运行期/构建期陷阱，9 项里 3 项已验证：

| # | 级别 | 问题 | 处理 |
|---|---|---|---|
| 1 | P1 | **更新时动了正在服务的树**：node 懒加载，树被 rename 走后运行中的 harness 下一次 `require()` 即 `MODULE_NOT_FOUND`（实测） | §4 明确顺序「装 .new → 停实例 → 两次 rename → 起新实例 → 删旧树」，并指出今天实现的同类问题 |
| 2 | P1 | 子进程 PATH 前置只读的 bundled node/bin 后，harness 里 `npm i -g` 会写 .app 而失败 | §4 要求注入 `npm_config_prefix` / `PNPM_HOME` 指向可写前缀（实测 npm 尊重该变量），§12 加验收 |
| 3 | P1 | 构建机下载物的 **quarantine xattr 会被带进包**（实测资源复制保留 xattr） | §5 要求 `xattr -cr` + 断言无 quarantine，§12 加门禁 |
| 4 | P1 | staging 目录里的残留会被静默打包（实测 20k 文件探针直接进了 .app，无任何提示） | §5 要求 staging 先清空，并在 bundle 前做文件数/体积闸门 |
| 5 | P2 | 运行期 npm cache 无上限增长 | §6 增加回收策略与"重置运行时"入口 |
| 6 | P2 | 只信下载来的 `SHASUMS256.txt`，校验链条止于 TLS | §5 建议把期望 SHA256 钉进仓库（`runtime.lock`） |
| 7 | P3 | 用 pnpm 树做 staging 会引入 store 链接 | §5 明确必须 npm 安装，禁 pnpm 复制 |
| 8 | P3 | 首启体验/时间预算未定义（490 MB 的应用首次启动会被深扫） | §12 增首启时间与 splash 文案要求 |
| 9 | P3 | CI 签名凭据与 updater 密钥管理未写 | §14 备注，实施 P2/P4 时补 |

**本轮实测明细**（可复现）：

- 懒加载：`node -e "setTimeout(() => require('/tmp/tree-demo/mod.js'), 1500)"` 期间删掉 `/tmp/tree-demo` → 
  `lazy require FAILED: MODULE_NOT_FOUND`；证明"先换目录后停实例"会打断活跃会话；
- 全局落点：`npm_config_prefix=/tmp/npm-prefix-probe npm prefix -g` → `/tmp/npm-prefix-probe`（默认是 `/opt/homebrew`）；
- xattr：源文件写 `com.apple.quarantine` → `make bundle` → bundle 内同名文件仍带 `com.apple.quarantine`；
  同批未标记的文件只带系统自动加的 `com.apple.provenance`；
- staging 污染：放入 20,000 个文件（78 MB）后 `make bundle` 正常成功，产物直接变大 —— 无任何告警。

## 18. 修订记录（v3.2：与 desktop-shell 方案的兼容性审查）

问题：用本方案构建出来的应用，和现有壳（`design-task-feat-dsh-tauri-desktop-shell.md`）是否兼容？

**结论：架构兼容，但有 2 个硬冲突必须先改，另有 2 处语义冲突要对齐。**
兼容的基础是：自带运行时只改变"用哪个 node 和哪个 dsh.js"，壳与 CLI 之间的接口（固定参数、启动 URL 行、
进程组、探针、窗口、日志）完全不变。

### 18.1 硬冲突（不改就会失败或写坏签名包）

**H1：`install_prefix()` 会把更新写进只读的签名 bundle**

- 现状：`lib.rs` 用 `location.dsh_js` 推导安装前缀（`update.rs` 的规则是取 `/lib/node_modules/` 之前的部分）。
  自带 seed 的路径是 `Contents/Resources/runtime/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js`，
  推导结果就在 `.app` 里；
- 另一个坑：**自带 npm 的全局前缀指向 node 树自身**（实测：对官方发行版跑 `npm prefix -g` 得到那份 node 目录），
  所以"不传 `--prefix`"同样会写进 bundle；
- 修法：自带运行时下**强制** `--prefix <app-data>/runtime/prefix`，不再走现有推导。
  这条是实现自带运行时的**前置条件**，不是可选优化；

**H2：`minimumSystemVersion = 10.15` 低于 Node 的实际门槛**

- 实测：官方 node v22.23.2 的 darwin-arm64 与 darwin-x64 都是 `LC_BUILD_VERSION minos 11.0`；
- 声明 10.15 的结果是"能装、能打开壳、一启动 harness 就崩"，比直接拒绝更难排查；
- 修法：打包自带运行时的构建把 `bundle.macOS.minimumSystemVersion` 提到 `11.0`（Linux 侧不变）。

### 18.2 语义冲突（文档层面，必须对齐）

| 项 | 壳方案的说法 | 本方案原说法 | 对齐结果 |
|---|---|---|---|
| 环境变量覆盖 | §3 与 README：`DSH_DESKTOP_DSH` / `DSH_DESKTOP_NODE` 优先 | §2.3 把 `DSH_DESKTOP_DSH` 排在自带候选之后 | §2.3 改为**显式覆盖永远第一**（排障开关不能被静默忽略） |
| 子进程 PATH | "agent 执行的 `node` 与你的终端一致" | §5 把自带 `node/bin` 前置 | §5 按来源区分：**自带运行时**才前置自带 node；**系统运行时**保持用户 PATH 优先 |
| 启动时序 | §4 的表里没有"解析运行时"这一步，且示例仍写 `--port 0`（实现是固定端口，见 README） | §3 定义了运行时解析 | 壳方案 §4 增加一行"解析运行时（§本方案 2.3）+ 记录来源与版本"，并修掉陈旧端口示例 |
| 体积/平台 | §10 与 README 写壳 `.app` ≈ 11 MB、"macOS 10.15+" | 自带后 ≈ 490 MB、要求 11.0+ | 两处交叉引用并注明"自带运行时版本"与"精简版本"的区别 |

### 18.3 已确认兼容、无需改动的部分

- **进程与生命周期**：自带/影子前缀只是换了可执行文件路径；独立进程组、SIGTERM → 轮询 → SIGKILL、
  僵尸识别、崩溃看护、退出清理全部不变；
- **更新链路的其余部分**：`npm_command` 的 PATH 修复让自带 npm（解引用后的 `#!/usr/bin/env node` 脚本）能直接跑；
  `version_from_package()` 对 seed 路径同样有效；缓存以版本字符串为 key，与来源无关；
  "安装后回读版本"能识别"装到了别处"，正好覆盖 H1 修好之前的异常情形；
- **上一轮的"更新前先停实例"**：自带运行时的更新流程正需要这个前置条件，两者方向一致（§4 的三步替换建立在其之上）；
- **app-data 与只读 bundle 的分工**：overlay、logs、state.json、update-check.json、npm cache 都在 app-data，
  与"不写签名包"的原则一致；
- **平台范围**：两份文档都是 macOS/Linux 优先、Windows 不支持，无冲突。

### 18.4 建议的落地顺序

1. 壳方案 §4 补"解析运行时"一行 + 修掉陈旧端口示例（文档，零风险）；
2. 实现运行时解析时，把 `install_prefix` 改成"可被运行时来源覆盖"，并加单测（对应 H1）；
3. 打包配置按 H2 调整 `minimumSystemVersion`；
4. 两份文档互相加交叉引用，避免读者只看一份时得出错误结论。

**实测明细**（可复现）：

- `otool -l <node>/bin/node | grep -A4 LC_BUILD_VERSION` → `minos 11.0`（arm64 与 x64 都是）；
- `PATH=<seed>/bin:$PATH <seed>/bin/npm prefix -g` → 输出那份 node 目录本身；
- 现有代码路径：`lib.rs` 调 `update::install_prefix(&location.dsh_js)`，规则见 `update.rs` 的 `install_prefix`。

## 19. 修订记录（v3.3：随包附带插件市场）

**触发**：干净机器上无法安装任何插件 —— dsh 在全新 `DSH_HOME` 造出来的 profile 是空的
（`dependencies: {}`，bundles 只有 base + web-app，实测），而 `dsh plugin --profile X …` 只是对 pnpm 的透传，
没有任何"安装入口"。因此原方案"不打包任何插件"的非目标必须改为"**只附带插件市场本身**"。

| # | 级别 | 问题 | 处理 |
|---|---|---|---|
| 1 | P0 | 干净机器的 profile 没有安装入口，用户无法手动装插件 | §2.5 新增"随包附带 dshmarket"：staging 生成 profile 模板 + 首启播种；§1 非目标相应修正 |
| 2 | P1 | 模板若手抄 node_modules，pnpm 不认（缺元数据），后续 `dsh plugin add` 可能重建整棵树 | §2.5/§5 要求**真 pnpm install** 生成模板并随包分发锁文件 |
| 3 | P1 | 播种可能覆盖用户既有 profile | §2.5 明确**仅当 profile 不存在时播种**，§12 增加"已有 profile 不被改动"验收 |
| 4 | P2 | 三方插件的许可与供应链未覆盖 | §10 收录 dshmarket 及其三个依赖的许可；§2.5 记录版本固定与锁文件审计 |

**实测明细**（macOS，dsh 0.1.5-rc.2，pnpm 12.3.4）：

- 全新 DSH_HOME 跑 `dsh web` → profile 的 `dependencies` 为 `{}`、`bundles` 只有两个 base 包，`node_modules/` 为空；
- `dsh plugin --profile web --help` 输出的是 **pnpm 自己的帮助** ⇒ 插件管理 = pnpm 透传；
- 用真 pnpm 生成模板：`pnpm install` → 6.5 MB（dshmarket/js-yaml/argparse/undici，**0 个 .node**）；
- 把模板放进全新 DSH_HOME：`dsh plugin --profile web list` → `dshmarket@1.45.1`；
- 启动：`dsh web` **4 秒**输出 URL、stderr 为空、无模块解析错误，profile 内出现 `.dsh-market/`（市场已加载）。

**未验证**：断网首启（§12 仍列为验收项）、从市场实际安装一个第三方插件的端到端链路。

## 20. 实施进展（P1 第一步，2026-09-13，分支 `feat/bundled-runtime`）

本轮把方案 P1 里"能独立验证的部分"先做掉，全部在本机真跑过。**未合并到 main**：
自带运行时会改变发行方式，等解析接入与签名验收完成后再决定合并时机。

### 20.1 已完成

| 项 | 内容 | 验证 |
|---|---|---|
| 运行时解析策略 | 新增 `src-tauri/src/runtime.rs`：来源（Env/Seed/Shadow/System）、§2.4 的四格矩阵、
  seed↔shadow 版本仲裁、更新落点（系统运行时 → 只提示） | 6 个单测（共 35 passed）、clippy 0 warning |
| 配置项 | `config.json` 新增 `runtime: auto\|bundled\|system`（默认 auto） | 部分配置照常解析 |
| `make runtime-fetch` | 按 uname 选发行版、下载、**按 SHASUMS256.txt 校验**、缓存到 `.runtime-cache/` | 实测 48 MB / 6.5 s，校验输出 `OK` |
| `make runtime-stage` | 解包 Node → `dsh-prefix`（`npm install -g --prefix`）→ `tools`（pnpm）→ profile 模板 → 许可清单 → 校验 | 实测 **520 MB**、22 s（缓存热） |
| profile 模板 | `scripts/make-profile-template.sh`：真 pnpm 生成含 dshmarket 的 profile（§2.5） | 模板内 dshmarket 1.45.1 + lockfile 齐全 |
| staging 校验 | `scripts/check-runtime-stage.sh`：必需文件 + **悬空链接** + **quarantine** + 版本回显 | 实测通过对；此前探针已证明这两类问题会让构建失败/被 Gatekeeper 拦 |
| 许可清单 | `scripts/write_third_party_notices.py`：扫描 dsh 依赖树与模板，汇总 name@version + license | 生成 `THIRD-PARTY-NOTICES.md`，UNKNOWN 条目 0 |
| `make bundle-bundled` | staging + 打包，`bundle.resources` 把 runtime/ 塞进 .app | 产出 **598 MB** .app，31,090 文件，包内 node 可直接执行 |

### 20.2 关键实测（决定后续实现方式）

1. **无系统 node 也能跑**：`env -i PATH=/usr/bin:/bin` + 自带 node + 自带 dsh → `0.1.5-rc.2`；
2. **离线首启形态成立**：全新 DSH_HOME + 模板播种 + 只用自带运行时 → `dsh web` **6 秒**输出 URL、stderr 干净、
   `.dsh-market/` 出现（插件市场已加载）；
3. **`npm install --prefix` 不带 `-g` 是错的**：会装成 `<prefix>/node_modules` 局部布局，
   而 dsh 与壳都按全局布局 `<prefix>/lib/node_modules` 找包 —— staging 必须写 `-g --prefix`；
4. **构建期缓存要独立**：staging 的 npm/pnpm 都改用 `.runtime-cache/` 下的 store（CI 可复用、不碰用户缓存）；
5. 打包耗时：31k 文件的资源复制 + 构建 ≈ **47 s**，磁盘峰值约 1.1 GB（staging 520 MB + bundle 复制一份）。

### 20.3 已接入启动流程（2026-09-13，第二次推进）

| 项 | 实现 | 验证 |
|---|---|---|
| 种子发现 | `seed_root_for()`：`DSH_DESKTOP_RUNTIME` → 应用资源目录 → `tauri dev` 在二进制旁的 `target/<profile>/runtime`（两种布局都试），node 兼容 `node.exe` | 单测 `finds_a_seed_under_the_resource_directory` |
| 运行时决策 | `resolve_runtime()` 组装 `Inputs`（系统那一半先过 arch/能力门槛）、调 `decide()`、日志记来源与更新落点 | 单测 `forcing_bundled_resolves_into_the_seed_tree` |
| 候选为空 | `decide()` 返回 `None` ⇒ 错误页提示装 node/dsh 或用环境变量指定 | 单测 `no_candidate_at_all_is_reported_as_none` |
| 偏好覆盖 | `DSH_DESKTOP_RUNTIME_PREFERENCE=bundled\|system\|auto`（排障/验证用） | 单测 `runtime_preference_parses_the_env_spelling` |
| 首启播种 | `$DSH_HOME/profiles/web` 不存在时复制 seed 的 `profile-template`（§2.5） | 复用模板实验结论 |
| PATH | 自带时把 app-data 与 seed 的 `tools/bin` 前置 | 代码审查 + 单测覆盖来源判定 |
| 全局安装落点 | 注入 `npm_config_prefix` / `PNPM_HOME` 指向可写 tools 前缀（§5） | 同上 |
| 更新落点 | 自带 → `app-data/runtime/prefix`；系统安装 → **只提示不安装**（§2.4/§18.1 H1） | 单测 + 更新流程改动 |

### 20.4 仍未做（P1 剩余）

- **带自带的 GUI 实机验证**：需要 `make bundle-bundled` 的产物 + `DSH_DESKTOP_RUNTIME_PREFERENCE=bundled` 跑一次；
  本轮因开发机上已有实例占用 3080（会抢占会话）而未执行；
- **平台矩阵**：`bundle.macOS.minimumSystemVersion` 提到 11.0（§18.1 H2）；Linux 侧 staging 尚未跑过；
- **签名与公证**（§7）：需要 Developer ID；本轮产物未签名；
- **构建期磁盘**：实测 tauri 会把整份 `runtime/` 复制到 `target/<profile>/runtime`（debug 下 **584 MB**），
  加上 staging 与 bundle 各一份 ⇒ 峰值约 1.7 GB，比 §6 原先估的 1.5 GB 更高；
- **Windows 侧**：交叉编译已能出 exe（见 shell 方案相关提交）；自带运行时改为在 CI 的 Windows runner 上原生 staging
  （koffi 的 postinstall 在 macOS 上跨平台 staging 会因缺 CMake 失败），已落地为 `.github/workflows/windows-portable.yml`，
  并已在实机验证通过（见 §20.6）；仍未做的是给 `release.yml` 加 Windows 任务、以及代码签名。

### 20.5 复现命令

```bash
git checkout feat/bundled-runtime
make runtime-fetch && make runtime-stage        # 约 520 MB，21 s（缓存热）
make bundle-bundled                             # 产出带运行时的 .app（约 598 MB）
make runtime-clean                              # 回收
```

### 20.6 Windows 免安装包（2026-09-13，第三次推进）

`.github/workflows/windows-portable.yml` 在 `windows-latest` 上原生 staging 并打包，输入
`dsh_version` / `node_version` / `build_ref`（默认 `feat/bundled-runtime`）/ `attach_to_release`。
产物 `dsh-desktop-windows-x64-portable`（zip 182–326 MB，约 3.3 万个文件，包内最长路径 205–223 字符），
另附 `SHA256SUMS-windows-x64`（release 任务会与 macOS/Linux 的合并成一份 `SHA256SUMS`）。

触发方式有两种，共用同一份实现（`workflow_call` + `workflow_dispatch`）：手动出包，或由 tag 触发的
`release.yml` 调用——后者新增 `windows-portable` 任务（`uses: ./.github/workflows/windows-portable.yml`，
`build_ref` 传 tag，`needs: [preflight, build, windows-portable]`），Windows 产物与 macOS/Linux 一起发布。
`release.yml` 此前只存在于 main（含 macos-x64 交叉编译与 release 任务的 checkout 修复），本次一并取回分支，
因此 **在本分支上打 tag 即可发布全套产物**。按上面的分支策略，`main` 上的 `release.yml` 与
`windows-portable.yml` 本次都不动：main 的代码没有自带运行时，在那里加 Windows 任务只会产出带一份
死 `runtime/` 的包；等分支合并时再一起同步。

**实机验证通过（2026-09-13，run 34752267176 的产物）**：Windows 11 上解压到 `D:\dsh` 后双击即可启动，
自带 node 22.23.2 + dsh 0.1.5-rc.2 正常拉起 Web GUI，首启播种的插件市场可用，会话内 `glob` / `write` /
`read` / `grep` / `edit` / `patch` / `todo_write` / `present` 全部正常（一次任务 14 s / 81K tokens）。
Windows runner 上的 `cargo test --lib` 为 40 passed（Unix-only 的三个用例在那里被 cfg 掉）。

本机无法执行 PE，Windows 侧行为只能由用户实测，因此每个失败回合都在缩小假设面：

| 现象 | 处置 |
|---|---|
| 解压报 `0x80010135`（路径太长） | 说明文件写明短路径 `D:\dsh`、`tar -xf`、7-Zip 三种方式，并追加 7z SFX 自解压包 |
| 包内多一层 `dist/` | 先 `cd dist` 再打包；说明文件改 ASCII 文件名（中文名在不同解压工具里乱码） |
| Git Bash 的 `cp -R` 在 pnpm 符号链接上失败 | 组装目录改用 tar 管道（`-h` 解引用） |
| `npm view` 找不到 node | `npm_command()` 把 npm 所在目录前置进 PATH |
| 启动即 `EISDIR: illegal operation on a directory, lstat 'D:'` | 见下：`\\?\` verbatim 前缀 + Unix-only 的 PATH 合并与查找 |

**`EISDIR: lstat 'D:'` 的根因与修法**（commit `4152ca7` 与随后的 Windows 子进程环境修复）

用户实测日志给出了完整证据链（`%APPDATA%\com.deepseek.dsh.desktop\logs\harness.log`）：

    [dsh-desktop] runtime: node \\?\D:\dsh\DSH Desktop\runtime\node\node.exe (bundled) + dsh \\?\D:\dsh\…\lib\bin.js (bundled) | updates: Shadow
    [dsh-desktop] login shell env import failed (/bin/zsh); using the app environment
    [dsh-desktop] child PATH = /opt/homebrew/bin:/usr/local/bin
    Error: EISDIR: illegal operation on a directory, lstat 'D:'
        at Object.realpathSync (node:fs:2749:25)
        at toRealPath (node:internal/modules/helpers:61:13)
        at Function._findPath (node:internal/modules/cjs/loader:760:22)
        at resolveMainPath (node:internal/modules/run_main:39:23)

三点结论（不是「workspace 里写了 D:」这么简单）：

1. **`\\?\` verbatim 前缀**：种子路径经过 canonicalize，在 Windows 上带出 `\\?\D:\…`。Win32 文件 API
   能用，但 node 的 `fs.realpathSync` 会逐组件解析它，走到 `lstat 'D:'`（盘符相对路径 = 目录）就 EISDIR。
   `unverbatim()` 在 `absolute()` / `seed_root_for()` / `locator::real_path()` 去掉前缀（超过 MAX_PATH 时保留）。
2. **`merge_path` 原本是 Unix-only**：`value.split(':')` 加上「必须以 `/` 开头」的过滤，在 Windows 上把整条
   PATH 砍成只剩硬编码的 `/opt/homebrew/bin:/usr/local/bin`（与日志完全一致）——子进程既没有系统目录，
   也没有自带 node 目录。改为 `std::env::split_paths` / `join_paths` + `is_absolute()`，按平台拼接；
   硬编码的 Homebrew 目录改成 `#[cfg(target_os = "macos")]`，Windows 补 `%SystemRoot%` 与 `System32`。
3. **登录 shell 与 npm 查找同样是 Unix-only**：Windows 上不该去跑 `/bin/zsh`（现直接跳过并记日志）；
   `npm` 的裸名会命中 npm 自带的 POSIX wrapper（日志里的 `os error 193`），改为 `.exe/.cmd/.bat` 优先。

顺带保留的加固（同类风险，仍然有价值）：`home_dir()` 校验（Windows 优先 `USERPROFILE`）、`workspace` /
`dsh_home` 不可用则回退并记日志、路径绝对化、`harness::spawn()` 前置校验，以及 `app data dir = … |
workspace = …` 和 `spawn: <完整命令行>` 两行日志——下一次失败可以直接从日志定位到具体路径。

回归方式：`cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`（本机可做，覆盖所有
`cfg(windows)` 代码），以及 CI 里 Windows runner 上的 `cargo test --lib`。

排障入口：`%APPDATA%\com.deepseek.dsh.desktop\logs\harness.log`，关键字
`runtime: node … (bundled|system) + dsh … | updates: …` 与 `spawn: …`。

---

## 21. 修订记录（v3.4：合并影响审查后的四处修正，2026-09-13）

对 `main..feat/bundled-runtime` 做了一次"合并到主分支会影响原有功能吗"的审查，结论是：普通用户的可启动路径
等价（同一个 node、同一份 `bin.js`、同样的启动参数；`src/`、`package.json`、`tauri.conf.json`、`Cargo.toml`
全部未动、没有新增 crate），但以下四处需要先修，否则合并会改变或破坏现有行为。四处都已实现并补了测试：

| 项 | 问题 | 修法 | 测试 |
|---|---|---|---|
| 交叉编译入口 | `Makefile` 的 `TARGET=` 支持只存在于 `main`（`tauri build … $(if $(TARGET),--target $(TARGET),)`），分支上没有；合并若按分支解析，`release.yml` 的 macOS x64 会静默编成 host 版，随后 `[ -d "$app" ]` 报错 | 把 main 的 `TARGET` 传参与产物路径判断取回分支（`make -n bundle TARGET=…` 已核对输出 `--target x86_64-apple-darwin`），`make help` 也补上该变量 | CI（tag 触发时由 `release.yml` 兜底） |
| 系统安装的自动升级 | 新增的 `Updates::Notify` 让系统安装**只提示不安装**，等于删掉壳的既有卖点（每次启动有新版就装并重启） | 新增 `config.json` 的 `system_updates: install（默认）\| notify`；`install` 时用 `update::install_prefix(dsh_js)` 反推用户前缀并就地升级（与自带运行时之前的实现一致），`notify` 才是只提示 | `system_updates_defaults_to_upgrading_the_user_install` |
| 架构门槛 | Rosetta 下的 x64 node 原本可用（dsh 树同架构、N-API 自洽），新门槛会直接拒掉它，**非自带构建**因此没有候选 ⇒ 错误页 | `system_runtime_gate()`：能力门槛（`stripTypeScriptTypes`，Node < 22.13 本来也跑不起来）保持硬性；架构不一致只在**有自带候选**时拒绝，否则警告并继续用系统安装；错误页也改为说明具体是哪道门槛 | `the_system_runtime_gate_only_refuses_what_it_must` |
| 能力探测没有超时 | `probe_node()` 用 `Command::output()` 阻塞，而它跑在启动状态机之前 —— 版本管理器 shim 一挂住，启动页就永久卡住，连"等 URL 超时"的兜底都用不上 | `probe_node_within(node, timeout)`：独立线程读管道 + `try_wait` 轮询，5 s 到期即 kill 并记日志；超时路径**不 join 读线程**（孙进程可能还握着管道，join 会把卡死搬回来） | `gives_up_on_a_node_that_never_answers`（300 ms 预算） |

顺带记录一处**未修**的既有缺口：§4 要求更新时追加 `--cache <app-data>/runtime/npm-cache` 复用依赖树缓存，
`update::install()` 目前没有传（每次更新重新下载整棵依赖树）。它不影响合并安全性，留待后续。

---

## 22. 全平台自带运行时构建（2026-09-13，`main`）

§20.4 记的两项遗留本轮补齐：**Linux 侧 staging 与打包**、以及**发行版只出精简包**。
`release.yml` 的每个 macOS/Linux 平台现在都先出精简版，再出 `-bundled` 的自带运行时版；
Windows 继续复用 `windows-portable.yml` 的免安装包（它本身就是自带运行时）。

| 改动 | 内容 |
|---|---|
| 交叉编译 staging | `Makefile` 的 `NODE_ARCH` 改为跟随 `TARGET` 的第一段（`x86_64-apple-darwin` → `x64`）；`TARGET` 含 `windows` 时 `NODE_OS` 取 `win`，解包按平台走 `tar` / `unzip`。此前在 arm64 runner 上打 x64 包会 stage **arm64** 的 node，产物装到目标机上直接起不来 |
| 产物 | 每平台两种：`…_<suffix>.app.zip` / `.deb`（精简版，需自备 node/dsh）与 `…_<suffix>-bundled.app.zip` / `-bundled.deb`（自带运行时，压缩后约 150–330 MB）；同平台两种产物写进同一份 `SHA256SUMS-<suffix>`，`release` 任务照旧合并成一份 |
| 打包顺序 | 先 `make bundle`，把精简版挪进 `thin/`，再 `make bundle-bundled`。第二遍构建会覆盖同名产物，顺序颠倒会丢掉精简版 |
| CI 依赖 | 新增 `actions/setup-python@v5`（`write_third_party_notices.py` 需要 python3）；macOS 超时 60→90 分钟、Linux 45→75 分钟（staging + 两份产物 + 500 MB 级资源复制） |
| CI 缓存 | staging 输入加 `actions/cache@v4`（`.runtime-cache/`：Node 压缩包、SHASUMS、npm 与 pnpm store；key 含 `runner.os` + 平台标识 + `runtime.lock`/`Makefile`/两个 staging 脚本的哈希，key 变化时用 `restore-keys` 恢复上一份，整份恢复、不做文件级合并）；cargo 与 `node_modules` 原本就有 `Swatinem/rust-cache` / `setup-node` 的缓存。**不缓存** `src-tauri/runtime/`：520 MB × 5 平台会挤占 10 GB 配额，而且放进缓存会引入"发出过期 dsh 版本"的风险（`check-runtime-stage.sh` 只校验结构，不校验版本） |
| 兜底断言 | macOS 断言 `Contents/Resources/runtime/node/bin/node` 存在；Linux 断言自带运行时版 `.deb` 至少是精简版的 3 倍（deb 内部路径由打包器决定，用体积比代替路径断言）。两者都是为了拦住「`bundle.resources` 没生效、安静发出一个只有壳的 `-bundled` 包」 |

已做的验证：`make -n runtime-stage TARGET=x86_64-apple-darwin` 显示下载并校验
`node-v22.23.2-darwin-x64.tar.gz`（本机不带 `TARGET` 时是 `darwin-arm64`）；`make -n bundle-bundled` 的
tauri 调用同时带上 `--target` 与 `--config`（resources + `minimumSystemVersion` 11.0）；两个 workflow 通过
YAML 解析；本机用假目录跑通「挪走精简版 → 再打自带运行时版 → 生成校验和」的分支逻辑，并验证体积比闸门在
劣化（bundled 与精简版一样大）时会失败。

~~**未验证**：CI 上尚未实跑（要等下一次 tag 触发）~~ **已补（2026-09-14，v0.2.0 的发布 run）**：
全平台两种产物都产出了，Linux staging 31072 文件 / 562 MB、macOS x64 用的是 `darwin-x64` 的 node、
两道产物断言与 staging 缓存命中都在真实发布里跑过。仍未做的只剩 Linux 侧的**实机启动**验证。

---

## 23. v0.2.0 之后的插件市场修复（2026-09-14，`main`）

`docs/design-task-fix-v0-2-0-post-merge-audit.md` 的 A1／A2 已修，两处都落在这份方案的范围内：

| 项 | 症状 | 修法 |
|---|---|---|
| A1 | 插件市场自动更新在 GUI 启动下必失败：`install_plugin` 的 PATH 只有 node 目录 + 应用自身 PATH，而 `dsh plugin add` 要转发给 pnpm（随包在 `<seed>/tools`）；失败前还先把实例停了，`Err` 分支又不写 attempted，于是每次启动重来 | 子进程环境组装（登录 shell 导入 + 工具前缀 + `npm_config_prefix`／`PNPM_HOME`）从 3d 提到 3b4，产物 `ChildEnv { path, vars }` 同时供插件安装与 harness 使用；安装前用 `update::find_pnpm` 解析 pnpm，解析不到就跳过（**不停实例**）；`Err` 分支补一个**短期**失败标记（`mark_plugin_attempt_failed`，5 分钟，见下） |
| A2 | 首启播种 profile 模板后立刻查 registry，破坏"首启不下载" | `seed_profile_template` 返回 `SeedOutcome { seeded, note }`，播过种的那一轮不查插件市场 |

测试 71 → 77（`find_pnpm`、跳过策略、子进程 PATH、播种返回值，以及失败标记的短期窗口与续期边界六个
用例）；命令行端到端：用组装出的 PATH 在全新 `DSH_HOME` 上跑 `dsh plugin --profile web add
dshmarket@1.46.1`，4.3 s 装好。

**失败标记为什么要短期**（审查文档 A5）：`attempted` 的语义是"装了但没生效，重试也没用"，会一直留到
下次发版；而安装失败（网络、registry、pnpm 自身出错）几乎都是瞬时的。若也写长存标记，一次抖动就会让
这个版本在 dshmarket 发布下一个版本之前都不再更新。所以 `Cache` 另加 `failed`/`failed_at`，只在
`FAILED_RETRY_MINUTES`（5 分钟）内抑制，窗口一到自动重试。

细节与未做项见那份审查文档 §10 与 §11.3。

---

## 24. macOS WebView 下限：启动前判定（2026-09-14）

用户反馈 macos-x64 产物启动后界面停在 `Failed to load plugins` / `Can't find variable: Iterator`。
根因不在架构也不在本仓库：随包的 `dsh-client-ui-sidebar-documentpreview`（dsh 0.1.5-rc.2）内联了
pdfjs-dist 6.3.289，其中给 `Iterator.prototype.join` 打补丁的那行没先判断全局 `Iterator` 是否存在，
而该全局是 **Safari 18.4（macOS 15.4）** 才有的 —— 比 §22 里 `minimumSystemVersion` 11.0 高得多。

壳侧已在打开 harness 窗口**之前**判定：splash 注入探针（ES5）→ 经 `core:event:emit` 上报 →
`window::unsupported_webview()` 最多等 500 ms → `lib.rs::hand_the_gui_to_the_browser` 命中就**改用默认
浏览器打开界面**（`window::open_external`）并留一个写明原因的管理窗口（关掉它会停 harness）；探测缺失
按支持处理（fail-open）。必需项只有 `Iterator`，`Math.sumPrecise` 等只作为「降级项」上报。

走浏览器而不是在 WebView 里打补丁，是因为旧系统上 `dsh web` 一直可用（浏览器引擎会更新，系统 WebView
不会），而补丁集既没法在本仓库验证、也会随 dsh 升级继续漂移 —— 理由与证据见那份审查文档 §3.2。

完整结论、证据链（BCD／WebKit 发布说明／pdfjs 那行源码）、影响面与未做项（上游修 + 真机验证）见
[`design-task-fix-webview-compat-audit.md`](./design-task-fix-webview-compat-audit.md)。测试 77 → 82。

> **2026-09-15 修正**：本节"必需项只有 `Iterator`、缺口一律改用浏览器"的结论已被兼容层取代。
> 探针改成「可补的 API 清单 + 用 `new Function` 编译 `class static block` 的语法判定」双清单；
> 可补的（`Iterator`、`Promise.try`、`Promise.withResolvers`、`Symbol.dispose`、`Math.sumPrecise`、
> `Uint8Array.fromBase64`、`Object.hasOwn`、`findLast`）由 harness 窗口经 `initialization_script`
> 注入 ES5 补丁（逐块自守卫、只装缺的那些），界面下限因此降到 **Safari 16.4（macOS 13.3）**；只有补不了
> 的语法才回退默认浏览器，且 `config.json` 的 `"webkit_compat": false` 可整体关掉。
> 本节保留当时的决策与理由；现行判定在 `window.rs::WebviewReport::gaps` / `compat_script`、
> `lib.rs::hand_the_gui_to_the_browser(app, url, version, compat)`。完整设计与证据见
> [`design-task-feat-legacy-webkit-compat-layer.md`](./design-task-feat-legacy-webkit-compat-layer.md)。
