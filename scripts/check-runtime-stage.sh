#!/bin/sh
# staging 校验：实测会让发布失败 / 被 Gatekeeper 拦下 / 把宿主内容带进包的问题都在这里拦住。
#
# 用法: check-runtime-stage.sh <runtime 目录>
#       check-runtime-stage.sh --self-test    用假目录自检本脚本的闸门（staging 之前跑）
#
# 同时接受 Unix 与 Windows 的布局：
#   node  : node/bin/node        | node/node.exe（Windows 官方发行版是扁平布局）
#   npm   : node/lib/node_modules/npm | node/node_modules/npm
#   dsh   : <prefix>/lib/node_modules/... | <prefix>/node_modules/...
#   pnpm  : tools/bin/pnpm | tools/pnpm | tools/pnpm.cmd
#
# 闸门阈值可用环境变量覆盖（Makefile 在 macOS/Linux 上传更紧的区间）：
#   DSH_RUNTIME_MIN_FILES / DSH_RUNTIME_MAX_FILES / DSH_RUNTIME_MAX_MB
set -eu

# 2026-09-13 实测（macOS arm64 的干净 staging）：31,069 个文件 / 520 MB（du 块占用）。
# 干净 staging 约 3 万文件：少一棵树（仅 dsh-prefix 就 2.5 万）或混进别的平台的 payload
# 都会明显偏出，而"往 staging 里多放两万个文件"正是方案 §5（v3.1 第 4 条）实测过的漏网场景。
min_files=${DSH_RUNTIME_MIN_FILES:-20000}
max_files=${DSH_RUNTIME_MAX_FILES:-55000}
max_mb=${DSH_RUNTIME_MAX_MB:-700}

if [ "${1:-}" = "--self-test" ]; then
  # 两条实测过的失效路径各造一个反例，避免闸门再被无声地绕过（review P0-1 / P1-9）。
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  loose="DSH_RUNTIME_MIN_FILES=0 DSH_RUNTIME_MAX_FILES=99999999 DSH_RUNTIME_MAX_MB=99999"

  mkdir -p "$tmp/missing/node/bin"
  printf '#!/bin/sh\necho v0.0.0\n' > "$tmp/missing/node/bin/node"
  chmod +x "$tmp/missing/node/bin/node"
  status=0
  env $loose sh "$0" "$tmp/missing" >/dev/null 2>&1 || status=$?
  if [ "$status" = "0" ]; then
    echo "自检失败：缺少必需内容的目录没有让校验失败" >&2
    exit 1
  fi

  full="$tmp/full"
  mkdir -p "$full/node/bin" "$full/node/lib/node_modules/npm/bin" \
    "$full/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib" \
    "$full/tools/bin" "$full/profile-template/node_modules/dshmarket"
  printf '#!/bin/sh\necho v0.0.0\n' > "$full/node/bin/node"
  chmod +x "$full/node/bin/node"
  : > "$full/node/lib/node_modules/npm/bin/npm-cli.js"
  : > "$full/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/package.json"
  printf '#!/bin/sh\n' > "$full/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js"
  : > "$full/tools/bin/pnpm"
  : > "$full/profile-template/package.json"
  : > "$full/profile-template/node_modules/dshmarket/package.json"
  : > "$full/profile-template/pnpm-lock.yaml"
  : > "$full/THIRD-PARTY-NOTICES.md"

  ln -s /usr/bin/true "$full/node/bin/absolute-link"
  status=0
  env $loose sh "$0" "$full" >/dev/null 2>&1 || status=$?
  if [ "$status" = "0" ]; then
    echo "自检失败：绝对符号链接没有让校验失败" >&2
    exit 1
  fi

  rm -f "$full/node/bin/absolute-link"
  env $loose sh "$0" "$full" >/dev/null 2>&1 || {
    echo "自检失败：干净的假目录没有通过校验" >&2
    exit 1
  }
  echo "check-runtime-stage.sh 自检通过"
  exit 0
fi

root=$1
[ -n "$root" ] || { echo "缺少 runtime 目录" >&2; exit 2; }

