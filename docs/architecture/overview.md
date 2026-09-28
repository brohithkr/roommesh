# Distributed Room Audio — Final Architecture

## 1. Product Model

There is **one macOS application** installed on every participating Mac.

Every installation contains the same capabilities:

- peer discovery;
- pairing and room membership;
- microphone capture;
- speaker playback;
- coordinator functionality;
- audio processing;
- virtual audio-device integration;
- room controls.

There is no separate coordinator application and no separate client application.

Every installation is a peer. One peer is temporarily selected as the **Coordinator** for a room.

```text
Mac A ─┐
Mac B ─┼── Same application
Mac C ─┤
Mac D ─┘

Room state:

Coordinator: Mac B
Fixed speaker: Mac C
Microphones: A, B, C, D
```

The coordinator role can move to another peer without installing or launching different software.

---

# 2. Technology Stack

The application should use:

```text
Swift / SwiftUI
        │
        │ Native macOS application shell
        │ UI
        │ menu bar
        │ permissions
        │ app lifecycle
        ▼

┌──────────────────────┐
│      Rust Core       │
│                      │
│ Networking logic     │
│ Room state           │
│ Audio transport      │
│ Clock sync           │
│ Jitter buffers       │
│ AEC orchestration    │
│ VAD                   │
│ Mic scoring          │
│ Mic arbitration      │
│ Audio mixing         │
└──────────────────────┘
```

## Swift responsibilities

Swift should own the parts tightly coupled to macOS:

- SwiftUI interface;
- menu-bar application UI;
- native settings;
- microphone permissions;
- local-network permissions;
- notifications;
- application lifecycle;
- launch-at-login integration;
- system audio-device selection;
- macOS-specific APIs;
- bridging the Rust core into the application.

## Rust responsibilities

Rust should implement the reusable processing/backend core:

- peer protocol;
- room protocol;
- transport abstraction;
- coordinator logic;
- room-state replication;
- audio packetization;
- realtime networking;
- sequence handling;
- jitter buffering;
- clock-offset estimation;
- clock-drift correction;
- resampling;
- audio stream synchronization;
- AEC pipeline coordination;
- voice activity detection;
- microphone quality scoring;
- active-microphone arbitration;
- crossfading;
- multi-speaker local mixing;
- telemetry and connection-quality calculations.

The architectural boundary should look approximately like:

```text
┌─────────────────────────────┐
│        Swift / SwiftUI      │
│                             │
│ Menu bar                    │
│ Main window                 │
│ Settings                    │
│ macOS integration           │
└─────────────┬───────────────┘
              │
          Rust bridge
              │
┌─────────────▼───────────────┐
│          Rust Core          │
│                             │
│ Room Engine                 │
│ Peer Engine                 │
│ Transport                   │
│ Audio Engine                │
│ Coordinator Engine          │
└─────────────────────────────┘
```

The Rust core should avoid depending directly on UI concepts.

For example:

```rust
RoomState
Peer
Coordinator
Transport
AudioStream
MicScore
PlaybackEndpoint
```

rather than:

```rust
MenuItem
SwiftWindow
Button
Popover
```

This keeps the core testable and eventually makes it possible to reuse it on other platforms.

---

# 3. Swift ↔ Rust Boundary

Keep the FFI boundary relatively high-level.

Avoid making thousands of tiny Swift-to-Rust calls for individual audio samples.

Prefer operations such as:

```text
create_room()
join_room()
leave_room()

get_room_state()

set_coordinator(peer_id)
set_speaker(peer_id)

set_peer_microphone_enabled(peer_id, bool)

start_audio_engine()
stop_audio_engine()

get_peer_metrics()
get_active_microphones()
```

Audio itself should stay inside the Rust/native audio pipeline as much as practical.

The UI should receive events such as:

```text
PeerJoined
PeerLeft
CoordinatorChanged
SpeakerChanged
ActiveMicChanged
ConnectionQualityChanged
AECStatusChanged
```

rather than polling continuously.

---

# 4. Peer Discovery

