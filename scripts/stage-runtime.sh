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
market_version=${DSHMARKET_VERSION:-1.45.1}
cache=${CACHE_DIR:-.runtime-cache}
python=${PYTHON:-python3}
here=$(cd "$(dirname "$0")" && pwd)

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

echo "校验 ${tarball}（SHASUMS256.txt）"
curl -fsSL -o "$cache/SHASUMS256-$node_os-$node_arch.txt" "$base_url/SHASUMS256.txt"
(cd "$cache" && grep " $tarball$" "SHASUMS256-$node_os-$node_arch.txt" > .expected \
  && (shasum -a 256 -c .expected 2>/dev/null || sha256sum -c .expected) && rm -f .expected)

echo "组装 $out"
rm -rf "$out/node" "$out/dsh-prefix" "$out/tools" "$out/profile-template"
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
PNPM_STORE_DIR="$cache/pnpm-store" sh "$here/make-profile-template.sh" \
  "$out/profile-template" "$market_version"

if command -v xattr >/dev/null 2>&1; then
  xattr -cr "$out" 2>/dev/null || true
fi
"$python" "$here/write_third_party_notices.py" \
  "$out" "$node_version" "$dsh_version" "$pnpm_version" "$market_version"
sh "$here/check-runtime-stage.sh" "$out"
du -sh "$out" 2>/dev/null || true
