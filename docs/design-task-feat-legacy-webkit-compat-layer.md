# 设计：旧 WebKit 兼容层（在原生窗口里补齐缺失的能力）

> 日期：2026-09-15 ｜ 分支 `main` ｜ 触发：issue [#1](https://github.com/tanuki-cat/dsh-desktop/issues/1)
> 前身：[`design-task-fix-webview-compat-audit.md`](./design-task-fix-webview-compat-audit.md)（2026-09-14，当时的结论是"缺能力就改用浏览器"）
> 本文件取代它 §3.2「为什么是浏览器而不是自己打补丁」与 §5 中"WebView 内的兼容补丁"条目，理由见 §1。

## 0. 结论摘要

dsh 前端在旧 WebKit 上加载失败的原因里，**只有一处是加载期的**：随包
`dsh-client-ui-sidebar-documentpreview` 内联的 pdf.js 有一行模块级补丁
`typeof Iterator.prototype.join !== "function"` —— 先读方法、后判全局，`Iterator` 不存在就
`ReferenceError`，模块求值失败 → harness 显示 `Failed to load plugins`。全包 grep 显示
`Iterator.from` 与各 helper（`map`/`filter`/`take`/`drop`/`toArray`…）**0 处使用**。

于是壳侧新增兼容层：splash 探针把「可补的 API」与「语法能力」分开上报；可补的在 harness 窗口里用
`initialization_script` 注入 ES5 补丁（harness 窗口仍然零 capability），补不了的（目前只有
`class static block` 语法）才回退默认浏览器。

**界面下限因此从 Safari 18.4（macOS 15.4）降到 Safari 16.4（macOS 13.3）。**

## 1. 为什么推翻前一轮结论

前一轮（2026-09-14）评估过打补丁并放弃。四条理由本轮逐条处置：

| 前一轮理由 | 本轮事实 |
|---|---|
| 「缺口不止一个，补丁集要跟着 dsh 版本走」 | 拆成加载期/运行期后：加载期只有 `Iterator`；运行期是 `Promise.try`/`Symbol.dispose`/`Math.sumPrecise`/`Uint8Array.fromBase64`，都能逐块补，且每块自守卫（引擎已有就跳过） |
| 「没法验证：本仓库没有旧 macOS/旧 WebKit」 | 兼容层可以在**真实 JS 引擎**里验证：先删掉这些全局再注入（`src-tauri/tests/webkit_compat_shim.rs`，随默认 `cargo test` 跑）；此外 issue 报告者愿意在 macOS 15.0.1 上验证 |
| 「PDF 相关功能仍会坏（class static block 语法、Math.sumPrecise）」 | `class static block` 是 Safari 16.4 的**语法**，18.0 本来就有，只影响 macOS ≤12 —— 那档语法探测不过，仍走浏览器；`Math.sumPrecise` 与 `Promise.try`、`fromBase64` 一并补上 |
| 「浏览器那条路用户已验证可用」 | 不变：兼容层只覆盖"能补"的部分，补不了的仍走浏览器，回退路径（`hand_the_gui_to_the_browser`）行为未变 |

## 2. 证据（随包 dsh `0.1.5-rc.2`；`src-tauri/runtime/dsh-prefix` 与系统安装两份树都核对）

| # | 事实 | 来源 |
|---|---|---|
| 1 | 全包只有 `dsh-client-ui-sidebar-documentpreview/lib/client.js` 引用全局 `Iterator`（`:3394` 未压缩、`:26138` 压缩段），且都是 `Iterator.prototype.join` 的模块级补丁 | `grep -rn "Iterator\.prototype\|Iterator\.from\|instanceof Iterator"` |
| 2 | `Iterator.from` 与 helper 方法：**0 处** | 同上 |
| 3 | 运行期用法：`Promise.try` 8 处、`Math.sumPrecise` 17 处（都在 document-preview）、`Symbol.dispose` 2 处（会话 UI）、`Uint8Array.fromBase64` 1 处 | 全包 grep |
| 4 | macOS 15.0.1 = Safari 18.0：缺 `Iterator`（18.4）、`Promise.try`/`Symbol.dispose`/`fromBase64`（18.2）、`Math.sumPrecise`（至今未发布）；而 `structuredClone`/`Object.hasOwn`/`findLast`（15.4）、`Promise.withResolvers`（17.4）、`class static block` 语法（16.4）都在 | 与 `mdn/browser-compat-data` 对照 |
| 5 | 本地实测：删掉全局 `Iterator` 后那条 guard 抛 `ReferenceError`；把 `Iterator.prototype` 指到 `%IteratorPrototype%` 后 guard 通过、`join` 可用 | node（后续固化成集成测试） |

## 3. 设计

### 3.1 探针（splash 窗口，唯一持有 core 权限的页面）

上报载荷：`{missing, degraded, syntax, agent}`

- `missing`：`SHIMMED_APIS` 里本引擎缺的 API。清单是**契约**：探针只报这些，而每一行都必须有一个兼容块
  （单测断言两边一一对应）。表达式的意义是"怎么问引擎"——原型方法不是全局，所以 `findLast` 的问法是
  `typeof [].findLast`。
- `degraded`：`WATCHED_APIS`（当前只有 `structuredClone`），只上报、不影响走向 —— 没有忠实实现。
- `syntax`：`REQUIRED_SYNTAX`。语法没法用 `typeof` 探测，用 `new Function(源码)` 编译（splash 页面
  `csp: null`，允许）；当前只有 `class static block`（Safari 16.4），它是整个兼容层的地板。
- 探针本身仍是 ES5（要在它诊断的那些老引擎上跑），单测继续断言无箭头函数、无反引号、无 `??`。

### 3.2 判定（`window.rs::WebviewReport`）

| 报告 | 判定 |
|---|---|
| `missing` 为空 | 什么都不做，正常开窗 |
| `missing` 非空、`syntax` 为空 | `supported = true`：注入兼容层后开原生窗口 |
| `syntax` 非空，或 `missing` 里有清单外的 API | `supported = false`：`lib.rs::hand_the_gui_to_the_browser` 改用默认浏览器 + 状态窗口 |
| `webkit_compat: false` | 把 `missing` 也算作缺口：行为等同旧版（缺任何能力都走浏览器） |

`needs_compat()` 与 `gaps(compat)` 是两个纯查询，判定矩阵与文案都有单测；`describe()` 的
"Safari 16.4（macOS 13.3）"也随之更新。

### 3.3 注入（`window.rs::create_harness`）

`WebviewWindowBuilder::initialization_script(compat)`：由壳注入 webview，页面自己拿不到任何回调能力，
harness 窗口的零 capability 不变。每次导航都会重放，所以每块都自守卫（`typeof X === "undefined"`），
重复注入是 no-op。

### 3.4 兼容层内容（`COMPAT_BLOCKS`，按探针结果逐块选择）

| API | 缺它的引擎 | 实现要点 |
|---|---|---|
| `Iterator` | Safari < 18.4 | 把 `Iterator.prototype` 指到引擎自己的 `%IteratorPrototype%`（`Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))`），helper 装在那个原型上 —— 页面已有的迭代器直接可用；helper 结果用 `Object.create(iteratorPrototype)`，因此链式调用与 `instanceof Iterator` 都成立；另装 `Iterator.from` 与 `join`（与 pdf.js 自己那版行为一致） |
| `Promise.try` | Safari < 18.2 | 在 executor 内同步调用回调：同步抛出变成 rejection |
| `Promise.withResolvers` | Safari < 17.4 | 手写 deferred |
| `Symbol.dispose` / `Symbol.asyncDispose` | Safari < 18.2 | 缺失时补一对 symbol；well-known symbol 无法跨 realm 共享，但页面内自洽 |
| `Math.sumPrecise` | **所有** Safari（至今未发布，也不在 node 里） | Neumaier 补偿求和；调用点全是整数长度的求和（`getSize()`、字节数、列宽），整数在 double 里精确，因此这些用法上与规格等价。**这一块在现代系统上也会注入** —— PDF 写路径本来就因为缺它而报错 |
| `Uint8Array.fromBase64` | Safari < 18.2 | `atob` + 逐字节；容忍 url-safe 字母表 |
| `Object.hasOwn` | Safari < 15.4 | `hasOwnProperty.call(Object(object), key)` |
| `findLast` / `findLastIndex` | Safari < 15.4 | 逆序扫描，`defineProperty` 非枚举 |

### 3.5 配置

`config.json` 新增 `"webkit_compat": true`（默认开）。关掉后 `missing` 也算缺口，等于回到 2026-09-14
的行为；当某个补丁本身可疑时，这是用户能立刻自救、我们能立刻定位的开关。

## 4. 约束与已知风险

1. **版本耦合没有消失，只是变窄**：dsh 若在加载期引入新 API，必须同时进 `SHIMMED_APIS` + `COMPAT_BLOCKS`
   （或在 `WATCHED_APIS` 里说明为什么不可补），否则旧引擎会重新落到 `Failed to load plugins`。
   这一点靠 §5 的单测把"清单 ↔ 兼容块"钉死。
2. **`Math.sumPrecise` 不是规格逐位等价**：规格算法未公开到可复刻的程度，采用 Neumaier 补偿求和；
   影响面是 PDF 写路径的数值结果，精度不低于原来的朴素求和。
3. **仍以 ES5 书写**：这些块要在缺 `Iterator` 的引擎上跑，单测断言不含箭头函数、反引号、`??`、
   `const`、`let`、生成器与 `class` 语法。
4. **macOS ≤12 仍走浏览器**：`class static block` 是语法，补不了；探针用编译探测把它挡住。
5. **真机验证依赖 issue 报告者**：本仓库的开发机都新于 Safari 18.4，只能靠"删掉全局再注入"模拟。

## 5. 验证

- `cargo test`（91 个单测）新增：判定矩阵（可补 / 不可补 / 开关关闭 / 降级）、`gaps` 文案
  （含 Safari 16.4 与 `webkit_compat` 说明）、兼容块与清单一一对应、只注入缺的块、兼容层 ES5 纪律、
  探针的语法编译探测。
- `src-tauri/tests/webkit_compat_shim.rs`：在 node 里先删掉清单里的 8 个 API（`Symbol.dispose` 不可删，
  测试里注明），注入后断言 —— pdf.js 那条 guard 不再抛、`Iterator.from(...).map(...).filter(...).toArray()`
  链式可用、`instanceof Iterator` 不抛、`Iterator()` 仍抛 TypeError、`Promise.try`（含同步抛）、
  `Promise.withResolvers`、`Math.sumPrecise`（`Set` 求值与补偿精度）、`Uint8Array.fromBase64`、
  `Object.hasOwn`、`findLast`/`findLastIndex` 全部通过，重复注入是 no-op。
- 同一文件里的第二个用例在 node 里跑**探针本体**：删掉 `Iterator`/`Promise.try` 后断言上报的
  `missing` 包含它们、`syntax` 为空、事件名与壳的常量一致、载荷三个列表类型正确。两个用例都没有
  `node` 时跳过（其余测试不受影响）。
- **待做**：报告者在 macOS 15.0.1 上跑一次构建，确认 ① 原生窗口加载成功、② 日志出现
  `WebView 缺少 Iterator：已注入兼容层`、③ 状态页不再出现浏览器回退说明、④ PDF 预览/导出的
  实际表现，并把结果回写本文档。

## 6. 未做与后续

- **上游那一行才是根治**：`packages/client/ui-sidebar-documentpreview` 把 guard 改成
  `"undefined" != typeof Iterator && …`；上游修好后本层自然变成空转（每块自守卫）。
- `AsyncIterator`（同样是 18.4）当前 **0 处使用**，未加入清单；将来若出现，按 §3.4 的方式补一块。
- `structuredClone` 仍只上报：忠实实现成本高，而缺它的引擎（< 15.4）本来就被语法地板挡住。