Use Apple's modern networking stack:

```text
Network.framework
```

with peer-to-peer support enabled.

Every application simultaneously knows how to:

```text
advertise
browse
connect
accept connections
```

Discovery should use Bonjour-style service discovery.

For example:

```text
_roomaudio._udp
```

Nearby instances appear automatically.

```text
Nearby

Rohith's MacBook
Amaan's MacBook
Puyan's MacBook
Meeting Mac mini
```

---

# 5. Transport Abstraction

Do not make the audio engine depend directly on Apple's P2P implementation.

Rust should expose something conceptually similar to:

```rust
trait PeerTransport {
    fn discover(...);
    fn connect(...);
    fn disconnect(...);

    fn send_control(...);
    fn send_realtime(...);
}
```

Initial implementation:

```text
PeerTransport
      │
      ▼
AppleP2PTransport
      │
      ▼
Network.framework
      │
      ▼
Apple peer-to-peer networking
```

Future implementation:

```text
PeerTransport
   ├── AppleP2PTransport
   └── LANTransport
```

Potentially later:

```text
PeerTransport
   ├── Apple P2P
   ├── LAN / Wi-Fi
   ├── Ethernet
   └── other transports
```

The room and audio layers should not care which transport is being used.

---

# 6. Room Creation and Pairing

A user creates a room:

```text
Create Room
```

Nearby peers can be invited.

```text
Conference Room

✓ Rohith's MacBook
✓ Amaan's MacBook
✓ Puyan's MacBook
```

Invited devices receive:

```text
Join "Conference Room"?

[Decline] [Join]
```

Once accepted, every device shares the same logical room membership.

Pairing should therefore be modeled around:

```text
Peer ∈ Room
```

rather than arbitrary individual pair relationships.

---

# 7. Shared Room State

Every peer receives a replicated room manifest.

Example:

```text
RoomManifest

roomID: 72F4
epoch: 18

members:
    A
    B
    C
    D

coordinator:
    B

speaker:
    C
```

Important configuration includes:

- room ID;
- room epoch;
- members;
- coordinator;
- selected speaker;
- enabled microphones;
- protocol version;
- capabilities.

---

# 8. Coordinator Selection

Every peer is capable of becoming coordinator.

The room UI allows:

```text
Coordinator

○ Mac A
● Mac B
○ Mac C
○ Mac D
```

Once B is selected, every peer begins treating B as the authority for realtime room coordination.

The role controls:

- microphone stream aggregation;
- AEC processing;
- microphone arbitration;
- meeting bridge;
- playback routing;
- synchronization.

The distinction remains runtime-only:

```text
same binary
same Rust core
same Swift app

different temporary role
```

---

# 9. Split-Brain Protection

Coordinator assignments should use monotonically increasing room epochs.

For example:

```text
Epoch 41
Coordinator B

Epoch 42
Coordinator D
```

After accepting epoch 42:

```text
commands from D / epoch 42 → valid
commands from B / epoch 41 → ignored
```

Authority is therefore identified by:

```text
roomID
+
roomEpoch
+
coordinatorID
```

---

# 10. Realtime Topology

Discovery is peer-to-peer.

Realtime audio should use a **star topology around the currently selected coordinator**.

If B is coordinator:

```text
Mac A mic ─────┐
Mac C mic ─────┼──► Mac B
Mac D mic ─────┘      Coordinator
```

B processes its own microphone locally.

This avoids sending every microphone stream to every peer.

---

# 11. Control and Realtime Channels

Use separate logical channels.

## Reliable control channel

For:

```text
pairing
membership
coordinator changes
speaker selection
capabilities
configuration
heartbeats
mute state
protocol negotiation
```

Reliability matters more than latency.

## Realtime channel

For:

```text
microphone frames
speaker playback frames
timestamps
playback timing
```

Latency matters more than retransmission.

A late microphone packet is normally less useful than simply dropping it.

Every realtime packet should include data such as:

```text
streamID
peerID
sequenceNumber
sampleIndex
timestamp
frameCount
```

