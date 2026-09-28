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

    /// A (new) transport connection to `peer` is up: returns our Hello frame to send.
    pub fn on_connected(&mut self, peer: PeerId) -> Vec<u8> {
        self.drop_session(peer);
        let hs = Handshake::new(self.local, self.name.clone());
        let hello = hs.hello();
        self.pending.insert(peer, hs);
        hello
    }

    pub fn on_frame(&mut self, peer: PeerId, frame: &[u8]) -> Result<FrameOutcome, ControlError> {
        match frame.first() {
            None => Err(ControlError::Empty),
            Some(&FRAME_HELLO) => {
                let hello = decode_hello(frame)?;
                if hello.peer_id != peer {
                    return Err(ControlError::IdentityMismatch);
                }
                let (hs, reply) = match self.pending.remove(&peer) {
                    Some(hs) => (hs, None),
                    None => {
                        let hs = Handshake::new(self.local, self.name.clone());
                        let r = hs.hello();
                        (hs, Some(r))
                    }
                };
                let session = hs.complete(&hello)?;
                let ev = ControlEvent::SessionUp {
                    peer,
                    name: session.remote_name().to_string(),
                    sas: session.sas().to_string(),
                };
                self.rt.write().insert(peer, session.realtime());
                self.sessions.insert(peer, session);
                Ok(FrameOutcome { event: Some(ev), reply })
            }
            Some(&FRAME_SEALED) => {
                let s = self.sessions.get_mut(&peer).ok_or(ControlError::NoSession)?;
                let pt = s.open_control(frame)?;
                let msg = protocol::decode(&pt)?;
                Ok(FrameOutcome { event: Some(ControlEvent::Message { from: peer, msg }), reply: None })
            }
            Some(_) => Err(ControlError::Secure(SecureError::Malformed)),
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
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2));
        let hb = b.on_connected(PeerId(1));
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        let oa = a.on_frame(PeerId(2), &hb).unwrap();
        assert!(ob.reply.is_none() && oa.reply.is_none());
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
        let ha = a.on_connected(PeerId(2));
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        let reply = ob.reply.expect("b must answer with its hello");
        assert!(matches!(a.on_frame(PeerId(2), &reply).unwrap().event, Some(ControlEvent::SessionUp { .. })));
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
    #[test]
    fn identity_mismatch_rejected_and_realtime_keys_shared() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2));
        assert!(b.on_frame(PeerId(9), &ha).is_err());
        let hb = b.on_connected(PeerId(1));
        b.on_frame(PeerId(1), &ha).unwrap();
        a.on_frame(PeerId(2), &hb).unwrap();
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
