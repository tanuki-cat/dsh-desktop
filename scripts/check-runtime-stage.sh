#!/bin/sh
# staging 校验：两种实测会让发布失败/被 Gatekeeper 拦下的问题都在这里拦住。
#
# 用法: check-runtime-stage.sh <runtime 目录>
#
# 同时接受 Unix 与 Windows 的布局：
#   node  : node/bin/node        | node/node.exe（Windows 官方发行版是扁平布局）
#   npm   : node/lib/node_modules/npm | node/node_modules/npm
#   dsh   : <prefix>/lib/node_modules/... | <prefix>/node_modules/...
#   pnpm  : tools/bin/pnpm | tools/pnpm | tools/pnpm.cmd
set -eu

root=$1
[ -n "$root" ] || { echo "缺少 runtime 目录" >&2; exit 2; }

fail=0
first_of() {
  for candidate in "$@"; do
    if [ -e "$candidate" ]; then
      echo "$candidate"
      return 0
    fi
  done
  echo "  [缺]   $1" >&2
  fail=1
  echo ""
}

echo "必需内容："
node_bin=$(first_of "$root/node/bin/node" "$root/node/node.exe")
npm_cli=$(first_of "$root/node/lib/node_modules/npm/bin/npm-cli.js" "$root/node/node_modules/npm/bin/npm-cli.js")
dsh_pkg=$(first_of "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/package.json" "$root/dsh-prefix/node_modules/@deepseek-ai/dsh/package.json")
dsh_js=$(first_of "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js" "$root/dsh-prefix/node_modules/@deepseek-ai/dsh/lib/bin.js")
pnpm_bin=$(first_of "$root/tools/bin/pnpm" "$root/tools/pnpm" "$root/tools/pnpm.cmd")
template=$(first_of "$root/profile-template/package.json")
market=$(first_of "$root/profile-template/node_modules/dshmarket/package.json")
lock=$(first_of "$root/profile-template/pnpm-lock.yaml")
notices=$(first_of "$root/THIRD-PARTY-NOTICES.md")
for found in "$node_bin" "$npm_cli" "$dsh_pkg" "$dsh_js" "$pnpm_bin" "$template" "$market" "$lock" "$notices"; do
  [ -n "$found" ] && echo "  [ok]   $found"
done

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

# Windows 的 MAX_PATH 是 260：包内路径越长，解压时越容易被资源管理器拒绝（0x80010135）
longest=$(cd "$root" && find . -type f | awk '{ print length($0), $0 }' | sort -rn | head -1)
longest_len=${longest%% *}
echo "最长路径   : $longest_len 字符（Windows MAX_PATH 260；解压目录还会再占一截）"
if [ "${longest_len:-0}" -gt 240 ]; then
  echo "  [警告] 超过 240：必须解压到短路径（如 D:\\dsh）或用 7-Zip / tar 解压" >&2
elif [ "${longest_len:-0}" -gt 200 ]; then
  echo "  [提示] 超过 200：解压目录请尽量短"
fi

if [ "$fail" = "0" ]; then
  echo "自带 node : $("$node_bin" --version)"
  echo "自带 dsh  : $("$node_bin" "$dsh_js" --version 2>/dev/null || echo "未知（当前平台无法执行该平台的 node）")"
  echo "staging 校验通过"
else
  echo "staging 校验失败" >&2
  exit 1
fi
