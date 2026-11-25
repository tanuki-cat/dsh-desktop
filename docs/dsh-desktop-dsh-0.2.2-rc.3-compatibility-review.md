# dsh-desktop 对 dsh 0.2.2-rc.3 的兼容性审查

审查对象：`@deepseek-ai/dsh@0.2.2-rc.3`（npm dist-tag `next`）  
审查仓库：`dsh-desktop`  
审查日期：2026-11-13  
审查环境：macOS 27.2.1（arm64）、随包 Node `24.22.2`、系统 WebView

本文只记录审查结论，结论基于本机实测；未修改壳的运行时契约代码。

## 1. 总体结论

**兼容。** rc.3 是 rc.2 之后的小版本：壳依赖的全部服务端契约文件与 rc.2 逐字节相同，
自带运行时 staging 的规模、闸门与最长路径都不变，插件市场仍是 `dshmarket@1.73.19`（npm 最新）。

- 版本线：`latest = 0.2.2-rc.1`、`next = 0.2.2-rc.3`、`alpha = 0.2.2-alpha.3`。
- rc.3 发布内容相对 rc.2 只增加了一个依赖包（`@deepseek-ai/dsh-deepseek-chat-import`），
  其余 `@deepseek-ai/*` 依赖从精确 `0.2.2-rc.2` 改为精确 `0.2.2-rc.3`；`engines.node` 仍是 `>= 24.9.0`。
- 壳的已测试区间是 `>= 0.1.5-rc.1, < 0.2.2`，rc.3 在区间内，`update.rs` 无需改动。

## 2. 逐项结论

| 检查项 | 结论 | 证据 |
|---|---|---|
| `dsh/lib/bin.js` | 通过 | 与 rc.2 逐字节相同（`cmp`） |
| `dsh-web-app/lib/index.js`、`lib/startup.js` | 通过 | 与 rc.2 逐字节相同 |
| `dsh-web-app/cordis.patch.yml` 的 `web-runtime` 行 | 通过 | 与 rc.2 逐字节相同；壳写的 `openBrowser` / `printUrl` / `surfaceContext` 仍能合并 |
| `dsh-client-connection/lib/index.js`（401 围栏、token/cookie） | 通过 | 与 rc.2 逐字节相同 |
| `dsh-cmdline`、`dsh-host-webserver`、`dsh-host-frontend-static` | 通过 | 与 rc.2 逐字节相同 |
| 命令行参数顺序 | 通过 | `--profile web --patch <overlay> --no-open --port N` 实测起服务 |
| 无 token 的 `GET /` | 通过 | `401` + `dsh web authentication required; reopen the URL printed by dsh web.` |
| token URL / cookie / 页面 | 通过 | `303` + `set-cookie`；带 cookie 返回 `200`，`41,384` 字节，`<title>DeepSeek Harness</title>` |
| 插件市场 | 通过 | `dshmarket@1.73.19`（模板播种）；`/dsh-market/status` 自报 `version: "1.73.19"`，`/dsh-market/api/v1/capabilities` 返回 `check/update/rollback/restart` 全部可用 |
| WebView 兼容层 | 通过 | 客户端 bundle 未引入兼容层之外的 API；`Error.isError` / `RegExp.escape` / `Uint8Array.fromHex` 等只出现在 inspector 的 DevTools 载荷与 source map 中，与 rc.2 相同 |
| 自带运行时 staging | 通过 | `18,881` 个文件 / 537 MB（文件字节）/ 590 MB（du 块占用），闸门 12,000–40,000 文件 / 700 MB |

## 3. 规模变化

rc.3 把客户端 bundle 解压后的体积从 5.7 MB 抬到 21 MB（`dsh-client-ui-*` 普遍变大，
`dsh-client-ui-sidebar-documentpreview` 13.8 → 25.9 MB），同时新增 `dsh-deepseek-chat-import`；
这些增量在 P1+P2 裁剪（类型/源码/source map/pdb/外平台 prebuild/pnpm 重复/zod 去重）之后
基本相互抵消：整树从 rc.2 的 18,859 文件 / 537 MB 变为 18,881 文件 / 537 MB，最长路径仍是
224 字符（`dsh-experimental-cua-macos` 里的 `DSH Computer Use.app`）。逐文件 deflate 后约
200 MB。

## 4. 本次改动

- `Makefile` 的 `DSH_VERSION`、`scripts/stage-runtime.sh` 的 `dsh_version` 默认值、
  `.github/workflows/windows-portable.yml` 两处 `dsh_version` 默认值：`0.2.2-rc.2` → `0.2.2-rc.3`。
- Node（`24.22.2`）、pnpm（`12.3.4`）、dshmarket（`1.73.19`）不变；`runtime.lock` 只改注释。
- `scripts/check-runtime-stage.sh` 补一条 rc.3 实测记录注释；`README.md` 与发版说明同步体积、
  文件数与最长路径数字。

## 5. 验证记录（2026-11-13，macOS arm64）

| 验证 | 结果 |
|---|---|
| `make test-scripts` | 通过 |
| `make test` | 319 passed / 1 ignored |
| `make runtime-stage` | 全绿：18,881 文件 / 590 MB；`dsh --version` = `0.2.2-rc.3` |
| `sh scripts/stage-runtime.sh <tmp>` | 全绿，18,880 文件 / 590 MB（比上一行少 1 个文件：临时目录没有 `src-tauri/runtime/README.md` marker） |
| 全新 DSH_HOME + 模板播种起 `dsh web` | 401 围栏 → 303 → 200；页面 41,384 字节；市场自报 1.73.19；日志无 error |
| release preflight 版本一致性 | `NODE_VERSION` / `DSH_VERSION` / `DSHMARKET_VERSION` 三处一致 |

## 6. 未验证项

- 未做 GUI 实机启动（本轮只验证到 `dsh web` 的 HTTP 契约与市场接口）。
- macOS 13.3 / Safari 16.4 老 WebKit、Linux 与 Windows 未实机验证。
- 正式版 `0.2.2` 仍在测试区间之外（`update.rs` 上界 `< 0.2.2`），发布后需单独审查。

## 7. 复现要点

```sh
# registry 版本
curl -s https://registry.npmjs.org/@deepseek-ai%2Fdsh | jq '."dist-tags"'

# 契约文件逐字节比对（rc.2 来自已 stage 的 runtime，rc.3 来自临时安装）
cmp <rc2>/dsh-web-app/lib/index.js <rc3>/dsh-web-app/lib/index.js
cmp <rc2>/dsh-client-connection/lib/index.js <rc3>/dsh-client-connection/lib/index.js

# staging + 冒烟
make runtime-stage
<runtime>/node/bin/node <runtime>/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --version
```
