#!/usr/bin/env bash
# Installer: RoomMesh.app → /Applications, RoomMesh.driver → /Library/Audio/Plug-Ins/HAL, then restarts coreaudiod.
# Optional env: DEVELOPER_ID_APP, DEVELOPER_ID_INSTALLER, NOTARY_PROFILE (xcrun notarytool keychain profile).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$ROOT/app/RoomMesh/Resources/Info.plist")
APP="$ROOT/app/build/Build/Products/Release/RoomMesh.app"
DIST="$ROOT/dist"
rm -rf "$DIST"; mkdir -p "$DIST/root/Applications" "$DIST/root/Library/Audio/Plug-Ins/HAL" "$DIST/scripts"
ditto "$APP" "$DIST/root/Applications/RoomMesh.app"
ditto "$ROOT/driver/build/RoomMesh.driver" "$DIST/root/Library/Audio/Plug-Ins/HAL/RoomMesh.driver"
if [ -n "${DEVELOPER_ID_APP:-}" ]; then
  for d in "$DIST/root/Library/Audio/Plug-Ins/HAL/RoomMesh.driver" "$DIST/root/Applications/RoomMesh.app/Contents/Resources/RoomMesh.driver"; do
    codesign --force --timestamp --options runtime --sign "$DEVELOPER_ID_APP" "$d"
  done
  codesign --force --timestamp --options runtime --entitlements "$ROOT/app/RoomMesh/Resources/RoomMesh.entitlements" \
    --sign "$DEVELOPER_ID_APP" "$DIST/root/Applications/RoomMesh.app"
fi
printf '#!/bin/sh\n/usr/bin/killall -9 coreaudiod || true\nexit 0\n' > "$DIST/scripts/postinstall"
chmod +x "$DIST/scripts/postinstall"
pkgbuild --root "$DIST/root" --scripts "$DIST/scripts" --identifier io.github.brohithkr.RoomMesh.pkg \
  --version "$VERSION" --install-location / "$DIST/RoomMesh-component.pkg"
if [ -n "${DEVELOPER_ID_INSTALLER:-}" ]; then
  productbuild --package "$DIST/RoomMesh-component.pkg" --sign "$DEVELOPER_ID_INSTALLER" "$DIST/RoomMesh-$VERSION.pkg"
else
  productbuild --package "$DIST/RoomMesh-component.pkg" "$DIST/RoomMesh-$VERSION.pkg"
fi
if [ -n "${NOTARY_PROFILE:-}" ]; then
  xcrun notarytool submit "$DIST/RoomMesh-$VERSION.pkg" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DIST/RoomMesh-$VERSION.pkg"
fi
echo "OK: $DIST/RoomMesh-$VERSION.pkg"
