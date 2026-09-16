# dsh-desktop v0.4.1 代码审查问题汇总

## 审查范围

- 仓库：<https://github.com/tanuki-cat/dsh-desktop>
- 分支：`main`
- 版本：`v0.4.1`
- 提交：`afa13c7902d9318ff6575378de4f5ff08fbed486`
- 审查日期：2026-09-15

总体而言，项目已经是一个完成度较高的 Tauri 桌面宿主，包含运行时定位与捆绑、Harness 进程监管、自动更新、WebView 隔离、异常恢复以及 Windows 便携构建。当前最需要优先处理的是端口识别可能误杀其他进程的问题，其次是默认自动更新和外部实例接管策略过于激进。

---

## 处理状态（2026-09-15）

本文是**审查结论 + 修复任务清单**。修复落地后把结果回写到本文，方案文档只在结论被推翻时才另开 supersede 文档。

**v0.4.1 审查轮已修 7 项**（1、2、5、8、9、11、14），**部分完成 3 项**（6、7、13），**未做 5 项**（3、4、10、12、15）。
逐项说明见各条末尾的「处理」段；未做项写明了原因与前置条件，部分完成项写明了缺哪一部分。

**后续轮次已补做 2 项**（2026-09-16）：第 1 项第 5 条建议 / 第 14 项的**接管前交互确认**（原「未做」），
与第 10 项的**更新事务与回滚**（原「未做」）。两者各有独立设计文档：
[`design-task-feat-takeover-confirmation.md`](./design-task-feat-takeover-confirmation.md)、
[`design-task-feat-update-transaction.md`](./design-task-feat-update-transaction.md)。
当前库内单测 **142 passed / 0 failed**（v0.4.1 轮为 116），集成用例 5 → 6 项，`cargo fmt --check` 通过，
`cargo clippy --all-targets` 0 warning。

> 后续：`44a347e` 补端口探针回归测试后为 **143**；最新一轮审查（
> [`dsh-desktop-latest-code-review.md`](./dsh-desktop-latest-code-review.md)）修复外部输出的内存边界后为 **154**。

独立验证：`cargo test` **116 passed / 0 failed**（单测；另有 5 个集成用例在真实 node 引擎里跑。原 102）、
`cargo fmt --check` 通过、`cargo clippy` 在 host 与 `x86_64-pc-windows-gnu` 两个目标上均 0 warning。

除单元测试外，本轮还跑了构建产物的实机冒烟：`make bundle` 后启动 `.app`，核对版本号、CSP 内联脚本哈希
注入（4/4 匹配）、启动日志与退出清理。**P2-5 的兼容层竞态就是这样发现的** —— 单元测试覆盖不到，
因为它依赖两个线程的真实时序。

P0-1 做了实机双向验证：普通返回 `200` 的本地服务被判为 `Other`（旧代码判为 Harness）；
3080 上真实的 Harness 仍被正确识别。

| # | 级别 | 问题 | 状态 |
|---|---|---|---|
| 1 | P0 | 可能误杀占用 3080 端口的其他程序 | **已修** |
| 2 | P1 | 自动更新默认策略过于激进 | **已修** |
| 3 | P1 | 桌面壳自身缺少安全更新机制 | **未做**（需签名基础设施） |
| 4 | P1 | 发布产物尚未签名和公证 | **未做**（需开发者证书） |
| 5 | P2 | CSP 被完全关闭 | **已修**（范围见该条） |
| 6 | P2 | 登录 shell 环境导入可能产生副作用 | **部分完成** |
| 7 | P2 | 日志脱敏覆盖范围有限 | **部分完成** |
| 8 | P2 | Windows 没有显式收紧敏感文件 ACL | **已修** |
| 9 | P2 | npm/pnpm 子进程缺少整体超时 | **已修** |
| 10 | P2 | 更新过程缺少完整事务与回滚 | **已修** |
| 11 | P3 | macOS 最低版本说明不统一 | **已修**（文档） |
| 12 | P3 | WebKit 兼容层维护成本较高 | **未做**（需上游配合） |
| 13 | P3 | 绘制看护自动重载可能丢失页面状态 | **部分完成** |
| 14 | P3 | 外部 Harness 接管行为过于突然 | **已修**（改为默认不接管） |
| 15 | P3 | Windows 发布形态仍不完整 | **未做**（需签名与安装器工作） |

## P0：发布阻断问题

### 1. 可能误杀占用 3080 端口的其他程序

`harness::probe()` 当前会把以下响应认定为 Harness：

- 带特定正文的 `HTTP 401`；
- 任意 `HTTP 200`。

默认配置同时为：

```json
{
  "port": 3080,
  "take_over_existing": true
}
```

因此，普通 Vite、Node、Java 或其他本地服务只要监听 `127.0.0.1:3080` 并返回 200，就可能被误认为外部 Harness，随后监听进程可能被终止。

