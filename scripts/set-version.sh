#!/usr/bin/env bash
# Sets the app's version in app/RoomMesh/Resources/Info.plist:
#   CFBundleShortVersionString  <marketing-version>   X.Y.Z (semver core; no pre-release suffix,
#                                                     macOS requires three integers)
#   CFBundleVersion             [build-number]        a positive integer; left unchanged if omitted
# The driver (driver/CMakeLists.txt reads the same Info.plist) and the pkg/dmg names
# (scripts/package.sh, scripts/make-dmg.sh) pick the version up from there.
#
#   scripts/set-version.sh 1.1.0          # marketing version only
#   scripts/set-version.sh 1.1.0 42       # and the build number
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLIST="$ROOT/app/RoomMesh/Resources/Info.plist"
PB=/usr/libexec/PlistBuddy

die() { echo "error: $*" >&2; exit 1; }
usage() { echo "usage: $0 <X.Y.Z> [build-number]" >&2; exit 2; }

[ $# -ge 1 ] && [ $# -le 2 ] || usage
VERSION=$1
BUILD=${2:-}

# No leading zeros (semver), each part at most 9 digits so it stays a sane integer.
PART='(0|[1-9][0-9]{0,8})'
[[ "$VERSION" =~ ^$PART\.$PART\.$PART$ ]] ||
  die "version '$VERSION' is not a semver X.Y.Z (e.g. 1.1.0; pre-release suffixes are not allowed)"
if [ -n "$BUILD" ]; then
  [[ "$BUILD" =~ ^[1-9][0-9]{0,8}$ ]] || die "build number '$BUILD' is not a positive integer"
fi
[ -f "$PLIST" ] || die "$PLIST not found"

"$PB" -c "Set :CFBundleShortVersionString $VERSION" "$PLIST"
if [ -n "$BUILD" ]; then "$PB" -c "Set :CFBundleVersion $BUILD" "$PLIST"; fi
plutil -lint "$PLIST" >/dev/null

GOT_VERSION=$("$PB" -c 'Print :CFBundleShortVersionString' "$PLIST")
GOT_BUILD=$("$PB" -c 'Print :CFBundleVersion' "$PLIST")
[ "$GOT_VERSION" = "$VERSION" ] || die "CFBundleShortVersionString is '$GOT_VERSION' after setting '$VERSION'"
[ -z "$BUILD" ] || [ "$GOT_BUILD" = "$BUILD" ] || die "CFBundleVersion is '$GOT_BUILD' after setting '$BUILD'"
echo "RoomMesh version $GOT_VERSION (build $GOT_BUILD) in $PLIST"
