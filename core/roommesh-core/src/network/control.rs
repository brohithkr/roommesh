//! Secure control channel: per-peer handshake and sealing of `ControlMessage`s, plus the
//! shared table of realtime ciphers used by the audio runtime.
use crate::ids::PeerId;
use crate::network::secure::{decode_hello, Handshake, RtCipher, SecureError, Session, FRAME_HELLO, FRAME_SEALED};
use crate::room::protocol::{self, ControlMessage, ProtocolError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub type RtSessions = Arc<RwLock<HashMap<PeerId, Arc<RtCipher>>>>;

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    SessionUp { peer: PeerId, name: String, sas: String },
    Message { from: PeerId, msg: ControlMessage },
    SessionDown(PeerId),
}

#[derive(Debug, Default)]
pub struct FrameOutcome {
    pub event: Option<ControlEvent>,
    pub reply: Option<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error(transparent)]
    Secure(#[from] SecureError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("hello identity mismatch")]
    IdentityMismatch,
    #[error("no session with peer")]
    NoSession,
    #[error("empty frame")]
    Empty,
    #[error("unexpected hello on an established session")]
    UnexpectedHello,
}

pub struct ControlChannel {
    local: PeerId,
    name: String,
    pending: HashMap<PeerId, Handshake>,
    sessions: HashMap<PeerId, Session>,
    rt: RtSessions,
}

impl ControlChannel {
    pub fn new(local: PeerId, name: String) -> Self {
        Self { local, name, pending: HashMap::new(), sessions: HashMap::new(), rt: Arc::default() }
    }
    pub fn realtime_sessions(&self) -> RtSessions {
        self.rt.clone()
    }
    pub fn has_session(&self, peer: PeerId) -> bool {
        self.sessions.contains_key(&peer)
    }
    pub fn sas(&self, peer: PeerId) -> Option<String> {
        self.sessions.get(&peer).map(|s| s.sas().to_string())
    }
    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    /// A (new) transport connection to `peer` is up. Only the numerically lower `PeerId` of the
    /// pair opens with a Hello; the higher side returns `None` and waits to answer. This makes
    /// the initial handshake deterministic instead of a race where both sides send an opening
    /// Hello and each has to disambiguate the other's -- the source of a potential handshake
    /// storm on a simultaneous/duplicate connect.
    pub fn on_connected(&mut self, peer: PeerId) -> Option<Vec<u8>> {
        self.drop_session(peer);
        if self.local > peer {
            return None;
        }
        let hs = Handshake::new(self.local, self.name.clone());
        let hello = hs.hello();
        self.pending.insert(peer, hs);
        Some(hello)
    }

    pub fn on_frame(&mut self, peer: PeerId, frame: &[u8]) -> Result<FrameOutcome, ControlError> {
        match frame.first() {
            None => Err(ControlError::Empty),
            Some(&FRAME_HELLO) => self.on_hello(peer, frame),
            Some(&FRAME_SEALED) => {
                let s = self.sessions.get_mut(&peer).ok_or(ControlError::NoSession)?;
                match s.open_control(frame) {
                    Ok(pt) => {
                        let msg = protocol::decode(&pt)?;
                        Ok(FrameOutcome { event: Some(ControlEvent::Message { from: peer, msg }), reply: None })
                    }
                    // A sealed frame that fails to decrypt/authenticate means our session state
                    // has desynced from the peer's (e.g. it restarted and rekeyed without us
                    // noticing). Keep retrying forever would just accumulate more garbage; drop
                    // the session and tell the caller so it can reconnect from scratch.
                    Err(SecureError::Crypto) => {
                        self.drop_session(peer);
                        Ok(FrameOutcome { event: Some(ControlEvent::SessionDown(peer)), reply: None })
                    }
                    Err(e) => Err(e.into()),
                }
            }
            Some(_) => Err(ControlError::Secure(SecureError::Malformed)),
        }
    }