---

# 12. Distributed Microphones

Every enabled peer continuously captures microphone audio.

```text
Mac A 🎤 ──────┐
Mac B 🎤 ──────┤
Mac C 🎤 ──────┼──► Coordinator
Mac D 🎤 ──────┘
```

The coordinator receives one logical microphone stream for every participating device.

---

# 13. Fixed Speaker

There is exactly **one room speaker at a time**.

It is manually selected:

```text
Room Speaker

○ Mac A
○ Mac B
● Mac C
○ Mac D
```

Once selected, it stays selected until changed by a user.

Microphone selection can change continuously:

```text
A → B → B → C → D → A
```

while the speaker remains:

```text
C → C → C → C → C → C
```

There is deliberately **no automatic speaker jumping**.

---

# 14. Meeting Playback

Remote meeting audio enters the coordinator.

```text
Meet / Zoom / Teams
        │
        ▼
Distributed Room Output
        │
        ▼
Coordinator
```

The coordinator uses the signal for two things:

```text
                     remote audio
                          │
              ┌───────────┴──────────┐
              │                      │
              ▼                      ▼
        AEC reference          selected speaker
                                      │
                                      ▼
                                   Mac C
                                     🔊
```

Only Mac C produces the far-end meeting audio.

All other speakers remain silent.

---

# 15. Echo Cancellation

Every microphone can hear the selected room speaker.

```text
                 Mac C 🔊
              )))     )))
            ))           ))
        🎤 A      🎤 B      🎤 D
```

Each incoming microphone therefore gets an independent AEC processor.

```text
Remote playback ───────┐
                       ▼
Mic A ──────────────► AEC A ─► Clean A

Remote playback ───────┐
                       ▼
Mic B ──────────────► AEC B ─► Clean B

Remote playback ───────┐
                       ▼
Mic C ──────────────► AEC C ─► Clean C

Remote playback ───────┐
                       ▼
Mic D ──────────────► AEC D ─► Clean D
```

Each AEC learns a different acoustic path.

Because the speaker remains fixed, those paths remain comparatively stable.

---

# 16. Clock Synchronization

Every Mac has its own hardware audio clock.

The coordinator needs to compensate for:

```text
clock offset
clock drift
network delay
jitter
```

Each stream therefore carries:

```text
sample index
capture timestamp
sequence number
```

The coordinator continuously estimates timing relationships between itself and every peer.

It then maps all microphone streams onto a common audio timeline.

---

# 17. Jitter Buffers

Every network microphone receives a small adaptive jitter buffer.

Example:

```text
Mac A → 14 ms
Mac C → 18 ms
Mac D → 12 ms
```

Buffers should remain as small as possible while still providing stable playback to the DSP pipeline.

The system is optimizing for:

```text
low latency
+
stable timing
+
sufficient synchronization
```

rather than perfect lossless transport.

---

# 18. Selecting the Best Microphone

After AEC, each microphone is analyzed.

Metrics can include:

```text
voice activity
speech probability
SNR
signal level
noise floor
direct-to-reverberant characteristics
clipping
historical quality
```

For example:

```text
Mac A = 0.27
Mac B = 0.93
Mac C = 0.41
Mac D = 0.22
```

Mac B becomes the active microphone.

The algorithm should really optimize for:

```text
best capture of the current speaker
```

rather than simply:

```text
loudest microphone
```

---

# 19. Hysteresis

Prevent rapid microphone switching.

Suppose:

```text
Current B = 0.80
Candidate C = 0.84
```

Keep B.

If:

```text
B = 0.53
C = 0.92
```

for sufficiently long, switch to C.

Use:

```text
confidence margin
+
confirmation period
+
minimum hold time
```

---

# 20. Mic Crossfade

When transitioning:

```text
B → C
```

use a short crossfade instead of a hard cut.

```text
B 100% ─────► 0%
C   0% ─────► 100%
```

The transition should happen over tens of milliseconds.

---

# 21. Simultaneous Local Speakers

