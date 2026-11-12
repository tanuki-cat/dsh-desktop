# dsh-desktop 对 dsh 0.2.2-rc.2 的兼容性审查

审查对象：`@deepseek-ai/dsh@0.2.2-rc.2`（npm dist-tag `next`）  
审查仓库：`dsh-desktop`  
审查分支：`main`  
审查提交：`ac7a06e`（应用版本 `0.5.1`）  
审查日期：2026-11-12  
审查环境：macOS 27.2.1（26B131，arm64）、Node `24.22.2`、系统 WebView；本机另装有已发布的 `0.4.15` 壳

本文只记录审查结论，未修改仓库代码。

## 1. 总体结论

**运行时契约兼容；自带运行时的 staging/打包链路有一个硬阻塞。**

- rc.2 与本壳依赖的 CLI、启动 URL、鉴权围栏、token 流程、`web-runtime` overlay、插件市场兼容性均未发现破坏性变化。
- 本机已安装的 `0.4.15` 壳（core 契约与 `0.5.1` 相同）已在 2026-11-12 17:13 用 system rc.2 实际启动过一次，WebView 兼容层正常注入，页面恢复绘制，没有壳层错误。
- rc.2 开始随包发布 `src/` 和 `*.map`，按仓库当前的 `scripts/check-runtime-stage.sh` 闸门，自带运行时 staging 会因文件数和体积超限失败。**在修正闸门或裁剪调试资源之前，不能把 rc.2 原样作为 bundled runtime 发布。**

## 2. 审查范围与方法

1. 读取 npm registry 元数据与 dist-tags：`latest = 0.2.2-rc.1`、`next = 0.2.2-rc.2`、`alpha = 0.2.2-alpha.3`。
2. 下载 rc.1/rc.2 的 `@deepseek-ai/dsh` CLI 包及关键依赖，逐文件 `diff`/`cmp`：
   - `dsh-web-app`（启动参数、URL 打印、`web-runtime` 行）
   - `dsh-client-connection`（401 鉴权围栏、cookie）
   - `dsh-cmdline`、`dsh-host-webserver`、`dsh-host-frontend-static`
   - `dsh-app-boot`（overlay patch 格式、`!!js` 标签、插件兼容性 preflight）
3. 用壳自己的命令行实际启动 rc.2：`dsh --profile web --patch <overlay> --no-open --port N`，验证 stdout 启动行、401 fence、token 跳转、cookie、页面加载。
4. 扫描 rc.1/rc.2 的全部 client bundle 与 `dsh-web-frontend/dist`：
   - 兼容层覆盖的 API 集合、ES 语法级别、CSS 特性
   - 滚动锚定、弹窗、菜单等注入脚本依赖的 DOM hook
5. 用 rc.2 的真实 npm 安装树 + 仓库现有 Node/pnpm/profile 模板模拟自带运行时，直接执行 `scripts/check-runtime-stage.sh`。
6. 读取本机已安装壳的 `harness.log`，核对 rc.2 在真实 WebView 中的启动、兼容层与页面状态。

## 3. 逐项结论

| 检查项 | 结论 | 证据 |
|---|---|---|
| `--profile web` / `--patch` / `--no-open` / `--port` 顺序 | 通过 | rc.2 `--profile web --help` 接受同一组参数；`--dump-config` 正常 |
| overlay 中的 `web-runtime` 行与三个 config key | 通过 | `--dump-config` 显示该行被 overlay patch；真实启动生效 |
| stdout 启动行 | 通过 | `dsh web: http://127.0.0.1:62549/?token=...`，冷启动到 URL 约 2.7s |
| 无 token 的 `GET /` | 通过 | `401` + `dsh web authentication required; reopen the URL printed by dsh web.` |
| token URL / cookie / 页面 | 通过 | `303` + `set-cookie`；带 cookie 返回 `200`，41 KB，`<title>DeepSeek Harness</title>`；token 可重复访问 |
| 服务端关键文件 | 通过 | `dsh-web-app/lib/index.js`、`startup.js`、`dsh-client-connection/lib/index.js`、`dsh-cmdline`、`dsh-host-webserver`、`dsh-host-frontend-static` 与 rc.1 逐字节相同 |
| WebView 兼容层 | 通过 | client bundle 的 API/语法集合与 rc.1 同类；未发现 rc.2 新增而未覆盖的 API；DOM hook 全部保留 |
| 插件市场 | 通过 | `dshmarket@1.73.19` 对 rc.2 的 `evaluatePluginCompatibility` 返回 compatible |
| 真实壳启动 | 通过（0.4.15 实测） | 日志：`runtime ... dsh 0.2.2-rc.2`、`WebView 缺少 Symbol.dispose：已注入兼容层`、页面恢复绘制 |
| 自带运行时 staging | **失败** | `100,410` 个文件 / `1,626 MB`，闸门为 `100,000` / `1,500 MB` |

