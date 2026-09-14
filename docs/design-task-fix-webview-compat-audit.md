# 代码审查结论：macOS WebView 下限（界面加载失败）

> 审查日期：2026-09-14 ｜ 分支 `main` ｜ 触发版本 v0.2.0（随包 dsh `0.1.5-rc.2`）
> 触发现象：Intel Mac 上用 macos-x64 产物启动后，界面停在
> `HARNESS / Failed to load plugins / failed to import loader entry … (@deepseek-ai/dsh-client-ui-sidebar-documentpreview): Can't find variable: Iterator`
> 姊妹文档：[`design-task-fix-v0-2-0-post-merge-audit.md`](./design-task-fix-v0-2-0-post-merge-audit.md)（A1–A6）、
> [`design-task-fix-desktop-shell-audit.md`](./design-task-fix-desktop-shell-audit.md)、
> [`design-task-fix-bundled-runtime-audit.md`](./design-task-fix-bundled-runtime-audit.md)

---

## 0. 结论摘要

**不是 x64 专属，也不是这一层代码的缺陷**：这个 dsh 版本的前端需要 **Safari 18.4（macOS 15.4）
或更新的 WebKit**，而我们的 `.app` 声明的最低系统是 11.0（自带运行时版）／10.15（精简版）。
旧机器上的表现是「装得上、壳能开、harness 也起来了，界面报插件错误」——比打不开更难排查。
Intel 机器更容易停在旧 macOS，所以现象看起来像「macos-x64 专属」。

| # | 级别 | 问题 | 位置 | 处理 |
|---|---|---|---|---|
| W1 | P1 | 旧 WebKit 上界面必然加载失败：pdfjs 给 `Iterator.prototype.join` 打补丁时没先判断全局 `Iterator` 是否存在，模块求值即抛 `ReferenceError` | 上游 `@deepseek-ai/dsh-client-ui-sidebar-documentpreview@0.1.5-rc.2`（内联 pdfjs-dist 6.3.289） | **上游修**（本仓库改不了，§5） |
| W2 | P2 | `minimumSystemVersion` 与实际需求脱节，且用户得不到任何解释 | `src-tauri/tauri.conf.json`、`Makefile` 的 `BUNDLED_CONFIG_JSON` | **已修**：启动前探测 + 失败页（§3） |
| W3 | P3 | 同一 bundle 的写 PDF 路径用了 `Math.sumPrecise`，而 Safari 至今没有这个 API，也没有随包补丁 | 同上 | 记录：作为「降级项」上报，不拦启动（§3.3） |

---

## 1. 证据链

| # | 证据 | 来源 |
|---|---|---|
| 1 | 出错加载项 `@deepseek-ai/dsh-client-ui-sidebar-documentpreview`，报 `Can't find variable: Iterator` | 用户截图 |
| 2 | 该包 `lib/client.js`（6.9 MB，内联 pdfjs）里的**模块级**补丁：`makeObj=()=>Object.create(null),makeSet=()=>new Set;"function"!=typeof Iterator.prototype.join&&(Iterator.prototype.join=function(e){return[...this].join(e)});` —— guard 判的是**方法**，不是**全局** | `src-tauri/runtime/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/node_modules/@deepseek-ai/dsh-client-ui-sidebar-documentpreview/lib/client.js` |
| 3 | 全部 client bundle 里只有这一个带这行补丁（其余 13 个 `client.js` 均为 0 处） | 对 `Iterator.prototype.join` 的全包 grep |
| 4 | BCD：`Iterator.Iterator` = **Safari 18.4**；`map/filter/take/toArray/from/every/some/…` 同为 18.4；`Iterator.join` 至今仍是 `preview`（未正式发布） | `mdn/browser-compat-data` → `javascript/builtins/Iterator.json` |
| 5 | WebKit 官方发布说明：Safari 18.4 "…new JavaScript features like **Iterators**…" | <https://webkit.org/blog/16574/webkit-features-in-safari-18-4/> |
| 6 | Safari 18.4 的到达途径：macOS 15.4 系统更新，或 Sonoma/Ventura 的 Safari 18.4 更新；**macOS 12 及更早拿不到** | Apple 支持页《Safari 18.4 のセキュリティコンテンツについて》及多个二手来源 |
| 7 | 我们声明的下限：精简版 `"minimumSystemVersion": "10.15"`；自带运行时版由 `BUNDLED_CONFIG_JSON` 覆盖为 `11.0`（理由是随包 node 的 `minos 11.0`） | `src-tauri/tauri.conf.json:26`、`Makefile` 的 `BUNDLED_CONFIG_JSON` |
| 8 | 写 PDF 路径调用 `Math.sumPrecise`（多处），而 BCD 中该特性在 Safari 下没有任何 `version_added` | 同 2 的文件 + `javascript/builtins/Math/sumPrecise.json` |