    fn on_hello(&mut self, peer: PeerId, frame: &[u8]) -> Result<FrameOutcome, ControlError> {
        let hello = decode_hello(frame)?;
        if hello.peer_id != peer {
            return Err(ControlError::IdentityMismatch);
        }
        // Once a session is up, a plaintext Hello is unexpected unless it's answering a fresh
        // handshake we ourselves just started (on_connected). Anything else -- a stray replay, a
        // forged/duplicate Hello, a late retransmission -- is rejected outright rather than
        // silently tearing down (and re-keying) a perfectly good session.
        if self.sessions.contains_key(&peer) && !self.pending.contains_key(&peer) {
            return Err(ControlError::UnexpectedHello);
        }
        match hello.reply_to {
            Some(expected) => {
                // A reply to one of our own opening Hellos: only accept it if it actually
                // answers our current pending handshake, and never answer a reply with another
                // Hello -- that would turn every stray/late reply into a new round.
                let Some(hs) = self.pending.get(&peer).filter(|hs| hs.public_key() == expected) else {
                    return Ok(FrameOutcome::default());
                };
                // Don't consume the pending handshake until `complete` actually succeeds: on a
                // transient failure (e.g. a version mismatch) a subsequent, valid reply to the
                // same opening Hello should still be able to complete it.
                let session = hs.complete(&hello)?;
                self.pending.remove(&peer);
                let ev = ControlEvent::SessionUp {
                    peer,
                    name: session.remote_name().to_string(),
                    sas: session.sas().to_string(),
                };
                self.rt.write().insert(peer, session.realtime());
                self.sessions.insert(peer, session);
                Ok(FrameOutcome { event: Some(ev), reply: None })
            }
            None => {
                // An opening Hello from the peer: we answer with our own Hello naming theirs as
                // the one we're replying to, and complete our side immediately -- we already
                // have everything we need (our fresh keypair plus their public key).
                self.pending.remove(&peer);
                let hs = Handshake::new(self.local, self.name.clone());
                let reply = hs.hello_reply(&hello);
                let session = hs.complete(&hello)?;
                let ev = ControlEvent::SessionUp {
                    peer,
                    name: session.remote_name().to_string(),
                    sas: session.sas().to_string(),
                };
                self.rt.write().insert(peer, session.realtime());
                self.sessions.insert(peer, session);
                Ok(FrameOutcome { event: Some(ev), reply: Some(reply) })
            }
        }
    }

    pub fn seal(&mut self, peer: PeerId, msg: &ControlMessage) -> Option<Vec<u8>> {
        let s = self.sessions.get_mut(&peer)?;
        Some(s.seal_control(&protocol::encode(msg)))
    }

    fn drop_session(&mut self, peer: PeerId) -> bool {
        self.pending.remove(&peer);
        self.rt.write().remove(&peer);
        self.sessions.remove(&peer).is_some()
    }

