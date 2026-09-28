//! Per-connection secure session: a commit-then-reveal handshake over ephemeral X25519 keys, with
//! every key and the short authentication string (SAS) derived from a hash of both Hellos.
//!
//! # Frames
//!
//! Every control-channel frame starts with a one-byte tag:
//!
//! | tag    | name            | body                                                        |
//! |--------|-----------------|-------------------------------------------------------------|
//! | `0x01` | `HELLO`         | postcard [`Hello`] (`protocol_version` first)               |
//! | `0x02` | `SEALED`        | ChaCha20-Poly1305 ciphertext of a `ControlMessage`          |
//! | `0x03` | `HELLO_REQUEST` | empty                                                       |
//! | `0x04` | `COMMIT`        | postcard [`Commit`] (`protocol_version` first)              |
//!
//! Both version-carrying frames are decoded version first (see [`decode_hello`] and
//! [`decode_commit`]): a peer on another protocol version gets a clean [`SecureError::Version`]
//! even if the rest of its frame has a different shape.
//!
//! # Handshake
//!
//! Only the lower `PeerId` of a pair (the initiator, `lo`) opens; the higher one (the
//! responder, `hi`) asks for that with a `HELLO_REQUEST` (see `network::control`).
//!
//! 1. `lo → hi: COMMIT(c)`, with `c = SHA-256("roommesh v3 commit" ‖ hello_lo)` and `hello_lo`
//!    the exact bytes of `lo`'s opening Hello frame (`reply_to: None`), not yet sent.
//! 2. `hi → lo: HELLO(hello_hi)`, `hi`'s full Hello with `reply_to: Some(c)`. `hi` sends it only
//!    after it holds `c`.
//! 3. `lo → hi: HELLO(hello_lo)`, the reveal. `hi` checks that it hashes to `c`.
//!
//! Both sides then hash the transcript `T = SHA-256("roommesh v3 transcript" ‖ len ‖ hello_lo ‖
//! len ‖ hello_hi)` (lengths u32 LE) over the exact frame bytes, which cover the protocol
//! version, both ids, both names, both public keys and `reply_to`. HKDF-SHA256 uses the X25519
//! shared secret as input keying material and `T` as salt. It yields the four ChaCha20-Poly1305
//! keys (control and realtime, one per direction) and the 6-digit SAS. An X25519 result that is
//! not contributory (a low-order peer key) is rejected.
//!
//! Why the commitment matters: a man in the middle has to fix its key toward `hi` (step 1)
//! before it sees `hello_hi`, and has to fix its key toward `lo` (step 2) before `lo` reveals
//! `hello_lo`. On each leg one side's contribution is random and unseen when the attacker
//! commits, so the two legs' SAS match only by chance (1 in 10^6 per attempt). It cannot search
//! offline for a matching pair.
//!
//! Control frames use an implicit per-direction counter nonce (TCP is ordered). Realtime packets
//! use nonce = kind | epoch | stream | sequence and the 44-byte header as AAD, so reordering and
//! loss are fine.
use crate::ids::PeerId;
use crate::network::realtime::{decode_packet, header_bytes, RtError, RtHeader, HEADER_LEN};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use x25519_dalek::{PublicKey, StaticSecret};

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_SEALED: u8 = 0x02;
/// Plaintext nudge asking the recipient to (re)send its opening Commit. Sent only by the higher
/// `PeerId` of a pair (which never opens a handshake itself) and answered only by the lower
/// `PeerId` -- see `network::control::ControlChannel`.
pub const FRAME_HELLO_REQUEST: u8 = 0x03;
/// The initiator's commitment to its not-yet-sent opening Hello (step 1 of the handshake).
pub const FRAME_COMMIT: u8 = 0x04;

/// A SHA-256 commitment to an opening Hello frame.
pub type Commitment = [u8; 32];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub peer_id: PeerId,
    pub name: String,
    pub public_key: [u8; 32],
    /// Set only on the responder's Hello: the [`Commitment`] it answers. `None` marks the
    /// initiator's opening Hello (the reveal). A Hello with `reply_to` set is never itself
    /// answered, which keeps the handshake to one commit/reply/reveal round instead of an
    /// unbounded ping-pong when a duplicate arrives.
    pub reply_to: Option<Commitment>,
}

