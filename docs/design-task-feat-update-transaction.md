# 更新事务与回滚（v0.4.1 审查第 10 项）

> 输入：`docs/dsh-desktop-v0.4.1-code-review.md` 的 P2-10 与「下一版本的剩余工作」第 2 项。
> 本文记录实现方案与落地结果。

## 1. 问题

更新流程是「停实例 → npm/pnpm 原地改写 → 重启」，中间没有任何可恢复点：

- npm 在**正在服务的那棵树**上原地写，写到一半失败就留下「既不是旧版本、也不是新版本」的目录；
- 插件更新同理，pnpm 原地改写 profile 的 `node_modules`；
- 装上了但**新版本起不来**（超出测试区间、依赖缺失、平台不兼容）时，用户已经没有可用的旧版本；
- 进程被强杀 / 断电发生在安装中途，下次启动面对的是同一棵坏树。

第 9 项（进程级超时）保证**不会永久挂起**，但不保证**中途失败后目录仍可用** —— 两者互补。

## 2. 方案

### 2.1 核心 CLI：暂存 → 校验 → 原子切换 → 确认

新增模块 `src-tauri/src/transaction.rs`，把一次更新拆成四步，**任何一步失败都不触碰正在使用的树**：

| 步骤 | 做什么 | 失败后果 |
| --- | --- | --- |
| 暂存 | `Staging::create()` 建空目录，npm 装进 `<staging>/prefix` | 活动树未动 |
| 校验 | `verify_install()` 检查包名、版本、入口脚本 `lib/bin.js` | 活动树未动 |
| 切换 | `commit()`：活动树 rename 到 `rollback/<version>`，暂存树 rename 到活动位置 | 旧树放回原位 |
| 确认 | CLI 打印启动 URL 后 `confirm_core_update()`：删记录、清理旧代 | 见 2.2 |

目录布局（全部在 `<app-data>/runtime/` 下，即壳自己拥有的那棵树）：

    runtime/
      prefix/                        ← 活动 CLI 树（npm 前缀）
      staging/staging-<version>/     ← 暂存；每次尝试前清空
      rollback/<version>/            ← last-known-good，保留 2 代
      profile-backup/<version>/      ← profile 快照，保留 2 代
      update-swap.json               ← 已切换但未确认的记录
      npm-cache/

`commit()` 是两次 rename：先 `prefix→rollback`，再 `staging→prefix`。中间存在一个
`prefix` 不存在的窗口（远小于一次安装），因此 `commit()` 在第二次 rename 失败时会把旧树放回，
而不是留下一个空位。跨文件系统时 `move_path()` 退化为 copy + remove（仅当用户把 `dsh_path`
指向另一块盘）。

### 2.2 切换记录：跨启动的回滚点

`update-swap.json` 在**切换之前**写入，在 CLI 打印启动 URL **之后**删除。它记录 `target`、
`backup`、`version`、`at`。

由此得到两条恢复路径：

- **启动期恢复** `recover_pending_swap()`：在 `start()` 最前面（解析运行时**之前**）执行。
  记录还在，说明上一次启动换了树却没等到 URL —— 进程被杀，或者启动超时。此时把旧树放回，
  否则这次启动会挑中同一棵起不来的树、以同样的方式失败。
- **本轮回滚** `roll_back_core_update()`：`wait_for_url()` 超时时立即回滚，错误页写明
  「新版本未能启动，已回滚到上一个版本」，用户不必再撞一次墙。

回滚把失败的树改名成 `<name>.failed` 留在旁边而不是删掉：那是唯一能解释「为什么起不来」的证据，
而下一次更新会覆盖它。

### 2.3 时序：切换推迟到真正要启动时

`stage_core_update()` 在 3b 步执行，但 `commit_core_update()` 推迟到 3d（spawn 之前）。
中间这段可能因为端口被占、外部实例不接管、版本超出测试区间等原因**根本不启动 Harness** ——
为一次不会发生的启动做切换，只会给下次启动留一条需要回滚的记录。

`StagedUpdate` 持有 `Staging` 守卫，因此提前 return 的路径会自动丢弃暂存树（`Drop`）。

**由此产生的一处配套修正**：复用分支的条件从 `!just_updated` 改为 `!just_updated && staged_update.is_none()`。
否则「已暂存但未切换」时复用了上次的实例，新树就永远停在暂存区、永远不会被启动。

### 2.4 插件市场：快照 → 安装 → 失败恢复

插件的安装方式不同：`dsh plugin add` 是 pnpm 的薄封装，**profile 的布局由 CLI 自己拥有**，
壳无法先在别处装好再切换。因此可逆性靠**安装前的快照**：

