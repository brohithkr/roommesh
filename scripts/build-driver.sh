#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cmake -S "$ROOT/driver" -B "$ROOT/driver/build" -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build "$ROOT/driver/build"
ctest --test-dir "$ROOT/driver/build" --output-on-failure
codesign --force --sign "${DEVELOPER_ID_APP:--}" --timestamp=none "$ROOT/driver/build/RoomMesh.driver"
echo "OK: $ROOT/driver/build/RoomMesh.driver"
