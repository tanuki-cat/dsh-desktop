#!/bin/sh
# Stage a bundled runtime for one platform (docs/design-task-feat-dsh-bundled-runtime.md §5).
#
# 用法: NODE_OS=win NODE_ARCH=x64 sh scripts/stage-runtime.sh <runtime 目录>
#
# 可覆盖：NODE_VERSION / NODE_OS / NODE_ARCH / DSH_VERSION / PNPM_VERSION /
#         DSHMARKET_VERSION / CACHE_DIR / PYTHON
#
# 与 Makefile 里的 runtime-stage 等价，但**不依赖 make**，因此能在 Windows 的 Git Bash 里跑：
# 原生安装是唯一能拿到正确平台原生模块的方式（在 macOS 上用 --os=win32 装 koffi 会去编译，
# 实测直接失败）。
set -eu

out=${1:?用法: stage-runtime.sh <runtime 目录>}
node_version=${NODE_VERSION:-22.23.2}
dsh_version=${DSH_VERSION:-0.1.5-rc.2}
pnpm_version=${PNPM_VERSION:-12.3.4}
market_version=${DSHMARKET_VERSION:-1.46.1}
cache=${CACHE_DIR:-.runtime-cache}
python=${PYTHON:-python3}
here=$(cd "$(dirname "$0")" && pwd)
lock="$here/../src-tauri/runtime.lock"

host_os=$(uname -s | tr "[:upper:]" "[:lower:]")
case "$host_os" in
  darwin) default_os=darwin ;;
  linux) default_os=linux ;;
  mingw*|msys*|cygwin*) default_os=win ;;
  *) echo "无法识别的系统: $host_os" >&2; exit 2 ;;
esac
node_os=${NODE_OS:-$default_os}

host_arch=$(uname -m)
case "$host_arch" in
  arm64|aarch64) default_arch=arm64 ;;
  x86_64|amd64) default_arch=x64 ;;
  *) echo "无法识别的架构: $host_arch" >&2; exit 2 ;;
esac
node_arch=${NODE_ARCH:-$default_arch}

dist="node-v$node_version-$node_os-$node_arch"
case "$node_os" in
  win) tarball="$dist.zip" ;;
  linux) tarball="$dist.tar.xz" ;;
  *) tarball="$dist.tar.gz" ;;
esac
base_url="https://nodejs.org/dist/v$node_version"

mkdir -p "$cache"
if [ ! -f "$cache/$tarball" ]; then
  echo "下载 $tarball"
  curl -fsSL -o "$cache/$tarball" "$base_url/$tarball"
fi

# 信任链钉在仓库里的 src-tauri/runtime.lock：只校验下载来的 SHASUMS256.txt 等于把信任交给
# TLS，清单被换掉时发现不了（review P2-12）。
echo "校验 ${tarball}（对照 src-tauri/runtime.lock）"
curl -fsSL -o "$cache/SHASUMS256-$node_os-$node_arch.txt" "$base_url/SHASUMS256.txt"
grep " $tarball$" "$lock" > "$cache/.locked" \
  || { echo "$lock 里没有 $tarball 的 SHA256，先补上再打包" >&2; exit 1; }
(cd "$cache" && grep " $tarball$" "SHASUMS256-$node_os-$node_arch.txt" > .downloaded \
  && cmp -s .locked .downloaded \
  || { echo "下载的 SHASUMS256.txt 与 runtime.lock 不一致：$tarball" >&2; exit 1; })
(cd "$cache" && (shasum -a 256 -c .locked 2>/dev/null || sha256sum -c .locked) && rm -f .locked .downloaded)

echo "组装 $out"
# 整体清空（只留 README.md 这个非空 marker）：只删四个已知子目录时，别的平台/上一次的残留
# 会被 bundle.resources 静默打进包（review P1-8）。
if [ -d "$out" ]; then
  find "$out" -mindepth 1 -maxdepth 1 ! -name README.md -exec rm -rf {} +
fi
rm -rf "$cache/unpacked" && mkdir -p "$cache/unpacked" "$out"
case "$node_os" in
  win) unzip -q "$cache/$tarball" -d "$cache/unpacked" ;;
  *) tar -xf "$cache/$tarball" -C "$cache/unpacked" ;;
esac
mv "$cache/unpacked/$dist" "$out/node"

# 用自带 node 跑它自带的 npm：不依赖 PATH，也不依赖平台的 shim 脚本。
node_bin=$out/node/bin/node
[ -x "$node_bin" ] || node_bin=$out/node/node.exe
[ -x "$node_bin" ] || { echo "自带 node 不存在: $out/node" >&2; exit 3; }
npm_cli=$out/node/lib/node_modules/npm/bin/npm-cli.js
[ -f "$npm_cli" ] || npm_cli=$out/node/node_modules/npm/bin/npm-cli.js
[ -f "$npm_cli" ] || { echo "自带 npm 不存在: $out/node" >&2; exit 3; }

echo "安装 dsh $dsh_version 到 dsh-prefix"
"$node_bin" "$npm_cli" install -g --prefix "$out/dsh-prefix" \
  --cache "$cache/npm" --no-fund --no-audit --loglevel=error "@deepseek-ai/dsh@$dsh_version"

echo "安装 pnpm $pnpm_version 到 tools"
"$node_bin" "$npm_cli" install -g --prefix "$out/tools" \
  --cache "$cache/npm" --no-fund --no-audit --loglevel=error "pnpm@$pnpm_version"

echo "生成 profile 模板（含插件市场 dshmarket@${market_version}）"
# `cache` defaults to the RELATIVE `.runtime-cache`, and make-profile-template.sh runs
# `pnpm install` from inside `$dest`. pnpm resolves a relative --store-dir against the project
# directory, so the store landed in `profile-template/.runtime-cache/pnpm-store`: shipped in the
# portable bundle, copied into the user profile on first seed, and never cached by CI (which
# caches the real `.runtime-cache`). Absolute, like the Makefile already passes.
mkdir -p "$cache"
store_dir="$(cd "$cache" && pwd)/pnpm-store"
# The tools prefix holds the pnpm this script just installed; passing it keeps the template on
# the same pnpm the bundle ships instead of whatever happens to be on PATH. npm puts the shim
# in the prefix root on Windows and in `bin/` elsewhere, and make-profile-template.sh accepts
# either as long as it can execute it.
tools_bin="$out/tools/bin"
[ -x "$tools_bin/pnpm" ] || tools_bin="$out/tools"
# pnpm is a `#!/usr/bin/env node` script: put the bundled node first, as the Makefile does, so the
# template is built by the node that ships rather than whatever happens to be on PATH.
node_dir=$(cd "$(dirname "$node_bin")" && pwd)
PATH="$node_dir:$PATH" PNPM_STORE_DIR="$store_dir" sh "$here/make-profile-template.sh" \
  "$out/profile-template" "$market_version" "$tools_bin"

if command -v xattr >/dev/null 2>&1; then
  xattr -cr "$out" 2>/dev/null || true
fi
"$python" "$here/write_third_party_notices.py" \
  "$out" "$node_version" "$dsh_version" "$pnpm_version" "$market_version"
sh "$here/check-runtime-stage.sh" "$out"
du -sh "$out" 2>/dev/null || true