- `snapshot_profile()` 把 profile 复制到 `runtime/profile-backup/<version>/`；
- 复制**跳过** `data/` 与 `.dsh-market/`（`transaction::PROFILE_LIVE_ENTRIES`）：凭据、
  会话状态与市场日志在 Harness 运行期间一直在写，把快照里的旧值恢复回去是第二个、更糟的故障；
- 安装失败 → `restore_profile()` 把快照放回，只动快照自己有的条目，因此安装期间 Harness 写下的
  实时状态不受影响；
- 拿不到快照就**不做本次安装**：插件市场不值得一次不可逆的 profile 改写。

### 2.5 与既有更新的衔接

- `attempted` 标记（npm 装到了别处、受管 CLI 没变）仍然有效：暂存 + 校验会先发现「暂存树里
  就是请求的版本」，但切换后受管 CLI 路径不变的情况依然由启动后的回读判定，逻辑未变；
- `may_install()`（会被拒绝启动的版本不安装）仍在安装**之前**判定，暂存不会绕过它；
- `stop_instance_before_update()` 仍在最前：树被换走时运行中的 harness 下一次 lazy require 会
  `MODULE_NOT_FOUND`（实测），这一点没有改变。

## 3. 验证

### 3.1 单测（库内 116 → 134，本项占 14 项）

`transaction.rs` 11 项：暂存目录的清理与自删、按**名字与版本**双重校验（含"暂存的是别的包"、
缺入口脚本、版本不符）、切换保留旧树、切换失败放回旧树、回滚保留失败树、代际裁剪（含 `0.10.0 > 0.9.0` 的版本序与非法名优先级）、
快照跳过实时条目、恢复不动实时状态、切换记录的读写与缺失。

`lib.rs` 4 项（另删掉 1 项被 `verify_install` 取代的 `installed_cli` 用例）：`a_staged_update_swaps_the_tree_and_an_unconfirmed_one_is_rolled_back` 走完整
链路（暂存 → 校验 → 切换 → 记录 → 下次启动回滚 → 失败树留存）、`a_confirmed_boot_keeps_the_new_tree_and_clears_the_record`、
`a_profile_snapshot_makes_a_failed_plugin_install_reversible`（含「恢复不回滚实时会话状态」）、`a_stale_staging_tree_is_cleared_without_taking_the_directory_with_it`。

### 3.2 真实 npm 安装（本机实测）

用真实的 `npm install -g --prefix <staging>/prefix @deepseek-ai/dsh@0.1.5-rc.2` 验证暂存布局
与校验函数的前提：

    OK: staged entry exists
    OK: staged @deepseek-ai/dsh 0.1.5-rc.2
    298M  runtime/staging/staging-0.1.5-rc.2/prefix
    real 3m17.840s   （冷缓存）

确认 npm 在 `--prefix` 下写出的就是 `<prefix>/lib/node_modules/@deepseek-ai/dsh/lib/bin.js`，
即 `verify_install()` 与 `package_dir_in()` 假设的布局。

### 3.3 门禁

`cargo test` **134 passed / 0 failed**（另有 6 项集成用例）、`cargo fmt --check` 通过、`cargo clippy --all-targets`
0 warning。

### 3.4 未验证

- **真实的切换 + 启动失败回滚**需要 registry 上真的有新版本，本轮只做到策略级与文件系统级验证；
- **强杀发生在切换与确认之间**（`update-swap.json` 存在、进程被 SIGKILL）由 `recover_pending_swap()`
  覆盖，单测直接调用该函数，未做真实强杀演练；
- 审查文档「测试与验证缺口」里那条「更新中断后的安装目录恢复」现在**有对应实现**了，实机演练
  仍待补：安装途中 `kill -9` 壳，确认下次启动回滚到旧版本。

## 4. 影响范围

- 新增 `src-tauri/src/transaction.rs`（约 240 行 + 10 项单测）；
- `src-tauri/src/lib.rs`：`UpdatePaths`、`StagedUpdate`、`stage_core_update`、
  `commit_core_update`、`confirm_core_update`、`roll_back_core_update`、`recover_pending_swap`、
  `package_dir_in`、`snapshot_profile`、`restore_profile`、`update_market_plugin`；
- 删除 `installed_cli()`（其职责由 `verify_install()` 与切换后的路径回读取代）；
- 无新依赖、无配置项变更；新增磁盘占用上限约 3 × 290 MB（活动树 + 2 代回滚）与 2 份 profile 快照。
