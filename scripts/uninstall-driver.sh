#!/usr/bin/env bash
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
rm -rf /Library/Audio/Plug-Ins/HAL/RoomMesh.driver
killall -9 coreaudiod || true   # launchd restarts it; audio pauses ~1 s
