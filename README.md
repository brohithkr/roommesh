# RoomMesh

RoomMesh turns every Mac in a meeting room into a node of a distributed room-audio system.
Any Mac can act as the **Coordinator** (aggregates the room's microphones, runs echo
cancellation, picks the best mic, and feeds the meeting); exactly one manually-chosen Mac is
the **Room Speaker**.

[`docs/architecture/overview.md`](docs/architecture/overview.md) is the original architecture,
kept verbatim. The implementation follows it with the amendments recorded in the implementation
plan, [`docs/superpowers/plans/2026-09-28-roommesh.md`](docs/superpowers/plans/2026-09-28-roommesh.md),
under "Decisions & amendments". Where the two differ, the amendments win. For example, every Mac
joins the meeting and roles gate the devices, the driver talks to the app through shared memory,
connections are encrypted, and coordinator failover is automatic. The security model is described
in [`docs/security.md`](docs/security.md).

## Prerequisites

- macOS 14.2 or later on Apple silicon (Intel builds: see [Universal builds](#universal-builds)).
- [Homebrew](https://brew.sh).
- **Full Xcode** from the App Store or developer.apple.com. The Command Line Tools alone are not
  enough. Select it and finish its first-launch setup once:

  ```sh
  sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
  sudo xcodebuild -runFirstLaunch
  ```

- The Rust toolchain and build tools. `make bootstrap` installs them: rustup (stable, with clippy
  and rustfmt, plus the `aarch64-apple-darwin` and `x86_64-apple-darwin` targets) and, through
  Homebrew, xcodegen, cmake, meson, ninja, pkg-config and opus.

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
`MACOSX_DEPLOYMENT_TARGET=14.2`. `make clean` removes every build output, including cargo's target
directory (`CARGO_TARGET_DIR` if you set it).

### Universal builds

By default the core and the app are built for **arm64 only**. The driver is always universal.
To also support Intel Macs, pass `UNIVERSAL=1` to any target:

```sh
make package UNIVERSAL=1
```

This builds the Rust core for `arm64` and `x86_64` (slow: WebRTC is compiled twice) and builds the
app for the generic macOS destination with `ARCHS='arm64 x86_64'`. `scripts/package.sh` checks with
`lipo` that the app and driver contain the expected architectures. The pkg's distribution file
declares the host architectures it supports: an arm64-only pkg has `hostArchitectures="arm64"` and
refuses to install on an Intel Mac.

## Install the driver

```sh
make install-driver
```

Copies `RoomMesh.driver` into `/Library/Audio/Plug-Ins/HAL` (requires `sudo`) and restarts
`coreaudiod`. This registers two virtual CoreAudio devices system-wide: **RoomMesh
Microphone** and **RoomMesh Speaker**. The copy is staged next to the destination and swapped in
with a rename, so a failed install leaves the previous driver in place. `make uninstall-driver`
removes it again.

## Meeting-app setup

On **every** Mac in the room, join the meeting (Google Meet, Zoom, ...) and select:

- **Microphone:** RoomMesh Microphone
- **Speaker:** RoomMesh Speaker

The app gates what each device actually carries based on the Mac's current role (coordinator
/ room speaker / neither) — role changes never require touching the meeting app's device
settings again.

**After installing or reinstalling the driver, quit and reopen the browser and meeting apps.**
Installing restarts macOS audio and recreates the RoomMesh devices; an app that was already
running (seen with Arc) keeps a stale view of them, and opening RoomMesh Microphone fails with
"Could not start audio source" / Meet refuses to unmute, while other microphones still work.

**Testing needs someone outside the room.** When every participant is a RoomMesh Mac in the
same room, nobody hears anything through the room speaker — by design: Meet sends each Mac
only the other participants' audio, the other room Macs send silence, and the people in the
room hear each other directly. Join from a phone (or a Mac with headphones, outside the room)
to hear the room and to be heard through the room speaker.

## Background noise and the noise baseline

The coordinator picks the active mic with a voice-activity detector that runs once per mic,
after echo cancellation and noise suppression. It tracks each mic's noise floor automatically,
and a frame only counts as speech when it is well above that floor.

**No mic is active until someone speaks.** In a silent room (a new room, or after the active mic
drops out) there is no active mic: RoomMesh Microphone carries silence and the app shows the
active mic as "—". The first mic that hears speech becomes active. After that, the last talker's
mic stays selected through silence until another mic clearly takes over. (Before, a silent room
picked the best-scoring mic, which in practice was the one with the least background noise.)

**Settings → Audio → Background noise** shows what the room hears from this Mac's mic. While
the tab is visible the app runs the mic through the same processing (and opens the mic even when
you aren't in a room). The meter spans −90…−20 dB:

- the bar is the processed level, and the thin tick is its recent peak;
- the vertical marker is the effective baseline;
- above the marker the bar is blue while it counts as speech, and orange when it is heard but
  isn't speech; below the marker it is grey (background, ignored).

The baseline is either **Automatic** (the detector's own floor, shown underneath) or a **Custom
baseline** (−90…−30 dB). With a custom baseline the floor is the higher of the two, so anything
at or below it never selects a mic, which helps against a noisy fan or chatter from the next
room. **Measure room noise** listens to this Mac's mic for 5 s while the room is quiet and sets
the baseline to the 95th percentile of the level plus 3 dB.

**The baseline belongs to the room, not to a Mac.** One baseline applies to every mic in the
room, so no Mac becomes the default active mic just because it has a lower baseline or a
quieter mic.

- **In a room**, the section reads "Background noise — *room name*". The picker, the slider and
  **Measure room noise** change the room's baseline for every Mac in it. Any member can change
  it. The value lives in the room manifest (`RoomManifest.noise_baseline_db`), set through the
  coordinator with `ChangeRequest::SetNoiseBaseline`, and it survives a coordinator handover.
  A change from another Mac shows up in the section, but it doesn't move the slider while you
  are dragging it. The meter uses the room's baseline.
- **Outside a room**, the section edits this Mac's own default. A room you create starts with it,
  and the meter uses it.

The default is Automatic. It lives in `DEFAULT_NOISE_BASELINE_DB`
(`core/roommesh-core/src/dsp/vad.rs`) and `SettingsStore.defaultNoiseBaselineDb`
(`app/RoomMesh/App/SettingsStore.swift`); change both together. The room baseline changed
the control protocol (version 5), so builds from before it can't connect to newer ones. To measure a room through the same chain from the command line, run
`cargo run -p roommesh-devtool -- noise-probe` from `core/` (in Terminal.app, with microphone
permission).

## Verifying the audio path (roommesh-devtool)

Run these from `core/`:

```sh
cargo run -p roommesh-devtool -- shm-status        # driver heartbeat, IO clients, liveness
cargo run -p roommesh-devtool -- shm-selftest      # in-process ring round trip, no driver needed
cargo run -p roommesh-devtool -- speaker-loopback  # tone into RoomMesh Speaker, read back from shm
cargo run -p roommesh-devtool -- mic-loopback      # tone into shm, captured from RoomMesh Microphone
```

The tool exits 0 on PASS, 1 on FAIL or error, and 2 on a usage error (`--help` lists the commands).

**Quit RoomMesh first before running the loopbacks.** They write to and read from the same shared
memory as the app, so they refuse to run while the app's heartbeat is fresh or a client is capturing
from RoomMesh Microphone. `--force` overrides this, but results taken with the app running are
not meaningful.

`mic-loopback` writes a tone into the mic ring and captures it from the RoomMesh Microphone through
`cpal`, to confirm the shared-memory path is live end to end.

**Run it from an interactive Terminal.app window that has been granted microphone
permission** (System Settings → Privacy & Security → Microphone). macOS TCC silently feeds
zero-filled audio to capture clients running in a process tree without granted mic consent
(non-Terminal shells, editor-embedded terminals, CI, etc.) rather than raising an error, which
reads as a driver failure but isn't one. When that happens, `mic-loopback` reports "callbacks
received but all samples exactly 0" and points you to Terminal.app. `speaker-loopback` and
`shm-status`/`shm-selftest` don't need microphone consent and can be run anywhere. See
[`docs/testing/driver-verification.md`](docs/testing/driver-verification.md) for the recorded
results, and [`docs/testing/manual-e2e.md`](docs/testing/manual-e2e.md) for the end-to-end
checklist.

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
make package   # -> dist/RoomMesh-1.0.0.pkg and dist/RoomMesh-1.0.0.dmg
make pkg       # just the installer package
make dmg       # the package, wrapped in a disk image
```

`scripts/package.sh` builds a guided Installer.app package. `scripts/make-dmg.sh` wraps it in a
disk image. The wizard's pages, install scripts and distribution template are in
[`installer/`](installer).

### What the user sees

Double-clicking `RoomMesh-1.0.0.dmg` mounts a **RoomMesh Installer** volume that contains
**Install RoomMesh.pkg** and **Uninstall RoomMesh.command**. Opening the pkg starts Installer.app
with these steps:

1. **Introduction**: what RoomMesh is and what will be installed.
2. **Read Me**: requirements (macOS 14.2 or later, Apple silicon unless built with `UNIVERSAL=1`),
   what the audio driver does, and the steps after installing (grant Microphone and Local Network
   access; select RoomMesh Microphone and RoomMesh Speaker in the meeting app on every Mac).
3. **License**: currently a placeholder (see below).
4. **Destination Select**: the package installs only on the startup disk, so Installer usually
   goes straight through this step.
5. **Installation Type**: two checkboxes, shown straight away:
   - **RoomMesh App** (`io.github.brohithkr.RoomMesh.app.pkg`: `RoomMesh.app` in `/Applications`). This is
     always installed, so its checkbox is disabled.
   - **RoomMesh Audio Driver** (`io.github.brohithkr.RoomMesh.driver.pkg`: `RoomMesh.driver` in
     `/Library/Audio/Plug-Ins/HAL`). It is selected by default and can be deselected; the app can
     install it later.
6. **Installation**: asks for an administrator password. The app's preinstall script quits a
   running RoomMesh, and its postinstall opens RoomMesh for the logged-in user. The driver's
   postinstall restarts `coreaudiod` (audio pauses for about a second).
7. **Summary**: next steps and how to uninstall.

The pkg refuses to install on macOS older than 14.2 and shows a message saying why. An arm64-only
build (the default) also declares `hostArchitectures="arm64"`, so Installer refuses it on an Intel
Mac. Both components are non-relocatable, so they always install to the paths above, even if
another copy of the app exists elsewhere on the disk.

**Before distributing:** `installer/resources/license.html` is a placeholder. Replace it with the
real license text.

### Signing

By default the app and driver are **ad-hoc signed** and the **pkg and dmg are unsigned**. That is
fine for local testing, but Gatekeeper blocks it on other Macs. Set these env vars to sign and
notarize:

| Variable | Effect |
|---|---|
| `DEVELOPER_ID_APP` | codesigns the app, the driver and the dmg with this Developer ID Application identity |
| `DEVELOPER_ID_INSTALLER` | signs the `.pkg` with this Developer ID Installer identity |
| `NOTARY_PROFILE` | notarizes the pkg and the dmg with this `xcrun notarytool` keychain profile, then staples the tickets. Requires both identities above |

The driver is signed once, and that same copy goes both into the driver component and into the
app's `Contents/Resources`. The app compares the two by hash, so byte-identical copies stop a
freshly installed app from asking to reinstall its driver. The script then runs
`codesign --verify --strict --deep` on the app and the driver. It also runs `spctl -a -t install`
on a signed pkg, which fails until the pkg is notarized.

## Uninstall

If you installed from the dmg, double-click **Uninstall RoomMesh.command** on the RoomMesh
Installer volume. It removes the app, the driver and the installer receipts, then restarts
`coreaudiod`. To do the same by hand:

1. Quit RoomMesh.
2. Remove the driver and restart `coreaudiod`:

   ```sh
   make uninstall-driver          # or: sudo scripts/uninstall-driver.sh
   ```

3. Remove the app and the installer receipts:

   ```sh
   sudo rm -rf /Applications/RoomMesh.app
   sudo pkgutil --forget io.github.brohithkr.RoomMesh.app.pkg
   sudo pkgutil --forget io.github.brohithkr.RoomMesh.driver.pkg
   sudo pkgutil --forget io.github.brohithkr.RoomMesh.pkg   # receipt from pre-wizard builds, if present
   ```

4. Optionally, remove its settings: `defaults delete io.github.brohithkr.RoomMesh`. A second-instance
   profile has its own domain, for example `io.github.brohithkr.RoomMesh.b`.

## License

RoomMesh is free software, licensed under the [GNU General Public License v3.0](LICENSE)
(GPL-3.0). It builds on third-party components under permissive licenses that are compatible
with GPL-3.0:

- [libASPL](https://github.com/gavv/libASPL) (the driver's HAL plug-in framework): MIT
- [WebRTC Audio Processing](https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing)
  (echo cancellation and noise suppression): BSD-3-Clause
- [libopus](https://opus-codec.org): BSD-3-Clause
- [UniFFI](https://github.com/mozilla/uniffi-rs) (the Rust ↔ Swift bindings): MPL-2.0
- [cpal](https://github.com/RustAudio/cpal) (audio device access): Apache-2.0

[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists them with their copyright notices, along
with the other Rust crates the core depends on.