代码注释声称认证探针已经证明目标是 Harness，但“任意 HTTP 200”并不能提供这种证明。

建议：

- 不再将任意 `HTTP 200` 直接识别为 Harness；
- 检查 Harness 专属接口、响应头或稳定页面标记；
- 同时核对监听 PID 的命令行是否包含 `dsh`、`--profile web` 等特征；
- 将 `take_over_existing` 的默认值改为 `false`；
- 接管前向用户展示进程、端口和 workspace 信息并要求确认。

**处理（已修，2026-09-15）**

根因比本条描述的更明确：dsh 的认证栅栏是**无条件**的 —— `BrowserAuth::writeUnauthorized` 对所有
不带 launch-token cookie 的请求一律回 `401`，而探测从不带 cookie。因此 **`200` 只可能来自非 Harness**，
那个分支是纯粹的误报通道，已整个删除（`Probe` 从 4 个变体收敛到 3 个）。

- 第 1、2 条建议：不再识别 `200`；判据收敛为唯一的认证栅栏（`harness.rs::probe`）。
- 第 3 条建议：新增 `harness::process_command()` 读取监听进程命令行，`looks_like_dsh_web()` 判定身份。
  该规则已应用到**全部 4 条 kill 路径**（启动接管、交接接管、重试接管、更新前停止）—— 原实现只有自愈
  分支做了命令行校验。两道信号必须同时成立才发信号。
- 第 4 条建议：`take_over_existing` 默认改为 `false`。
- 第 5 条建议：**已补做（2026-09-16）**。接管前弹面板让用户当场选，且没有引入 `tauri-plugin-dialog`：
  询问面板画在 splash 页面上（那个窗口本来就持有 `core:default`），复用「重新启动 Harness」按钮同一条
  core 事件桥，新增事件 `dsh-desktop:takeover-choice`。面板写明对方 pid、**完整命令行**、端口，以及
  **接管后会改用本应用配置的 workspace**（不会继续对方的工作目录）。选项：接管 / 保留并用浏览器打开 /
  **保留并让本应用换端口** / 什么都不做退出。120 秒无答复才回落到 `take_over_existing` 的语义。

  四条会对外部进程发信号的路径全部接上（启动检测、交接后接管、自启失败重试接管、更新前停止实例）；
  身份校验是唯一的闸门 —— 未识别的进程连问题都不会问。

  > **同日修正（第一版把询问也交给了配置项）**：第一版在 `take_over_existing: false` 时直接返回
  > `UseBrowser`，只有 `true` 才询问。测试与文档都按这个行为写了，但**默认用户因此永远看不到面板** ——
  > 选择权被前置成了一道配置题，与第 14 项「让用户选择」的要求相反。实机确认了后果：默认配置下启动，
  > 直接以浏览器形式打开并停在错误页。现在**识别出来就问**，配置项只决定 120 秒无答复时怎么办；
  > 更新前的停止判据（`may_stop_before_update`）有同一处错误，已一并改掉。
  >
  > 同时修掉一个由此暴露的循环：浏览器回退页原本走 `show_failure`，带着一个**必然失败**的重启按钮 ——
  > 外部实例还活着、本壳又不接管它，所以每次点击都重跑一遍注定失败的启动、再开一个浏览器标签页。
  > 该情形改用 `show_notice`（无按钮）。用户日志里的 6 次 `restart requested from the status page`
  > 就是这个循环。

  详见 [`design-task-feat-takeover-confirmation.md`](./design-task-feat-takeover-confirmation.md)。

**实机验证**：普通返回 `200` 的服务 → `Other`；3080 上真实 Harness → 仍识别为 `Harness`。

## P1：高优先级问题

### 2. 自动更新默认策略过于激进

当前默认配置相当于：

```json
{
  "auto_update": true,
  "auto_update_plugins": true,
  "update_tags": ["latest", "next"],
  "system_updates": "install",
  "require_tested_dsh": false
}
```

由此带来的问题：

- 自动从 `latest` 和预发布渠道 `next` 中选取更高版本；
- 可以原地修改用户自己安装的全局 dsh；
- 自动改写 web profile、`package.json`、锁文件和插件依赖树；
- 新版本超出桌面壳测试区间时，默认仍继续运行；
- 增大 npm 供应链变更对桌面端的直接影响。

建议采用更保守的默认值：

```json
{
  "auto_update": true,
  "auto_update_plugins": false,
  "update_tags": ["latest"],
  "system_updates": "notify",
  "require_tested_dsh": true
}
```

应用自带的 shadow runtime 可以自动更新，但用户的系统级安装应默认只通知。

**处理（已修，2026-09-15）**