/// Body of a `COMMIT` frame. `protocol_version` comes first, as in [`Hello`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub protocol_version: u16,
    pub commitment: Commitment,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SecureError {
    #[error("malformed frame")]
    Malformed,
    #[error("decryption failed")]
    Crypto,
    #[error("unsupported protocol version {0}")]
    Version(u16),
    #[error("attempted to complete a handshake with ourselves")]
    SelfConnection,
    #[error("the Hellos do not match the handshake commitment")]
    Commitment,
    #[error("the peer's key produced a non-contributory (low-order) shared secret")]
    NonContributory,
    #[error(transparent)]
    Rt(#[from] RtError),
}

/// Peeks the leading `protocol_version` of a version-first frame body before the whole body is
/// decoded: another protocol version may use a different wire shape entirely (fields added,
/// removed or reordered), which would otherwise surface as an opaque "malformed frame" instead
/// of the actionable version mismatch.
fn check_version(body: &[u8]) -> Result<(), SecureError> {
    let (version, _) =
        postcard::take_from_bytes::<u16>(body).map_err(|_| SecureError::Malformed)?;
    if version != crate::room::state::PROTOCOL_VERSION {
        return Err(SecureError::Version(version));
    }
    Ok(())
}

fn decode_exact<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, SecureError> {
    let (v, rest) = postcard::take_from_bytes(body).map_err(|_| SecureError::Malformed)?;
    if !rest.is_empty() {
        return Err(SecureError::Malformed);
    }
    Ok(v)
}

pub fn decode_hello(frame: &[u8]) -> Result<Hello, SecureError> {
    if frame.first() != Some(&FRAME_HELLO) {
        return Err(SecureError::Malformed);
    }
    check_version(&frame[1..])?;
    decode_exact(&frame[1..])
}

pub fn decode_commit(frame: &[u8]) -> Result<Commitment, SecureError> {
    if frame.first() != Some(&FRAME_COMMIT) {
        return Err(SecureError::Malformed);
    }
    check_version(&frame[1..])?;
    decode_exact::<Commit>(&frame[1..]).map(|c| c.commitment)
}

/// Encodes `hello` as a `HELLO` frame. These exact bytes are what gets committed to and hashed
/// into the transcript.
pub fn encode_hello(hello: &Hello) -> Vec<u8> {
    let mut v = vec![FRAME_HELLO];
    v.extend(postcard::to_allocvec(hello).expect("hello"));
    v
}

/// The commitment to an opening Hello frame (its exact bytes).
pub fn commitment(hello_frame: &[u8]) -> Commitment {
    Sha256::new()
        .chain_update(b"roommesh v3 commit")
        .chain_update(hello_frame)
        .finalize()
        .into()
}

/// The `COMMIT` frame for an opening Hello frame.
pub fn commit_frame(hello_frame: &[u8]) -> Vec<u8> {
    let c = Commit {
        protocol_version: crate::room::state::PROTOCOL_VERSION,
        commitment: commitment(hello_frame),
    };
    let mut v = vec![FRAME_COMMIT];
    v.extend(postcard::to_allocvec(&c).expect("commit"));
    v
}

/// `SHA-256` over both Hello frames, lower `PeerId`'s first, each length-prefixed.
fn transcript_hash(hello_lo: &[u8], hello_hi: &[u8]) -> [u8; 32] {
    let len = |b: &[u8]| u32::try_from(b.len()).expect("hello length").to_le_bytes();
    Sha256::new()
        .chain_update(b"roommesh v3 transcript")
        .chain_update(len(hello_lo))
        .chain_update(hello_lo)
        .chain_update(len(hello_hi))
        .chain_update(hello_hi)
        .finalize()
        .into()
}

pub struct Handshake {
    local: PeerId,
    name: String,
    secret: StaticSecret,
    public: PublicKey,
}

impl Handshake {
    pub fn new(local: PeerId, name: String) -> Self {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("rng");
        Self::from_seed(local, name, seed)
    }
    fn from_seed(local: PeerId, name: String, seed: [u8; 32]) -> Self {
        let secret = StaticSecret::from(seed);
        let public = PublicKey::from(&secret);
        Self {
            local,
            name,
            secret,
            public,
        }
    }
    pub fn public_key(&self) -> [u8; 32] {
        self.public.to_bytes()
    }
    /// The initiator's opening Hello frame (`reply_to: None`). Commit to it with
    /// [`commit_frame`] first; send it only once the responder's Hello has arrived.
    pub fn hello(&self) -> Vec<u8> {
        self.build_hello(None)
    }
    /// The responder's Hello, answering commitment `to`. Never call this in response to a
    /// Hello.
    pub fn hello_reply(&self, to: &Commitment) -> Vec<u8> {
        self.build_hello(Some(*to))
    }
    fn build_hello(&self, reply_to: Option<Commitment>) -> Vec<u8> {
        encode_hello(&Hello {
            protocol_version: crate::room::state::PROTOCOL_VERSION,
            peer_id: self.local,
            name: self.name.clone(),
            public_key: self.public.to_bytes(),
            reply_to,
        })
    }
    /// Derives the session from the exact Hello frames exchanged: `local_hello` is the one this
    /// handshake built and sent, `remote_hello` the peer's. Checks that the lower id's Hello is
    /// the opening one and that the higher id's Hello answers its commitment, so each side
    /// only ever completes a well-formed commit/reply/reveal exchange.
    pub fn complete(
        &self,
        local_hello: &[u8],
        remote_hello: &[u8],
    ) -> Result<Session, SecureError> {
        let remote = decode_hello(remote_hello)?;
        if remote.peer_id == self.local {
            return Err(SecureError::SelfConnection);
        }
        let local = decode_hello(local_hello)?;
        debug_assert!(local.public_key == self.public_key() && local.peer_id == self.local);
        let local_is_low = self.local < remote.peer_id;
        let ((lo, lo_bytes), (hi, hi_bytes)) = if local_is_low {
            ((&local, local_hello), (&remote, remote_hello))
        } else {
            ((&remote, remote_hello), (&local, local_hello))
        };
        if lo.reply_to.is_some() || hi.reply_to != Some(commitment(lo_bytes)) {
            return Err(SecureError::Commitment);
        }
        let shared = self
            .secret
            .diffie_hellman(&PublicKey::from(remote.public_key));
        if !shared.was_contributory() {
            return Err(SecureError::NonContributory);
        }
        let salt = transcript_hash(lo_bytes, hi_bytes);
        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
        let key = |label: &str| {
            let mut k = [0u8; 32];
            hk.expand(label.as_bytes(), &mut k).expect("hkdf");
            ChaCha20Poly1305::new(Key::from_slice(&k))
        };
        let (c_lo_hi, c_hi_lo) = (
            key("roommesh v3 ctrl lo->hi"),
            key("roommesh v3 ctrl hi->lo"),
        );
        let (r_lo_hi, r_hi_lo) = (key("roommesh v3 rt lo->hi"), key("roommesh v3 rt hi->lo"));
        let mut sas_bytes = [0u8; 8];
        hk.expand(b"roommesh v3 sas", &mut sas_bytes).expect("hkdf");
        let code = u64::from_le_bytes(sas_bytes) % 1_000_000;
        let (ctrl_tx, ctrl_rx, rt_tx, rt_rx) = if local_is_low {
            (c_lo_hi, c_hi_lo, r_lo_hi, r_hi_lo)
        } else {
            (c_hi_lo, c_lo_hi, r_hi_lo, r_lo_hi)
        };
        Ok(Session {
            remote: remote.peer_id,
            remote_name: remote.name,
            sas: format!("{:03} {:03}", code / 1000, code % 1000),
            ctrl_tx,
            ctrl_rx,
            rt: Arc::new(RtCipher {
                tx: rt_tx,
                rx: rt_rx,
            }),
            tx_counter: 0,
            rx_counter: 0,
        })
    }
}

/// Realtime keys for one peer. Shared (`Arc`) between the FFI receive path and the DSP thread.
pub struct RtCipher {
    tx: ChaCha20Poly1305,
    rx: ChaCha20Poly1305,
}

impl RtCipher {
    pub fn seal(&self, h: &RtHeader, plaintext: &[u8]) -> Vec<u8> {
        let ct_len = plaintext.len() + 16;
        let aad = header_bytes(h, ct_len);
        let ct = self
            .tx
            .encrypt(
                &rt_nonce(h),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .expect("encrypt");
        let mut v = Vec::with_capacity(HEADER_LEN + ct.len());
        v.extend_from_slice(&aad);
        v.extend(ct);
        v
    }
    pub fn open(&self, packet: &[u8]) -> Result<(RtHeader, Vec<u8>), SecureError> {
        let (h, ct) = decode_packet(packet)?;
        let pt = self
            .rx
            .decrypt(
                &rt_nonce(&h),
                Payload {
                    msg: ct,
                    aad: &packet[..HEADER_LEN],
                },
            )
            .map_err(|_| SecureError::Crypto)?;
        Ok((h, pt))
    }
}

pub struct Session {
    remote: PeerId,
    remote_name: String,
    sas: String,
    ctrl_tx: ChaCha20Poly1305,
    ctrl_rx: ChaCha20Poly1305,
    rt: Arc<RtCipher>,
    tx_counter: u64,
    rx_counter: u64,
}

fn counter_nonce(c: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&c.to_le_bytes());
    *Nonce::from_slice(&n)
}
fn rt_nonce(h: &RtHeader) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = h.kind as u8;
    // Low 3 bytes of the epoch (LE): without this, sequence numbers restart at epoch 0 on every
    // coordinator handover would reuse (key, nonce) pairs from the previous epoch.
    n[1..4].copy_from_slice(&h.epoch.0.to_le_bytes()[..3]);
    n[4..8].copy_from_slice(&h.stream.0.to_le_bytes());
    n[8..12].copy_from_slice(&h.sequence.to_le_bytes());
    *Nonce::from_slice(&n)
}

