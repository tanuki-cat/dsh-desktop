# runtime/ —— 自带运行时（staging 目录）

此目录由 `make runtime-stage` 填充，随后被 `bundle.resources` 原样打进
`DSH Desktop.app/Contents/Resources/runtime/`：

```
runtime/
├── node/          # 官方 Node 发行版（自带 npm），平台相关
└── dsh-prefix/    # dsh 安装树，按 npm 全局前缀布局：lib/node_modules/@deepseek-ai/dsh
```

本文件同时是 **glob 非空的保证**：`bundle.resources = ["runtime/**/*"]` 是相对
`src-tauri/` 解析的，若该目录没有任何文件，`tauri build` 会直接失败
（实测报错：`glob pattern runtime/**/* path not found or didn't match any files`）。

方案见 `../../docs/design-task-feat-dsh-bundled-runtime.md`。
