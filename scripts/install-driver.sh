#!/usr/bin/env bash
# Installs RoomMesh.driver into /Library/Audio/Plug-Ins/HAL atomically, then restarts coreaudiod.
set -euo pipefail
SRC="${1:-$(cd "$(dirname "$0")/.." && pwd)/driver/build/RoomMesh.driver}"
HAL=/Library/Audio/Plug-Ins/HAL
DST="$HAL/RoomMesh.driver"
NEW="$HAL/RoomMesh.driver.new"
OLD="$HAL/RoomMesh.driver.old"
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
# Guard against wiping out an existing $DST with a bad/empty $SRC (e.g. a
# stale or partial build, or a typo'd path argument).
[ -d "$SRC/Contents/MacOS" ] || { echo "not a valid driver bundle (missing Contents/MacOS): $SRC"; exit 1; }
mkdir -p "$HAL"
# Stage a complete copy next to the destination (same volume, so the mv's below are renames),
# swap it in, then drop the old one: coreaudiod never sees a half-copied bundle, and a failed
# copy leaves the installed driver untouched.
rm -rf "$NEW" "$OLD"
ditto "$SRC" "$NEW"
chown -R root:wheel "$NEW"
if [ -e "$DST" ]; then mv "$DST" "$OLD"; fi
mv "$NEW" "$DST"
rm -rf "$OLD"
# launchd restarts coreaudiod; audio pauses ~1 s.
launchctl kickstart -k system/com.apple.audio.coreaudiod 2>/dev/null || killall -9 coreaudiod || true
echo "installed $DST"