默认值已按建议收敛：`update_tags` 只留 `latest`、`auto_update_plugins` 关闭、`system_updates` 改
`notify`、`require_tested_dsh` 开启（`auto_update` 保持 `true`，自带/影子运行时仍自动更新自己的树）。

**实现中发现本条未预见的冲突**：`auto_update: true` 与 `require_tested_dsh: true` 同时开启会**自锁** ——
壳自动装上 `latest`，随后拒绝启动它，而用户原本可用的版本已被覆盖。新增 `update::may_install()`，让安装与
启动受同一个测试区间约束：会被拒绝启动的版本不会被装上。这一条超出了本条建议的字面范围，但不加就会把
「更保守的默认值」变成「启动不了」。

### 3. 桌面壳自身缺少安全更新机制

目前可以自动更新 dsh 核心和 `dshmarket`，但 Tauri 桌面应用自身没有完整的：

- 签名更新清单；
- 更新签名验证；
- 自动升级流程；
- 失败回滚机制。

一旦桌面壳自身出现漏洞，用户只能手动下载和替换。

**处理（未做，2026-09-15）**

本轮未实现。`Cargo.toml` 中没有 `tauri-plugin-updater`，全仓无签名清单与公钥，与审查结论一致。
实现前提是第 4 项（签名基础设施）：没有可信的发布者身份，自动升级只会把「手动替换」换成「自动拉取
一个无法验证的来源」。建议与签名/公证同批处理。

### 4. 发布产物尚未签名和公证

尚未完成：

- macOS Developer ID 签名；
- macOS notarization；
- Windows Authenticode 签名。

这会导致 Gatekeeper 或 SmartScreen 警告，也使用户难以验证二进制发布者，不适合直接面向普通用户正式分发。

**处理（未做，2026-09-15）**

本轮未做，原因是需要**开发者证书**（Apple Developer ID + Windows 代码签名证书），属于外部资源而非代码
改动：两个 workflow 与 `Makefile` 中 `codesign`/`notarytool`/`signtool` 零命中，`README.md` 亦自述未做。
拿到证书后需要在 `release.yml` 注入签名步骤，并注意自带运行时的 node 必须带
`--preserve-metadata=entitlements`（否则丢失 JIT entitlement，node 启动即 `Trace/BPT trap: 5`）。

## P2：中优先级问题

### 5. CSP 被完全关闭

当前配置为：

```json
{
  "security": {
    "csp": null
  }
}
```

Harness 页面没有 Tauri capability，已经显著降低风险；但本地 splash 页面拥有 `core:default`，仍建议为其配置严格 CSP，作为纵深防御。

**处理（已修，2026-09-15）**

`tauri.conf.json` 的 `app.security.csp` 已配置为不含 `unsafe-inline` / `unsafe-eval` 的严格策略
（`default-src 'self'`、`script-src 'self'`、`object-src 'none'`、`frame-ancestors 'none'` 等），
Tauri 会为页面内联脚本自动补 sha256、为样式补 nonce。

**本条的作用范围需要修正**（经查阅 vendored 的 `tauri-2.11.5` / `wry-0.55.1` 源码确认）：
`app.security.csp` **只对经 `tauri://` 资产协议提供的本地页面生效**。harness 窗口加载的是
`http://127.0.0.1:<port>/`，该请求不经过 Tauri 的协议处理器，因此**加不上 CSP** —— 那一边的隔离仍然
只靠零 capability + 导航围栏。`tauri-runtime` 全 crate 无 CSP 字段，Tauri v2 也没有 per-window CSP。
所以这一项是 splash 页面的纵深防御，不能理解为「两个窗口都有了 CSP」。

**实现中修掉一个由 CSP 引入的回归**：原有 WebView 能力探测用 `new Function` 编译语法片段，而
`script-src` 恰好禁止 eval —— 被拦下的 eval 与「引擎太老、解析不了」抛的是同一类错误，结果是**每台机器
都会被判为需要回退浏览器**。已改为由 splash 页面内的 `<script>` 元素判定（解析失败则该元素被丢弃，
页面其余部分照常运行），并加测试把 Rust 常量与 HTML 绑定。

**实机冒烟又暴露一个既有竞态**（本轮一并修掉）：`needed_compat_script()` 直接读上报槽、不等待，而
splash 页面的探测是在 CLI 启动期间异步到达的 —— 于是「上报还没到」被当成了「没有要补的能力」，
窗口在需要兼容层的机器上**静默地不打补丁**，日志还把原因写成 `webkit_compat=false`（配置里根本没设
过这一项），把排查引向一个无关的设置。修法：新增 `wait_for()` 作为可测试的等待原语，
`needed_compat_script()` 与 `unsupported_webview()` 共用它；`compat` 改到上报确定之后再算（原先在
spawn 之前算），状态行不再展示这个当时还无法确定的猜测，日志也区分「配置关闭」与「探测未到达」。
发现方式是跑构建产物看日志，单元测试没覆盖到——因为它依赖两个线程的真实时序。