## 4. 运行时与 WebView 细节

### 4.1 服务端契约没有变化

rc.2 的 CLI 入口被拆成了 `lib/bin.js` + 带哈希的 `bin-*.js`，但 `lib/bin.js` 仍存在、仍可直接执行，壳的 `locator` 和命令匹配不受影响。

以下文件 rc.1 与 rc.2 `cmp` 结果一致：

- `dsh-web-app/lib/index.js`（启动 URL 前缀、`printUrl`、服务启动）
- `dsh-web-app/lib/startup.js`（`--host/--port/--no-open/--public-url/--trusted-host/TLS`）
- `dsh-client-connection/lib/index.js`（401 fence 与 token/cookie 逻辑）
- `dsh-cmdline/lib/index.js`
- `dsh-host-webserver/lib/index.js`
- `dsh-host-frontend-static/lib/index.js`

`dsh-web-app/cordis.patch.yml` 的 `web-runtime` 行仍为：

```yaml
- id: web-runtime
  name: '@deepseek-ai/dsh-web-app'
  inject: [webStartup]
  config:
    openBrowser: !!js ctx.webStartup.openBrowser
    printUrl: true
    publicUrl: !!js ctx.webStartup.publicUrl
    surfaceContext: true
```

壳写的 overlay 仍然是 `- id: web-runtime` + `openBrowser/printUrl/surfaceContext`，rc.2 能正常合并。

### 4.2 WebView 兼容层没有新增缺口

按当前 client bundle 扫描：

- 兼容层已有的 8 个 API（`Iterator`、`Promise.try`、`Promise.withResolvers`、`Symbol.dispose`、`Math.sumPrecise`、`Uint8Array.fromBase64`、`Object.hasOwn`、`findLast`）仍是 rc.2 使用的主要缺口集合。
- rc.2 新增的 UI 包（`dsh-client-ui-account-notifications`、`dsh-client-ui-agent-team`、`dsh-mcp-manager`、`dsh-cua` 等）没有引入新的、兼容层未覆盖的 API。
- ES2022 语法解析除 `client.pdf.js` 的 `v` 正则 flag 外全部通过；该问题 rc.1 已存在，不是 rc.2 新增。
- 滚动锚定依赖的 hook 仍存在：`data-conversation-scroll`、`data-chat-flow-key`、`data-chat-following-tail`、`data-disclosure-row`。
- 本机 WebView 仍只缺 `Symbol.dispose`，与 rc.1 时的日志一致；rc.2 的 `Symbol.dispose` 使用量没有变化。

rc.1 已有、rc.2 继续保留并扩展了可选的桌面桥（`globalThis.dshDesktop`、`documentElement.dataset.platform`），用于 `fileActions`、`keyboard`、`shortcuts`、`browser`、`fileDrag` 等原生能力；rc.2 增加了 `protocolVersion === 1` 的显式版本检查。本壳没有提供这些全局，因此 rc.2 会按 **web 模式**运行。这不是兼容性破坏，但 "Save as"、原生快捷键、桌面更新通道等桌面增强不会启用。

### 4.3 插件市场

`dshmarket@1.73.19`（仓库 staging 默认版本，2026-11-12 由 1.73.17 更新）的 peerDependencies 中 `@deepseek-ai/dsh-settings` 范围为 `^0.2.1-rc.1`，rc.2 的 `evaluatePluginCompatibility` 使用 `includePrerelease: true`，实测返回 compatible。

