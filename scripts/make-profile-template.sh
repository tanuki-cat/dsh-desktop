#!/bin/sh
# 生成 profile 模板：一个只含插件市场 dshmarket 的 dsh profile。
#
# 用法: make-profile-template.sh <目标目录> <dshmarket 版本> <工具 bin 目录>
#
# 为什么需要它（方案 §2.5）：dsh 在全新 DSH_HOME 上生成的 profile 是空的，没有任何安装入口，
# 所以发行版必须自带插件市场。模板必须由**真 pnpm** 生成 —— 手抄的 node_modules 缺 pnpm 元数据，
# pnpm 认不出来，后续 `dsh plugin add` 可能重建整棵树。
set -eu

dest=$1
market_version=$2
tools_bin=${3:-}

[ -n "$dest" ] || { echo "缺少目标目录" >&2; exit 2; }
[ -n "$market_version" ] || { echo "缺少 dshmarket 版本" >&2; exit 2; }

# 用自带的 pnpm（若已 stage），否则退回系统 pnpm
if [ -n "$tools_bin" ] && [ -x "$tools_bin/pnpm" ]; then
  PATH="$tools_bin:$PATH"
  export PATH
fi
command -v pnpm >/dev/null 2>&1 || { echo "需要 pnpm（或先 stage tools）" >&2; exit 3; }

rm -rf "$dest"
mkdir -p "$dest"

cat > "$dest/package.json" <<JSON
{
  "name": "dsh-profile-web",
  "private": true,
  "dependencies": {
    "dshmarket": "$market_version"
  },
  "dsh": {
    "profile": {
      "bundles": [
        "@deepseek-ai/dsh-base",
        "@deepseek-ai/dsh-web-app",
        "dshmarket"
      ],
      "patchReload": "live"
    }
  }
}
JSON

# 与 dsh 自己生成的 profile 保持一致
cat > "$dest/pnpm-workspace.yaml" <<YAML
packages:
  - .

nodeLinker: hoisted
autoInstallPeers: false
YAML

# store 放构建缓存：CI 可复用，也不碰用户自己的 pnpm store
(cd "$dest" && pnpm install --store-dir "${PNPM_STORE_DIR:-.pnpm-store}" --reporter=append-only)

[ -f "$dest/node_modules/dshmarket/package.json" ] || { echo "模板里没有 dshmarket" >&2; exit 4; }
[ -f "$dest/pnpm-lock.yaml" ] || { echo "模板缺 pnpm-lock.yaml" >&2; exit 4; }
echo "profile 模板就绪: $dest (dshmarket $market_version)"