### 6. 登录 shell 环境导入可能产生副作用

默认开启 `import_shell_env`，程序会调用登录 shell 的 `-lic env` 或 `-lc env`，从而执行 `.zprofile`、`.zshrc` 等初始化脚本。

如果 shell 配置包含联网、文件修改、后台进程或耗时操作，GUI 启动就可能变慢或产生意外副作用。8 秒超时只能限制等待时间，无法撤销已经发生的操作。

建议：

- 在 README 和首次启动提示中明确说明；
- 提供无需执行登录 shell 的环境变量配置方式；
- 考虑默认只导入 PATH 和明确允许的密钥变量。

**处理（部分完成，2026-09-15）**

- 第 1 条「README 说明」：**已做**。README 现写明 `import_shell_env` 会执行 `.zprofile`/`.zshrc`，
  若其中有联网、改文件、拉起后台进程或耗时操作，每次启动应用都会连带发生；并说明 8 秒超时只能让壳不再
  等它、**不能撤销已发生的副作用**。（「首次启动提示」未做，见下。）
- 第 2 条「无需执行登录 shell 的配置方式」：**已做**（原本就存在）。`config.json` 的 `env` 字段可显式
  给出变量、优先级最高，README 已把它写成 `import_shell_env: false` 的替代方案；另一条等价路径是在
  Web GUI 的 Models 页面填 API Key（写入 `~/.dsh` 凭证服务，与 shell 环境无关）。
- 第 3 条「默认只导入 PATH 与白名单密钥变量」：**未做**。当前仍是全量导入（`shellenv::parse_env` 只排除
  `HOME`/`TMPDIR`/`SHELL`/`SHLVL`/`PWD`/`_`/`DSH_*` 等保留名）。收紧为白名单会改变默认行为并可能让
  现有用户的 provider 凭据失效，属于需要产品决策的变更。
- 「首次启动提示」：**未做**。仓库没有首启提示机制（无 onboarding/首次运行标记）。

### 7. 日志脱敏覆盖范围有限

当前主要过滤 `token=`，但以下内容仍可能由 dsh 或插件写入 stdout/stderr 并进入日志：

- `Authorization` / Bearer token；
- API Key；
- Cookie / Set-Cookie；
- provider 请求错误中的敏感信息；
- 用户路径、workspace 路径等隐私信息。

建议增加常见敏感字段过滤，并提供关闭完整 Harness 日志或切换为精简日志的配置。

**处理（部分完成，2026-09-15）**

前半句「增加常见敏感字段过滤」**已做**：`harness::redact()` 从只处理 `token=` 扩展为按字段名过滤，
覆盖 `token`、`api_key`/`apikey`/`api-key`、`authorization`（含 `Bearer <凭据>`）、`cookie`/`set-cookie`、
`password`/`passwd`、`secret`、`private_key`、`access_key`、`session_id`。字段名大小写不敏感，可出现在
更长标识符尾部（`DEEPSEEK_API_KEY`）。所有进入日志的内容都经此过滤，含 provider 回显请求头的情形。

后半句「提供关闭完整日志或切换为精简日志的配置」：**未做**。当前没有日志级别配置项。

「用户路径、workspace 路径等隐私信息」：**有意未脱敏**。壳自己写入的 `spawn:` 行含 CLI 路径与 workspace，
这是排障时定位「壳真正监管的是哪棵树」的关键信息（README 已把该行作为排查入口）。这是一个取舍而非遗漏，
如需最小留存请清理 `<app-data>/logs/`。README 已明确写出不脱敏的范围。

### 8. Windows 没有显式收紧敏感文件 ACL

Unix 上会将 `config.json` 和 `state.json` 权限设为 `0600`；Windows 下的 `restrict()` 为空实现，只依赖用户目录原有的 NTFS ACL。

如果 `config.json.env` 保存 API Key，应显式创建仅当前用户可读的 ACL，或者避免在配置文件中保存明文密钥。

**处理（已修，2026-09-15）**

`process::restrict()` 的 Windows 分支不再是空实现：用 `icacls <file> /inheritance:r /grant:r <account>:F`
去掉继承并只授权当前用户（`USERDOMAIN\USERNAME`，缺失时退化为 `USERNAME`）。**读不到账户时不改 ACL** ——
只做 `/inheritance:r` 会剥掉所有条目、可能让文件不可访问，比保持现状更糟。与 Unix 分支一样是 best-effort，
失败不影响启动。`icacls` 是每个受支持 Windows 都自带的组件，因此没有为此引入 Win32 安全依赖。