fail=0
missing=""
first_of_result=""
# 结果通过全局变量返回：用 $(first_of ...) 的命令替换会开子 shell，函数里的 fail=1 会在
# 子 shell 结束时消失，于是"缺必需内容"永远不会让脚本失败（review P0-1）。
first_of() {
  first_of_result=""
  for candidate in "$@"; do
    if [ -e "$candidate" ]; then
      first_of_result=$candidate
      return 0
    fi
  done
  echo "  [缺]   $1" >&2
  missing="$missing $1"
  return 0
}

echo "必需内容："
first_of "$root/node/bin/node" "$root/node/node.exe"; node_bin=$first_of_result
first_of "$root/node/lib/node_modules/npm/bin/npm-cli.js" "$root/node/node_modules/npm/bin/npm-cli.js"; npm_cli=$first_of_result
first_of "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/package.json" "$root/dsh-prefix/node_modules/@deepseek-ai/dsh/package.json"; dsh_pkg=$first_of_result
first_of "$root/dsh-prefix/lib/node_modules/@deepseek-ai/dsh/lib/bin.js" "$root/dsh-prefix/node_modules/@deepseek-ai/dsh/lib/bin.js"; dsh_js=$first_of_result
first_of "$root/tools/bin/pnpm" "$root/tools/pnpm" "$root/tools/pnpm.cmd"; pnpm_bin=$first_of_result
first_of "$root/profile-template/package.json"; template=$first_of_result
first_of "$root/profile-template/node_modules/dshmarket/package.json"; market=$first_of_result
first_of "$root/profile-template/pnpm-lock.yaml"; lock=$first_of_result
first_of "$root/THIRD-PARTY-NOTICES.md"; notices=$first_of_result
for found in "$node_bin" "$npm_cli" "$dsh_pkg" "$dsh_js" "$pnpm_bin" "$template" "$market" "$lock" "$notices"; do
  [ -n "$found" ] && echo "  [ok]   $found"
done
[ -z "$missing" ] || fail=1

# 悬空符号链接会让 tauri build 直接失败（实测：resource path ... does not exist）
dangling=$(find -L "$root" -type l 2>/dev/null || true)
if [ -n "$dangling" ]; then
  echo "悬空符号链接（tauri build 会失败）：" >&2
  echo "$dangling" >&2
  fail=1
else
  echo "  [ok]   无悬空符号链接"
fi

# 绝对符号链接会被"解引用"复制，把宿主机的文件原样带进包（实测：宿主 node 的 67 KB 启动器，
# 哈希一致）；相对链接（node_modules/.bin/*）是正常布局，放行。
# 这里用 if 而不是 case：macOS 自带的 bash 3.2 解析不了 $( ) 里的 case（$(case …) 直接报语法错误）。
absolute_links=$(find "$root" -type l 2>/dev/null | while IFS= read -r link; do
  target=$(readlink "$link" 2>/dev/null || true)
  if [ "${target#/}" != "$target" ]; then
    printf '%s -> %s\n' "$link" "$target"
  elif printf '%s' "$target" | grep -qE '^[A-Za-z]:'; then
    printf '%s -> %s\n' "$link" "$target"
  fi
done)
if [ -n "$absolute_links" ]; then
  echo "绝对符号链接（会把宿主文件复制进包）：" >&2
  echo "$absolute_links" >&2
  fail=1
else
  echo "  [ok]   无绝对符号链接"
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

# 文件数与体积闸门：staging 残留会被 bundle.resources 静默打进包（实测 20,000 个文件 / 78 MB
# 也不会报错），所以这里必须自己兜住。
files=$(find "$root" -type f 2>/dev/null | wc -l | tr -d ' ')
mb=$(du -sk "$root" 2>/dev/null | awk '{ printf "%d", $1 / 1024 }')
echo "文件数     : ${files}（闸门 ${min_files} - ${max_files}）"
echo "体积       : ${mb} MB（上限 $max_mb MB）"
if [ "$files" -lt "$min_files" ] || [ "$files" -gt "$max_files" ]; then
  echo "文件数 $files 超出闸门：staging 不完整，或混进了不属于本平台的残留" >&2
  fail=1
fi
if [ "$mb" -gt "$max_mb" ]; then
  echo "体积 ${mb} MB 超过上限 $max_mb MB：staging 不完整，或混进了不属于本平台的残留" >&2
  fail=1
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
