# dsh-desktop — 构建入口（暂时只覆盖 macOS 与 Linux）
#
#   make            显示帮助（默认目标）
#   make doctor     检查工具链与平台依赖
#   make check      cargo check --all-targets
#   make test       离线单元测试（含 scripts/ 自检）
#   make test-live  联网集成测试（查询 npm registry）
#   make test-scripts  只跑 scripts/ 的自检
#   make dev        运行 debug 版
#   make build      编译 release 可执行文件
#   make bundle     打包安装包（macOS: .app；Linux: .deb）
#   make run        构建后启动
#   make icons      从 icon.png 重新生成 icon.icns（仅 macOS）
#   make icon-art   从 make_icon.py 重新绘制 icon.png（需 python3 + Pillow）
#   make clean      清理构建产物
#   make distclean  清理产物 + 依赖缓存
#
# 可覆盖变量：CARGO= / PNPM= / PYTHON= / CARGO_HOME= / PNPM_STORE= / BUNDLE_TARGETS=

SHELL := /bin/sh
CARGO  ?= cargo
PNPM   ?= pnpm
PYTHON ?= python3

TAURI_DIR   := src-tauri
MANIFEST    := $(TAURI_DIR)/Cargo.toml
RELEASE_BIN := $(TAURI_DIR)/target/release/dsh-desktop
UNAME_S     := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
PLATFORM       := macos
BUNDLE_TARGETS ?= app
BUNDLE_PATH    := $(TAURI_DIR)/target/release/bundle/macos/DSH Desktop.app
else ifeq ($(UNAME_S),Linux)
PLATFORM       := linux
BUNDLE_TARGETS ?= deb
BUNDLE_PATH    := $(TAURI_DIR)/target/release/bundle/deb
else
$(error 暂不支持 $(UNAME_S)：本 Makefile 只覆盖 macOS 与 Linux)
endif

# 留空则用工具自身默认（cargo 用 ~/.cargo，pnpm 用自身 store）
CARGO_HOME ?=
PNPM_STORE ?=

CARGO_ENV         := $(if $(CARGO_HOME),CARGO_HOME=$(CARGO_HOME),)
PNPM_INSTALL_ARGS := $(if $(PNPM_STORE),--store-dir=$(PNPM_STORE),)

.PHONY: help doctor check fmt fmt-check clippy test test-live test-scripts dev build bundle node-deps run icons clean distclean

help:
	@echo "dsh-desktop 构建入口（平台: $(PLATFORM)，打包目标: $(BUNDLE_TARGETS)）"
	@echo ''
	@echo "  make doctor      检查工具链与平台依赖"
	@echo "  make check       cargo check --all-targets"
	@echo "  make test        离线单元测试（含 scripts/ 自检）"
	@echo "  make test-live   联网集成测试（查询 npm registry）"
	@echo "  make test-scripts  只跑 scripts/ 的自检"
	@echo "  make fmt / fmt-check / clippy"
	@echo "  make dev         运行 debug 版"
	@echo "  make build       编译 release 可执行文件"
	@echo "  make bundle      打包（$(BUNDLE_TARGETS)）"
	@echo "  make run         构建后启动"
	@echo "  make icons       重新生成 icon.icns（仅 macOS）"
	@echo "  make icon-art    重新绘制 icon.png（需 python3 + Pillow）"
	@echo "  make runtime-stage / bundle-bundled / runtime-clean   自带运行时（无需预装 node/dsh）"
	@echo "  make clean / distclean"
	@echo ''
	@echo "变量：CARGO_HOME= PNPM_STORE= BUNDLE_TARGETS= TARGET= CARGO= PNPM= PYTHON="

doctor:
	@echo "平台  : $(UNAME_S) ($(PLATFORM))"
	@echo "cargo : $(shell command -v $(CARGO) 2>/dev/null || echo 缺失)"
	@echo "node  : $(shell command -v node 2>/dev/null || echo 缺失)"
	@echo "pnpm  : $(shell command -v $(PNPM) 2>/dev/null || echo 缺失（打包需要）)"
ifeq ($(PLATFORM),macos)
	@echo "macOS : 需要 Xcode Command Line Tools —— xcode-select --install"