**未做实机验证**：本机是 macOS，只做了 `cargo clippy --target x86_64-pc-windows-gnu` 交叉类型检查，
ACL 的实际效果需要在 Windows 上确认（见文末「测试与验证缺口」）。

### 9. npm/pnpm 子进程缺少整体超时

registry 请求虽然设置了 `--fetch-timeout=8000` 和 `--fetch-retries=1`，但 `Command::output()` 本身没有进程级超时。

npm 仍可能因为代理、DNS、锁、生命周期脚本或子进程异常而长时间挂起。以下操作均应设置整体超时并在超时后清理进程树：

- `npm view`；
- `npm install`；
- `dsh plugin add`。

**处理（已修，2026-09-15）**

新增 `update::run_with_timeout()`，三处调用全部改用它（`fetch_dist_tags` 用 30 s 的 `QUERY_TIMEOUT`，
`install` 与 `install_plugin` 用 300 s 的 `INSTALL_TIMEOUT`）。实现要点：

- 子进程放入自己的进程组（Windows 用 `CREATE_NEW_PROCESS_GROUP`），超时后 `process::kill_tree()` 连树一起
  清理 —— 只杀父进程会留下握着 registry 连接的 node 子进程；
- stdout/stderr 由独立线程排空：子进程写满管道缓冲区会阻塞，否则超时会打在一个「只是在等被读」的进程上；
- 排空结果经 channel 且有 2 s 上限（`DRAIN_GRACE`）。若孙进程在 kill 后仍握着管道，`join()` 会把刚砍掉的
  无限等待重新引入一遍；输出只用于解释失败，不值得再等；
- `install` 同时补上了 `--fetch-timeout` / `--fetch-retries`（此前只有 `npm view` 带这两个参数）。

### 10. 更新过程缺少完整事务与回滚

更新流程会先停止 Harness，再让 npm/pnpm 原地修改安装目录或 profile，但没有完整的：

- 临时目录安装；
- 安装结果完整性验证；
- 原子目录切换；
- last-known-good；
- 更新失败后恢复旧依赖树。

npm/pnpm 中途失败时，原目录可能已经处于部分修改状态。

**处理（已修，2026-09-16）**

五项要求全部落地，新增模块 `src-tauri/src/transaction.rs` 承载可测试的文件系统原语：

- **临时目录安装**：`npm install -g --prefix <app-data>/runtime/staging/staging-<版本>/prefix`。npm 不再
  写进正在服务的那棵树；暂存目录每次尝试前清空，上一次留下的半棵树不会被当成这一次的成果；
- **安装结果完整性验证**：`verify_install()` 检查包名确为 `@deepseek-ai/dsh`、版本确为请求的那个、
  入口脚本 `lib/bin.js` 存在。三条都在**切换之前**判定，因此 registry 给了别的版本 / 下载被截断 /
  npm 忽略了 `--prefix` 都变成一条日志，而不是一棵坏掉的树；
- **原子目录切换**：`commit()` 是两次 `rename`（活动树 → `runtime/rollback/<版本>`，暂存树 → 活动位置）。
  第二次失败时把旧树放回，而不是留下一个空位；
- **last-known-good**：`runtime/rollback/` 保留 2 代，成功确认后自动裁剪；
- **失败后恢复旧依赖树**：切换**之前**写 `runtime/update-swap.json`，CLI 打印启动 URL **之后**才删除。
  由此有两条恢复路径 —— 本轮启动等 URL 超时立即回滚（错误页写明「已回滚到上一个版本」），
  进程在切换与确认之间被强杀则下次启动先 `recover_pending_swap()` 再解析运行时。失败的那棵树改名
  为 `<name>.failed` 留在旁边作为排查证据。

**实现中发现本条未预见的一处**：切换必须**推迟**到确定要启动的那一刻。中间还可能因为端口被占、
外部实例不接管、版本超出测试区间而根本不启动 Harness —— 为一次不会发生的启动做切换，只会给下次
启动留一条需要回滚的记录。配套地，复用分支的判据从 `!just_updated` 改成 `!just_updated &&
staged_update.is_none()`：否则「已暂存但未切换」时会复用上次实例，新树永远停在暂存区。

插件市场（`auto_update_plugins`）的安装方式不同 —— `dsh plugin add` 是 pnpm 的薄封装，profile 布局由
CLI 自己拥有，无法先在别处装好再切换 —— 因此可逆性靠**安装前的 profile 快照**
（`runtime/profile-backup/<版本>/`，保留 2 份），失败即恢复。快照**跳过** `data/` 与 `.dsh-market/`：
凭据、会话状态与市场日志在 Harness 运行期间一直在写，恢复旧值回去是第二个、更糟的故障。拿不到快照
就不做本次安装。

与第 9 项的关系：进程级超时（已修）保证**不会永久挂起**，但不保证**中途失败后目录仍可用** —— 两者互补，
不能互相替代。

