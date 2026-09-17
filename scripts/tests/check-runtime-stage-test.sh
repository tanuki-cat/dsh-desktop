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
chmod +x "$full/tools/bin/pnpm"

# 每条闸门各造一个反例：闸门失效是无声的，只有"必须失败"的用例能证明它还在工作。
# $1 是反例名称，$2 是造出该状态后必须让闸门失败的说明。
expect_fail() {
  status=0
  env $loose sh "$gate" "$full" >/dev/null 2>&1 || status=$?
  if [ "$status" = "0" ]; then
    echo "自检失败：$1 没有让校验失败（$2）" >&2
    exit 1
  fi
}

expect_pass() {
  status=0
  env $loose sh "$gate" "$full" >/dev/null 2>&1 || status=$?
  if [ "$status" != "0" ]; then
    echo "自检失败：$1 让校验失败了（$2）" >&2
    exit 1
  fi
}

expect_pass "干净的假目录" "闸门本身失效：连正常 staging 都通不过"

# 1) 可执行位：node / pnpm 丢 +x 后仍能"存在"，这一条曾完全放行（review A2）。
chmod -x "$full/node/bin/node"
expect_fail "node 没有可执行位" "丢 +x 的 node 会被打进包，用户那里一拉 harness 就失败"
chmod +x "$full/node/bin/node"
chmod -x "$full/tools/bin/pnpm"
expect_fail "pnpm 没有可执行位" "插件安装会以 127 退出"
chmod +x "$full/tools/bin/pnpm"

# 2) 绝对符号链接：会被解引用，把宿主机文件复制进包。
ln -s /usr/bin/true "$full/node/bin/absolute-link"
expect_fail "绝对符号链接" "宿主文件会被复制进包"
rm -f "$full/node/bin/absolute-link"

# 3) 悬空符号链接：tauri build 直接失败。
ln -s "$full/does-not-exist" "$full/node/bin/dangling-link"
expect_fail "悬空符号链接" "tauri build 会在资源路径上失败"
rm -f "$full/node/bin/dangling-link"

# 4) profile 模板里混进 pnpm store：会被打进包并播种进用户 profile。
mkdir -p "$full/profile-template/.runtime-cache/pnpm-store"
expect_fail "模板内的 pnpm store" "构建缓存会进包并进用户 profile"
rm -rf "$full/profile-template/.runtime-cache"

# 5) 文件数闸门：这一项此前被 loose 变量绕开，从没有任何反例。
counted=$(find "$full" -type f | wc -l | tr -d " ")
env DSH_RUNTIME_MIN_FILES=$((counted + 1)) DSH_RUNTIME_MAX_FILES=99999999 DSH_RUNTIME_MAX_MB=99999 \
  sh "$gate" "$full" >/dev/null 2>&1 && {
  echo "自检失败：文件数低于下限没有让校验失败" >&2
  exit 1
}

# 6) 体积闸门：同上。先放一个 2 MB 的文件，否则小目录的 MB 取整后是 0。
dd if=/dev/zero of="$full/node/bin/ballast" bs=1024 count=2048 >/dev/null 2>&1
env DSH_RUNTIME_MIN_FILES=0 DSH_RUNTIME_MAX_FILES=99999999 DSH_RUNTIME_MAX_MB=0 \
  sh "$gate" "$full" >/dev/null 2>&1 && {
  echo "自检失败：体积超过上限没有让校验失败" >&2
  exit 1
}
rm -f "$full/node/bin/ballast"

# 7) quarantine：只在有 xattr 的平台上可造（macOS）。
if command -v xattr >/dev/null 2>&1; then
  xattr -w com.apple.quarantine "0081;00000000;test;" "$full/node/bin/node" 2>/dev/null || true
  if xattr "$full/node/bin/node" 2>/dev/null | grep -q quarantine; then
    expect_fail "quarantine 属性" "属性会被原样复制进 .app，触发 Gatekeeper"
    xattr -c "$full/node/bin/node" 2>/dev/null || true
  fi
fi

echo "check-runtime-stage.sh 自检通过"