市场恢复接口 `/dsh-market/recovery`、`/dsh-market/recovery/release` 及其 payload 标记在 1.73.19 与 1.73.17 中一致；本次市场版本更新未改变该契约。1.73.19 的客户端 bundle 仍是 ES2022 可解析、未使用兼容层之外的 API。

## 5. 硬阻塞：自带运行时 staging

用 rc.2 的真实 npm 安装树，加上仓库现有 `node`、`tools`、`profile-template`，执行：

```sh
DSH_RUNTIME_MAX_FILES=100000 DSH_RUNTIME_MAX_MB=1500 \
  sh scripts/check-runtime-stage.sh <runtime-sim>
```

结果：

| 指标 | rc.1 实测 | rc.2 模拟 | 闸门 |
|---|---:|---:|---|
| 文件数 | 75,799 | **100,410** | ≤ 100,000 |
| 体积 | ~1,140 MB | **1,626 MB** | ≤ 1,500 MB |
| 最长路径 | 221 字符 | **254 字符** | > 240 警告 |

根因是 rc.2 的发布内容变化：

- `dsh-prefix` 文件数 70,161 → 94,773。
- `*.map`：9,919 个文件，约 101 MB。
- `src`：27,745 个文件，约 236 MB。
- `dsh-web-frontend` 解包体积 5.6 MB → 21 MB；`@deepseek-ai/dsh` tarball 25 KB → 2.2 MB。
- 最长路径来自新增的 `dsh-experimental-cua-macos/native/.../DSH Computer Use.app`。

处理选项：

1. **不改运行时树**：把 `DSH_RUNTIME_MAX_FILES` 提到约 110,000，`DSH_RUNTIME_MAX_MB` 提到约 1,700。
2. **保留文件数闸门**：staging 后删除 `*.map`，实测变为 90,491 个文件 / 1,498 MB；文件数通过，但体积只剩 2 MB 余量，仍需把 `DSH_RUNTIME_MAX_MB` 提到约 1,600。
3. **不要无条件删除所有 `*/src/*`**：`node-fetch` 的 `main` 指向 `./src/index.js`，OpenTelemetry 的运行时入口在 `build/src/`；按当前树实测，盲目删除虽然 CLI 还能打印版本，但会破坏部分运行时包。若裁剪 `src`，只能按 `main`/`exports`/`bin` 白名单筛选。

## 6. 其他风险与限制

- **自动更新范围**：`src-tauri/src/update.rs` 的测试区间是 `>= 0.1.5-rc.1, < 0.2.2`，rc.2 在区间内；默认 `update_tags` 含 `next`，默认配置的机器会看到并安装 rc.2。正式版 `0.2.2` 仍会被判为 "Newer"，发布后需单独验证并抬高上界。
- **种子运行时仍是 rc.1**：`Makefile:199`、`scripts/stage-runtime.sh:16`、`.github/workflows/windows-portable.yml` 两处默认值、README 的版本/体积说明需要一起改；release preflight 会校验这三处一致。
- **安装超时与磁盘**：`update.rs` 的 `INSTALL_TIMEOUT` 为 300s；rc.2 的 npm 缓存约 340 MB，低于约 9 Mbps 的链路可能接近或超过 5 分钟。更新时 staging + 旧树峰值约 2.7 GB。
- **第三方插件**：rc.2 仍会按精确 peer 禁用不匹配插件（rc.1 已有该 preflight）。本机日志中 `mcp-idea`、`computer-use`、`computer-use-cua-driver-native` 被禁用，`dsh-dream-skin` 因 class 名变化报纯装饰性 warning；这些属于插件侧，不是壳的回归。
- **CUA 原生包**：`DSH Computer Use.app` 的 `LSMinimumSystemVersion` 是 14.0，高于 bundled 壳声明的 macOS 13.5；该包只在启用 computer-use 插件时使用，不影响默认 web profile 启动。
- **Windows 路径**：最长路径 254 字符，超过 240 的提示线；继续使用 7z 自解压包并解压到短路径。

## 7. 建议改动清单（若决定把 rc.2 作为 bundled runtime）

