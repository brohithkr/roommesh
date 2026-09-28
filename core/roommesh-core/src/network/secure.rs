//! Per-connection secure session. Both sides send a plaintext Hello carrying an ephemeral
//! X25519 key; keys are derived with HKDF-SHA256. Control frames use an implicit per-direction
//! counter nonce (TCP is ordered). Realtime packets use nonce = kind|stream|sequence and the
//! 44-byte header as AAD, so reordering/loss is fine.
use crate::ids::PeerId;
use crate::network::realtime::{decode_packet, header_bytes, RtError, RtHeader, HEADER_LEN};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use x25519_dalek::{PublicKey, StaticSecret};

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_SEALED: u8 = 0x02;
/// Plaintext nudge asking the recipient to (re)send its opening Hello. Sent only by the higher
/// `PeerId` of a pair (which never opens with a Hello itself) and answered only by the lower
/// `PeerId` -- see `network::control::ControlChannel`.
pub const FRAME_HELLO_REQUEST: u8 = 0x03;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub peer_id: PeerId,
    pub name: String,
    pub public_key: [u8; 32],
    /// Set only when this Hello answers an opening Hello: the public key being answered. A
    /// Hello with `reply_to` set must never itself be answered with another Hello -- that
    /// invariant is what keeps the handshake to a single request/response round instead of an
    /// unbounded ping-pong when both peers race to connect or a duplicate Hello arrives.
    pub reply_to: Option<[u8; 32]>,
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
    #[error(transparent)]
    Rt(#[from] RtError),
}

