#!/bin/sh
# check-runtime-stage.sh 的自检：用假目录验证闸门本身有效。
#
# 用法: sh scripts/tests/check-runtime-stage-test.sh
#
# 闸门失效是无声的 —— 校验通过、发布照常，问题要等到用户那里才出现（review P0-1 / P1-9 都是
# 这一类）。所以每条实测过的失效路径都在这里造一个反例。
set -eu

here=$(cd "$(dirname "$0")" && pwd)
gate=$(cd "$here/.." && pwd)/check-runtime-stage.sh
[ -f "$gate" ] || { echo "找不到被测脚本: $gate" >&2; exit 2; }

# 两条实测过的失效路径各造一个反例，避免闸门再被无声地绕过（review P0-1 / P1-9）。
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
loose="DSH_RUNTIME_MIN_FILES=0 DSH_RUNTIME_MAX_FILES=99999999 DSH_RUNTIME_MAX_MB=99999"

mkdir -p "$tmp/missing/node/bin"
printf '#!/bin/sh\necho v0.0.0\n' > "$tmp/missing/node/bin/node"
chmod +x "$tmp/missing/node/bin/node"
status=0
env $loose sh "$gate" "$tmp/missing" >/dev/null 2>&1 || status=$?
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
env $loose sh "$gate" "$full" >/dev/null 2>&1 || status=$?
if [ "$status" = "0" ]; then
  echo "自检失败：绝对符号链接没有让校验失败" >&2
  exit 1
fi

rm -f "$full/node/bin/absolute-link"
env $loose sh "$gate" "$full" >/dev/null 2>&1 || {
  echo "自检失败：干净的假目录没有通过校验" >&2
  exit 1
}
echo "check-runtime-stage.sh 自检通过"
