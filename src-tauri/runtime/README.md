# runtime/ —— 自带运行时（staging 目录）

此目录由 `make runtime-stage` 填充，随后被 `bundle.resources` 原样打进
`DSH Desktop.app/Contents/Resources/runtime/`：

```
runtime/
├── node/          # 官方 Node 发行版（自带 npm），平台相关
└── dsh-prefix/    # dsh 安装树，按 npm 全局前缀布局：lib/node_modules/@deepseek-ai/dsh
```

本文件同时是 **glob 非空的保证**：`make bundle-bundled` 用 `--config` 注入
`bundle.resources = ["runtime/**/*"]`（相对 `src-tauri/` 解析），若该目录没有任何文件，
`tauri build` 会直接失败
（实测报错：`glob pattern runtime/**/* path not found or didn't match any files`）。

> **2026-09-13 合并后**：`feat/bundled-runtime` 已并入 `main`。基座 `tauri.conf.json` 不再常开这条 glob，
> 所以普通 `make bundle` 不会把 payload 打进包；只有 `make bundle-bundled` 才会，因此本文件必须保留。
> `make runtime-stage` 在组装前会清空本目录（只留本文件），不会把别的平台的残留静默打包。

方案见 `../../docs/design-task-feat-dsh-bundled-runtime.md`。
