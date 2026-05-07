SHELL := /bin/bash
.DEFAULT_GOAL := help

export PATH := $(HOME)/.cargo/bin:$(PATH)

CARGO ?= $(HOME)/.cargo/bin/cargo
RUSTUP ?= $(HOME)/.cargo/bin/rustup
PACKAGE := warp
BIN_NAME := warp-oss
APP_NAME := WarpSOLO
BUNDLE_ID := dev.warp.WarpSOLO
LEGACY_APP_NAMES := WarpOSS WarpOss

CARGO_PROFILE ?= dev
PROFILE_DIR := $(if $(filter dev,$(CARGO_PROFILE)),debug,$(if $(filter release,$(CARGO_PROFILE)),release,$(CARGO_PROFILE)))
BIN_PATH := target/$(PROFILE_DIR)/$(BIN_NAME)
UNIVERSAL_TARGETS := aarch64-apple-darwin x86_64-apple-darwin
UNIVERSAL_DIR := target/universal/$(PROFILE_DIR)
UNIVERSAL_BIN_PATH := $(UNIVERSAL_DIR)/$(BIN_NAME)
BUNDLE_BIN_PATH ?= $(BIN_PATH)

DIST_DIR := dist
APP_PATH := $(DIST_DIR)/$(APP_NAME).app
CONTENTS_DIR := $(APP_PATH)/Contents
MACOS_DIR := $(CONTENTS_DIR)/MacOS
RESOURCES_DIR := $(CONTENTS_DIR)/Resources
ICON_SOURCE := app/channels/oss/icon/no-padding/512x512.png
ICON_NAME := $(APP_NAME)
ICON_FILE := $(ICON_NAME).icns

INSTALL_DIR ?= $(HOME)/Applications
INSTALLED_APP := $(INSTALL_DIR)/$(APP_NAME).app
LEGACY_INSTALLED_APPS := $(addsuffix .app,$(addprefix $(INSTALL_DIR)/,$(LEGACY_APP_NAMES)))
OPEN_AFTER_INSTALL ?= 0
ENTITLEMENTS ?= script/Debug-Entitlements.plist

# auto: use the first Apple Development identity, or ad-hoc signing if none is available.
# You can also pass an explicit identity:
#   make install CODESIGN_IDENTITY="Developer ID Application: Example (TEAMID)"
CODESIGN_IDENTITY ?= auto

.PHONY: help print-config build build-universal bundle bundle-universal sign sign-universal prepare-bundle install install-universal install-bundle run uninstall signing-identities clean-bundle

help:
	@printf '%s\n' \
		'Targets:' \
		'  make build               Build the warp-oss binary' \
		'  make build-universal     Build a universal macOS warp-oss binary' \
		'  make bundle              Build and sign WarpSOLO.app' \
		'  make bundle-universal    Build and sign a universal WarpSOLO.app' \
		'  make install             Install WarpSOLO.app to ~/Applications' \
		'  make install-universal   Install a universal WarpSOLO.app to ~/Applications' \
		'  make run                 Run warp-oss from cargo' \
		'  make uninstall           Remove the installed local bundle' \
		'  make signing-identities  List local codesigning identities' \
		'  make clean-bundle        Remove the generated app bundle' \
		'' \
		'Useful overrides:' \
		'  CARGO=cargo|/path/to/cargo' \
		'  CARGO_PROFILE=dev|release|release-lto|...' \
		'  CODESIGN_IDENTITY=auto|"-"|"<identity>"' \
		'  INSTALL_DIR=/Applications' \
		'  OPEN_AFTER_INSTALL=1'

print-config:
	@printf 'PACKAGE=%s\n' '$(PACKAGE)'
	@printf 'BIN_NAME=%s\n' '$(BIN_NAME)'
	@printf 'BIN_PATH=%s\n' '$(BIN_PATH)'
	@printf 'UNIVERSAL_BIN_PATH=%s\n' '$(UNIVERSAL_BIN_PATH)'
	@printf 'APP_PATH=%s\n' '$(APP_PATH)'
	@printf 'INSTALLED_APP=%s\n' '$(INSTALLED_APP)'
	@printf 'ICON_SOURCE=%s\n' '$(ICON_SOURCE)'
	@printf 'CARGO_PROFILE=%s\n' '$(CARGO_PROFILE)'
	@printf 'CODESIGN_IDENTITY=%s\n' '$(CODESIGN_IDENTITY)'

build:
	$(CARGO) build -p '$(PACKAGE)' --bin '$(BIN_NAME)' --profile '$(CARGO_PROFILE)'

build-universal:
	$(RUSTUP) target add $(UNIVERSAL_TARGETS)
	@for target in $(UNIVERSAL_TARGETS); do \
		echo "Building $(BIN_NAME) for $$target"; \
		$(CARGO) build -p '$(PACKAGE)' --bin '$(BIN_NAME)' --profile '$(CARGO_PROFILE)' --target "$$target"; \
	done
	mkdir -p '$(UNIVERSAL_DIR)'
	lipo -create $(foreach target,$(UNIVERSAL_TARGETS),'target/$(target)/$(PROFILE_DIR)/$(BIN_NAME)') -output '$(UNIVERSAL_BIN_PATH)'
	lipo -info '$(UNIVERSAL_BIN_PATH)'

bundle: sign

bundle-universal: sign-universal

sign: build prepare-bundle

