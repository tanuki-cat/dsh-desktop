# 自带运行时体积缩减实施方案（安装包 / 解压包）

> 状态：**P1 + P2 已实施并实测**（2026-11-13）；P3 未实施（待实现/验证）；P4 未开始。
> 日期：2026-11-13
> 适用版本：dsh 0.2.2-rc.2 / Node 24.22.2 / pnpm 12.3.4 / dshmarket 1.73.19
> 关联：
> - `docs/design-task-feat-dsh-bundled-runtime.md`（自带运行时的引入与 staging 管线）
> - `docs/design-task-feat-app-updates-and-icons.md`（桌面壳自更新下载的就是这里的安装包）
> - `scripts/stage-runtime.sh`、`Makefile` 的 `runtime-stage`、`scripts/check-runtime-stage.sh`
>
> **替代关系**：本文替代 `docs/design-task-feat-dsh-bundled-runtime.md` §6「体积预算与取舍」中
> 「**不做手工裁剪**（推荐）」这一条建议。原因不是改变原则，而是事实变了：当时 dsh 树 289 MB、
> 12 个原生模块；0.2.2-rc.2 实测已到 1.0 GB / 8.5 万文件，其中一半以上是运行时根本不读的类型与
> TS 源码，且 393 MB 是同一份 zod 被装了 67 遍。旧文档保留原决策与理由，本文给出可执行的替换方案。

---

## 1. 结论

体积不是「上游变胖了，只能接受」，而是四类**可判定**的浪费叠加：

1. **运行时不可达的文件**：`.d.ts` / TS 源码 / `*.map` / `*.pdb` / 非本平台 prebuild，
   合计 338 MB、约 5.1 万个文件——Node 只按 `import`/`require`/`default` 条件解析，
   这些文件没有一个会被加载（已用整树启动验证）。
2. **依赖重复**：`zod` 被装了 **68 份**（67 个包精确 pin `4.6.6`，根上还有一份 `4.6.7`），
   同一份代码 393 MB、约 4 万个文件。
3. **平台无关载荷**：LibreOffice 的 wasm 引擎（145 MB）与原生引擎（154 MB）同时随包，
   而 `resolveEngine()` 实测优先原生、只在没有原生包时才用 wasm。
4. **工具链整包**：Node 发行版的 C 头文件（58 MB）与 corepack；pnpm 的同一份 33.8 MB
   二进制在包内出现两次（入口与 `@pnpm/exe-*` 是同一个文件的两个硬链接，复制/压缩后各算一份）。

| 指标 | 实施前（实测） | 阶段一 P1 | 阶段二 P1+P2（**已实施，实测**） | 阶段三 +P3（未实施，待实现/验证） |
|---|---|---|---|---|
| runtime 解压后 | **1,264 MB** | 688 MB | **537 MB** | 339 MB |
| 文件数 | **90,490** | ~36,400 | **18,859** | 17,412 |
| macOS `.app.zip`（deflate + zip 头） | **约 386 MB** | 约 230 MB | **约 191 MB** | 约 116 MB |
| 相对实施前（解压 / 安装包） | — | −46% / −40% | **−58% / −50%** | −73% / −70% |

P1 + P2 已落地：不删任何功能、不改变离线首启、不改资产名，解压后体积和安装包都腰斩（实测见 §1.1）。
P3 只影响 office / 语音 / DevTools 三个可选特性，**本次未实施，待实现/验证**（§7）。

### 1.1 实施记录（2026-11-13，macOS arm64）

| 项 | 结果 |
|---|---|
| staging | **18,859 文件 / 536.9 MB（文件字节）/ 188.7 MB（逐文件 deflate）/ 590 MB（du 块占用）** |
| 相对实施前 | 文件 −79.2%，字节 −57.5%，deflate −49.9% |
| 启动冒烟 | `dsh --version` = 0.2.2-rc.2；`dsh web` 2 s 内返回 **401** 围栏并打印 token URL |
| zod | **1 份**（4.6.6），override 生效 |
| pnpm | `tools/bin/pnpm --version` = 12.3.4 |
| 闸门 | `make runtime-stage` 与纯 `sh scripts/stage-runtime.sh <dir>` 两条路径都全绿 |

改动文件：

- 新增 `scripts/prune-runtime.py`：R1–R8 裁剪规则 + 按 `package.json` 运行时条件生成的白名单；
- `scripts/stage-runtime.sh` 与 `Makefile`：dsh 改项目布局安装 + `overrides.zod`，安装完后调用 prune；
- `scripts/check-runtime-stage.sh`：新增裁剪规则闸门（类型/源码、pdb、map、include、pnpm 重复、
  单份 zod、平台 LO），并把文件数/体积阈值收紧到 12,000–40,000 / 700 MB（Linux 800 MB）；
