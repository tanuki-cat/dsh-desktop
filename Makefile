# dsh-desktop — 构建入口（暂时只覆盖 macOS 与 Linux）
#
#   make            显示帮助（默认目标）
#   make doctor     检查工具链与平台依赖
#   make check      cargo check --all-targets
#   make test       离线单元测试
#   make test-live  联网集成测试（查询 npm registry）
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

.PHONY: help doctor check fmt fmt-check clippy test test-live dev build bundle node-deps run icons clean distclean

help:
	@echo "dsh-desktop 构建入口（平台: $(PLATFORM)，打包目标: $(BUNDLE_TARGETS)）"
	@echo ''
	@echo "  make doctor      检查工具链与平台依赖"
	@echo "  make check       cargo check --all-targets"
	@echo "  make test        离线单元测试"
	@echo "  make test-live   联网集成测试（查询 npm registry）"
	@echo "  make fmt / fmt-check / clippy"
	@echo "  make dev         运行 debug 版"
	@echo "  make build       编译 release 可执行文件"
	@echo "  make bundle      打包（$(BUNDLE_TARGETS)）"
	@echo "  make run         构建后启动"
	@echo "  make icons       重新生成 icon.icns（仅 macOS）"
	@echo "  make icon-art    重新绘制 icon.png（需 python3 + Pillow）"
	@echo "  make clean / distclean"
	@echo ''
	@echo "变量：CARGO_HOME= PNPM_STORE= BUNDLE_TARGETS= CARGO= PNPM= PYTHON="

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

test:
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
	$(CARGO_ENV) $(PNPM) tauri build --bundles $(BUNDLE_TARGETS) $(if $(TARGET),--target $(TARGET),)
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

clean:
	$(CARGO_ENV) $(CARGO) clean --manifest-path $(MANIFEST)

distclean: clean
	rm -rf node_modules .pnpm-store .cargo-home $(TAURI_DIR)/gen