else
	@echo "Linux 系统依赖："
	@for pkg in webkit2gtk-4.1 gtk+-3.0 libsoup-3.0; do \
		if pkg-config --exists $$pkg 2>/dev/null; then echo "  [ok] $$pkg"; else echo "  [缺] $$pkg"; fi; \
	done
	@echo "  Debian/Ubuntu : sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev"
	@echo "  Fedora        : sudo dnf install webkit2gtk4.1-devel openssl-devel curl wget file libappindicator-gtk3-devel librsvg2-devel"
	@echo "  Arch          : sudo pacman -S webkit2gtk-4.1 base-devel curl wget file openssl appmenu-gtk-module libappindicator-gtk3 librsvg"
endif

check:
	$(CARGO_ENV) $(CARGO) check --manifest-path $(MANIFEST) --all-targets

fmt:
	$(CARGO) fmt --manifest-path $(MANIFEST)

# 只检查不改工作区：CI 门禁用这个，避免“先改再查”把 runner 的工作树改脏
fmt-check:
	$(CARGO) fmt --manifest-path $(MANIFEST) --check

clippy:
	$(CARGO_ENV) $(CARGO) clippy --manifest-path $(MANIFEST) --all-targets -- -D warnings

# scripts/ 下的自检：闸门失效是无声的，单独跑一遍比藏在 runtime-stage 里更早暴露问题
test-scripts:
	@sh scripts/tests/check-runtime-stage-test.sh

test: test-scripts
	$(CARGO_ENV) $(CARGO) test --manifest-path $(MANIFEST)

test-live:
	DSH_DESKTOP_LIVE_TESTS=1 $(CARGO_ENV) $(CARGO) test --manifest-path $(MANIFEST) -- --nocapture

dev:
	$(CARGO_ENV) $(CARGO) run --manifest-path $(MANIFEST)

build:
	$(CARGO_ENV) $(CARGO) build --manifest-path $(MANIFEST) --release
	@echo "可执行文件: $(RELEASE_BIN)"

node-deps:
	@command -v $(PNPM) >/dev/null 2>&1 || { echo "需要 pnpm：npm i -g pnpm"; exit 1; }
	$(PNPM) install $(PNPM_INSTALL_ARGS)

# TARGET 用于交叉编译（例如在 arm64 runner 上产出 Intel 包）：
#   make bundle TARGET=x86_64-apple-darwin
bundle: node-deps
	$(CARGO_ENV) $(PNPM) tauri build --bundles $(BUNDLE_TARGETS) $(if $(TARGET),--target $(TARGET),) $(if $(TAURI_CONFIG_EXTRA),--config '$(TAURI_CONFIG_EXTRA)',)
	@echo "产物: $(if $(TARGET),$(TAURI_DIR)/target/$(TARGET),$(TAURI_DIR)/target)/release/bundle/$(if $(filter macos,$(PLATFORM)),macos,deb)"

ifeq ($(PLATFORM),macos)
run: bundle
	open "$(BUNDLE_PATH)"
icons:
	@cd $(TAURI_DIR)/icons && rm -rf icon.iconset && mkdir -p icon.iconset \
		&& sips -z 16 16 icon.png --out icon.iconset/icon_16x16.png >/dev/null \
		&& sips -z 32 32 icon.png --out icon.iconset/icon_16x16@2x.png >/dev/null \
		&& sips -z 32 32 icon.png --out icon.iconset/icon_32x32.png >/dev/null \
		&& sips -z 64 64 icon.png --out icon.iconset/icon_32x32@2x.png >/dev/null \
		&& sips -z 128 128 icon.png --out icon.iconset/icon_128x128.png >/dev/null \
		&& sips -z 256 256 icon.png --out icon.iconset/icon_128x128@2x.png >/dev/null \
		&& sips -z 256 256 icon.png --out icon.iconset/icon_256x256.png >/dev/null \
		&& sips -z 512 512 icon.png --out icon.iconset/icon_256x256@2x.png >/dev/null \
		&& sips -z 512 512 icon.png --out icon.iconset/icon_512x512.png >/dev/null \
		&& sips -z 1024 1024 icon.png --out icon.iconset/icon_512x512@2x.png >/dev/null \
		&& iconutil -c icns icon.iconset -o icon.icns && rm -rf icon.iconset
	@echo "icon.icns 已更新"
else
run: build
	$(RELEASE_BIN)