- `scripts/tests/check-runtime-stage-test.sh`：为每条新闸门造一个反例。

本次**未完成**：P3（§7）、更新前缀（§6.5）、P4（§8）、五平台打包验证（§10.2）。

---

## 2. 目标与约束

- **不降低默认功能**：P1/P2 后 `dsh web`、office 转换（原生引擎）、插件市场安装全部可用。
- **离线首启不变**：运行时不从 registry 拉取任何东西。
- **资产命名不变**：`src-tauri/src/app_update.rs` 按
  `dsh-desktop_<version>_<os>-<arch>[-bundled].app.zip|.deb` 精确匹配，
  Windows 是 `-portable.zip`；本次不碰这套契约。
- **闸门先于优化**：任何裁剪规则都必须有一道 `check-runtime-stage.sh` 能拦住回退的检查，
  否则下一次上游变更会静默把体积加回来（历史事实：staging 残留 20k 文件曾被静默打进包）。
- **度量口径统一**：体积一律按**文件字节数**统计；压缩体积用逐文件 `zlib.compress(level=6)`
  近似 zip（不含每文件约 100 B 的 zip 头）。`du` 的块占用会明显偏大（本次 `du` 1.4 G vs 实际 1.26 GB），
  不作为验收依据。

---

## 3. 实施前基线（macOS arm64，`src-tauri/runtime`）

> 以下数字是**实施前**（全局安装 + 只删 `*.map`）的实测，用来解释收益来源；
> 实施后的 staging 实测见 §1.1。

### 3.1 产物形态

| 平台 | 安装包 | 解压包 | 说明 |
|---|---|---|---|
| macOS | `…-bundled.app.zip` | `.app` 目录 | `ditto -c -k`，deflate |
| Windows | `…-portable.zip`（另有同内容 SFX `.exe`） | 解压目录 | 7z 生成 zip，deflate |
| Linux | `…-bundled.deb` | `dpkg-deb -x` 展开 | deb 内部已用 xz，压缩率略好于 zip |

桌面壳自更新（`app_update`）下载的就是这三种资产，所以**安装包体积同时也是更新带宽**。

### 3.2 组成（实测）

| 类别 | 文件数 | 原始 MB | zip 估算 MB | 说明 |
|---|---:|---:|---:|---|
| JS 运行时（`.js/.cjs/.mjs`） | 25,097 | 262.9 | 63.3 | 真正被加载的代码 |
| TS 源码（`.ts/.mts/.cts/.tsx`） | 25,919 | 228.1 | 53.6 | 只被 `source` / `@zod/source` 条件引用 |
| Node 发行版 | 4,797 | 188.3 | 52.5 | 其中 `include/` 58.2、npm 11.6、corepack 0.6 |
| LibreOffice 原生引擎 | 801 | 153.9 | 64.4 | `libreoffice-kit-darwin-arm64` |
| LibreOffice wasm 引擎 | 50 | 145.3 | 44.7 | `libreoffice-kit-wasm`，仅无原生包时需要 |
| 类型声明（`.d.*ts`） | 25,228 | 84.1 | 19.1 | 只被 `types` 条件引用 |
| tools（pnpm） | 454 | 68.2 | 32.2 | 其中 33.8 MB 是重复的同一个二进制 |
| 原生模块（`.node/.dylib/.wasm/.data`） | 15 | 54.2 | 18.7 | 含 sherpa-onnx 32.5、sharp 17.7 |
| 其它（数据、字体、wasm 附属） | 5,780 | 31.4 | 14.4 | |
| Windows 调试符号 `*.pdb` | 4 | 19.8 | 4.0 | 全平台都用不到 |
| Markdown | 1,944 | 16.2 | 6.1 | 技能/说明可能读取，暂不动 |
| profile 模板 | 385 | 5.8 | 1.6 | dshmarket + pin 依赖 |
| 非目标平台 prebuild | 16 | 5.6 | 2.1 | node-pty 的 linux/win32 产物 |
| **合计** | **90,490** | **1,263.8** | **376.8** | 加 zip 头约 386 MB |

校准：仓库里现存的 0.4.11 自有运行时包实测 280 MB（树 1.09 GB），与上表同量级，
说明当前 0.2.2 线的「约 380 MB 安装包 / 1.26 GB 解压」是可信基线。

### 3.3 根因与证据

1. **上游包同时发布 JS + TS 源 + 类型**
   `@deepseek-ai/dsh` 的 `files` 里带 `docs`、`src`；各依赖包带 `dist-types`、`src`。
   Node 解析 `exports` 时只认 `node/import/require/default`，`types`、`source`、`@zod/source`
   都是**构建期条件**。全树扫描：运行时入口指向 `*.ts` 的只有 5 个文件（且都是 `.d.ts`），
   其余 5 万多个类型/源码文件不可达。整树删除后 `dsh web` 正常启动（见 §10.3）。