Eventually, if two independent people are speaking:

```text
Mic B = strong speech
Mic D = strong speech
```

allow both into a normalized mix:

```text
B ─┐
   ├──► outgoing audio
D ─┘
```

rather than throwing away one speaker.

This can come after single-speaker arbitration is reliable.

---

# 22. Virtual Meeting Devices

The coordinator exposes something like:

```text
Distributed Room Microphone
Distributed Room Speaker
```

The meeting app uses those two devices.

```text
Google Meet

Microphone:
Distributed Room Microphone

Speaker:
Distributed Room Speaker
```

The conferencing application has no knowledge that several Macs participate underneath.

---

# 23. Application UX Philosophy

The app should behave primarily like a **menu-bar utility**, not a large desktop application that users constantly keep open.

Most meeting controls should be accessible from the menu bar.

Example:

```text
🎙 RoomMesh

Conference Room
────────────────────────

Coordinator
● Rohith's MacBook

Room Speaker
● Meeting MacBook

Active Mic
● Amaan's MacBook

Connected
4 devices

──────────────

Mute My Mic
Manage Room…
Audio Diagnostics…
Settings…
Leave Room
```

This allows common actions to take one or two clicks.

---

# 24. Menu-Bar Controls

The menu-bar interface should expose the things users are likely to change during a meeting.

## Room status

```text
Room: Conference Room
4 Macs connected
```

## Active microphone

```text
Active Mic

● Amaan's MacBook
```

This should update live.

## Coordinator

```text
Coordinator

● Rohith's MacBook
```

Selecting it can open a submenu:

```text
Coordinator
  ✓ Rohith's MacBook
    Amaan's MacBook
    Puyan's MacBook
```

## Speaker

```text
Room Speaker
  Rohith's MacBook
✓ Meeting MacBook
  Amaan's MacBook
```

Changing the selected speaker should happen directly from the menu bar.

## Personal mic controls

```text
✓ Use This Mac's Microphone
Mute My Microphone
```

## Room controls

```text
Invite Nearby Device…
Manage Room…
Leave Room
```

## Diagnostics

A small status indicator could show:

```text
● Excellent
● Good
● Degraded
```

with detailed metrics available deeper in the application.

---

# 25. Menu-Bar Icon State

The menu-bar icon itself can communicate useful state.

Conceptually:

```text
Normal:
🎙

Muted:
🎙̸

Not connected:
○

Connected / healthy:
●

Warning:
!
```

Avoid making the icon overly dynamic or distracting.

Its primary purpose is to provide fast access.

---

# 26. Main Application Window

Opening the app from Launchpad, Finder, Spotlight, or the Applications folder should show a **small simple window**, not a complicated control console.

Something like:

```text
┌───────────────────────────────────────┐
│ RoomMesh                              │
│                                       │
│ Conference Room                       │
│ ● Connected                           │
│                                       │
│ 4 Macs                                │
│                                       │
│ Coordinator                           │
│ Rohith's MacBook               [⌄]   │
│                                       │
│ Room Speaker                          │
│ Meeting MacBook                [⌄]   │
│                                       │
│ Active Microphone                     │
│ ● Amaan's MacBook                     │
│                                       │
│ ───────────────────────────────────   │
│                                       │
│ Rohith's MacBook         🎤           │
│ Amaan's MacBook          🎤 ACTIVE    │
│ Meeting MacBook          🎤 🔊        │
│ Puyan's MacBook          🎤           │
│                                       │
│          [ Leave Room ]               │
└───────────────────────────────────────┘
```

The main window should emphasize:

- current room;
- connected devices;
- coordinator;
- fixed speaker;
- active microphone;
- health status.

It should not overwhelm users with DSP configuration.

---

# 27. No-Room State

When no room exists:

```text
┌───────────────────────────────────┐
│ RoomMesh                          │
│                                   │
│ No active room                    │
│                                   │
│ Nearby Macs                       │
│                                   │
│ Amaan's MacBook        [Connect] │
│ Puyan's MacBook        [Connect] │
│                                   │
│        [ Create Room ]            │
└───────────────────────────────────┘
```