icons:
	@echo "icon.icns 仅 macOS 需要；Linux 打包直接使用 icons/icon.png"
endif

# 图标源：src-tauri/icons/make_icon.py（自绘矢量路径）。改设计改脚本，不要手改 png。
icon-art:
	$(PYTHON) $(TAURI_DIR)/icons/make_icon.py $(TAURI_DIR)/icons/icon.png 1024
	@$(MAKE) --no-print-directory icons
	@$(MAKE) --no-print-directory icon-ico

# Windows 目标的 exe 资源需要 .ico（tauri-build 在 Windows target 下强制要求）
icon-ico:
	@cd $(TAURI_DIR)/icons && $(PYTHON) make_ico.py icon.ico icon.png

# 实验性：交叉编译 Windows 可执行文件（只能出裸 exe：NSIS/MSI 需要在 Windows 上打包）
#   前置：brew install mingw-w64 且 rustup target add x86_64-pc-windows-gnu
#   注意：Windows 专用的进程监管/退出清理语义尚未实现正确（见方案文档），且自带运行时
#         必须按平台单独 staging —— 否则会把别的平台的运行时打进包里。
WINDOWS_TARGET ?= x86_64-pc-windows-gnu
windows:
	@if [ -e $(RUNTIME_DIR)/node/bin/node ] && [ ! -e $(RUNTIME_DIR)/node/bin/node.exe ]; then \
		echo "警告：$(RUNTIME_DIR)/ 里是别的平台的运行时，会被一起打进 Windows 包；先 make runtime-clean" >&2; \
	fi
	$(CARGO_ENV) $(PNPM) tauri build --target $(WINDOWS_TARGET) --no-bundle
	@case "$(WINDOWS_TARGET)" in *aarch64*) arch=arm64;; *i686*) arch=x86;; *) arch=x64;; esac; \
	  cargo_home=$(if $(CARGO_HOME),$(CARGO_HOME),$$HOME/.cargo); \
	  dll=$$(find "$$cargo_home/registry/src" -path "*webview2-com-sys-*/$$arch/WebView2Loader.dll" 2>/dev/null | head -1); \
	  if [ -n "$$dll" ]; then cp "$$dll" $(TAURI_DIR)/target/$(WINDOWS_TARGET)/release/ && echo "已附带 WebView2Loader.dll ($$arch)"; \
	  else echo "提示：未找到 WebView2Loader.dll；目标机需要有 WebView2 运行时" >&2; fi
	@echo "产物目录: $(TAURI_DIR)/target/$(WINDOWS_TARGET)/release/（拷贝其中的 dsh-desktop.exe 与 WebView2Loader.dll）"

clean:
	$(CARGO_ENV) $(CARGO) clean --manifest-path $(MANIFEST)

distclean: clean
	rm -rf node_modules .pnpm-store .cargo-home $(TAURI_DIR)/gen