2. **zod 68 份**
   67 个 `@deepseek-ai/*` 包写 `"zod": "4.6.6"`（精确 pin），而 `^4.2.0` 的依赖把根解析到
   `4.6.7`，npm 只能为每个包各装一份 `4.6.6`。单份 5.86 MB，合计 **398.8 MB**。
3. **LibreOffice 双引擎**
   `libreoffice-kit` 的 `optionalDependencies` 同时含原生包与 wasm 包。`resolveEngine()`
   先解析 `@deepseek-ai/libreoffice-kit-<platform>-<arch>`，只有该包缺失/不兼容才回退 wasm；
   macOS/Windows 都有原生包，wasm 是纯备胎。Linux 没有原生包，**wasm 必须保留**。
4. **Node 发行版整包随行**
   `node/include` 是给本地 `node-gyp` 编译用的 C 头；node-gyp 默认从 nodejs.org 下载同版本头文件，
   不依赖它。`corepack` 在本项目里没有任何调用方。
5. **pnpm 的同一二进制两份**
   `tools/lib/node_modules/pnpm/pnpm`（入口）与
   `tools/lib/node_modules/pnpm/node_modules/@pnpm/exe.<platform>/pnpm` 是同一 inode；
   复制进 `.app` 与压进 zip 后变成两份，**+33.8 MB 原始、+16.6 MB 压缩**。
   实测删掉后者后 `pnpm --version` 仍为 12.3.4。
6. **跨平台符号与 prebuild**
   node-pty 自带 win32 的 `*.pdb`（19.8 MB）与 linux/win32 prebuild；构建机上只可能加载本平台那一份。

### 3.4 现有裁剪为什么不够

`stage-runtime.sh` / `Makefile` 目前只删 `*.map`（0.2.2 起上游随包发布）。实测该规则已生效
（树里 `*.map` 为 0），但它只覆盖约 100 MB 量级，而类型/源码是 312 MB、zod 重复是 393 MB。

---

## 4. 方案总览

三条主线，按「零功能损失 → 需要产品决策」排序：

| 阶段 | 主线 | 内容 | 解压后 | 安装包 |
|---|---|---|---:|---:|
| P1 | 删不可达载荷 | 类型/TS/pdb/跨平台 prebuild/Node 头文件/corepack/pnpm 重复/wasm（仅 mac+win） | 688 MB | 230 MB |
| P2 | 依赖去重 | 用项目布局 + `overrides` 安装 dsh，zod 68 → 1 | 547 MB | 195 MB |
| P3 | 平台/特性分层 | office 原生（154 MB）、语音（33 MB）、DevTools（22 MB）改为按需/精简渠道 | 339 MB | 116 MB |
| P4 | 压缩与制品 | 7z LZMA2 / zstd / dmg 的实验；资产名兼容性 | 不变 | 视实验 |

> P1 与 P2 互相重叠：P1 先把 68 份 zod 里的类型/源码删掉，P2 再删 67 份只剩 JS 的副本
> （141 MB）。顺序反过来也成立（先去重复 393 MB，再删剩余类型 88 MB），总量一致；
> 建议按 P1 → P2 实施，因为 P1 风险更低、可独立回滚。

---

## 5. P1：删除运行时不可达载荷（无功能损失）

### 5.1 规则表

所有路径相对 `<runtime>`。规则必须**按目标平台执行**，以目标平台判定「本平台 prebuild」。

| ID | 规则 | 实测删除（macOS arm64） | 依据 | 例外 |
|---|---|---|---|---|
| R1 | 删 `**/*.d.ts`、`**/*.d.cts`、`**/*.d.mts` | 84.1 MB / 25,228 | `types` 条件不参与 Node 解析 | 白名单（§5.2） |
| R2 | 删 `**/*.ts`、`**/*.tsx`、`**/*.mts`、`**/*.cts` | 228.1 MB / 25,919 | `source` / `@zod/source` 条件同理 | 白名单 |
| R3 | 删 `**/*.pdb` | 19.8 MB / 4 | Windows 调试符号，运行不需要 | — |
| R4 | 删非目标平台 `**/prebuilds/{darwin,linux,win32}-*`、`**/native/bin/<其它平台>-<其它架构>` | 5.6 MB / 16 | 原生模块按平台 `require` | 目标平台那一份 |
| R5 | 删 `node/include`、`node/lib/node_modules/corepack` 及 `node/bin/corepack` 链接、`node/share/doc`、`node/share/man` | 58.8 MB / 2,865 | 头文件只服务本地编译；node-gyp 自行下载 | `DSH_RUNTIME_KEEP_NODE_HEADERS=1` |
| R6 | 删 `tools/lib/node_modules/pnpm/node_modules/@pnpm/exe-*`（保留 `tools/.../pnpm/pnpm` 入口） | 33.8 MB / 1 | 同一文件的两个硬链接；实测 `pnpm --version` 正常 | 检测不到 >1 MB 启动器时跳过（Windows 的入口布局未本机验证，闸门在 win32 上也不强求） |
| R7 | macOS / Windows：删 `**/libreoffice-kit-wasm` | 145.3 MB / 50 | 原生引擎存在时 wasm 不参与解析 | Linux 必须保留 |
| R8 | 删 `**/*.map`（保留现有行为） | 0（已生效） | 调试资源 | — |

