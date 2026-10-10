# Repository Guidelines

## 项目结构

本项目是 Tauri 2 桌面壳，负责启动与监管 `dsh web`，通过系统 WebView 承载 Harness UI；不是 Harness 前端源码仓库。
- `src/index.html`：本地启动与状态页面。
- `src-tauri/src/`：Rust 核心；进程监管在 `harness`、`process`，更新在 `update`、`update_flow`、`transaction`，窗口与兼容层在 `window`。
- 模块子目录中的 `tests.rs` 放单元测试；`src-tauri/tests/` 放集成测试。
- `scripts/`：运行时 staging、校验与许可证脚本；`docs/`：设计文档；`.github/workflows/`：发布流程。
- 不手改 `src-tauri/gen/`、构建产物或缓存；随包运行时载荷由 staging 流程生成。

## 构建与开发

Rust 使用 edition 2021，声明最低版本 1.77；包管理器为 `pnpm@12.3.4`。以下命令均在仓库根目录执行：
- `make check`：检查所有 Rust targets。
- `make dev`：运行 debug 版；`pnpm dev`：Tauri 开发入口。
- `make build`：编译 release 可执行文件；`pnpm build`：Tauri 打包。
- `make bundle`：平台安装包，依赖步骤会安装 Node 开发依赖；执行前确认环境。Makefile 主要覆盖 macOS、Linux。

## 代码风格

沿用现有模块边界，Rust 标识符与注释使用英文，用户文案和说明文档以简体中文为主。使用 `make fmt` 格式化、`make fmt-check` 只检查；`make clippy` 对所有 targets 将 warnings 视为错误。平台差异通过现有 `cfg` 分支处理，不假设 GUI 进程继承终端 PATH。

## 测试

优先按改动范围执行 `cargo test --manifest-path src-tauri/Cargo.toml <测试过滤词>`。`make test` 包含脚本自检与默认 Rust 测试；脚本改动可用 `make test-scripts`。WebKit 兼容层集成测试需要 PATH 中有 Node，否则跳过。`make test-live` 启用 `DSH_DESKTOP_LIVE_TESTS=1` 并联网，仅在需要时显式执行。涉及文件与子进程的测试沿用隔离临时目录，避免污染用户配置。只报告实际执行的验证，不能把配置命令当作通过证据。

## 提交约定

近期提交采用 `fix(release): ...`、`fix(startup): ...`、`chore(release): ...` 等 Conventional Commits，说明通常为中文。使用匹配变更的类型与 scope；提交前检查 diff，排除无关产物与敏感信息。

## 安全

不记录或提交 launch token、Cookie、API key 等凭据；保留日志脱敏。维护 Harness 窗口零 capability、当前 authority 导航限制及外链 http/https 白名单。本地资产 CSP 不覆盖远程 Harness 页面，不以放宽 CSP 替代修复。进程清理必须验证归属，更新与下载不得静默覆盖未知文件。
