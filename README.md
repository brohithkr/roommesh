# RoomMesh

RoomMesh turns every Mac in a meeting room into a node of a distributed room-audio system.
Any Mac can act as the **Coordinator** (aggregates the room's microphones, runs echo
cancellation, picks the best mic, and feeds the meeting); exactly one manually-chosen Mac is
the **Room Speaker**. See [`docs/architecture/overview.md`](docs/architecture/overview.md) for
the full design.

## Build

```sh
make bootstrap    # one-time: installs toolchains (rustup, cmake, ninja, xcodegen, ...)
make test         # cargo test + clippy, driver ctest, Xcode unit tests
make app          # builds app/build/Build/Products/Release/RoomMesh.app
```

`make` targets build incrementally in order: `core` (Rust static lib + generated Swift
bindings) → `driver` (the `RoomMesh.driver` HAL plug-in) → `project` (xcodegen) → `app`
(Release build via `xcodebuild`). Every build step resolves the macOS SDK through `xcrun`
(the Command Line Tools' default SDK can be broken) and pins
`MACOSX_DEPLOYMENT_TARGET=14.2`.

## Install the driver

```sh
make install-driver
```

Copies `RoomMesh.driver` into `/Library/Audio/Plug-Ins/HAL` (requires `sudo`) and restarts
`coreaudiod`. This registers two virtual CoreAudio devices system-wide: **RoomMesh
Microphone** and **RoomMesh Speaker**.

## Meeting-app setup

On **every** Mac in the room, join the meeting (Google Meet, Zoom, ...) and select:

- **Microphone:** RoomMesh Microphone
- **Speaker:** RoomMesh Speaker

The app gates what each device actually carries based on the Mac's current role (coordinator
/ room speaker / neither) — role changes never require touching the meeting app's device
settings again.

## Verifying the audio path (mic-loopback)

`cargo run -p roommesh-devtool -- mic-loopback` (from `core/`) captures from the RoomMesh
Microphone via `cpal` to confirm the shared-memory path is live end to end.

**Run it from an interactive Terminal.app window that has been granted microphone
permission** (System Settings → Privacy & Security → Microphone). macOS TCC silently feeds
zero-filled audio to capture clients running in a process tree without granted mic consent
(non-Terminal shells, editor-embedded terminals, CI, etc.) rather than raising an error, which
reads as a driver failure but isn't one. `speaker-loopback` and `shm-status`/`shm-selftest`
don't need microphone consent and can be run anywhere.

## Local two-instance test

Before testing on multiple physical Macs, you can sanity-check the control plane (discovery,
invite/join, coordinator/speaker election) with two app instances on one Mac. Each instance
needs its own identity, so the second one runs under a separate `UserDefaults` profile:

```sh
open app/build/Build/Products/Release/RoomMesh.app
ROOMMESH_PROFILE=b open -n app/build/Build/Products/Release/RoomMesh.app
```

The two instances should discover each other under "Nearby Macs" within a few seconds; from
there you can drive the full invite/join/failover flow between them before moving to real
hardware.

## Packaging

```sh
make package   # -> dist/RoomMesh-1.0.0.pkg
```

Builds an installer that places `RoomMesh.app` in `/Applications` and `RoomMesh.driver` in
`/Library/Audio/Plug-Ins/HAL`, with a postinstall script that restarts `coreaudiod`. Unsigned
by default; set these env vars to sign/notarize:

| Variable | Effect |
|---|---|
| `DEVELOPER_ID_APP` | codesigns the app and driver with this Developer ID Application identity |
| `DEVELOPER_ID_INSTALLER` | signs the `.pkg` with this Developer ID Installer identity |
| `NOTARY_PROFILE` | submits the signed `.pkg` to notarization using this `xcrun notarytool` keychain profile, then staples the ticket |