合计：**575.5 MB / 约 5.4 万个文件**。

> 这一列是在**实施前基线**上逐条实测的。实际流水线先做 P2（zod 去重）再 prune，
> 所以 R1/R2 在真实运行中只删 37.6 MB / 30.8 MB（zod 副本已经没了），总量仍是同一批文件。

### 5.2 白名单算法（避免误删运行时入口）

对树内每个 `package.json`：

1. 取 `main`、`module`、`bin`，以及 `exports` 中**运行时条件**
   （`node`/`import`/`require`/`default`，含数组形式）指向的路径；
2. 忽略 `types`、`typings`、`source`、`@zod/source`、`browser`、`development`；
3. 若入口以 `.ts/.mts/.cts/.tsx` 结尾 → 加入保留集，**不删**。

实测保留集只有 5 个文件（`pkce-challenge` 的 `.d.ts`、`bowser` 的 `.d.ts`、
`@standard-schema/spec/src/index.ts` 等），合计 < 10 KB。白名单按**内容扫描**生成而不是按包名硬编码：
上游换版本后自动跟随，规则不失效。

### 5.3 实现位置

新增 `scripts/prune-runtime.py`（只用标准库）：

```text
用法: python3 scripts/prune-runtime.py <runtime 目录> --platform darwin|win32|linux --arch arm64|x64
行为: 依次执行 R1..R8，逐条打印「规则 / 文件数 / MB」，最后打印总数；
     任一必需目录不存在（如 node/、dsh-prefix/）时以非 0 退出，避免在错误目录上静默成功。
开关: DSH_RUNTIME_KEEP_NODE_HEADERS=1 保留 include；DSH_RUNTIME_KEEP_TS=1 只做体积实验。
```

接入顺序（两条 staging 路径必须一致）：

1. `scripts/stage-runtime.sh`：把现有 `find … -name '*.map' -delete` 换成
   `prune-runtime.py` 调用，位置在 dsh/pnpm/模板全部装好之后、
   `write_third_party_notices.py` 之前。
2. `Makefile` 的 `runtime-stage`：同样位置加一行调用（Windows 便携包走
   `stage-runtime.sh`，这里只影响 macOS/Linux 的 `make runtime-stage`）。
3. `write_third_party_notices.py` 不需要改：它只读 `package.json`，删除文件不影响许可清单。

**已实施**：`scripts/prune-runtime.py` 已接入上面两条 staging 路径，`make runtime-stage` 与
`sh scripts/stage-runtime.sh <dir>` 都实测通过（删除量 R1 8,721 / R2 3,675 / R3 4 / R4 33 /
R5 2,868 / R6 4 / R7 50 / R8 9,922，合计 25,277 文件 / 435.7 MB）。

### 5.4 闸门（`scripts/check-runtime-stage.sh`）

新增三类检查，并把阈值从「1600 MB / 100,000 文件」收紧到「700 MB / 40,000 文件」：

| 类型 | 检查 | 失败示例 |
|---|---|---|
| 禁止 | 存在 `*.d.ts` / `*.pdb` | 说明 prune 没跑或规则被绕过 |
| 禁止 | macOS/Windows 存在 `libreoffice-kit-wasm`；Linux 存在原生 LO 包 | 平台判定错误 |
| 禁止 | 存在 `node/include`、`@pnpm/exe-*` | R5/R6 未生效 |
| 必需 | 目标平台原生 LibreOffice 包存在（mac/win） | 删了 wasm 又没原生包 = office 全坏 |
| 必需 | `tools/bin/pnpm` 可执行（现有） | 现有检查 |
| 数量 | 文件数 12,000–40,000；体积 ≤ 700 MB | 残留/裁剪失败 |

**已实施**，阈值取 12,000–40,000 文件、700 MB（Linux 800 MB，因为它多一份 LibreOffice wasm）。
`scripts/tests/check-runtime-stage-test.sh` 用假目录验证闸门自身：已为每条新规则加了反例
（第二份 zod、macOS 带 wasm、缺原生 LO、node/include、残留 `*.pdb`、`@pnpm/exe-*`、25 个 `.ts`）。