    pub fn on_disconnected(&mut self, peer: PeerId) -> Option<ControlEvent> {
        self.drop_session(peer).then_some(ControlEvent::SessionDown(peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::network::realtime::*;
    use crate::room::protocol::ControlMessage;

    #[test]
    fn handshake_both_sides_connected() {
        // PeerId(1) < PeerId(2): only A (the lower id) opens with a Hello; B answers it.
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2)).expect("lower peer id opens");
        assert!(b.on_connected(PeerId(1)).is_none(), "higher peer id never opens");
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        let reply = ob.reply.clone().expect("b answers the opening hello");
        let oa = a.on_frame(PeerId(2), &reply).unwrap();
        assert!(oa.reply.is_none(), "a must not answer b's reply with another hello");
        let (Some(ControlEvent::SessionUp { sas: s1, .. }), Some(ControlEvent::SessionUp { sas: s2, name, .. })) =
            (ob.event, oa.event)
        else {
            panic!()
        };
        assert_eq!(s1, s2);
        assert_eq!(name, "B");
        let msg = ControlMessage::Leave { room_id: RoomId(3) };
        let frame = a.seal(PeerId(2), &msg).unwrap();
        match b.on_frame(PeerId(1), &frame).unwrap().event {
            Some(ControlEvent::Message { from, msg: m }) => {
                assert_eq!(from, PeerId(1));
                assert_eq!(m, msg);
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn responder_without_on_connected_replies_with_hello() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        assert!(b.on_connected(PeerId(1)).is_none(), "b is the higher peer id: it never opens");
        let ha = a.on_connected(PeerId(2)).unwrap();
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        let reply = ob.reply.expect("b must answer with its hello");
        assert!(matches!(a.on_frame(PeerId(2), &reply).unwrap().event, Some(ControlEvent::SessionUp { .. })));
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
    #[test]
    fn duplicate_on_connected_from_lower_side_converges_without_a_storm() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        // A duplicate/spurious "connected" event on the lower side must not produce more than
        // one live handshake attempt or any unbounded reply traffic: the second call simply
        // supersedes the first (only the latest pending handshake is ever completed).
        let _stale = a.on_connected(PeerId(2)).unwrap();
        let ha = a.on_connected(PeerId(2)).expect("lower peer id opens");
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        let reply = ob.reply.clone().expect("b answers the opening hello");
        let oa = a.on_frame(PeerId(2), &reply).unwrap();
        assert!(oa.reply.is_none(), "a's reply-to-a-reply must not generate another hello");
        let (Some(ControlEvent::SessionUp { sas: s1, .. }), Some(ControlEvent::SessionUp { sas: s2, .. })) =
            (ob.event, oa.event)
        else {
            panic!("expected exactly one SessionUp per side")
        };
        assert_eq!(s1, s2);
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
    #[test]
    fn unexpected_hello_on_established_session_is_rejected() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2)).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert!(a.has_session(PeerId(2)));
        let sas_before = a.sas(PeerId(2));
        // An unrelated, freshly generated Hello claiming to be from B, injected while A still
        // has no pending handshake of its own for B: A must reject it and keep the existing
        // session (and SAS) untouched, rather than silently re-keying.
        let forged = Handshake::new(PeerId(2), "B".into()).hello();
        let err = a.on_frame(PeerId(2), &forged).unwrap_err();
        assert!(matches!(err, ControlError::UnexpectedHello));
        assert!(a.has_session(PeerId(2)));
        assert_eq!(a.sas(PeerId(2)), sas_before);
    }
    #[test]
    fn crypto_failure_on_sealed_frame_drops_session_and_reports_down() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2)).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        let msg = ControlMessage::Leave { room_id: RoomId(3) };
        let mut frame = a.seal(PeerId(2), &msg).unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0xff; // corrupt the ciphertext/tag
        let outcome = b.on_frame(PeerId(1), &frame).unwrap();
        assert!(matches!(outcome.event, Some(ControlEvent::SessionDown(p)) if p == PeerId(1)));
        assert!(!b.has_session(PeerId(1)));
    }
    #[test]
    fn pending_handshake_survives_a_failed_complete() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let ha = a.on_connected(PeerId(2)).unwrap();
        let opening = decode_hello(&ha).unwrap();
        let hs_b = Handshake::new(PeerId(2), "B".into());
        // A reply that matches A's pending handshake's key but fails to complete (bad version).
        let mut bad_reply = decode_hello(&hs_b.hello_reply(&opening)).unwrap();
        bad_reply.protocol_version += 1;
        let mut bad_frame = vec![FRAME_HELLO];
        bad_frame.extend(postcard::to_allocvec(&bad_reply).unwrap());
        assert!(a.on_frame(PeerId(2), &bad_frame).is_err());
        assert!(!a.has_session(PeerId(2)));
        // The pending handshake must still be there: a subsequent, well-formed reply to the
        // *same* opening Hello completes normally.
        let good_reply = hs_b.hello_reply(&opening);
        assert!(matches!(a.on_frame(PeerId(2), &good_reply).unwrap().event, Some(ControlEvent::SessionUp { .. })));
        assert!(a.has_session(PeerId(2)));
    }
    #[test]
    fn identity_mismatch_rejected_and_realtime_keys_shared() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2)).unwrap();
        assert!(b.on_frame(PeerId(9), &ha).is_err());
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        let reply = ob.reply.expect("b answers the opening hello");
        a.on_frame(PeerId(2), &reply).unwrap();
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
        let pkt = a.realtime_sessions().read().get(&PeerId(2)).unwrap().seal(&h, b"x");
        let (hh, p) = b.realtime_sessions().read().get(&PeerId(1)).unwrap().open(&pkt).unwrap();
        assert_eq!((hh.sender, p.as_slice()), (PeerId(1), &b"x"[..]));
        assert!(matches!(b.on_disconnected(PeerId(1)), Some(ControlEvent::SessionDown(_))));
        assert!(b.realtime_sessions().read().get(&PeerId(1)).is_none());
    }
}
