#!/usr/bin/env bash
# Builds dist/RoomMesh-<version>.pkg: a guided Installer.app package (Introduction → Read Me →
# License → Destination Select → Installation Type → Installation → Summary) with two components:
#   io.github.brohithkr.RoomMesh.app.pkg     RoomMesh.app    → /Applications                (required)
#   io.github.brohithkr.RoomMesh.driver.pkg  RoomMesh.driver → /Library/Audio/Plug-Ins/HAL  (optional;
#                                    its postinstall restarts coreaudiod)
# The wizard's pages, scripts and distribution template live in installer/. scripts/make-dmg.sh
# wraps the result in a disk image.
# Optional env:
#   UNIVERSAL=1             the app was built arm64 + x86_64 (make package UNIVERSAL=1); otherwise
#                           it is arm64-only and the pkg refuses to install on Intel Macs
#   DEVELOPER_ID_APP        Developer ID Application identity for the app and driver
#   DEVELOPER_ID_INSTALLER  Developer ID Installer identity for the pkg
#   NOTARY_PROFILE          xcrun notarytool keychain profile (requires both identities above)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$ROOT/app/RoomMesh/Resources/Info.plist")
APP="$ROOT/app/build/Build/Products/Release/RoomMesh.app"
DRIVER="$ROOT/driver/build/RoomMesh.driver"
INSTALLER="$ROOT/installer"
MIN_OS=14.2
DIST="$ROOT/dist"
WORK="$DIST/build"
PKG="$DIST/RoomMesh-$VERSION.pkg"
APP_ID=io.github.brohithkr.RoomMesh.app.pkg
DRIVER_ID=io.github.brohithkr.RoomMesh.driver.pkg
APP_PKG_NAME=RoomMesh-App.pkg
DRIVER_PKG_NAME=RoomMesh-Driver.pkg

die() { echo "error: $*" >&2; exit 1; }

if [ -n "${NOTARY_PROFILE:-}" ]; then
  [ -n "${DEVELOPER_ID_APP:-}" ] && [ -n "${DEVELOPER_ID_INSTALLER:-}" ] ||
    die "NOTARY_PROFILE needs both DEVELOPER_ID_APP and DEVELOPER_ID_INSTALLER (notarization requires Developer ID signatures)"
fi
[ -d "$APP" ] || die "$APP not built (run make app)"
[ -d "$DRIVER/Contents/MacOS" ] || die "$DRIVER not built (run make driver)"
[ -f "$INSTALLER/Distribution.xml.in" ] || die "missing $INSTALLER/Distribution.xml.in"

# ---- architectures ----------------------------------------------------------------------------
# The driver is always universal (CMAKE_OSX_ARCHITECTURES). The app is arm64-only unless UNIVERSAL=1.
if [ "${UNIVERSAL:-0}" = 1 ]; then
  APP_ARCHS="arm64 x86_64"
  ARCH_REQUIREMENT="A Mac with Apple silicon or an Intel processor."
else
  APP_ARCHS="arm64"
  ARCH_REQUIREMENT="A Mac with Apple silicon (M1 or later). This build does not run on Intel Macs."
fi
DRIVER_ARCHS="arm64 x86_64"
archs() { lipo -archs "$1" | tr ' ' '\n' | sort | xargs; }
check_archs() {
  local got
  got=$(archs "$1")
  [ "$got" = "$2" ] || die "$1 has architectures '$got', expected '$2'"
}
check_archs "$APP/Contents/MacOS/RoomMesh" "$APP_ARCHS"
check_archs "$DRIVER/Contents/MacOS/RoomMesh" "$DRIVER_ARCHS"

rm -rf "$DIST"
APP_ROOT="$WORK/app-root"
DRIVER_ROOT="$WORK/driver-root"
COMPONENTS="$WORK/components"
mkdir -p "$APP_ROOT/Applications" "$DRIVER_ROOT/Library/Audio/Plug-Ins/HAL" "$WORK/stage" "$COMPONENTS"

# ---- driver: sign once, then use the same bytes everywhere --------------------------------------
# The app's DriverInstaller compares a SHA-256 of the embedded driver's executable against the
# installed one, so the HAL copy and the app-embedded copy must be byte-identical. Signing each
# copy separately with --timestamp gives different bytes (the timestamp differs), which would make
# a freshly installed app prompt to reinstall the driver it was just installed with.
STAGED_DRIVER="$WORK/stage/RoomMesh.driver"
ditto "$DRIVER" "$STAGED_DRIVER"
if [ -n "${DEVELOPER_ID_APP:-}" ]; then
  codesign --force --timestamp --options runtime --sign "$DEVELOPER_ID_APP" "$STAGED_DRIVER"
fi
HAL_DRIVER="$DRIVER_ROOT/Library/Audio/Plug-Ins/HAL/RoomMesh.driver"
ditto "$STAGED_DRIVER" "$HAL_DRIVER"