impl Session {
    pub fn remote_peer(&self) -> PeerId {
        self.remote
    }
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }
    /// Short authentication string both users can compare, e.g. "482 913".
    pub fn sas(&self) -> &str {
        &self.sas
    }

    pub fn seal_control(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ct = self
            .ctrl_tx
            .encrypt(&counter_nonce(self.tx_counter), plaintext)
            .expect("encrypt");
        self.tx_counter += 1;
        let mut v = Vec::with_capacity(1 + ct.len());
        v.push(FRAME_SEALED);
        v.extend(ct);
        v
    }
    pub fn open_control(&mut self, frame: &[u8]) -> Result<Vec<u8>, SecureError> {
        if frame.first() != Some(&FRAME_SEALED) {
            return Err(SecureError::Malformed);
        }
        let pt = self
            .ctrl_rx
            .decrypt(&counter_nonce(self.rx_counter), &frame[1..])
            .map_err(|_| SecureError::Crypto)?;
        self.rx_counter += 1;
        Ok(pt)
    }
    pub fn realtime(&self) -> Arc<RtCipher> {
        self.rt.clone()
    }
    pub fn seal_realtime(&self, h: &RtHeader, plaintext: &[u8]) -> Vec<u8> {
        self.rt.seal(h, plaintext)
    }
    pub fn open_realtime(&self, packet: &[u8]) -> Result<(RtHeader, Vec<u8>), SecureError> {
        self.rt.open(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::network::realtime::*;

    /// Runs the full commit/reply/reveal exchange between an initiator `a` (the lower id) and
    /// a responder `b`, returning (a's session, b's session).
    fn run(
        a: &Handshake,
        b: &Handshake,
    ) -> (Result<Session, SecureError>, Result<Session, SecureError>) {
        let ha = a.hello();
        let c = decode_commit(&commit_frame(&ha)).unwrap();
        let hb = b.hello_reply(&c);
        (a.complete(&ha, &hb), b.complete(&hb, &ha))
    }
    fn pair() -> (Session, Session) {
        let a = Handshake::new(PeerId(1), "A".into());
        let b = Handshake::new(PeerId(2), "B".into());
        let (sa, sb) = run(&a, &b);
        (sa.unwrap(), sb.unwrap())
    }
    fn fixed(id: u64, name: &str, seed: u8) -> Handshake {
        Handshake::from_seed(PeerId(id), name.into(), [seed; 32])
    }
    fn sas_of(a: &Handshake, b: &Handshake) -> String {
        let (sa, sb) = run(a, b);
        let (sa, sb) = (sa.unwrap(), sb.unwrap());
        assert_eq!(sa.sas(), sb.sas());
        sa.sas().to_string()
    }

    #[test]
    fn both_sides_agree_on_sas() {
        let (a, b) = pair();
        assert_eq!(a.sas(), b.sas());
        assert_eq!(a.sas().len(), 7); // "123 456"
        assert_eq!(a.remote_peer(), PeerId(2));
        assert_eq!(b.remote_name(), "A");
    }
    #[test]
    fn both_sides_agree_on_sas_and_keys() {
        let (mut a, mut b) = pair();
        assert_eq!(a.sas(), b.sas());
        // Control keys agree in both directions ...
        let f = a.seal_control(b"lo->hi");
        assert_eq!(b.open_control(&f).unwrap(), b"lo->hi");
        let f = b.seal_control(b"hi->lo");
        assert_eq!(a.open_control(&f).unwrap(), b"hi->lo");
        // ... and so do the realtime keys.
        let h = |sender| RtHeader {
            kind: PacketKind::Mic,
            epoch: Epoch(1),
            stream: StreamId::MIC,
            sender,
            sequence: 1,
            sample_index: 0,
            timestamp_ns: 0,
            frame_count: 480,
        };
        let p = a.seal_realtime(&h(PeerId(1)), b"up");
        assert_eq!(b.open_realtime(&p).unwrap().1, b"up");
        let p = b.seal_realtime(&h(PeerId(2)), b"down");
        assert_eq!(a.open_realtime(&p).unwrap().1, b"down");
        // The directions use different keys: a packet can't be reflected back at its sender.
        let p = a.seal_realtime(&h(PeerId(1)), b"up");
        assert!(a.open_realtime(&p).is_err());
    }
    #[test]
    fn fixed_keys_give_a_deterministic_sas() {
        let (a, b) = (fixed(10, "A", 1), fixed(20, "B", 2));
        assert_eq!(sas_of(&a, &b), sas_of(&a, &b));
    }
    #[test]
    fn changing_a_name_or_id_in_either_hello_changes_the_sas() {
        let base = sas_of(&fixed(10, "A", 1), &fixed(20, "B", 2));
        // Same keys, one identity field changed: every variant must give a different code.
        // (Distinct 6-digit codes; a collision here would be a 1-in-10^6 coincidence for these
        // fixed seeds, which the assertion would catch deterministically.)
        for (a, b) in [
            (fixed(10, "A2", 1), fixed(20, "B", 2)),
            (fixed(10, "A", 1), fixed(20, "B2", 2)),
            (fixed(11, "A", 1), fixed(20, "B", 2)),
            (fixed(10, "A", 1), fixed(21, "B", 2)),
        ] {
            assert_ne!(sas_of(&a, &b), base, "{} / {}", a.name, b.name);
        }
    }
    #[test]
    fn responder_rejects_a_reveal_that_does_not_match_the_commitment() {
        let a = Handshake::new(PeerId(1), "A".into());
        let b = Handshake::new(PeerId(2), "B".into());
        let committed = a.hello();
        let hb = b.hello_reply(&commitment(&committed));
        // a reveals a different Hello from the one it committed to (here: another name).
        let mut other = decode_hello(&committed).unwrap();
        other.name = "Mallory".into();
        let revealed = encode_hello(&other);
        assert_eq!(
            b.complete(&hb, &revealed).err(),
            Some(SecureError::Commitment)
        );
        // A responder Hello that answers some other commitment is refused by the initiator.
        let stray = b.hello_reply(&[9; 32]);
        assert_eq!(
            a.complete(&committed, &stray).err(),
            Some(SecureError::Commitment)
        );
        // The honest exchange still completes.
        assert!(b.complete(&hb, &committed).is_ok());
        assert!(a.complete(&committed, &hb).is_ok());
    }
    #[test]
    fn the_lower_ids_hello_must_be_the_opening_one() {
        let a = Handshake::new(PeerId(1), "A".into());
        let b = Handshake::new(PeerId(2), "B".into());
        // Roles swapped: the higher id "opens" and the lower id replies.
        let hb = b.hello();
        let ha = a.hello_reply(&commitment(&hb));
        assert_eq!(a.complete(&ha, &hb).err(), Some(SecureError::Commitment));
        assert_eq!(b.complete(&hb, &ha).err(), Some(SecureError::Commitment));
    }
    #[test]
    fn low_order_public_key_is_rejected() {
        let a = Handshake::new(PeerId(1), "A".into());
        let ha = a.hello();
        // The all-zero point (and every other small-order point) makes the shared secret
        // all-zero, independent of our secret key.
        for key in [[0u8; 32], {
            let mut one = [0u8; 32];
            one[0] = 1;
            one
        }] {
            let hb = encode_hello(&Hello {
                protocol_version: crate::room::state::PROTOCOL_VERSION,
                peer_id: PeerId(2),
                name: "B".into(),
                public_key: key,
                reply_to: Some(commitment(&ha)),
            });
            assert_eq!(
                a.complete(&ha, &hb).err(),
                Some(SecureError::NonContributory)
            );
        }
    }
    #[test]
    fn control_seal_open_in_order() {
        let (mut a, mut b) = pair();
        for i in 0..5u8 {
            let frame = a.seal_control(&[i; 10]);
            assert_eq!(b.open_control(&frame).unwrap(), vec![i; 10]);
        }
        let reply = b.seal_control(b"ok");
        assert_eq!(a.open_control(&reply).unwrap(), b"ok");
    }
    #[test]
    fn control_tamper_detected() {
        let (mut a, mut b) = pair();
        let mut frame = a.seal_control(b"hello");
        let last = frame.len() - 1;
        frame[last] ^= 1;
        assert!(b.open_control(&frame).is_err());
    }
    #[test]
    fn realtime_seal_open_any_order() {
        let (a, b) = pair();
        let h = |seq| RtHeader {
            kind: PacketKind::Mic,
            epoch: Epoch(1),
            stream: StreamId::MIC,
            sender: PeerId(1),
            sequence: seq,
            sample_index: 0,
            timestamp_ns: 0,
            frame_count: 480,
        };
        let p1 = a.seal_realtime(&h(1), b"one");
        let p2 = a.seal_realtime(&h(2), b"two");
        let (h2, pl2) = b.open_realtime(&p2).unwrap();
        assert_eq!((h2.sequence, pl2.as_slice()), (2, &b"two"[..]));
        let (_, pl1) = b.open_realtime(&p1).unwrap();
        assert_eq!(pl1, b"one");
        let mut bad = p1.clone();
        bad[30] ^= 0xff; // header tamper (AAD)
        assert!(b.open_realtime(&bad).is_err());
    }
    #[test]
    fn realtime_payload_tamper_detected() {
        let (a, b) = pair();
        let h = RtHeader {
            kind: PacketKind::Mic,
            epoch: Epoch(1),
            stream: StreamId::MIC,
            sender: PeerId(1),
            sequence: 1,
            sample_index: 0,
            timestamp_ns: 0,
            frame_count: 480,
        };
        let p = a.seal_realtime(&h, b"hello world, this is voice data");
        let mut bad = p.clone();
        let last = bad.len() - 1; // last byte of the ciphertext/tag, not the header
        bad[last] ^= 0xff;
        assert!(b.open_realtime(&bad).is_err());
    }
    #[test]
    fn epoch_changes_nonce_so_ciphertext_differs_for_same_plaintext() {
        let (a, b) = pair();
        let h = |epoch| RtHeader {
            kind: PacketKind::Mic,
            epoch: Epoch(epoch),
            stream: StreamId::MIC,
            sender: PeerId(1),
            sequence: 5,
            sample_index: 0,
            timestamp_ns: 0,
            frame_count: 480,
        };
        let p1 = a.seal_realtime(&h(1), b"same plaintext!!");
        let p2 = a.seal_realtime(&h(2), b"same plaintext!!");
        // Compare only the ciphertext body (excluding the header/AAD and the 16-byte tag): the
        // epoch is folded into the nonce, so this differs even for identical plaintext.
        let body = |p: &[u8]| p[HEADER_LEN..p.len() - 16].to_vec();
        assert_ne!(body(&p1), body(&p2));
        let (h1, pl1) = b.open_realtime(&p1).unwrap();
        let (h2, pl2) = b.open_realtime(&p2).unwrap();
        assert_eq!(pl1, b"same plaintext!!");
        assert_eq!(pl2, b"same plaintext!!");
        assert_eq!(h1.epoch, Epoch(1));
        assert_eq!(h2.epoch, Epoch(2));
    }
    #[test]
    fn self_connection_rejected() {
        let a = Handshake::new(PeerId(1), "A".into());
        let a2 = Handshake::new(PeerId(1), "A".into());
        let ha = a.hello();
        let reply = a2.hello_reply(&commitment(&ha));
        assert_eq!(
            a.complete(&ha, &reply).err(),
            Some(SecureError::SelfConnection)
        );
    }
    #[test]
    fn protocol_version_mismatch_rejected() {
        let a = Handshake::new(PeerId(1), "A".into());
        let b = Handshake::new(PeerId(2), "B".into());
        let ha = a.hello();
        let mut hb = decode_hello(&b.hello_reply(&commitment(&ha))).unwrap();
        hb.protocol_version += 1;
        assert_eq!(
            a.complete(&ha, &encode_hello(&hb)).err(),
            Some(SecureError::Version(hb.protocol_version))
        );
        let mut c = vec![FRAME_COMMIT];
        c.extend(
            postcard::to_allocvec(&Commit {
                protocol_version: hb.protocol_version,
                commitment: [0; 32],
            })
            .unwrap(),
        );
        assert_eq!(
            decode_commit(&c),
            Err(SecureError::Version(hb.protocol_version))
        );
    }
    #[test]
    fn decode_rejects_trailing_bytes() {
        let a = Handshake::new(PeerId(1), "A".into());
        let mut frame = a.hello();
        let mut commit = commit_frame(&frame);
        frame.push(0xAB);
        commit.push(0xAB);
        assert_eq!(decode_hello(&frame), Err(SecureError::Malformed));
        assert_eq!(decode_commit(&commit), Err(SecureError::Malformed));
    }
    #[test]
    fn decode_reports_version_mismatch_even_if_wire_format_differs() {
        // Simulate a peer on an older protocol version whose `Hello`/`Commit` has a completely
        // different (incompatible) tail after `protocol_version` -- decoding must still report
        // the version mismatch cleanly rather than a generic Malformed error from failing to
        // parse the rest of the (differently-shaped) struct.
        for tag in [FRAME_HELLO, FRAME_COMMIT] {
            let mut frame = vec![tag];
            frame.extend(postcard::to_allocvec(&1u16).unwrap()); // old protocol_version = 1
            frame.extend([0xde, 0xad, 0xbe, 0xef, 0xff, 0xff]);
            let got = if tag == FRAME_HELLO {
                decode_hello(&frame).map(|_| ())
            } else {
                decode_commit(&frame).map(|_| ())
            };
            assert_eq!(got, Err(SecureError::Version(1)));
        }
    }
}