验证：`cargo test` **142 passed / 0 failed**（库内单测 116 → 142：新增 `transaction.rs` 11 项、更新事务相关
`lib.rs` 4 项、接管确认 `lib.rs` 9 项与 `window.rs` 3 项，并删掉 1 项被取代的 `installed_cli` 用例；
集成用例 5 → 6 项）；
另用真实的 `npm install -g --prefix` 装 `@deepseek-ai/dsh@0.1.5-rc.2` 确认了暂存布局与校验函数的前提
（`OK: staged @deepseek-ai/dsh 0.1.5-rc.2`，298 MB，冷缓存 3m18s）。**未做**：真实切换 + 启动失败回滚的
实机演练需要 registry 上真的有新版本。详见
[`design-task-feat-update-transaction.md`](./design-task-feat-update-transaction.md)。

## P3：兼容性与用户体验问题

### 11. macOS 最低版本说明不统一

项目存在多个不同下限：

| 能力 | 当前下限或声明 |
| --- | --- |
| 精简壳安装 | macOS 10.15 |
| 捆绑 Node 运行时 | macOS 11 |
| 原生 WebView 较完整兼容 | 约 macOS 13.3 / Safari 16.4 |

旧系统虽然可以回退到浏览器，但“支持 macOS 10.15”容易让用户误以为所有功能都能在原生窗口中工作。建议分别说明应用启动下限、捆绑运行时下限和原生 WebView 下限。

**处理（已修，2026-09-15）**

README 顶部新增一张表，把三个下限分开说明，并标注各自由谁决定：

| 说的是什么 | 下限 | 由谁决定 |
| --- | --- | --- |
| 精简壳能装、能启动 | macOS 10.15 | `tauri.conf.json` 的 `minimumSystemVersion` |
| 自带运行时版能装、能启动 | macOS 11.0 | 随包 node 22 的 `minos`，由 `make bundle-bundled` 用 `--config` 覆盖 |
| 界面能用原生窗口 | Safari 16.4（macOS 13.3） | 壳打开界面前的运行时能力探测 + 兼容层 |

并写明低于第三个下限不是「不能用」：壳会改用系统浏览器渲染界面，自己继续监管 harness。

### 12. WebKit 兼容层维护成本较高

项目自行补充 Iterator helpers、`Promise.try`、`Promise.withResolvers`、`Math.sumPrecise`、`Uint8Array.fromBase64`、`Object.hasOwn`、`findLast` 和 `Symbol.dispose` 等能力。

兼容层能解决当前问题，但依赖上游前端和 pdf.js 的实现细节。后续插件升级仍可能引入无法 polyfill 的新语法，或出现 polyfill 与标准行为不完全一致的问题。

长期方案应是推动上游前端正确转译或降低构建目标，而不是持续扩大桌面壳的兼容脚本。

**处理（未做，2026-09-15）**

这是长期方向而非本轮可关闭的缺陷，未做改动。本轮与该层的唯一交集是：CSP 的 `script-src` 会禁止
`new Function`，因此把语法探测从 eval 改为页面内 `<script>` 元素判定（见第 5 项），兼容层本身未动。
上游修复（pdf.js 那行 guard 加全局判断）仍是从根上消除该层的唯一途径，README 与兼容层设计文档均已写明。

### 13. 绘制看护自动重载可能丢失页面状态

连续检测到页面停止绘制或无响应后，程序会重载当前 token URL，最多三次。这样能恢复 WebView 卡死，但可能丢失：

- 尚未提交的输入；
- 页面临时选择状态；
- 未保存的插件表单；
- 正在上传或下载的操作；
- 仅存在于前端内存中的状态。

建议在重载前显示短暂提示，并在模型仍在生成或用户正在输入时延迟处理。

**处理（部分完成，2026-09-15）**

**「用户正在输入时延迟处理」已做**：新增 `ACTIVITY_PROBE`（ES5、只读），在看护判定要重载前询问页面
是否正被使用 —— 焦点在 `input`/`textarea`/`contenteditable`，或最近一个探测间隔（15 s）内有
按键/粘贴/指针事件。命中则本轮改为 `LivenessAction::Busy`：标题显示「等待输入结束…」，日志记一条，
**推迟一轮**。重载预算不受影响（既不清零也不消耗），输入停下后照常重载。

**「无响应」不走这条**：能回答「我在输入」的代码正是已经停掉的那部分，所以 `PageState::Silent` 一律按
需要恢复处理，不等待 —— 否则一个真正卡死的页面会永远等下去。

**「模型仍在生成」未做**：活动探测只读输入事件，不感知流式生成状态。要判定它需要让页面报告生成中
标志（Harness 前端是否有稳定的可读标志尚未核实）。当前实现覆盖了本条列举的「尚未提交的输入」与
「未保存的插件表单」两类损失。

