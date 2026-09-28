# RoomMesh HAL driver verification (Task 34)

Verification performed against the driver already installed on this Mac (`sudo scripts/install-driver.sh`
was run by the user beforehand — Step 1 of Task 34 is out of scope here). All commands were run from
`core/` with:

```sh
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
export MACOSX_DEPLOYMENT_TARGET=14.2
```

## Step 2 — devices present

```sh
$ system_profiler SPAudioDataType | grep -A8 RoomMesh
```

```
        RoomMesh Microphone:

          Input Channels: 1
          Manufacturer: RoomMesh
          Current SampleRate: 48000
          Transport: Virtual
          Input Source: Default

        RoomMesh Speaker:

          Manufacturer: RoomMesh
          Output Channels: 2
          Current SampleRate: 48000
          Transport: Virtual
          Output Source: Default
```

**PASS** — both devices present with the expected channel counts and sample rate.

## Step 3 — shared memory reachable

```sh
$ cargo run -p roommesh-devtool -- shm-status
magic=0x524d5348 version=1 rate=48000 generation=234726220387625
driver heartbeat age=274 ms, app heartbeat age=216657 ms
mic io active=0 speaker io active=0
driver alive: true
```

**PASS** — magic matches `0x524D5348` ('RMSH'), version/rate/ring size all validate inside
`SharedRegion::open` (which itself asserts these before returning), and `driver alive: true` (driver
heartbeat well under the 2s liveness threshold). `app heartbeat age` is large here because no app was
writing at the moment of this particular invocation — mic io/speaker io both show `0` clients since no
loopback was mid-flight.

`cargo run -p roommesh-devtool -- shm-selftest` (pure in-process ring test, no driver involved):

```
PASS shm-selftest
```

`cargo run -p roommesh-devtool -- devices` (confirms RoomMesh devices are correctly excluded from the
app-facing physical device list, per `device_io::is_roommesh_device`):

```
Nothing Ear (open)                       in=true out=false default=true
BlackHole 2ch                            in=true out=false default=false
MacBook Pro Microphone                   in=true out=false default=false
Nothing Ear (open)                       in=false out=true default=true
BlackHole 2ch                            in=false out=true default=false
MacBook Pro Speakers                     in=false out=true default=false
```

## Step 4 — end-to-end through CoreAudio

### speaker-loopback: PASS (energy=3023.9)

```sh
$ cargo run -p roommesh-devtool -- speaker-loopback
PASS speaker-loopback energy=3023.9
```

This exercises the full real path: devtool plays a 440 Hz tone into "RoomMesh Speaker" via cpal →
driver's `SpeakerIOHandler::OnWriteMixedOutput` downmixes and writes it into the speaker ring →
devtool's `SpeakerReader` reads it back out of shared memory. A PASS here confirms the shared-memory
contract, the driver's output IO handler, and device format negotiation (stereo → mono downmix) all
work correctly end-to-end on this Mac.

**Bug found and fixed while diagnosing an initial FAIL here:** the task-31.md reference implementation
for `speaker-loopback` calls `reader.read(1)` once, sleeps for the whole 2-second test, then drains
with a single `read(1 << 16)` loop. The speaker ring holds only ~680 ms of audio (`RING_FRAMES` =
32768 @ 48 kHz). Sleeping past that window before the first real read causes
`SpeakerReader::read`'s overrun-recovery logic (`w - *cur > RING_FRAMES - 4_800`) to snap the cursor
forward to the live edge, silently discarding all but the last ~20 ms of the 2 s tone. That produced a
real but tiny energy value (30.4, under the 100.0 threshold → misreported FAIL) even though the driver
and shared memory were working correctly. Fixed in `core/roommesh-devtool/src/main.rs` by draining the
ring continuously (every 20 ms) for the duration of the test instead of sleeping and bulk-draining
once. This is a devtool bug, not a driver bug — no driver code was touched.

### mic-loopback: FAIL (energy=0.0) — environment limitation, not a driver defect

```sh
$ cargo run -p roommesh-devtool -- mic-loopback
FAIL mic-loopback energy=0.0
```

**Root-cause investigation:**

1. Confirmed the shared-memory write path is correct: writing the test tone into the mic ring via
   `MicWriter::write` and reading it back in-process via `SharedRegion::test_driver_read_mic` (the same
   driver-side read the C++ driver performs) shows the expected ~440 Hz tone with energy ≈128 per 4096
   samples — the app→shm plumbing on the live `/roommesh.v1` region is correct.
2. Confirmed `shm-status` reports `mic io active=1` and a fresh (<10 ms) app heartbeat while the
   loopback runs, so the driver's IO-active and heartbeat-freshness gates in
   `driver/src/SharedRegion.cpp::ReadMic` should not be the blocker.
3. Confirmed the RoomMesh Microphone/Speaker `kAudioDevicePropertyVolumeScalar` are both already at
   `1.0` (unity gain) — ruled out a muted/zeroed volume control as the cause.
4. **Decisive test:** built a standalone cpal capture probe against the *physical* "MacBook Pro
   Microphone" (no RoomMesh involvement at all) using the same toolchain/process. It captured 2 seconds
   of audio (96,256 samples) that were **all exactly `0.0`** — not just quiet, but bit-for-bit zero,
   which is not physically possible for a live analog microphone in any environment (self-noise alone
   guarantees nonzero samples).

This shows the zero-energy result is **not specific to the RoomMesh driver or the devtool** — no audio
input at all is reaching cpal capture streams in this execution context (this agent's shell, several
process hops removed from an interactive Terminal.app window). This is consistent with a macOS TCC
microphone-permission gate: input HAL clients that lack (or whose responsible process lacks) granted
microphone consent are typically fed silence rather than an explicit error, for both physical and
virtual capture devices alike. The task brief's note that "Terminal already has microphone permission
(a cpal capture test passed earlier)" evidently does not carry over to this non-interactive process
tree.

**Everything the devtool and shared-memory contract can verify without live microphone capture has
been verified and passes** (shm-status, shm-selftest, the write side of mic-loopback, and the full
round trip on the speaker side). The remaining gap is specifically: a human running
`cargo run -p roommesh-devtool -- mic-loopback` from an interactive Terminal.app window with granted
microphone permission should re-run it to get a real PASS/FAIL signal on the input path; that
verification could not be completed from this environment. No driver or app code changes are indicated
by this finding — it is an environment/TCC characteristic of the current process, not a bug.

## Summary

| Check | Result |
|---|---|
| `system_profiler` shows both devices | PASS |
| `shm-status` (magic/version/rate, driver alive) | PASS |
| `shm-selftest` | PASS |
| `devices` (RoomMesh excluded from physical list) | PASS |
| `speaker-loopback` (after devtool fix) | PASS — energy=3023.9 |
| `mic-loopback` | FAIL — energy=0.0, but isolated to a TCC microphone-permission gap in this process tree, not the driver (see investigation above); the write side and shared-memory mechanics are independently verified correct |