Discovery should feel almost automatic.

---

# 28. Advanced Settings

DSP and networking controls should **not** clutter the normal UI.

Put them behind:

```text
Settings
    │
    ├── General
    ├── Audio
    ├── Network
    └── Advanced
```

Possible advanced diagnostics:

```text
latency
jitter
packet loss
clock drift
mic scores
AEC convergence
audio buffer depth
transport in use
```

These are useful for debugging but shouldn't be part of the normal meeting workflow.

---

# 29. Coordinator Failure

If the coordinator disappears, peers detect missing heartbeats.

For an initial version:

```text
Coordinator disconnected.

Select new coordinator:
○ Mac A
○ Mac C
○ Mac D
```

Later, coordinator election can be automatic.

Because every installation contains the same Rust core, any remaining peer can take over.

---

# 30. Speaker Failure

If the fixed playback device disappears:

```text
Room speaker disconnected.

Select another speaker:
○ Mac A
○ Mac B
○ Mac D
```

Do not automatically enable every speaker.

An optional setting can provide:

```text
Fallback speaker:
Coordinator Mac
```

---

# 31. Core Rust Module Structure

A reasonable initial Rust layout:

```text
core/

├── room/
│   ├── state
│   ├── membership
│   ├── coordinator
│   └── protocol
│
├── network/
│   ├── transport
│   ├── apple_p2p
│   ├── control
│   ├── realtime
│   └── clock_sync
│
├── audio/
│   ├── frames
│   ├── jitter_buffer
│   ├── resampler
│   ├── synchronization
│   └── mixer
│
├── dsp/
│   ├── aec
│   ├── vad
│   ├── scoring
│   ├── arbitration
│   └── crossfade
│
└── bridge/
    └── ffi
```

The Swift project then looks conceptually like:

```text
macOS App/

├── App
├── MenuBar
├── MainWindow
├── RoomUI
├── Settings
├── Permissions
├── AudioIntegration
└── RustBridge
```

---

# 32. Architectural Principle

The system should keep four concerns separate:

```text
USER INTERFACE
      │
      ▼
ROOM SEMANTICS
      │
      ▼
AUDIO ENGINE
      │
      ▼
TRANSPORT
```

Therefore:

```text
SwiftUI
```

can evolve without rewriting audio processing.

```text
Rust DSP
```

can evolve without changing the room interface.

And:

```text
Apple P2P
```

can later be supplemented or replaced by:

```text
LAN
```

without rewriting AEC or microphone selection.

---

# 33. Final Product Model

Every Mac runs:

```text
┌──────────────────────────────┐
│       RoomMesh.app           │
│                              │
│ Swift / SwiftUI              │
│ ├─ Menu bar                  │
│ ├─ Small main window         │
│ ├─ Settings                  │
│ └─ macOS integration         │
│                              │
│ Rust Core                    │
│ ├─ P2P protocol              │
│ ├─ Room state                │
│ ├─ Audio transport           │
│ ├─ Clock sync                │
│ ├─ AEC                       │
│ ├─ VAD                       │
│ ├─ Mic selection             │
│ └─ Mixing                    │
└──────────────────────────────┘
```

At runtime:

```text
                Mac A 🎤
                   \
                    \
Mac D 🎤 ─────► Mac B ◄───── Mac C 🎤 🔊
                Coordinator
                     │
                     ▼
              virtual microphone
                     │
                     ▼
             Meet / Zoom / Teams
```

The fundamental rules remain:

**One application.**

**Every Mac is a peer.**

**Any Mac can become coordinator.**

**Many distributed microphones.**

**One dynamically selected microphone path.**

**One manually selected, fixed room speaker.**

**Apple P2P first, with the networking layer designed so LAN can be added later.**

**Rust owns the realtime/core processing system.**

**Swift owns the native macOS experience.**

**The menu bar handles everyday meeting controls, while opening the app provides a small, clean room-management window.**
