# runtime/ —— 自带运行时（staging 目录）

此目录由 `feat/bundled-runtime` 分支的 `make runtime-stage` 填充，随后被该分支的 `bundle.resources`
原样打进 `DSH Desktop.app/Contents/Resources/runtime/`：

```
runtime/
├── node/          # 官方 Node 发行版（自带 npm），平台相关
└── dsh-prefix/    # dsh 安装树，按 npm 全局前缀布局：lib/node_modules/@deepseek-ai/dsh
```

在该分支上，本文件同时是 **glob 非空的保证**：`bundle.resources = ["runtime/**/*"]` 是相对
`src-tauri/` 解析的，若该目录没有任何文件，`tauri build` 会直接失败
（实测报错：`glob pattern runtime/**/* path not found or didn't match any files`）。

> **`main` 上没有这条配置**（2026-09-13 移除）：main 既没有 staging 目标，也没有读取运行时种子的
> 代码，留着它只会把别的分支残留在本目录里的 payload（实测 520 MB）静默打进 `.app`。
> 所以本目录在 main 上不参与打包；保留 README 只是让合并分支时目录仍在、`.gitignore` 规则继续有效。

方案见 `../../docs/design-task-feat-dsh-bundled-runtime.md`。
