#!/bin/sh
# staging 校验：两种实测会让发布失败/被 Gatekeeper 拦下的问题都在这里拦住。
#
# 用法: check-runtime-stage.sh <runtime 目录>
set -eu

root=$1
[ -n "$root" ] || { echo "缺少 runtime 目录" >&2; exit 2; }

fail=0
need() {
  if [ -e "$1" ]; then
    echo "  [ok]   $1"
  else
    echo "  [缺]   $1" >&2
    fail=1
  fi
}

echo "必需内容："
need "$root/node/bin/node"
need "$root/node/bin/npm"
need "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/package.json"
need "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js"
need "$root/tools/bin/pnpm"
need "$root/profile-template/package.json"
need "$root/profile-template/node_modules/dshmarket/package.json"
need "$root/profile-template/pnpm-lock.yaml"
need "$root/THIRD-PARTY-NOTICES.md"

# 悬空符号链接会让 tauri build 直接失败（实测：resource path ... does not exist）
dangling=$(find -L "$root" -type l 2>/dev/null || true)
if [ -n "$dangling" ]; then
  echo "悬空符号链接（tauri build 会失败）：" >&2
  echo "$dangling" >&2
  fail=1
else
  echo "  [ok]   无悬空符号链接"
fi

# macOS：quarantine xattr 会被原样复制进 .app，触发 Gatekeeper
if command -v xattr >/dev/null 2>&1; then
  quarantined=$(xattr -r "$root" 2>/dev/null | grep -c quarantine || true)
  if [ "$quarantined" != "0" ]; then
    echo "仍有 quarantine 属性（$quarantined 处）：先 xattr -cr $root" >&2
    fail=1
  else
    echo "  [ok]   无 quarantine 属性"
  fi
fi

if [ "$fail" = "0" ]; then
  echo "自带 node 版本: $("$root/node/bin/node" --version)"
  echo "自带 dsh 版本 : $("$root/node/bin/node" "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js" --version 2>/dev/null || echo 未知)"
  echo "staging 校验通过"
else
  echo "staging 校验失败" >&2
  exit 1
fi