---

## 6. P2：依赖去重（zod 68 份 → 1 份）

### 6.1 成因

- 67 个 `@deepseek-ai/*` 包写死 `"zod": "4.6.6"`；
- 根上出现 `4.6.7` 是因为另一条 `^4.2.0` 的依赖被解析到最新 patch；
- npm 对精确 pin 不做提升，于是每个包各带一份。`npm dedupe` 在全局模式直接拒绝：
  `EDEDUPEGLOBAL`（已实测）。

### 6.2 方案：项目布局 + overrides

把 dsh 的安装从「全局前缀」改为「项目安装」，用 npm 官方支持的 `overrides` 把 zod 收敛到 4.6.6：

```sh
mkdir -p "$out/dsh-prefix/lib"
cat > "$out/dsh-prefix/lib/package.json" <<JSON
{
  "name": "dsh-bundled-runtime",
  "private": true,
  "version": "0.0.0",
  "dependencies": { "@deepseek-ai/dsh": "$dsh_version" },
  "overrides": { "zod": "$zod_override" }
}
JSON
(cd "$out/dsh-prefix/lib" && "$node" "$npm" install --omit=dev --no-fund --no-audit \
  --loglevel=error --cache "$cache/npm")
```

得到的布局是 `dsh-prefix/lib/node_modules/...`，**与原来的全局前缀完全一致**
（`src-tauri/src/lib.rs` 的 `DSH_JS_SUFFIXES` 两项都支持；`check-runtime-stage.sh` 的
`dsh-prefix/lib/…` 路径不变）。差异只有 `lib/package.json` 与 `lib/package-lock.json`
（后者顺带让 staging 可复现，建议保留）。

`zod_override` 默认 `4.6.6`（即 67 个包要求的精确版本）。选择降到 4.6.6 而不是升到 4.6.7：
受影响的是 67 个精确 pin 的调用方，而 `4.6.7` 只出现在 `@earendil-works/pi-schema` 的
**devDependencies** 里（实测该包运行时没有 zod 依赖），所以降级没有运行时影响。

**已实施**：`scripts/stage-runtime.sh` 与 `Makefile` 都改成了这段安装方式，变量名
`DSH_ZOD_OVERRIDE`（默认 4.6.6），实测 zod 68 → 1，staging 与启动冒烟均通过。

### 6.3 实测

| 项目 | 全局安装（现状） | 项目布局 + override |
|---|---:|---:|
| zod 副本数 | 68 | **1** |
| dsh 树（P1 前，含类型/源码） | 1,001.5 MB / 84,854 文件 | ≈710 MB / 38,494 文件 |
| dsh 树（P1 后：类型/源码/pdb/跨平台/wasm 全删） | 518.6 MB | **376.0 MB / 16,239 文件** |
| 整树 deflate（P1 后） | 166.9 MB | **133.1 MB** |
| `dsh --version` | 0.2.2-rc.2 | 0.2.2-rc.2 |
| `dsh web` 启动 | 401 围栏 | **401 围栏（实测通过）** |

> 口径说明：项目布局的实测树是 2026-11-13 重新解析安装的，`ws`、`encodeurl`
> 比 11-12 的 staging 高了 patch 版本，总量差异 < 1 MB，不影响结论。

### 6.4 备选与取舍

| 备选 | 结论 |
|---|---|
| `npm dedupe -g` | 不可用（`EDEDUPEGLOBAL`），实测报错 |
| pnpm `node-linker=hoisted` 安装 dsh | 可去重，但引入硬链接/符号链接与 pnpm 元数据，复制进包后去重收益消失；不建议 |
| 安装后手工删嵌套 zod | 需要自己实现 semver 兼容判定，风险高于收益 |
| 上游统一 zod 版本 | **最优长期解**，见 §13；本方案先让桌面端不再受制于它 |

回滚：撤销 `stage-runtime.sh` / `Makefile` 里这一段安装命令即可（两个文件各一处），
运行时与闸门都不需要改；闸门的「单份 zod」检查会立刻指出回滚后的体积回归。

### 6.5 更新前缀（**未实施，待实现/验证**）

`src-tauri/src/update.rs` / `update_flow` 的暂存更新仍然用
`npm install -g @deepseek-ai/dsh@<version>` 装进 `<app-data>/runtime/staging-*/prefix`，
**没有 overrides**。这一项本次没有改，标记为待实现/验证，原因是它需要一次真实的
联网更新演练（安装 → 切换 → 回滚）才能验收，不适合和纯构建侧改动一起上线。可选的两种做法：

1. 更新也改为项目布局 + 同一个 override（推荐，复用同一段安装逻辑）；
2. 或在更新完成后对目标前缀执行 `prune-runtime.py`（只能省类型/源码，zod 仍 393 MB）。

