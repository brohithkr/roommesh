#!/usr/bin/env bash
# Wraps dist/RoomMesh-<version>.pkg (from scripts/package.sh) in dist/RoomMesh-<version>.dmg, a
# "RoomMesh Installer" volume holding "Install RoomMesh.pkg" and "Uninstall RoomMesh.command".
# Optional env: DEVELOPER_ID_APP signs the dmg; NOTARY_PROFILE also notarizes and staples it.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$ROOT/app/RoomMesh/Resources/Info.plist")
DIST="$ROOT/dist"
PKG="$DIST/RoomMesh-$VERSION.pkg"
DMG="$DIST/RoomMesh-$VERSION.dmg"
UNINSTALLER="$ROOT/installer/Uninstall RoomMesh.command"
STAGE="$DIST/build/dmg"

die() { echo "error: $*" >&2; exit 1; }

[ -f "$PKG" ] || die "$PKG not found (run scripts/package.sh or make pkg)"
if [ -n "${NOTARY_PROFILE:-}" ] && [ -z "${DEVELOPER_ID_APP:-}" ]; then
  die "NOTARY_PROFILE needs DEVELOPER_ID_APP to sign the dmg"
fi

rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
ditto "$PKG" "$STAGE/Install RoomMesh.pkg"
if [ -f "$UNINSTALLER" ]; then
  ditto "$UNINSTALLER" "$STAGE/Uninstall RoomMesh.command"
  chmod +x "$STAGE/Uninstall RoomMesh.command"
else
  echo "warning: $UNINSTALLER is missing; the dmg will not contain an uninstaller" >&2
fi

hdiutil create -volname "RoomMesh Installer" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
rm -rf "$STAGE"

if [ -n "${DEVELOPER_ID_APP:-}" ]; then
  codesign --force --timestamp --sign "$DEVELOPER_ID_APP" "$DMG"
  codesign --verify --strict "$DMG"
fi
if [ -n "${NOTARY_PROFILE:-}" ]; then
  xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
  spctl -a -vv -t open --context context:primary-signature "$DMG"
fi
echo "OK: $DMG"
