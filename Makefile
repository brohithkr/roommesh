SHELL := /bin/bash
export PATH := /opt/homebrew/opt/rustup/bin:$(HOME)/.cargo/bin:/opt/homebrew/bin:$(PATH)
# The CommandLineTools default SDK can be broken on this machine; always resolve the macOS SDK
# through xcrun, and pin the deployment target, for every build step (cargo, cmake, xcodebuild).
export SDKROOT := $(shell xcrun --sdk macosx --show-sdk-path)
export MACOSX_DEPLOYMENT_TARGET := 14.2
# UNIVERSAL=1 builds the core, the app and the pkg for arm64 + x86_64 (slow: WebRTC is compiled
# twice). The driver is always universal. Without it everything is arm64-only and the pkg refuses
# to install on Intel Macs.
UNIVERSAL ?= 0
export UNIVERSAL
XCB_BASE := xcodebuild -project app/RoomMesh.xcodeproj -scheme RoomMesh -derivedDataPath app/build
# A bare 'platform=macOS' destination resolves to the generic "Any Mac" destination. For a Release
# build ONLY_ACTIVE_ARCH is NO, so xcodebuild still targets ARCHS_STANDARD (arm64 + x86_64)
# regardless of the destination's arch, and linking fails since the core xcframework is single-arch
# by default. So a non-universal build pins both the destination and ARCHS to arm64.
XCB_ARM64 := $(XCB_BASE) -destination 'platform=macOS,arch=arm64' ARCHS=arm64 ONLY_ACTIVE_ARCH=YES
ifeq ($(UNIVERSAL),1)
XCB := $(XCB_BASE) -destination 'generic/platform=macOS' ARCHS='arm64 x86_64' ONLY_ACTIVE_ARCH=NO
else
XCB := $(XCB_ARM64)
endif

.PHONY: all bootstrap core driver project app test test-core test-app install-driver uninstall-driver pkg dmg package clean
all: app
bootstrap:
	scripts/bootstrap.sh
core:
	scripts/build-core.sh
driver:
	scripts/build-driver.sh
project: core
	cd app && xcodegen generate
app: core driver project
	$(XCB) -configuration Release build
test: test-core driver test-app
test-core:
	cd core && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
# Tests run on this Mac, so they always use the concrete arm64 destination (a universal core
# xcframework links fine there too).
test-app: core driver project
	$(XCB_ARM64) test
install-driver: driver
	sudo scripts/install-driver.sh
uninstall-driver:
	sudo scripts/uninstall-driver.sh
# pkg: the guided installer (dist/RoomMesh-<ver>.pkg); dmg: that pkg plus the uninstaller in
# dist/RoomMesh-<ver>.dmg; package: both.
pkg: app
	scripts/package.sh
dmg: pkg
	scripts/make-dmg.sh
package: dmg
# cargo clean removes cargo's target directory, honouring CARGO_TARGET_DIR (build-core.sh keeps its
# ffi/ staging there too).
clean:
	cd core && cargo clean
	rm -rf driver/build app/build app/build-* app/RoomMesh.xcodeproj app/Frameworks app/RoomMesh/RustBridge/Generated dist
