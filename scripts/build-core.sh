#!/usr/bin/env bash
# Builds the Rust core as a static library + Swift bindings + RoomMeshCore.xcframework.
#   app/Frameworks/RoomMeshCore.xcframework        (static lib + Headers/roommesh_coreFFI/{roommesh_coreFFI.h,module.modulemap})
#   app/RoomMesh/RustBridge/Generated/*.swift      (UniFFI Swift bindings, not committed)
# UNIVERSAL=1 also builds x86_64 (slow: WebRTC is compiled twice).
set -euo pipefail
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
# The CommandLineTools default SDK can be broken; always resolve the macOS SDK through xcrun.
SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
export SDKROOT
export MACOSX_DEPLOYMENT_TARGET=14.2

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/core"
# Cargo's output directory (relative CARGO_TARGET_DIR is relative to core/, as for cargo).
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$ROOT/core/$TARGET_DIR" ;; esac

TARGETS=(aarch64-apple-darwin)
[ "${UNIVERSAL:-0}" = "1" ] && TARGETS+=(x86_64-apple-darwin)
for t in "${TARGETS[@]}"; do
  rustup target add "$t" >/dev/null 2>&1 || true
  cargo build -p roommesh-core --release --target "$t"
done

OUT="$ROOT/app/Frameworks"
GEN="$ROOT/app/RoomMesh/RustBridge/Generated"
TMP="$TARGET_DIR/ffi"
# The C module lives in its own subdirectory of Headers (Headers/roommesh_coreFFI/), so its
# module.modulemap can't collide with another library's in a shared include directory.
MODDIR="$TMP/Headers/roommesh_coreFFI"
rm -rf "$OUT/RoomMeshCore.xcframework" "$GEN" "$TMP"
mkdir -p "$OUT" "$GEN" "$MODDIR" "$TMP/lib"

LIBS=()
for t in "${TARGETS[@]}"; do LIBS+=("$TARGET_DIR/$t/release/libroommesh_core.a"); done
lipo -create "${LIBS[@]}" -output "$TMP/lib/libroommesh_core.a"

# Library mode: the bindings are read from the metadata embedded in the built dylib.
# The generated Swift does `import roommesh_coreFFI`, so the C module must carry that name. The
# modulemap is a plain (non-`framework`) module: this is a library-type xcframework whose Headers
# land on the header search path, where the importer finds Headers/roommesh_coreFFI/
# module.modulemap by module name. It autolinks libc++ (WebRTC) and the audio frameworks, so the
# app target only needs to link the xcframework.
DYLIB="$TARGET_DIR/aarch64-apple-darwin/release/libroommesh_core.dylib"
BINDGEN=(cargo run -q -p uniffi-bindgen --)
"${BINDGEN[@]}" --headers "$DYLIB" "$MODDIR"
"${BINDGEN[@]}" --modulemap --module-name roommesh_coreFFI --modulemap-filename module.modulemap \
  --link-frameworks CoreAudio --link-frameworks AudioToolbox --link-frameworks CoreFoundation \
  "$DYLIB" "$MODDIR"
sed -i '' 's/^}$/    link "c++"\n}/' "$MODDIR/module.modulemap"
if ! grep -q 'link "c++"' "$MODDIR/module.modulemap"; then
  echo "error: failed to add 'link \"c++\"' to $MODDIR/module.modulemap" >&2
  exit 1
fi
"${BINDGEN[@]}" --swift-sources "$DYLIB" "$GEN"

XCF="$OUT/RoomMeshCore.xcframework"
if ! xcodebuild -create-xcframework \
    -library "$TMP/lib/libroommesh_core.a" -headers "$TMP/Headers" \
    -output "$XCF" >"$TMP/xcodebuild.log" 2>&1; then
  # xcodebuild can fail to load its plugins (e.g. CoreSimulator missing until
  # `xcodebuild -runFirstLaunch`). An xcframework is just this directory layout, so assemble it
  # -- but only for that failure; anything else is a real error.
  if ! grep -q "failed to load a required plug-in" "$TMP/xcodebuild.log"; then
    echo "error: xcodebuild -create-xcframework failed:" >&2
    cat "$TMP/xcodebuild.log" >&2
    exit 1
  fi
  echo "xcodebuild could not load its plug-ins (see $TMP/xcodebuild.log); assembling manually" >&2
  rm -rf "$XCF"
  ARCHS=()
  for t in "${TARGETS[@]}"; do ARCHS+=("${t%%-*}"); done
  ARCHS=("${ARCHS[@]/aarch64/arm64}")
  ID="macos-$(IFS=_; echo "${ARCHS[*]}")"
  mkdir -p "$XCF/$ID"
  cp "$TMP/lib/libroommesh_core.a" "$XCF/$ID/"
  cp -R "$TMP/Headers" "$XCF/$ID/Headers"
  ARCH_XML=""
  for a in "${ARCHS[@]}"; do ARCH_XML+="				<string>$a</string>"$'\n'; done
  cat >"$XCF/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AvailableLibraries</key>
	<array>
		<dict>
			<key>BinaryPath</key>
			<string>libroommesh_core.a</string>
			<key>HeadersPath</key>
			<string>Headers</string>
			<key>LibraryIdentifier</key>
			<string>$ID</string>
			<key>LibraryPath</key>
			<string>libroommesh_core.a</string>
			<key>SupportedArchitectures</key>
			<array>
${ARCH_XML}			</array>
			<key>SupportedPlatform</key>
			<string>macos</string>
		</dict>
	</array>
	<key>CFBundlePackageType</key>
	<string>XFWK</string>
	<key>XCFrameworkFormatVersion</key>
	<string>1.0</string>
</dict>
</plist>
PLIST
  plutil -lint "$XCF/Info.plist" >/dev/null
fi

echo "OK: $XCF and $GEN/*.swift"
