# RoomMesh security model

RoomMesh runs over Apple peer-to-peer / LAN links (Bonjour `_roomaudio._tcp` for control,
`_roomaudio._udp` for realtime audio). The transport layer adds no security of its own (plain TCP
and UDP, no TLS). All confidentiality and integrity come from the Rust core
(`core/roommesh-core/src/network/secure.rs` and `control.rs`).

## Session setup

- **Per-connection ephemeral X25519.** Every control connection starts with a plaintext `Hello`
  frame (`0x01`) carrying the protocol version, the sender's `PeerId`, its display name, a
  freshly generated X25519 public key and, in a reply, `reply_to` (the opening Hello's key being
  answered). No long-term identity keys exist. Each connection gets new keys, which gives forward
  secrecy per connection.
- **Lower PeerId initiates.** Only the numerically lower `PeerId` of a pair sends the opening
  Hello. The higher side sends a one-byte `HELLO_REQUEST` (`0x03`) to ask for it and answers the
  opening Hello with exactly one reply. A reply is never answered, so the handshake is a single
  round. An opening Hello received by the lower side is rejected. On an established session, an
  opening Hello or `HELLO_REQUEST` is honoured only if the transport reported a new connection to
  that peer within the last 5 s (`fresh`). That flag is cleared by the first authenticated frame
  and expires with the handshake watchdog. A byte-identical retransmitted Hello is ignored.
- **Version first.** `decode_hello` checks `protocol_version` before parsing the rest. A peer on
  another version fails with `Version`. The core then stops dialing it for 60 s, doubling per
  failed retry up to 10 min, and tells the user once.
- **Key schedule.** HKDF-SHA256 with the X25519 shared secret as input keying material and
  salt = `lo_pub ‖ hi_pub` (ordered by PeerId). Separate ChaCha20-Poly1305 keys per direction for
  control (`roommesh v1 ctrl lo->hi` / `hi->lo`) and realtime (`roommesh v1 rt lo->hi` /
  `hi->lo`).
- **SAS.** 4 bytes expanded with the label `roommesh v1 sas` are shown as a 6-digit code
  (`"482 913"`). Both ends of a connection see the same code, and a man in the middle produces
  different codes on each side. The code is shown next to an invitation (`InviteReceived.sas`)
  and in the nearby list, for the two users to compare out of band.

## Framing and nonces

- **Control** (`0x02` + ciphertext): ChaCha20-Poly1305 with an implicit per-direction 64-bit
  counter nonce (TCP is ordered, so nothing is sent on the wire). A frame that fails to
  authenticate drops the session and the transport connection (`SessionFailed`). A new
  handshake follows on reconnect.
- **Realtime** (UDP): the 44-byte header travels in clear and is the AEAD's associated data. It
  holds magic, version, kind, epoch, stream, sender, sequence, sample index, timestamp, frame
  count and ciphertext length. Nonce = `kind (1) ‖ epoch low 3 bytes LE ‖ stream u32 LE ‖
  sequence u32 LE`. The key is chosen by the header's sender, so a packet authenticates only
  under that peer's session key. Uniqueness: every sender takes sequence numbers from counters
  that live as long as the audio runtime (clock pings, mic uplink, far-end playback) or the Core
  (clock pongs). They never restart within one session and epoch, even when pipelines are
  rebuilt. A pong never reuses the ping's sequence.

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

## Attacker model

We assume an attacker on the same LAN / AWDL segment. They can observe, drop, delay, reorder
and inject traffic, advertise Bonjour services, and open connections under any PeerId.
Mitigated: eavesdropping, tampering and replay of sealed traffic, and man-in-the-middle on a
handshake whose SAS the users actually compared. Not mitigated: denial of service (dropping
traffic, flooding, forged connections that claim a peer's id, forged version failures that
trigger the redial backoff), and traffic analysis (who talks to whom, packet timing and sizes,
names and ids in cleartext Hellos and Bonjour records).

## Known limitations

- **M8: reconnects are unauthenticated.** Every reconnect, including an automatic one after a
  network blip, sleep or driver restart, is a fresh, unauthenticated DH. The SAS is only shown
  at invite time, so an attacker who can force a reconnect (for example by dropping traffic
  until the watchdog tears the link down) can man-in-the-middle an existing member's later
  sessions without anyone comparing a code. Future work: key continuity (pin a long-term
  identity key per peer on first SAS confirmation and sign later handshakes with it), or
  re-show the SAS whenever an existing member's session is re-established.
- **The preamble is unauthenticated.** A dialed control connection starts with
  `RMHELLO1:<peer id>` in clear. The Swift transport attributes every later frame on that
  connection to that id. The core only trusts that attribution after a handshake, but anyone
  can claim any id at the transport level. A new connection for an id can replace the old one
  (see `promote` in `AppleP2PTransport.swift`), which resets that peer's session (denial of
  service). Together with M8, the claim also becomes a
  path to impersonation.
- The PeerId is a random 64-bit value with no cryptographic binding. Display names, PeerIds and
  the protocol version are sent in clear.
- Only the low 24 bits of the epoch enter the realtime nonce. A room would need 2^24
  coordinator handovers within one session for this to matter.