这一项不改变安装包体积（安装包来自 §5/§6 的 staging），但决定用户磁盘会不会继续膨胀到 GB 级。
在它完成之前，自动更新路径仍会按全局布局安装依赖，这一点必须在发布说明里写清楚。

---

## 7. P3：平台与特性分层（**未实施，待实现/验证**）

> 本节内容本次**没有实施**：需要产品先确认 office / 语音 / DevTools 是否可以按需安装，
> 以及是否要额外的「完整版」资产名（会改 `app_update` 的资产契约）。当前 staging 仍随包
> 带全部三项，实测 208.7 MB / 78.5 MB（zip）。实施后按本文 §7.2 的机制选择落地并重新过闸门。

### 7.1 三个候选载荷（实测）

| 载荷 | 原始 MB | zip MB | 功能 | 建议 |
|---|---:|---:|---|---|
| `libreoffice-kit-<platform>`（office 转换/渲染） | 153.9 | 64.4 | 文档预览、Office↔PDF | 保留在完整版；精简版/按需安装 |
| `sherpa-onnx-*`（语音识别） | 32.5 | 9.3 | 语音输入（实验特性） | 按需或仅完整版 |
| `dsh-experimental-inspector`（DevTools 前端） | 22.3 | 4.8 | 调试面板 | 仅开发版或按需 |
| 合计 | 208.7 | 78.5 | | |

去掉这三项后：**339 MB 解压 / 约 116 MB 安装包**。Linux 若同时去掉 wasm（office 在 Linux
完全不可用），再减 145.3 MB / 44.7 MB。

### 7.2 机制选择

| 方案 | 优点 | 代价 | 建议 |
|---|---|---|---|
| A. 「精简版 / 完整版」两个发行渠道 | 实现简单，只改构建参数 | 资产矩阵翻倍；`app_update` 的资产名要加 flavour；用户不理解差异 | 短期可做，仅当产品接受 |
| B. 首次使用时从 npm 下载到 app-data | 默认包最小 | 需要安装/校验/回滚/UI 提示；破坏「离线可用」的完整性；更新时还要保活 | 中期，需上游配合 |
| C. 上游把这些能力改成插件市场的可安装插件 | 最干净，体积随用户选择 | 需要 dsh 仓库改默认依赖与插件边界 | **长期推荐** |

本仓库能独立完成的是 A；B/C 需要与 dsh 主线协同。建议先把 P1/P2 落地，
同时把「office/语音/inspector 的默认随包」作为问题提给上游（见 §13）。

### 7.3 明确不做的两件事

- **不删 `*.md`**：技能、指令、插件说明可能按路径读取（16.2 MB，收益低、风险高）。
  只删包内 `README.md`/`CHANGELOG.md` 可以再省约 6 MB 压缩体积，留作 P1 的可选项，需逐包验证。
- **不删 LICENSE / 许可文件**：`THIRD-PARTY-NOTICES.md` 只汇总 package.json 的 license 字段，
  许可正文仍应随包；删了会有合规风险。

---

## 8. P4：压缩与制品（未开始）

P1–P3 解决的是「内容」，P4 才轮到「编码」：

1. **现状**：macOS 用 `ditto -c -k`（deflate），Windows 用 7z 的 zip（deflate），
   Linux deb 内部是 xz。逐文件 deflate 的实测压缩比约 3.3–4.4×（文本高、二进制低）。
2. **可做实验**：对 P1+P2 后的树分别用 `7z -mx=9`（LZMA2）与 `zstd -19` 打一份，
   记录体积与解压耗时。预期再省 10%–20%（文本占比高的部分收益更大）。
3. **不能直接换**：`app_update` 按 `.app.zip` / `.deb` 后缀选资产并在 `SHA256SUMS` 中校验，
   换格式等于同时改 `src-tauri/src/app_update.rs` 与 `release.yml`，还要解决 macOS 用户
   双击解压的体验（Safari 原生只认 zip）。**收益小于 P1–P3，排在其后**。
4. **不采用**：把 runtime 打成单个压缩包、首启解压到 app-data。安装包不会更小（zip 已经压过），
   解压后磁盘更大（包内一份 + 解压一份），首次启动多 10–30 s，且运行时解出的 Mach-O
   不再被 `.app` 签名覆盖，Gatekeeper 风险上升。与「离线首启、双击即用」的目标冲突。

---

## 9. 落地步骤（文件级）

按依赖顺序：