**「重载前显示短暂提示」未做**：当前只在窗口标题上体现状态，没有重载前的可见提示（重载本身无法取消，
提示只能起告知作用）。

### 14. 外部 Harness 接管行为过于突然

即便确认目标确实是 Harness，默认直接终止外部实例仍可能：

- 中断终端会话；
- 终止正在执行的 agent 任务；
- 断开浏览器中的现有会话；
- 用不同 workspace 重启实例。

建议检测到外部实例后让用户选择：使用浏览器打开、终止并接管或更换桌面端口。

**处理（已修，2026-09-15）**

本条与第 1 项同源，一并处理：`take_over_existing` 默认改为 `false`，因此**默认行为就是建议里的
「使用浏览器打开」** —— 壳不再终止外部实例，改为在系统浏览器中打开它并说明原因，提示里同时给出另外
两个选项（设 `take_over_existing: true` 接管，或换端口）。

选择接管时也加了第二道身份校验：只有 401 认证栅栏 + 命令行确为 `dsh web` 同时成立才发信号，
因此不会出现「确认目标不是 Harness 却照样杀」的情况。

**已补做（2026-09-16）**：真正的交互式选择已实现，覆盖本条建议的全部三个选项 ——「用浏览器打开」、
「终止并接管」、以及本条额外提到的**「更换桌面端口」**（在配置端口之上取第一个空闲端口，只对本次启动
生效），另加「什么都不做退出」。画在 splash 页面上的面板里，没有引入对话框依赖 —— 该窗口本就持有
`core:default`，复用重启按钮那条事件桥即可。

`take_over_existing` 的语义因此从「是否接管」收敛为「**没答复时**是否接管」：`true` 不再等于静默接管，
`false` 也不再等于不问。**这后半句是第一版漏掉的** —— 第一版按配置项决定要不要问，默认用户看不到面板，
与「检测到外部实例后让用户选择」相反；同日已修正。详见
[`design-task-feat-takeover-confirmation.md`](./design-task-feat-takeover-confirmation.md)。

### 15. Windows 发布形态仍不完整

当前 Windows 重点是便携压缩包和自解压包，仍存在：

- 没有 MSI/MSIX 或正式安装器；
- 没有开始菜单与标准卸载管理；
- 没有代码签名；
- 完整运行时体积较大；
- 深层 Node 依赖树可能触发路径过长问题；
- 依赖 WebView2；
- Windows 实机覆盖少于 macOS。

**处理（未做，2026-09-15）**

本轮未做，除代码签名外均需 Windows 环境与打包工作：MSI/MSIX 或正式安装器、开始菜单与卸载登记属于
发布形态变更（`tauri.conf.json` 的 `bundle.targets` 目前是 `app`，Windows 走便携 zip + 7z sfx）。
本轮在 Windows 侧只改了两处，且只做了交叉类型检查（`cargo clippy --target x86_64-pc-windows-gnu`）：
`process::restrict()` 的 ACL（第 8 项）与 `kill_tree` 的进程树终止（第 9 项）。两者都需要实机确认。

## 测试与验证缺口

仓库包含较多单元测试和 CI 门禁，但以下场景仍需要实机或集成验证：

- 普通 HTTP 服务占用 3080 时不会被误杀；
- Harness 意外退出后的完整自动恢复；
- WebView 停帧看护的真实触发；
- 附件上传和下载；
- `window.open` 和外部导航；
- macOS TCC 权限归因；
- 旧 macOS WebKit 兼容；
- 更新中断后的安装目录恢复；
- Windows 的关闭窗口、退出应用和强制结束进程路径；
- Windows 子进程树是否始终被完整清理。

本次审查环境没有安装 Cargo，因此未重新执行仓库声明的 Rust 测试；已确认测试源码和 CI 测试门禁存在。

**本轮补充（2026-09-15）**

审查环境已具备 Cargo 1.97.0，测试已复跑：`cargo test` **116 passed / 0 failed**
（审查时的基线为 102 passed）。新增 14 项单测覆盖本轮修复，其中四项在编写、冒烟或 CI 中**抓出了真实缺陷**：
脱敏对 `api_key: value` 这类「分隔符后带空格」的写法会漏掉值；`looks_like_dsh_web` 会把
`dsh plugin --profile web add …` 误判为服务进程；P2-5 记的兼容层竞态（`needed_compat_script`
不等待上报，导致需要兼容层的机器静默不打补丁）；以及下面这条只在 Windows 暴露的测试缺陷。

