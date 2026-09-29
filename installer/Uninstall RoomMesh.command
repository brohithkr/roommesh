#!/bin/bash
# Removes RoomMesh.app, the RoomMesh audio driver and the installer receipts, then restarts
# macOS audio. Double-click it in the RoomMesh Installer volume; it asks for an admin password.
set -u

APP=/Applications/RoomMesh.app
DRIVER=/Library/Audio/Plug-Ins/HAL/RoomMesh.driver

echo "This removes RoomMesh from this Mac:"
echo "  $APP"
echo "  $DRIVER"
echo "macOS audio restarts for about a second."
printf "Continue? [y/N] "
read -r answer
case "$answer" in
  [yY] | [yY][eE][sS]) ;;
  *) echo "Nothing was removed."; exit 0 ;;
esac

/usr/bin/pkill -x RoomMesh 2>/dev/null || true

/usr/bin/sudo /bin/sh -c '
  rm -rf "$1" "$2" "$2.new" "$2.old"
  for id in io.github.brohithkr.RoomMesh.app.pkg io.github.brohithkr.RoomMesh.driver.pkg io.github.brohithkr.RoomMesh.pkg; do
    /usr/sbin/pkgutil --forget "$id" >/dev/null 2>&1 || true
  done
  /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod 2>/dev/null \
    || /usr/bin/killall -9 coreaudiod 2>/dev/null || true
' sh "$APP" "$DRIVER" || { echo "Uninstall failed."; exit 1; }

echo "RoomMesh has been removed. Your settings stay in ~/Library/Preferences/io.github.brohithkr.RoomMesh.plist."