# ---- app: embed that driver, then sign the app --------------------------------------------------
# The app keeps its own copy so it can install the driver later if the user skipped it.
APP_OUT="$APP_ROOT/Applications/RoomMesh.app"
APP_DRIVER="$APP_OUT/Contents/Resources/RoomMesh.driver"
ditto "$APP" "$APP_OUT"
embedded_changed=0
diff -rq "$STAGED_DRIVER" "$APP_DRIVER" >/dev/null 2>&1 || embedded_changed=1
rm -rf "$APP_DRIVER"
ditto "$STAGED_DRIVER" "$APP_DRIVER"
if [ -n "${DEVELOPER_ID_APP:-}" ]; then
  codesign --force --timestamp --options runtime --entitlements "$ROOT/app/RoomMesh/Resources/RoomMesh.entitlements" \
    --sign "$DEVELOPER_ID_APP" "$APP_OUT"
elif [ "$embedded_changed" = 1 ]; then
  # The driver was rebuilt after the app: the app's ad-hoc seal no longer covers the new bytes.
  echo "note: embedded driver differs from the app build's copy; re-signing the app ad hoc" >&2
  codesign --force --sign - --preserve-metadata=identifier,entitlements,requirements,flags,runtime "$APP_OUT"
fi

cmp "$HAL_DRIVER/Contents/MacOS/RoomMesh" "$APP_DRIVER/Contents/MacOS/RoomMesh" ||
  die "HAL driver and app-embedded driver differ"
codesign --verify --strict --deep "$HAL_DRIVER"
codesign --verify --strict --deep "$APP_OUT"

# ---- component pkgs -----------------------------------------------------------------------------
# Non-relocatable: without this, Installer "relocates" a bundle to wherever it finds another copy
# with the same bundle ID (e.g. app/build/.../RoomMesh.app) instead of installing it where it belongs.
component() { # <root> <identifier> <scripts dir> <output pkg>
  local root=$1 id=$2 scripts=$3 out=$4 plist n=0
  plist="$WORK/$(basename "$out" .pkg).plist"
  pkgbuild --analyze --root "$root" "$plist" >/dev/null
  while /usr/libexec/PlistBuddy -c "Print :$n" "$plist" >/dev/null 2>&1; do
    # --analyze omits the key when it would be the default (relocatable), so Add rather than Set.
    /usr/libexec/PlistBuddy -c "Delete :$n:BundleIsRelocatable" "$plist" >/dev/null 2>&1 || true
    /usr/libexec/PlistBuddy -c "Add :$n:BundleIsRelocatable bool false" "$plist"
    n=$((n + 1))
  done
  [ "$n" -gt 0 ] || die "pkgbuild --analyze found no bundles in $root"
  pkgbuild --root "$root" --component-plist "$plist" --scripts "$scripts" \
    --identifier "$id" --version "$VERSION" --install-location / "$out"
}
component "$APP_ROOT" "$APP_ID" "$INSTALLER/scripts/app" "$COMPONENTS/$APP_PKG_NAME"
component "$DRIVER_ROOT" "$DRIVER_ID" "$INSTALLER/scripts/driver" "$COMPONENTS/$DRIVER_PKG_NAME"

# ---- wizard pages and distribution --------------------------------------------------------------
subst() {
  sed -e "s|@VERSION@|$VERSION|g" -e "s|@MIN_OS@|$MIN_OS|g" \
    -e "s|@HOST_ARCHITECTURES@|$(echo "$APP_ARCHS" | tr ' ' ',')|g" \
    -e "s|@ARCH_REQUIREMENT@|$ARCH_REQUIREMENT|g" \
    -e "s|@APP_PKG@|$APP_PKG_NAME|g" -e "s|@DRIVER_PKG@|$DRIVER_PKG_NAME|g" "$1"
}
RESOURCES="$WORK/resources"
mkdir -p "$RESOURCES"
for f in "$INSTALLER"/resources/*; do
  case "$f" in
    *.html) subst "$f" >"$RESOURCES/$(basename "$f")" ;;
    *) ditto "$f" "$RESOURCES/$(basename "$f")" ;;
  esac
done
DISTXML="$WORK/Distribution.xml"
subst "$INSTALLER/Distribution.xml.in" >"$DISTXML"
! grep -n '@[A-Z_][A-Z_]*@' "$DISTXML" "$RESOURCES"/*.html || die "unsubstituted placeholders (above)"
xmllint --noout "$DISTXML"

SIGN_ARGS=()
[ -n "${DEVELOPER_ID_INSTALLER:-}" ] && SIGN_ARGS=(--sign "$DEVELOPER_ID_INSTALLER")
productbuild --distribution "$DISTXML" --resources "$RESOURCES" --package-path "$COMPONENTS" \
  "${SIGN_ARGS[@]+"${SIGN_ARGS[@]}"}" "$PKG"
rm -rf "$COMPONENTS" # intermediate component pkgs; they are embedded in $PKG

if [ -n "${NOTARY_PROFILE:-}" ]; then
  xcrun notarytool submit "$PKG" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$PKG"
fi
if [ -n "${DEVELOPER_ID_INSTALLER:-}" ]; then
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    spctl -a -vv -t install "$PKG"
  else
    # Gatekeeper rejects an un-notarized Developer ID pkg; report it without failing the build.
    spctl -a -vv -t install "$PKG" || echo "warning: spctl rejected $PKG (expected until it is notarized)" >&2
  fi
fi
echo "OK: $PKG"