sign-universal: BUNDLE_BIN_PATH := $(UNIVERSAL_BIN_PATH)
sign-universal: build-universal prepare-bundle

prepare-bundle:
	rm -rf '$(APP_PATH)'
	mkdir -p '$(MACOS_DIR)' '$(RESOURCES_DIR)'
	cp -f '$(BUNDLE_BIN_PATH)' '$(MACOS_DIR)/$(BIN_NAME)'
	@iconset='$(RESOURCES_DIR)/$(ICON_NAME).iconset'; \
	if [[ -f '$(ICON_SOURCE)' ]]; then \
		rm -rf "$$iconset"; \
		mkdir -p "$$iconset"; \
		sips -z 16 16 '$(ICON_SOURCE)' --out "$$iconset/icon_16x16.png" >/dev/null; \
		sips -z 32 32 '$(ICON_SOURCE)' --out "$$iconset/icon_16x16@2x.png" >/dev/null; \
		sips -z 32 32 '$(ICON_SOURCE)' --out "$$iconset/icon_32x32.png" >/dev/null; \
		sips -z 64 64 '$(ICON_SOURCE)' --out "$$iconset/icon_32x32@2x.png" >/dev/null; \
		sips -z 128 128 '$(ICON_SOURCE)' --out "$$iconset/icon_128x128.png" >/dev/null; \
		sips -z 256 256 '$(ICON_SOURCE)' --out "$$iconset/icon_128x128@2x.png" >/dev/null; \
		sips -z 256 256 '$(ICON_SOURCE)' --out "$$iconset/icon_256x256.png" >/dev/null; \
		cp -f '$(ICON_SOURCE)' "$$iconset/icon_256x256@2x.png"; \
		cp -f '$(ICON_SOURCE)' "$$iconset/icon_512x512.png"; \
		iconutil -c icns "$$iconset" -o '$(RESOURCES_DIR)/$(ICON_FILE)'; \
		rm -rf "$$iconset"; \
	else \
		echo "Warning: icon source not found: $(ICON_SOURCE)" >&2; \
	fi
	printf '%s\n' \
		'<?xml version="1.0" encoding="UTF-8"?>' \
		'<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
		'<plist version="1.0">' \
		'<dict>' \
		'  <key>CFBundleExecutable</key>' \
		'  <string>$(BIN_NAME)</string>' \
		'  <key>CFBundleIdentifier</key>' \
		'  <string>$(BUNDLE_ID)</string>' \
		'  <key>CFBundleName</key>' \
		'  <string>$(APP_NAME)</string>' \
		'  <key>CFBundleDisplayName</key>' \
		'  <string>$(APP_NAME)</string>' \
		'  <key>CFBundleIconFile</key>' \
		'  <string>$(ICON_NAME)</string>' \
		'  <key>CFBundlePackageType</key>' \
		'  <string>APPL</string>' \
		'  <key>CFBundleShortVersionString</key>' \
		'  <string>0.1.0</string>' \
		'  <key>CFBundleVersion</key>' \
		'  <string>1</string>' \
		'  <key>LSMinimumSystemVersion</key>' \
		'  <string>14.0</string>' \
		'  <key>LSApplicationCategoryType</key>' \
		'  <string>public.app-category.developer-tools</string>' \
		'  <key>NSHighResolutionCapable</key>' \
		'  <true/>' \
		'  <key>UIDesignRequiresCompatibility</key>' \
		'  <true/>' \
		'  <key>CFBundleURLTypes</key>' \
		'  <array>' \
		'    <dict>' \
		'      <key>CFBundleURLName</key>' \
		'      <string>Custom App</string>' \
		'      <key>CFBundleURLSchemes</key>' \
		'      <array>' \
		'        <string>warposs</string>' \
		'      </array>' \
		'    </dict>' \
		'  </array>' \
		'</dict>' \
		'</plist>' \
		> '$(CONTENTS_DIR)/Info.plist'
	@identity='$(CODESIGN_IDENTITY)'; \
	if [[ "$$identity" == auto ]]; then \
		identity="$$(security find-identity -p codesigning -v | awk -F '"' '/Apple Development/ { print $$2; exit }')"; \
		if [[ -z "$$identity" ]]; then \
			identity='-'; \
		fi; \
	fi; \
	echo "Codesigning $(APP_PATH) with $$identity"; \
	codesign --force --deep --options runtime --sign "$$identity" '$(APP_PATH)' --entitlements '$(ENTITLEMENTS)'

install: bundle install-bundle

install-universal: bundle-universal install-bundle

install-bundle:
	mkdir -p '$(INSTALL_DIR)'
	rm -rf '$(INSTALLED_APP)'
	@for app in $(LEGACY_INSTALLED_APPS); do \
		if [[ "$$app" != '$(INSTALLED_APP)' ]]; then \
			rm -rf "$$app"; \
		fi; \
	done
	ditto '$(APP_PATH)' '$(INSTALLED_APP)'
	@if [[ '$(OPEN_AFTER_INSTALL)' == 1 ]]; then \
		open '$(INSTALLED_APP)'; \
	fi
	@echo 'Installed $(INSTALLED_APP)'

run:
	$(CARGO) run -p '$(PACKAGE)' --bin '$(BIN_NAME)'

uninstall:
	rm -rf '$(INSTALLED_APP)'

signing-identities:
	security find-identity -p codesigning -v

clean-bundle:
	rm -rf '$(APP_PATH)'