pub fn decode_hello(frame: &[u8]) -> Result<Hello, SecureError> {
    if frame.first() != Some(&FRAME_HELLO) {
        return Err(SecureError::Malformed);
    }
    let body = &frame[1..];
    // `protocol_version` is `Hello`'s first field, so peek it with a standalone decode before
    // attempting to decode the whole struct: a different protocol version may use a different
    // wire shape entirely (fields added/removed/reordered), which would otherwise surface as an
    // opaque "malformed frame" instead of the actionable version mismatch.
    let (version, _) =
        postcard::take_from_bytes::<u16>(body).map_err(|_| SecureError::Malformed)?;
    if version != crate::room::state::PROTOCOL_VERSION {
        return Err(SecureError::Version(version));
    }
    let (hello, rest) = postcard::take_from_bytes(body).map_err(|_| SecureError::Malformed)?;
    if !rest.is_empty() {
        return Err(SecureError::Malformed);
    }
    Ok(hello)
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
    /// Plaintext frame to send first on a fresh control connection (an "opening" Hello:
    /// `reply_to: None`).
    pub fn hello(&self) -> Vec<u8> {
        self.build_hello(None)
    }
    /// Plaintext reply to an opening Hello, naming the public key it answers. Never call this
    /// in response to a Hello that itself has `reply_to` set.
    pub fn hello_reply(&self, opening: &Hello) -> Vec<u8> {
        self.build_hello(Some(opening.public_key))
    }
    fn build_hello(&self, reply_to: Option<[u8; 32]>) -> Vec<u8> {
        let h = Hello {
            protocol_version: crate::room::state::PROTOCOL_VERSION,
            peer_id: self.local,
            name: self.name.clone(),
            public_key: self.public.to_bytes(),
            reply_to,
        };
        let mut v = vec![FRAME_HELLO];
        v.extend(postcard::to_allocvec(&h).expect("hello"));
        v
    }
    pub fn complete(&self, remote: &Hello) -> Result<Session, SecureError> {
        if remote.protocol_version != crate::room::state::PROTOCOL_VERSION {
            return Err(SecureError::Version(remote.protocol_version));
        }
        if remote.peer_id == self.local {
            return Err(SecureError::SelfConnection);
        }
        let shared = self
            .secret
            .diffie_hellman(&PublicKey::from(remote.public_key));
        let local_is_low = self.local < remote.peer_id;
        let (lo_pub, hi_pub) = if local_is_low {
            (self.public.to_bytes(), remote.public_key)
        } else {
            (remote.public_key, self.public.to_bytes())
        };
        let mut salt = Vec::with_capacity(64);
        salt.extend_from_slice(&lo_pub);
        salt.extend_from_slice(&hi_pub);
        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
        let key = |label: &str| {
            let mut k = [0u8; 32];
            hk.expand(label.as_bytes(), &mut k).expect("hkdf");
            ChaCha20Poly1305::new(Key::from_slice(&k))
        };
        let (c_lo_hi, c_hi_lo) = (
            key("roommesh v1 ctrl lo->hi"),
            key("roommesh v1 ctrl hi->lo"),
        );
        let (r_lo_hi, r_hi_lo) = (key("roommesh v1 rt lo->hi"), key("roommesh v1 rt hi->lo"));
        let mut sas_bytes = [0u8; 4];
        hk.expand(b"roommesh v1 sas", &mut sas_bytes).expect("hkdf");
        let code = u32::from_le_bytes(sas_bytes) % 1_000_000;
        let (ctrl_tx, ctrl_rx, rt_tx, rt_rx) = if local_is_low {
            (c_lo_hi, c_hi_lo, r_lo_hi, r_hi_lo)
        } else {
            (c_hi_lo, c_lo_hi, r_hi_lo, r_lo_hi)
        };
        Ok(Session {
            remote: remote.peer_id,
            remote_name: remote.name.clone(),
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

    fn pair() -> (Session, Session) {
        let a = Handshake::new(PeerId(1), "A".into());
        let b = Handshake::new(PeerId(2), "B".into());
        let ha = a.hello();
        let hb = b.hello();
        let sa = a.complete(&decode_hello(&hb).unwrap()).unwrap();
        let sb = b.complete(&decode_hello(&ha).unwrap()).unwrap();
        (sa, sb)
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
        // Compare only the ciphertext body (excluding the header/AAD and the 16-byte tag): with
        // the epoch folded into the nonce this must differ even though the plaintext is
        // identical, otherwise the same (key, nonce) pair would be reused across epochs.
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
        let hello = decode_hello(&a.hello()).unwrap();
        match a.complete(&hello) {
            Err(e) => assert_eq!(e, SecureError::SelfConnection),
            Ok(_) => panic!("expected SelfConnection error"),
        }
    }
    #[test]
    fn protocol_version_mismatch_rejected() {
        let a = Handshake::new(PeerId(1), "A".into());
        let b = Handshake::new(PeerId(2), "B".into());
        let mut hb = decode_hello(&b.hello()).unwrap();
        hb.protocol_version += 1;
        match a.complete(&hb) {
            Err(e) => assert_eq!(e, SecureError::Version(hb.protocol_version)),
            Ok(_) => panic!("expected Version error"),
        }
    }
    #[test]
    fn decode_hello_rejects_trailing_bytes() {
        let a = Handshake::new(PeerId(1), "A".into());
        let mut frame = a.hello();
        frame.push(0xAB);
        assert_eq!(decode_hello(&frame), Err(SecureError::Malformed));
    }
    #[test]
    fn decode_hello_reports_version_mismatch_even_if_wire_format_differs() {
        // Simulate a peer on an older protocol version whose `Hello` has a completely different
        // (incompatible) tail after `protocol_version` -- decode_hello must still report the
        // version mismatch cleanly rather than a generic Malformed error from failing to parse
        // the rest of the (differently-shaped) struct.
        let mut frame = vec![FRAME_HELLO];
        frame.extend(postcard::to_allocvec(&1u16).unwrap()); // old protocol_version = 1
        frame.extend([0xde, 0xad, 0xbe, 0xef, 0xff, 0xff]);
        assert_eq!(decode_hello(&frame), Err(SecureError::Version(1)));
    }
}
