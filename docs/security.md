# RoomMesh security model

RoomMesh runs over Apple peer-to-peer / LAN links (Bonjour `_roomaudio._tcp` for control,
`_roomaudio._udp` for realtime audio). The transport layer adds no security of its own (plain TCP
and UDP, no TLS). All confidentiality and integrity come from the Rust core
(`core/roommesh-core/src/network/secure.rs` and `control.rs`).

## Session setup

- **Preamble first.** A dialed control connection starts with one plaintext frame,
  `RMHELLO1:<peer id>` (16 lowercase hex digits), from the Swift transport
  (`Preamble` in `Framing.swift`). It tells the acceptor who is dialing. The acceptor attributes
  every later frame on that connection to that id. The preamble is not authenticated (see
  [Known limitations](#known-limitations)). Everything after it belongs to the core.
- **Frames.** Every core frame starts with a tag byte: `0x01` HELLO, `0x02` SEALED, `0x03`
  HELLO_REQUEST, `0x04` COMMIT.
- **Per-connection ephemeral X25519.** Every handshake uses freshly generated X25519 keys. A
  Hello carries the protocol version, the sender's `PeerId`, its display name, its public key
  and, on the responder's Hello, `reply_to`. No long-term identity keys exist. Each connection
  gets new keys, which gives forward secrecy per connection.
- **Commit, reply, reveal.** Only the numerically lower `PeerId` of a pair (the initiator)
  opens a handshake:
  1. The initiator sends `COMMIT(c)`, where `c = SHA-256("roommesh v3 commit" ‖ hello_lo)` and
     `hello_lo` is the exact frame of its opening Hello, not sent yet.
  2. The responder answers with its full Hello, with `reply_to = c`. It only does so after
     it holds the commitment.
  3. The initiator reveals `hello_lo`. The responder checks that it hashes to `c`. If it doesn't,
     the attempt is abandoned (`CommitMismatch`).

  The higher side can send a one-byte `HELLO_REQUEST` to ask for the Commit. The initiator
  answers a request, or a repeated `Connected`, by resending the identical Commit rather than
  starting over with new keys. A Commit or opening Hello received by the lower id is rejected, as
  is a responder Hello received by the higher id, or an opening Hello with no Commit before it.
- **Transcript binding.** Both sides hash the transcript
  `T = SHA-256("roommesh v3 transcript" ‖ len ‖ hello_lo ‖ len ‖ hello_hi)` (lengths u32 LE)
  over the exact Hello frames. `T` covers both protocol versions, ids, names, public keys and
  `reply_to`. Changing any of them changes every key and the SAS, so a relay can't rewrite a name
  or id, or splice two handshakes together (unknown-key-share), without the users seeing
  different codes. `complete` also checks the shape itself: the lower id's Hello must be the
  opening one, and the higher id's must answer its commitment.
- **Key schedule.** HKDF-SHA256 with the X25519 shared secret as input keying material and `T`
  as salt. It yields separate ChaCha20-Poly1305 keys per direction for control (`roommesh v3 ctrl
  lo->hi` / `hi->lo`) and realtime (`roommesh v3 rt lo->hi` / `hi->lo`). A shared secret that
  is not contributory (the peer sent a low-order point, which makes the result independent of
  our secret) is rejected.
- **SAS.** 8 bytes expanded from the same HKDF with the label `roommesh v3 sas`, reduced to a
  6-digit code (`"482 913"`). The code is shown next to an invitation (`InviteReceived.sas`) and
  in the nearby list, for the two users to compare out of band.
- **Why a man in the middle can't grind the SAS.** A MITM has to commit to its key toward the
  responder before it sees the responder's Hello. It also has to send its reply to the
  initiator before the initiator reveals its Hello. So on each leg, one side's key is random and
  still unseen when the attacker makes its choice, and each leg's code is uniformly random to
  the attacker. The two codes match with probability 10^-6 per attempt, and it can't search
  offline for a match. Each further try costs a full new connection, and the displayed code
  changes every time.
- **Retransmits and re-keys.** The responder answers a repeated copy of the Commit it is
  answering with the identical Hello. Once a session is up, it silently ignores a Commit or
  reveal of the handshake that produced that session. Duplicates are recognised by the
  commitment, a hash of the whole opening Hello, not just its public key. The initiator ignores
  any responder Hello that doesn't answer its pending Commit, including a late duplicate after
  its session is up. On an established session, a new Commit (at the responder) or
  `HELLO_REQUEST` (at the initiator) is honoured only if the transport reported a new connection
  to that peer within the last 5 s (`fresh`). That flag is cleared by the first authenticated
  frame and expires with the handshake watchdog. A responder keeps its old session until the new
  handshake completes; an initiator honouring a `HELLO_REQUEST` drops it and starts over. The
  watchdog (5 s) also covers a responder still waiting for a reveal.
- **Version first.** HELLO and COMMIT both carry `protocol_version` first and are decoded version
  first. A peer on another version fails with `Version`. The core then stops dialing it for
  60 s, doubling per failed retry up to 10 min, and tells the user once. The current version is
  3.

## Framing and nonces

- **Control** (`0x02` + ciphertext): ChaCha20-Poly1305 with an implicit per-direction 64-bit
  counter nonce (TCP is ordered, so nothing is sent on the wire). A frame that fails to
  authenticate drops the session and the transport connection (`SessionFailed`). A new
  handshake follows on reconnect.
- **Realtime** (UDP): the 44-byte header travels in clear and is the AEAD's associated data. It
  holds magic, version, kind, epoch, stream, sender, sequence, sample index, timestamp, frame
  count and ciphertext length. Nonce = `kind (1) ‖ epoch low 3 bytes LE ‖ stream u32 LE ‖
  sequence u32 LE`. The key is chosen by the header's sender, so a packet authenticates only
  under that peer's session key.
- **Nonce uniqueness doesn't depend on the epoch.** Every sender takes sequence numbers from
  counters that live as long as the audio runtime (clock pings, mic uplink, far-end playback)
  or the Core (clock pongs). They never restart, not on an epoch change and not when pipelines
  are rebuilt, and every connection has its own keys. The epoch in the nonce is extra
  separation only. A pong never reuses the ping's sequence. The 32-bit counters would wrap after
  2^32 packets of one kind on one connection (about 16 months of continuous mic uplink).

## Clock-ping replay protection

The coordinator answers clock pings on the receive thread. Each sender has a 64-entry sliding
window over ping sequence numbers, scoped to its session key (a rekey resets it) and epoch.
Pings from an older epoch than the window's are rejected, because epochs only increase. A
sender's window is dropped when its session goes down or it leaves the member set. A pong echoes
`t1` and the ping's sequence. The pinging side accepts a pong only for a ping it still has
outstanding (at most 32, for at most 1 s) with the same `t1`, and only once. So replayed or
forged-late pongs never become clock samples, and replayed pings are not answered, which also
prevents amplification. Audio packets have no separate replay window. They rely on the jitter
buffer's duplicate and late checks.

## Room membership

- **Joining needs an invitation.** The member that sends an `Invite` remembers the invitee for
  10 minutes, and the invitee sends its `JoinRequest` back to that member. The coordinator
  admits a direct `JoinRequest` only from a peer it invited itself, or from a current member
  re-joining. Any other member relays a joiner's request to the coordinator only for a peer it
  invited, and the coordinator admits requests relayed by members. Knowing the `RoomId` alone
  gets nobody in. The invitation list is local to the inviter (not in the manifest): the inviter
  is the one that vouches, and after a coordinator change it relays to the new coordinator.
- **Denylist.** An invitation is used up once the invitee is a member. A member that is removed
  (or leaves) is denied until someone invites it again.
- **No manifest for non-members.** A non-member's Heartbeat, and the removed member's copy of
  the change, get a removal notice: the room's version and coordinator id, with no name and no
  member list.
- **Trust model.** Every member is trusted with the room's roles. Any member may invite, remove
  members, pick the coordinator or the room speaker, and rename the room, directly or relayed.
  Changes that speak for one member (its name and capabilities, and whether its microphone is
  used) are accepted only from that member itself, and only directly, because a relayed request
  can't be told from a forged one.
- **Epoch cap.** A manifest may move the epoch forward by at most 1000 in one step, so no member
  can push the room's epoch to `u32::MAX`. Legitimate jumps (coordinator changes, a healed split
  brain) are a few epochs.

## Local audio path (driver shared memory)

The driver runs inside coreaudiod as `_coreaudiod` and exchanges audio with the app through
the POSIX shared-memory region `/roommesh.v1`. The driver creates it with mode 0666
(`driver/src/SharedRegion.cpp`), so that the app, which runs as the logged-in user, can open it.

- The app only opens a region owned by `_coreaudiod` (looked up by name, uid 202 as a fallback).
  So another local user can't create the name first ("squatting") and feed the app a region
  of its own.
- Samples read from the speaker ring are scrubbed: NaN and Inf become silence, and the rest are
  clamped to [-1, 1].
- **Remaining exposure:** because the region is world-readable and world-writable, any local
  process of any user can read the room's audio (the arbitrated room mic and what meeting apps
  play) and inject audio into either direction.