| # | 文件 | 改动 | 状态 |
|---|---|---|---|
| 0 | `scripts/measure-runtime-size.py`（新增） | 逐文件 deflate + 分类统计，输出 §3.2 形态的表 | ✅ 已实施 |
| 1 | `scripts/prune-runtime.py`（新增） | R1–R8 + 白名单扫描 + 逐条统计 | ✅ 已实施 |
| 2 | `scripts/stage-runtime.sh` | 删 map 改为调用 prune；dsh 改项目布局 + `overrides`；暴露 `DSH_ZOD_OVERRIDE` | ✅ 已实施并实测 |
| 3 | `Makefile` | `runtime-stage` 接入 prune 与项目布局；`DSH_RUNTIME_*` 阈值收紧 | ✅ 已实施并实测 |
| 4 | `scripts/check-runtime-stage.sh` | 新增禁止项/必需项、单份 zod、文件数与体积阈值；注释里更新实测基线 | ✅ 已实施 |
| 5 | `scripts/tests/check-runtime-stage-test.sh` | 新增闸门失效用例（第二份 zod、wasm、缺原生 LO、include、pdb、`@pnpm/exe-*`、类型文件） | ✅ 已实施 |
| 6 | `.github/workflows/release.yml` / `windows-portable.yml` | staging 闸门已覆盖体积/文件数断言；workflow 内未加独立断言 | ⏳ 待实现（可选） |
| 7 | `src-tauri/src/update.rs` | 更新前缀使用同一安装布局/override | ⏳ 待实现/验证（§6.5） |
| 8 | `README.md` | 更新「约 1.4 GB / 9 万文件」的说明与新数字 | ✅ 已更新 |
| 9 | 本文 §1.1 / §10.3 | 落地后把「估算」替换为实测 | ✅ 已回填 |

> 顺序理由：先让裁剪与去重各自可测，再收紧闸门。若先收紧闸门，未实施的 staging 会直接
> 让 `make bundle-bundled` 失败，掩盖真实问题。
>
> 实际耗时（macOS arm64，含两次完整 staging 与一次 `stage-runtime.sh` 复跑）：约 1.5 h；
> 剩余工作主要是五平台 CI 与更新演练。

---

## 10. 验证

### 10.1 每个平台必跑

1. `prune-runtime.py` 报告与 §1.1 的实施记录一致（±5%）；§5.1 是实施前基线上的逐条实测。
2. `<runtime>/node/bin/node <runtime>/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --version`
   输出 0.2.2-rc.2。
3. 启动冒烟：`… bin.js --profile web --no-open --port <p>`，2–10 s 内 `curl` 得到 **401** 并打印 token URL。
4. office：转换一份小 `.docx`（mac/win 走原生引擎；linux 走 wasm）。
5. 插件：用自带 pnpm 在 profile 里安装一个市场插件（验证 R6 后 pnpm 仍可完整工作）。
6. 打包产物：`du -sm`、`find | wc -l`、zip 体积落在 §1 的目标区间。
7. 桌面壳自更新：下载一份真实 `-bundled.app.zip` 并校验 SHA256（数量级变化后仍是同一链路）。

### 10.2 交叉验证矩阵

macOS arm64 / macOS x64 / Linux x64 / Linux arm64 / Windows x64——五个平台各有
「原生 LO 的取舍不同 / wasm 保留与否不同 / pnpm 布局不同」三处分叉，必须各自 staging。
Windows 额外看：最长相对路径（实施前 241 → 实施后 224 字符）、解压后的 MAX_PATH 告警。

### 10.3 本次已完成的本机验证（macOS arm64）

| 验证 | 结果 |
|---|---|
| `make runtime-stage`（Makefile 路径） | 全绿：18,859 文件 / 590 MB（du） |
| `sh scripts/stage-runtime.sh .stage-check`（Windows 便携包同款路径） | 全绿，输出与上一行一致 |
| `dsh --version` | 0.2.2-rc.2 |
| `dsh web --profile web --no-open` | 2 s 内 **401** 围栏 + token URL |
| `tools/bin/pnpm --version` | 12.3.4 |
| zod 副本 | 1 份（4.6.6） |
| 逐文件 deflate | 536.9 MB → 188.7 MB；加 zip 头约 **191 MB** 安装包 |
| 裁剪规则闸门 | 8 条规则全过；自检 7 条反例全部生效（`make test-scripts`） |

**未验证**（需要真机/联网/多平台）：office 转换（原生引擎与 Linux wasm）、真实插件安装、
五平台打包与解压、桌面壳自更新与回滚演练（§10.1 第 4–7 条）。

> 这些是**桌面壳启动链路**的验证，不替代上面的未验证项。

---