# ---------------------------------------------------------------- 自带运行时（方案 §5）
# 目标：在没有 node / dsh 的机器上双击即用。
#   make runtime-fetch    下载官方 Node 并校验 SHASUMS256.txt
#   make runtime-stage    组装 src-tauri/runtime/{node,dsh-prefix,tools,profile-template,THIRD-PARTY-NOTICES.md}
#   make bundle-bundled   runtime-stage + 打包（.app 会带上整套运行时，约 490 MB）
#   make runtime-clean    回收 staging 与下载缓存
# staging 必须按平台各自执行：dsh 树里有平台相关的原生模块。
NODE_VERSION      ?= 22.23.2
DSH_VERSION       ?= 0.1.5-rc.2
PNPM_VERSION      ?= 12.3.4
DSHMARKET_VERSION ?= 1.46.1
RUNTIME_DIR       := $(TAURI_DIR)/runtime
RUNTIME_CACHE     := .runtime-cache
# 交叉编译时（TARGET 非空）必须按**目标**平台 staging：在 arm64 runner 上打 x64 包却塞进 arm64 的
# node，产物装到目标机上直接起不来。Rust 三元组的第一段就是架构（x86_64-apple-darwin → x86_64）。
# `make windows` 走的是 WINDOWS_TARGET，同样要按它的架构取 node（见下面的 NODE_OS 判定）。
NODE_ARCH_HOST    := $(shell uname -m | sed -e s/arm64/arm64/ -e s/aarch64/arm64/ -e s/x86_64/x64/)
NODE_TRIPLE       := $(if $(TARGET),$(TARGET),$(if $(filter windows,$(MAKECMDGOALS)),$(WINDOWS_TARGET)))
NODE_ARCH_TARGET  := $(shell echo "$(NODE_TRIPLE)" | cut -d- -f1 | sed -e s/aarch64/arm64/ -e s/x86_64/x64/)
NODE_ARCH         := $(if $(NODE_TRIPLE),$(NODE_ARCH_TARGET),$(NODE_ARCH_HOST))
ifeq ($(PLATFORM),macos)
NODE_OS           := darwin
else
NODE_OS           := linux
endif
# make windows（交叉编译实验）时 tarball 也要跟着换，否则 staging 里会混进 unix 的 node。
# 判据是「TARGET 指向 windows」或「本次的目标就是 windows」：windows: 目标用的是
# WINDOWS_TARGET 而不是 TARGET，只看 TARGET 会让这条分支对文档里的用法永不生效（review D5）。
# 也不能直接看 WINDOWS_TARGET —— 它有 `?=` 默认值，在任何平台上都非空。
ifneq ($(findstring windows,$(TARGET))$(filter windows,$(MAKECMDGOALS)),)
NODE_OS           := win
endif
ifeq ($(NODE_OS),win)
NODE_TARBALL      := node-v$(NODE_VERSION)-win-$(NODE_ARCH).zip
else ifeq ($(NODE_OS),darwin)
NODE_TARBALL      := node-v$(NODE_VERSION)-darwin-$(NODE_ARCH).tar.gz
else
NODE_TARBALL      := node-v$(NODE_VERSION)-linux-$(NODE_ARCH).tar.xz
endif
NODE_DIST_NAME    := node-v$(NODE_VERSION)-$(NODE_OS)-$(NODE_ARCH)
NODE_BASE_URL     := https://nodejs.org/dist/v$(NODE_VERSION)
NODE_BIN          := $(abspath $(RUNTIME_DIR))/node/bin
RUNTIME_LOCK      := $(TAURI_DIR)/runtime.lock

# 自带运行时版才声明 resources：普通 `make bundle`（精简版）不该把 520 MB 的 staging 打进包 ——
# 这正是桌面壳审查 §10 记过的陷阱（bundle.resources 常开 + 残留 payload = 静默变胖）。
ifeq ($(PLATFORM),macos)
# macOS 上还要声明 11.0：随包的 node 22 是 minos 11.0，写 10.15 会"能装、能开壳、一起 harness 就崩"。
BUNDLED_CONFIG_JSON := {"bundle":{"resources":["runtime/**/*"],"macOS":{"minimumSystemVersion":"11.0"}}}
else
BUNDLED_CONFIG_JSON := {"bundle":{"resources":["runtime/**/*"]}}
endif

# 信任链钉在仓库里的 $(RUNTIME_LOCK)：只校验"下载来的 SHASUMS256.txt"等于把信任交给 TLS，
# 清单被换掉时发现不了（review P2-12）。
runtime-fetch:
	@mkdir -p $(RUNTIME_CACHE)
	@if [ ! -f $(RUNTIME_CACHE)/$(NODE_TARBALL) ]; then \
		echo "下载 $(NODE_TARBALL)"; \
		curl -fsSL -o $(RUNTIME_CACHE)/$(NODE_TARBALL) $(NODE_BASE_URL)/$(NODE_TARBALL); \
	fi
	@curl -fsSL -o $(RUNTIME_CACHE)/SHASUMS256.txt $(NODE_BASE_URL)/SHASUMS256.txt
	@grep " $(NODE_TARBALL)$$" $(RUNTIME_LOCK) > $(RUNTIME_CACHE)/.locked \
		|| { echo "$(RUNTIME_LOCK) 里没有 $(NODE_TARBALL) 的 SHA256，先补上再打包" >&2; exit 1; }
	@cd $(RUNTIME_CACHE) && grep " $(NODE_TARBALL)$$" SHASUMS256.txt > .downloaded \
		&& cmp -s .locked .downloaded \
		|| { echo "下载的 SHASUMS256.txt 与 $(RUNTIME_LOCK) 不一致：$(NODE_TARBALL)" >&2; exit 1; }
	@cd $(RUNTIME_CACHE) && (shasum -a 256 -c .locked 2>/dev/null || sha256sum -c .locked) && rm -f .locked .downloaded
	@echo "已校验 $(NODE_TARBALL)（对照 $(RUNTIME_LOCK)）"

