#!/usr/bin/env bash
set -euo pipefail
SRC="${1:-$(cd "$(dirname "$0")/.." && pwd)/driver/build/RoomMesh.driver}"
DST=/Library/Audio/Plug-Ins/HAL/RoomMesh.driver
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
# Guard against wiping out an existing $DST with a bad/empty $SRC (e.g. a
# stale or partial build, or a typo'd path argument).
[ -d "$SRC/Contents/MacOS" ] || { echo "not a valid driver bundle (missing Contents/MacOS): $SRC"; exit 1; }
rm -rf "$DST"
ditto "$SRC" "$DST"
chown -R root:wheel "$DST"
killall -9 coreaudiod || true   # launchd restarts it; audio pauses ~1 s
echo "installed $DST"