## 11. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| 上游未来把 `.ts` 当运行时入口（Node 24 可原生 type stripping） | R1/R2 误删 | 白名单按 `package.json` 运行时条件扫描；闸门在 staging 阶段失败而不是发布后 |
| zod 版本漂移（新包 pin 4.6.7+） | override 失配，安装失败或双份 | 「单份 zod」闸门 + `DSH_ZOD_OVERRIDE` 变量；失败时退回全局布局 |
| 删 `node/include` 后离线编译原生插件失败 | 离线编译类插件不可用 | `DSH_RUNTIME_KEEP_NODE_HEADERS=1` 兜底；文档写明需要联网下载头文件 |
| 删 wasm 后原生包缺失/损坏 | office 全坏 | 闸门断言 mac/win 必有原生包；可回退保留 wasm |
| 删 pnpm 重复载荷影响插件安装 | 市场不可用 | CI 冒烟「真实安装一个插件」；失败即回退 R6 |
| 项目布局改变 npm 元数据 | 运行时找不到 dsh | `DSH_JS_SUFFIXES` 两套布局都支持；启动冒烟覆盖 |
| 缩短后的树触发新的手写路径假设 | 启动失败 | 现有 `check-runtime-stage` 的必需内容清单不变；启动冒烟作为发布门禁 |
| 更新前缀继续长胖 | 用户磁盘 GB 级 | §6.5 同步改造，或更新后跑 prune |

---

## 12. 不采用的方案

| 方案 | 不采用的理由 |
|---|---|
| 首启从 registry 拉整棵 dsh 树 | 违背「离线首启」；首启需要网络与数分钟 |
| 运行时装成单个压缩包、首启解压 | 安装包不再变小；解压后双份磁盘；签名与 Gatekeeper 风险（见 §8 第 4 条） |
| 无证据的「瘦身」（删 README/LICENSE/任意目录） | 会静默破坏技能/合规；每条规则都必须有可达性或平台依据 |
| 硬链接/符号链接去重 | zip 与 `tauri` 的资源复制都会展开链接，体积收益在产物中消失 |
| 直接换 LZMA2/zstd 资产 | 收益小于内容裁剪，且要改自更新资产契约与 macOS 解压体验（§8 第 3 条） |
| 在 dsh 侧 pin 掉全部依赖到更小版本 | 改变上游语义，风险与收益不成比例 |

---

## 13. 待确认 / 需要上游配合

1. **产品**：office（154 MB）、语音（33 MB）、DevTools（22 MB）能否接受
   「精简版默认不带 / 首次使用时安装」？这决定 P3 是否进入本次发布。
2. **产品**：是否需要同时发布「完整版」资产名（如 `-bundled-full`）？
   这会改变 `app_update` 的资产选择和 `SHA256SUMS` 的消费方式。
3. **上游（dsh 仓库）**：
   - 包的 `files` 是否必须随包发 `src`/`dist-types`？若只为 IDE 跳转，可从发布物移除或放到独立包；
   - 67 个包统一 `zod` 版本（4.6.6/4.6.7 收敛），可直接省掉 P2 的 override；
   - office / 语音 / inspector 是否应作为可安装插件而不是 `@deepseek-ai/dsh` 的默认依赖。
4. **构建**：CI 的 macOS/Linux/Windows runner 磁盘与耗时变化（文件数从 9 万降到 1.9 万，
   资源复制与压缩应更快，但需要实测确认）。

---

## 附录 A：复现命令（macOS arm64）

```sh
# 1) 现状分类统计：逐文件 deflate(6)、按类别汇总（P0 落成 scripts/measure-runtime-size.py）
python3 scripts/measure-runtime-size.py <runtime>

# 2) 项目布局 + override 安装（在临时目录）
mkdir -p /tmp/dsh-ovr/lib && cd /tmp/dsh-ovr/lib
cat > package.json <<'JSON'
{"name":"dsh-bundled-runtime","private":true,"dependencies":{"@deepseek-ai/dsh":"0.2.2-rc.2"},"overrides":{"zod":"4.6.6"}}
JSON
<runtime>/node/bin/node <runtime>/node/lib/node_modules/npm/bin/npm-cli.js install --omit=dev

# 3) 裁剪
python3 scripts/prune-runtime.py <runtime> --platform darwin --arch arm64

# 4) 冒烟
<runtime>/node/bin/node <runtime>/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js \
  --profile web --no-open --port 18432 &
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18432/   # 期望 401
```

## 附录 B：度量口径

- 原始体积：`os.lstat().st_size` 求和（符号链接不计）。
- 压缩体积：逐文件 `zlib.compressobj(6, DEFLATED, -15)` 的字节数之和；
  真实 zip 另加每文件约 100 B 头，9 万文件约 +9 MB、1.9 万文件约 +2 MB。
- `du` 的块占用（本次基线 1.4 GB vs 实际 1.26 GB）仅用于快速观察，不进入验收。
- `*.map` 在现状 staging 已被删除；本文所有「现状」数字都是删 map 之后的树。