## Attacker model

We assume an attacker on the same LAN / AWDL segment. They can observe, drop, delay, reorder
and inject traffic, advertise Bonjour services, and open connections under any PeerId.

Mitigated:

- eavesdropping, tampering and replay of sealed traffic;
- man-in-the-middle on a handshake whose SAS the users actually compared (including an offline
  search for matching codes);
- name or id rewriting in Hellos;
- joining a room by knowing its `RoomId`.

Local attackers on the same Mac are covered only as described under
[Local audio path](#local-audio-path-driver-shared-memory).

Out of scope:

- **Denial of service**: dropping traffic, flooding, forged connections that claim a peer's id,
  forged Commits or reveals that abandon a handshake attempt, and forged version failures that
  trigger the redial backoff.
- **Traffic analysis**: who talks to whom, packet timing and sizes, and the names and ids in
  cleartext Hellos and Bonjour records.
- **A malicious member**: members are trusted with roles and membership, see the
  [trust model](#room-membership).

## Known limitations

- **M8: reconnects are unauthenticated (no key continuity).** Every reconnect, including an
  automatic one after a network blip, sleep or driver restart, is a fresh, unauthenticated DH.
  The SAS is only shown at invite time, so an attacker who can force a reconnect (for example
  by dropping traffic until the watchdog tears the link down) gets a new 1-in-10^6 chance to
  man-in-the-middle an existing member's later session each time, without anyone comparing a
  code. Future work: key continuity (pin a long-term identity key per peer on first SAS
  confirmation and sign later handshakes with it), or re-show the SAS whenever an existing
  member's session is re-established.
- **The preamble is unauthenticated.** Anyone can claim any id in `RMHELLO1:`. The core only
  trusts the attribution after a handshake, and that handshake is only as good as the SAS
  check (and M8). A new connection for an id can replace the old one (see `promote` in
  `AppleP2PTransport.swift`), which resets that peer's session (denial of service).
- The PeerId is a random 64-bit value with no cryptographic binding. Display names, PeerIds and
  the protocol version are sent in clear.
- A peer on protocol version 2 doesn't recognise the version-3 COMMIT frame. If it's the higher
  id of a pair, it logs a malformed frame instead of reporting the version mismatch, and the
  version-3 side's handshake watchdog keeps reconnecting.
- Changes that speak for one member aren't relayed. While a member has no direct link to the
  coordinator, its own name, capability and mic changes wait until the link returns.
