SHELL := /bin/bash
export PATH := /opt/homebrew/opt/rustup/bin:$(HOME)/.cargo/bin:/opt/homebrew/bin:$(PATH)
# The CommandLineTools default SDK can be broken on this machine; always resolve the macOS SDK
# through xcrun, and pin the deployment target, for every build step (cargo, cmake, xcodebuild).
export SDKROOT := $(shell xcrun --sdk macosx --show-sdk-path)
export MACOSX_DEPLOYMENT_TARGET := 14.2
# A bare 'platform=macOS' destination resolves to the generic "Any Mac" destination. For a Release
# build ONLY_ACTIVE_ARCH is NO, so xcodebuild still targets ARCHS_STANDARD (arm64 + x86_64)
# regardless of the destination's arch, and linking fails since the core xcframework is single-arch
# by default (UNIVERSAL=1 in build-core.sh is opt-in). Pin both the destination and ARCHS to arm64.
XCB := xcodebuild -project app/RoomMesh.xcodeproj -scheme RoomMesh -derivedDataPath app/build -destination 'platform=macOS,arch=arm64' ARCHS=arm64 ONLY_ACTIVE_ARCH=YES

.PHONY: all bootstrap core driver project app test test-core test-app install-driver package clean
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
test-app: core driver project
	$(XCB) test
install-driver: driver
	sudo scripts/install-driver.sh
package: app
	scripts/package.sh
clean:
	rm -rf core/target driver/build app/build app/build-* app/RoomMesh.xcodeproj app/Frameworks app/RoomMesh/RustBridge/Generated dist
