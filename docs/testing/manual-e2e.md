# RoomMesh manual end-to-end verification

A manual checklist, run by a person. Start with two app instances on one Mac, then test a real room of
2–4 Macs. Mark each row **✓** (pass) or **✗** (fail) in the Result column. Add details in Notes, and open
an issue for every ✗ (link it in Notes).

Copy the results template at the end for each test run.

## Before you start

- Build the app (`make app`), or build and install the package (`make package`, then open
  `dist/RoomMesh-1.0.0.pkg`).
- For Step 2, check the driver on every Mac with [`driver-verification.md`](driver-verification.md):
  both devices appear in `system_profiler SPAudioDataType`, and `roommesh-devtool shm-status` reports
  `driver alive: true`.

## Step 1: local two-instance control-plane test (one Mac)

Start two instances. The second one uses a separate profile, so it gets its own identity:

```sh
open app/build/Build/Products/Release/RoomMesh.app
ROOMMESH_PROFILE=b open -n app/build/Build/Products/Release/RoomMesh.app
```

| # | Check | Result (✓/✗) | Notes |
|---|---|---|---|
| 1.1 | Each instance lists the other under **Nearby Macs** within ~5 s (grant Local Network access when prompted). | | |
| 1.2 | **Connect** on instance A makes B show "Join “…'s Room”?" with a 6-digit code identical to the code in A's Invite sheet. | | |
| 1.3 | After **Join**, both windows show 2 Macs, coordinator = A, room speaker = A. | | |
| 1.4 | Changing the coordinator to B from A's menu makes both show B within 1 s. Settings → Advanced shows RTT/offset for the peer. | | |
| 1.5a | Quitting B (⌘Q) makes A show B as offline. | | |
| 1.5b | With auto-failover off in A's settings and B as coordinator, quit A instead: B prompts "Coordinator disconnected. Select new coordinator:". | | |
| 1.6 | **Leave Room** returns both instances to the no-room state. | | |

## Step 2: real room test (2–4 Macs)

Install `dist/RoomMesh-1.0.0.pkg` on each Mac. Every Mac joins the same Google Meet with
**Microphone: RoomMesh Microphone** and **Speaker: RoomMesh Speaker**. A remote participant joins from a
phone.

| # | Check | Result (✓/✗) | Notes |
|---|---|---|---|
| 2.1 | Every Mac joins the Meet with the RoomMesh devices selected, and the phone participant joins. | | |
| 2.2 | Far-end (phone) audio plays **only** from the selected room-speaker Mac. | | |
| 2.3a | Speaking next to each Mac moves **Active Mic** to that Mac within about 0.5 s. | | |
| 2.3b | Two people alternating quickly do not cause flapping (hold ≥ 0.6 s). | | |
| 2.4 | With the phone talking and nobody in the room talking, the phone hears **no echo** (Diagnostics AEC ERLE > 15 dB after ~5 s). | | |
| 2.5 | Changing the room speaker mid-call moves playback, and AEC re-converges within a few seconds. | | |
| 2.6 | Changing the coordinator mid-call resumes room audio to the phone within ~2 s. Meet settings stay untouched. | | |
| 2.7 | Quitting the coordinator app triggers automatic failover (or the prompt, if disabled), and audio resumes. | | |
| 2.8 | Quitting the room-speaker app shows the "Room speaker disconnected. Select another speaker:" prompt. No other Mac starts playing on its own. | | |
| 2.9a | **Mute My Microphone** on one Mac removes it from arbitration. | | |
| 2.9b | Turning **Use This Mac's Microphone** off removes it for everyone. | | |
| 2.10 | With **Allow simultaneous talkers** on, the phone hears two people in different corners who talk at once. | | |

## Step 3: record results

Fill in a copy of the template below for each run. Attach Diagnostics screenshots (AEC ERLE, Active
Mic, RTT/offset) next to this file or link them. Open issues for any ✗.

---

## Results template

**Date:**
**Tester:**
**RoomMesh version / commit:**
**Driver version / commit:**
**Meeting app and remote client:** (for example Google Meet in Chrome, phone on the Meet app)
**Network:** (Wi-Fi SSID/band, same subnet?)

### Macs

| Mac | Model | Chip | macOS version | Role(s) during the test | Notes |
|---|---|---|---|---|---|
| A | | | | | |
| B | | | | | |
| C | | | | | |
| D | | | | | |

### Summary

| Step | Passed | Failed | Issues opened |
|---|---|---|---|
| 1: two-instance control plane | | | |
| 2: real room | | | |

### Diagnostics screenshots

-

### Notes

-