**v0.4.2 首次发布在 Windows 上失败（2026-09-15，已修）**：`build windows-x64` 的离线测试门禁挂在
`the_auth_fence_identifies_a_harness`（`left: Other, right: Harness`），macOS 与 Linux 全绿。
根因是**测试辅助函数**而非被测代码：`serve_once()` 写入响应后直接关闭 socket，但从不读取请求。
接收缓冲区仍有未读数据时关闭会发 RST 而不是 FIN，**Windows 见到 RST 会丢弃对端已收到的数据**，
于是探测读到空 body 判为 `Other`；macOS/Linux 会先交付已收到的字节，所以本地永远绿。
实测确认：旧写法 30 次里 29 次触发 `ECONNRESET`，新写法 0 次。

更值得记的是**同一缺陷让另一个测试假通过**：`an_unrelated_http_server_is_not_a_harness` 期望 `Other`，
而空 body 恰好也是 `Other` —— 它在 Windows 上通过，理由却是错的。修法：`serve_once()` 读完请求再
优雅关闭，并返回一个「已排空」标志供断言。该断言**故意不依赖探测结果**，因为 macOS 上 RST 仍会交付
body、平台相关的现象抓不住回归；标志在任何平台都会失败（已用负向验证确认：移除排空逻辑后本地即挂）。

逐条状态（本轮）：

- **普通 HTTP 服务占用 3080 时不会被误杀** —— 已补单测（`an_unrelated_http_server_is_not_a_harness`）
  并做实机双向验证：普通 200 服务 → `Other`，3080 上真实 Harness → `Harness`。
- Harness 意外退出后的完整自动恢复、WebView 停帧看护的真实触发、附件上传和下载、`window.open` 与外部导航、
  macOS TCC 权限归因、旧 macOS WebKit 兼容、更新中断后的安装目录恢复、Windows 关闭/退出/强杀路径与
  子进程树清理 —— **仍需实机**，本机为 macOS，无法覆盖这些场景。
- 「更新中断后的安装目录恢复」**已有对应实现**（第 10 项，2026-09-16）：切换记录 + 启动期回滚 +
  本轮超时回滚 + profile 快照恢复，单测覆盖文件系统层面的全部路径。**实机演练仍待补**：安装途中
  `kill -9` 壳，确认下次启动回滚到旧版本。

## 推荐修复顺序

1. 修复“任意 HTTP 200 被当作 Harness”的识别逻辑。
2. 将 `take_over_existing` 默认改为 `false`。
3. 默认只跟踪 `latest`，系统安装默认只通知，不自动改写。
4. 为 npm/pnpm 操作增加进程级超时。
5. 实现更新事务、完整性验证和 last-known-good 回滚。
6. 完成 macOS 与 Windows 代码签名。
7. 为 splash 页面增加严格 CSP。
8. 扩展日志脱敏，并完善 Windows 敏感文件保护。
9. 补齐外部端口、崩溃恢复、停帧和 Windows 实机测试。

其中第 1～3 项应作为下一版本发布前的优先修复项。

**执行结果（2026-09-15）**：第 1、2、3、4、7、8 项已完成；第 5、6 项经确认作为独立任务单列
（分别需要成规模的特性工作与外部开发者证书）；第 9 项部分完成 —— 外部端口误杀已由单测与实机双向验证
覆盖，其余场景需要实机环境。

**下一版本的剩余工作**（按建议优先级）：

1. ~~**接管前的交互确认**（第 1 项第 5 条建议、第 14 项）~~ —— **已完成（2026-09-16）**：
   [`design-task-feat-takeover-confirmation.md`](./design-task-feat-takeover-confirmation.md)。
   没有引入对话框依赖：面板画在 splash 页面（本就持有 `core:default`）上，复用重启按钮那条 core 事件桥。
   ⚠️ 第一版按 `take_over_existing` 决定**要不要问**，默认用户看不到面板；同日修正为「识别出来就问」，
   配置项只决定 120 秒无答复时怎么办。
2. ~~**更新事务与回滚**（第 10 项）~~ —— **已完成（2026-09-16）**：
   [`design-task-feat-update-transaction.md`](./design-task-feat-update-transaction.md)。
3. **代码签名与公证**（第 4 项）—— 其余分发相关项（第 3、15 项）的前置条件。
4. **实机验证**（第 9 项剩余部分）—— 需 Windows 与旧 macOS 环境。
5. 较小的可配置性缺口：`import_shell_env` 白名单模式（第 6 项）、日志级别配置（第 7 项）、
   重载前可见提示与「模型生成中」判定（第 13 项）。

**本轮（2026-09-16）新引入的实机验证项** —— 两者都有单测覆盖文件系统/决策层面，但触发条件需要真实环境：

- **接管面板**：需要一个真实的外部 `dsh web` 占住配置端口才能弹出来。步骤见设计文档 §4；
- **更新回滚**：需要 registry 上真的有新版本，或安装途中 `kill -9` 壳。步骤见设计文档 §3.4。