runtime-stage: runtime-fetch
	@sh scripts/tests/check-runtime-stage-test.sh
	@echo "组装 $(RUNTIME_DIR) …"
	@# 整体清空（只留 README.md 这个非空 marker）：只删四个已知子目录时，别的平台/上一次的残留
	@# 会被 bundle.resources 静默打进包（review P1-8）。
	@if [ -d $(RUNTIME_DIR) ]; then find $(RUNTIME_DIR) -mindepth 1 -maxdepth 1 ! -name README.md -exec rm -rf {} +; fi
	@rm -rf $(RUNTIME_CACHE)/unpacked && mkdir -p $(RUNTIME_CACHE)/unpacked $(RUNTIME_DIR)
	@if [ "$(NODE_OS)" = win ]; then unzip -q $(RUNTIME_CACHE)/$(NODE_TARBALL) -d $(RUNTIME_CACHE)/unpacked; \
	else tar -xf $(RUNTIME_CACHE)/$(NODE_TARBALL) -C $(RUNTIME_CACHE)/unpacked; fi
	@mv $(RUNTIME_CACHE)/unpacked/$(NODE_DIST_NAME) $(RUNTIME_DIR)/node
	@echo "安装 dsh $(DSH_VERSION) 到 dsh-prefix …"
	@PATH="$(NODE_BIN):$$PATH" $(RUNTIME_DIR)/node/bin/npm install -g --prefix $(RUNTIME_DIR)/dsh-prefix \
		--cache $(RUNTIME_CACHE)/npm --no-fund --no-audit --loglevel=error @deepseek-ai/dsh@$(DSH_VERSION)
	@echo "安装 pnpm $(PNPM_VERSION) 到 tools …"
	@PATH="$(NODE_BIN):$$PATH" $(RUNTIME_DIR)/node/bin/npm install -g --prefix $(RUNTIME_DIR)/tools \
		--cache $(RUNTIME_CACHE)/npm --no-fund --no-audit --loglevel=error pnpm@$(PNPM_VERSION)
	@echo "生成 profile 模板（含插件市场 dshmarket@$(DSHMARKET_VERSION)）…"
	@PATH="$(NODE_BIN):$$PATH" PNPM_STORE_DIR="$(abspath $(RUNTIME_CACHE))/pnpm-store" \
		sh scripts/make-profile-template.sh \
		"$(RUNTIME_DIR)/profile-template" "$(DSHMARKET_VERSION)" "$(RUNTIME_DIR)/tools/bin"
	@xattr -cr $(RUNTIME_DIR) 2>/dev/null || true
	@$(PYTHON) scripts/write_third_party_notices.py "$(RUNTIME_DIR)" "$(NODE_VERSION)" "$(DSH_VERSION)" "$(PNPM_VERSION)" "$(DSHMARKET_VERSION)"
	@DSH_RUNTIME_MAX_FILES=45000 DSH_RUNTIME_MAX_MB=600 sh scripts/check-runtime-stage.sh "$(RUNTIME_DIR)"
	@du -sh $(RUNTIME_DIR) 2>/dev/null || true

# 打包"无需预装 node/dsh"的版本：bundle.resources 会把 runtime/ 一起塞进 .app。
# 自带运行时的发行版必须声明 macOS 11.0：随包的 node 22 是 `minos 11.0`，而 tauri.conf.json 里的
# 10.15 只对精简版成立 —— 声明 10.15 的结果是"能装、能开壳、一起 harness 就崩"（方案 §18.1 H2 实测）。
bundle-bundled: runtime-stage
	@$(MAKE) --no-print-directory bundle TAURI_CONFIG_EXTRA='$(BUNDLED_CONFIG_JSON)'

runtime-clean:
	rm -rf $(RUNTIME_CACHE)
	@if [ -d $(RUNTIME_DIR) ]; then find $(RUNTIME_DIR) -mindepth 1 -maxdepth 1 ! -name README.md -exec rm -rf {} +; fi
	@echo "已回收 staging 与下载缓存（src-tauri/runtime/README.md 保留）"