## 2. 影响面（按 WebKit 版本，不按架构）

| 系统 | 结果 |
|---|---|
| macOS ≤ 12（Monterey 及更早） | **必失败**：拿不到 Safari 18.4 |
| macOS 13/14/15.0–15.3 且未装 Safari 18.4 更新 | **失败**（很常见，用户不一定会升级 Safari） |
| macOS 15.4+，或已装 Safari 18.4 的 13/14 | 可用（W3 的写 PDF 路径除外） |

架构只是**相关**而非因果：能停在旧 macOS 的机器基本都是 Intel，所以看上去像 x64 专属；
把 arm64 包装在 macOS 14 上，症状完全一样。

## 3. 修复（W2）：启动前判定 + 明确的失败页

### 3.1 判定通道

- **探针注入**：`window.rs::probe_script()` 由 Rust 生成（ES5，因为要在它诊断的那些旧引擎上跑），
  在 `create_splash` 时作为 `initialization_script` 注入。splash 是我们自己的页面，也是**唯一**持有
  core 权限的窗口（`src-tauri/capabilities/splash.json`），harness 窗口仍然一个权限都不给。
- **上报**：`typeof <API> === "undefined"` 逐项检查，把 `missing`／`degraded`／`navigator.userAgent`
  通过 `plugin:event|emit`（`core:event:default` 含 `allow-emit`）发给壳；`window.__TAURI_INTERNALS__`
  可能晚于探针脚本出现，所以上报在 1 秒内重试、之后安静放弃。
- **存储与等待**：`window.rs::record_report` 解析并存进 `static REPORT`；`unsupported_webview()` 最多等
  `PROBE_WAIT`（500 ms）。**从未上报按「支持」处理**（fail-open）：诊断本身出问题不该把人锁在门外。
- **判定点**：`lib.rs::refuse_an_old_webview(app, version)` 在两处 `create_harness` 之前调用——
  复用已有实例那条快路径、以及正常 spawn 之后。命中就 `window::show_failure(...)` + 一条日志，
  不再打开 harness 窗口。

### 3.2 文案

失败页写三件事：缺什么（如 `Iterator`）、这个 dsh 版本需要什么（Safari 18.4／macOS 15.4）、以及
当前 WebView 的 UA（便于排查）。这正是被替换掉的那句 `Failed to load plugins` 完全没有的。

### 3.3 判定清单

| 类别 | API | 行为 |
|---|---|---|
| 必需 | `Iterator` | 缺失即拒绝打开界面（现场唯一被证实会让加载期崩掉的） |
| 降级 | `Promise.withResolvers`、`Math.sumPrecise`、`structuredClone` | 只上报、只写进失败页/日志，不拦启动 |

两个清单都写在 `window.rs` 顶部（`REQUIRED_APIS`／`OPTIONAL_APIS`），加一行就能扩展。

## 4. 验证

- `make test` → **82 passed**（77 + 5）：上报解析与 `describe` 文案（含「必须提到 Safari 18.4」）、
  降级项不拦启动、坏 payload 不清掉已有结论、判定函数读到的就是最近一次上报、探针脚本覆盖 `Iterator`
  且保持 ES5；
- **探针脚本真跑过**：把 Rust 生成的脚本抽出来，在 Node 里模拟三种引擎——删掉 `Iterator`（旧 WebKit）→
  上报 `missing:["Iterator"]`；现代引擎 → `missing:[]`；`__TAURI_INTERNALS__` 晚到 → 重试后仍上报；
- `make fmt-check` 通过、`make clippy` 0 warning。

## 5. 未做

- **上游修（W1）**：本仓库无法修复随包的第三方 bundle。建议在 deepseek-harness 的
  `packages/client/ui-sidebar-documentpreview` 里把那行改成先判全局：
  `"undefined" != typeof Iterator && "function" != typeof Iterator.prototype.join && (…)`，
  并给 `Math.sumPrecise` 之类的写路径加兜底；或者把 client bundle 的构建目标对齐桌面壳声明的最低 WebKit。
- **真机复现失败页**：需要在旧 macOS 上跑一次壳（或把 `Iterator` 从环境里拿掉）确认失败页确实出现；
  本轮只在 Node 里验证了探针脚本与在单测里验证了判定链。
- **W3 的实际影响**：PDF 保存/编辑路径没有实机复现。