1. 更新 `Makefile` 的 `DSH_VERSION`。
2. 更新 `scripts/stage-runtime.sh` 的 `dsh_version` 默认值。
3. 更新 `.github/workflows/windows-portable.yml` 两处 `dsh_version` 默认值。
4. 修正 `scripts/check-runtime-stage.sh` 的闸门，或在 staging 中安全删除 `*.map`。
5. 更新 README 的版本、文件数、体积说明（审查时文案仍写 rc.1 / 75,799 文件 / 1.15 GB）。
6. 重新生成 `src-tauri/runtime/THIRD-PARTY-NOTICES.md`。
7. `runtime.lock` 的 Node 版本不变（24.22.2），只需随版本说明更新注释。

如果目标只是"允许用户运行 rc.2"，则不需要改代码：版本区间已放行，真实运行已验证通过；是否把 rc.2 固定为 bundled/种子版本，取决于上述 staging 修正。

## 8. 未验证项

- 仓库 `0.5.1` 的 GUI 未实际启动；实时日志来自已安装的 `0.4.15`，但两者的 core 调用契约相同。
- macOS 13.3 / Safari 16.4 老 WebKit 未实机验证；结论基于 rc.1/rc.2 的 API 与语法扫描。
- Windows、Linux 未做实机 GUI 验证。
- 未执行完整的 `make runtime-stage`（会清空并重建 `src-tauri/runtime`）；staging 结论来自等价的 npm 安装树 + 仓库现有 node/tools/template 的模拟。
- 未验证 macOS 公证/签名、嵌套 `DSH Computer Use.app` 的 Gatekeeper 行为。

## 9. 复现要点

```sh
# registry 版本
curl -s https://registry.npmjs.org/@deepseek-ai%2Fdsh | jq '."dist-tags"'

# 安装 rc.2 到临时 prefix
<node> <npm-cli> install -g --prefix /tmp/rc2 \
  --cache /tmp/npm-cache @deepseek-ai/dsh@0.2.2-rc.2

# 用壳的参数启动
DSH_HOME=/tmp/dsh-home /tmp/rc2/bin/dsh --profile web \
  --patch /tmp/force-print-url.yml --no-open --port <free>

# 鉴权围栏
curl -i http://127.0.0.1:<port>/

# token / cookie / 页面
curl -i -c /tmp/jar "http://127.0.0.1:<port>/?token=<token>"
curl -sS -b /tmp/jar -L "http://127.0.0.1:<port>/?token=<token>" | head

# 关键文件一致性
cmp <rc1>/dsh-web-app/lib/index.js <rc2>/dsh-web-app/lib/index.js
cmp <rc1>/dsh-client-connection/lib/index.js <rc2>/dsh-client-connection/lib/index.js

# staging 闸门（模拟树）
DSH_RUNTIME_MAX_FILES=100000 DSH_RUNTIME_MAX_MB=1500 \
  sh scripts/check-runtime-stage.sh <runtime-sim>
```

## 10. 执行记录（2026-11-12）

按第 7 节执行了 bundled runtime 升级：

- `Makefile`、`scripts/stage-runtime.sh`、`.github/workflows/windows-portable.yml` 两处默认版本改为 `0.2.2-rc.2`。
- staging 在 dsh 安装后删除 `dsh-prefix` 下的 `*.map`；体积闸门从 1500 MB 调到 1600 MB，文件数上限保持 100000。
- `make runtime-stage` 实测：**90,492 个文件 / 1480 MB / 1.4 GB**，校验通过；自带 dsh `0.2.2-rc.2`、Node `24.22.2`、dshmarket `1.73.19`。
- README 的版本、文件数、体积说明已同步；`runtime.lock` 注释更新；`THIRD-PARTY-NOTICES.md` 重新生成。
- 验证：`make test-scripts` 通过；Makefile / stage-runtime.sh / windows-portable.yml 的三处版本一致性检查通过；用 staging 产物复跑 `--version` 为 `0.2.2-rc.2`，401/303/200 与 41,384 字节页面正常；市场模板为 `dshmarket@1.73.19`，`/dsh-market/status` 返回 `200`。

未改：应用版本仍是 `0.5.1`；`release.yml` 中 v0.5.1 的历史发布说明未动；测试区间上界仍是 `< 0.2.2`，正式版 `0.2.2` 仍需单独验证。
