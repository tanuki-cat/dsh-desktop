# dsh-desktop 对 dsh 0.2.2-rc.5 的兼容性审查

审查对象：`@deepseek-ai/dsh@0.2.2-rc.5`（npm dist-tag `latest` / `next`）  
审查仓库：`dsh-desktop`  
审查日期：2026-11-25  
审查环境：macOS（arm64）、随包 Node `24.22.2`、系统 WebView

本文只记录审查结论，结论基于本机实测；未修改壳的运行时契约代码。

## 1. 总体结论

**兼容。** rc.5 是 rc.3 之后的小版本，壳依赖的服务端契约未变；插件市场从 `dshmarket@1.73.19` 升到 **`1.84.1`**（npm 最新），在 dsh 0.2.2-rc.5 下实测正常加载。自带运行时 staging 因市场大版本升级而增大，但仍在闸门内。

- 版本线：`latest = 0.2.2-rc.5`、`next = 0.2.2-rc.5`、`alpha = 0.2.3-alpha.2`。
- 壳的已测试区间是 `>= 0.1.5-rc.1, < 0.2.2`，rc.5 在区间内，`update.rs` 无需改动。
- 插件市场 `dshmarket@1.84.1` 的 peer 依赖 `@deepseek-ai/dsh-settings`（`^0.2.1-rc.1` 等）由随包 `dsh-settings@0.2.2-rc.5` 满足（dsh 的 `evaluatePluginCompatibility` 用 `includePrerelease: true`）；新增的 `@deepseek-ai/cordis ^4.0.1` / `@deepseek-ai/schemastery ^3.18.1` 分别由随包 `cordis ~4.1.0` / `schemastery ~3.19.0` 满足。

## 2. 逐项结论

| 检查项 | 结论 | 证据 |
|---|---|---|
| `dsh web` 启动 | 通过 | rc.5 真实起服务，stdout 打印 token URL |
| 命令行参数顺序 | 通过 | `--profile web --no-open --port N` 实测 |
| 无 token 的 `GET /` | 通过 | `401` 围栏 |
| token URL / cookie / 页面 | 通过 | `303` + `set-cookie`；带 cookie 返回 `200`，`41,120` 字节 |
| 插件市场加载 | 通过 | profile 置于 `profiles/web` 后，`/dsh-market/status` 自报 `version: "1.84.1"`、`installed: {dshmarket: 1.84.1}` |
| 市场能力接口 | 通过 | `/dsh-market/api/v1/capabilities`：check / update / progress / rollback / restart 全部 `true` |
| 插件 peer 依赖 | 通过 | 随包树含 `dsh-settings@0.2.2-rc.5` / `cordis ~4.1.0` / `schemastery ~3.19.0`，满足 1.84.1 的 peer 范围 |
| 自带运行时 staging | 通过 | `19,033` 个文件 / 592 MB（文件字节）/ 593 MB（du 块占用），闸门 12,000–40,000 文件 / 700 MB |

## 3. 规模变化

rc.5 相对 rc.3 的增量主要来自**插件市场大版本升级**（`1.73.19 → 1.84.1`，跨 11 个 minor）与 dsh 核心 `rc.3 → rc.5`：

- 整树从 rc.3 的 `18,881` 文件 / 537 MB 变为 `19,033` 文件 / 592 MB（**+152 文件 / +55 MB 文件字节**），du 块占用 590 → 593 MB。
- 最长路径仍是 `224` 字符（`dsh-experimental-cua-macos` 里的 `DSH Computer Use.app`），未变。
- 闸门（12,000–40,000 文件 / 700 MB）未变，仍有充足余量。

## 4. 本次改动

- `Makefile` 的 `DSH_VERSION`、`scripts/stage-runtime.sh` 的 `dsh_version` 默认值、`.github/workflows/windows-portable.yml` 两处 `dsh_version` 默认值：`0.2.2-rc.3` → `0.2.2-rc.5`。
- 插件市场 `DSHMARKET_VERSION`：`1.73.19` → `1.84.1`（`Makefile` / `scripts/stage-runtime.sh` / `windows-portable.yml` 两处）。
- Node（`24.22.2`）、pnpm（`12.3.4`）不变；`runtime.lock` 只改注释。
- `scripts/check-runtime-stage.sh` 补一条 rc.5 实测记录注释；`README.md` 与发版说明同步体积、文件数与最长路径数字。

## 5. 验证记录（2026-11-25，macOS arm64）

| 验证 | 结果 |
|---|---|
| `make test` | 319 passed / 1 ignored |
| `make test-scripts` | 通过 |
| `make runtime-stage` | 全绿：19,033 文件 / 592 MB（du 593 MB）；`dsh --version` = `0.2.2-rc.5`；市场模板 `dshmarket@1.84.1` |
| 全新 DSH_HOME（profile 置于 `profiles/web`）起 `dsh web` | 401 围栏 → 303 → 200；页面 41,120 字节；`/dsh-market/status` 自报 1.84.1；capabilities 全可用；日志无 error |
| release preflight 版本一致性 | `NODE_VERSION` / `DSH_VERSION` / `DSHMARKET_VERSION` 三处一致 |

## 6. 未验证项

- 本轮未做契约文件逐字节比对（rc.3 的 staging 树已被 rc.5 覆盖），改以真实启动 + 市场加载 + peer 依赖在树内验证。
- 未做 GUI 实机启动（本轮只验证到 `dsh web` 的 HTTP 契约与市场接口）。
- 插件市场 1.84.1 的更新 / 回滚 / 重启等**写操作**未实测（只验证了 status 与 capabilities 读接口）。
- macOS 13.3 / Safari 16.4 老 WebKit、Linux 与 Windows 未实机验证。
- 正式版 `0.2.2` 仍在测试区间之外（`update.rs` 上界 `< 0.2.2`），发布后需单独审查。

## 7. 复现要点

```sh
# registry 版本
curl -s https://registry.npmjs.org/@deepseek-ai%2Fdsh | jq '."dist-tags"'
curl -s https://registry.npmjs.org/dshmarket | jq '."dist-tags"'

# staging + 冒烟（profile 必须放在 DSH_HOME/profiles/web，不是 DSH_HOME 根）
make runtime-stage
export DSH_HOME=$(mktemp -d) && mkdir -p "$DSH_HOME/profiles/web"
cp -R src-tauri/runtime/profile-template/. "$DSH_HOME/profiles/web/"
src-tauri/runtime/node/bin/node src-tauri/runtime/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --no-open --port 3999
# 然后带 cookie curl /dsh-market/status 与 /dsh-market/api/v1/capabilities
```
