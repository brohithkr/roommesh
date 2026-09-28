#!/usr/bin/env bash
set -euo pipefail
SRC="${1:-$(cd "$(dirname "$0")/.." && pwd)/driver/build/RoomMesh.driver}"
DST=/Library/Audio/Plug-Ins/HAL/RoomMesh.driver
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
rm -rf "$DST"
ditto "$SRC" "$DST"
chown -R root:wheel "$DST"
killall -9 coreaudiod || true   # launchd restarts it; audio pauses ~1 s
echo "installed $DST"
