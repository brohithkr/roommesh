#!/usr/bin/env bash
# Removes RoomMesh.driver from /Library/Audio/Plug-Ins/HAL, then restarts coreaudiod.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
HAL=/Library/Audio/Plug-Ins/HAL
rm -rf "$HAL/RoomMesh.driver" "$HAL/RoomMesh.driver.new" "$HAL/RoomMesh.driver.old"
# launchd restarts coreaudiod; audio pauses ~1 s.
launchctl kickstart -k system/com.apple.audio.coreaudiod 2>/dev/null || killall -9 coreaudiod || true
echo "removed $HAL/RoomMesh.driver"
