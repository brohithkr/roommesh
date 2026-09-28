//! Secure control channel: per-peer handshake and sealing of `ControlMessage`s, plus the
//! shared table of realtime ciphers used by the audio runtime.
use crate::ids::PeerId;
use crate::network::secure::{
    decode_hello, Handshake, RtCipher, SecureError, Session, FRAME_HELLO, FRAME_HELLO_REQUEST, FRAME_SEALED,
};
use crate::room::protocol::{self, ControlMessage, ProtocolError};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
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

/// An in-flight handshake we (the lower `PeerId`, or a higher id answering a cold
/// HELLO_REQUEST) initiated: the key material plus the exact opening-Hello bytes we sent, kept
/// around so a HELLO_REQUEST retry can resend the identical frame instead of generating new
/// keys (which would make the two attempts unable to agree on a single session).
struct Pending {
    hs: Handshake,
    hello_bytes: Vec<u8>,
}

pub struct ControlChannel {
    local: PeerId,
    name: String,
    pending: HashMap<PeerId, Pending>,
    sessions: HashMap<PeerId, Session>,
    rt: RtSessions,
    /// Peers for whom `on_connected` started a handshake attempt that hasn't resolved into a
    /// session yet, with the local timestamp (ms) it started at. Cleared on `SessionUp` and on
    /// `on_disconnected`; polled (and pruned) by `stalled()`.
    awaiting: HashMap<PeerId, u64>,
    /// Peers whose most recent `on_connected` call hasn't yet been consumed by a resulting
    /// `SessionUp`. Sending an opening Hello to a peer we still believe has a session recorded
    /// this way tells `on_hello` it's allowed to accept the Hello anyway (the peer likely
    /// restarted and our own state is stale) instead of rejecting it as unexpected. One-shot:
    /// consumed the moment it's actually used to accept a Hello.
    fresh: HashSet<PeerId>,
}

impl ControlChannel {
    pub fn new(local: PeerId, name: String) -> Self {
        Self {
            local,
            name,
            pending: HashMap::new(),
            sessions: HashMap::new(),
            rt: Arc::default(),
            awaiting: HashMap::new(),
            fresh: HashSet::new(),
        }
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

    fn start_handshake(&mut self, peer: PeerId) -> Vec<u8> {
        let hs = Handshake::new(self.local, self.name.clone());
        let hello = hs.hello();
        self.pending.insert(peer, Pending { hs, hello_bytes: hello.clone() });
        hello
    }

    /// A (new) transport connection to `peer` is up. Only the numerically lower `PeerId` of the
    /// pair opens with a plaintext Hello; the higher side sends a HELLO_REQUEST instead, nudging
    /// the lower side into (re)sending its Hello. This keeps the initial handshake deterministic
    /// (no race where both sides send an opening Hello) while still letting either side recover
    /// if the other's original Hello was lost, delayed, or simply arrived before we were ready
    /// for it.
    ///
    /// If we already believe we have a working session with `peer`, this is a no-op: a late or
    /// duplicate `Connected` notification must never tear down a session that's still good. We
    /// still mark the connection `fresh`, though, so that if the peer really did restart (and
    /// sends a new opening Hello despite our now-stale session) `on_hello` won't reject it.
    pub fn on_connected(&mut self, peer: PeerId, now_ms: u64) -> Option<Vec<u8>> {
        self.fresh.insert(peer);
        if self.sessions.contains_key(&peer) {
            return None;
        }
        self.awaiting.insert(peer, now_ms);
        if self.local > peer {
            return Some(vec![FRAME_HELLO_REQUEST]);
        }
        Some(self.start_handshake(peer))
    }

    pub fn on_frame(&mut self, peer: PeerId, frame: &[u8]) -> Result<FrameOutcome, ControlError> {
        match frame.first() {
            None => Err(ControlError::Empty),
            Some(&FRAME_HELLO) => self.on_hello(peer, frame),
            Some(&FRAME_HELLO_REQUEST) => Ok(self.on_hello_request(peer)),
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

    /// Only the lower `PeerId` of a pair ever answers a HELLO_REQUEST -- the higher id is the
    /// one that sends them (from `on_connected`) and never answers one itself.
    fn on_hello_request(&mut self, peer: PeerId) -> FrameOutcome {
        if self.local > peer {
            return FrameOutcome::default();
        }
        if let Some(p) = self.pending.get(&peer) {
            // Already mid-handshake: resend the exact same opening Hello (idempotent) rather
            // than generating fresh key material, so a reply to either copy still completes it.
            return FrameOutcome { event: None, reply: Some(p.hello_bytes.clone()) };
        }
        if self.sessions.contains_key(&peer) {
            // We think we have a session, but our peer is asking us for a Hello -- it evidently
            // doesn't. Drop our stale side, tell the caller, and restart the handshake.
            self.drop_session(peer);
            let hello = self.start_handshake(peer);
            return FrameOutcome { event: Some(ControlEvent::SessionDown(peer)), reply: Some(hello) };
        }
        // Cold start: neither pending nor session -- begin a fresh handshake.
        FrameOutcome { event: None, reply: Some(self.start_handshake(peer)) }
    }

    fn on_hello(&mut self, peer: PeerId, frame: &[u8]) -> Result<FrameOutcome, ControlError> {
        let hello = decode_hello(frame)?;
        if hello.peer_id != peer {
            return Err(ControlError::IdentityMismatch);
        }
        let is_lower = self.local < peer;
        match hello.reply_to {
            Some(expected) => {
                if !is_lower {
                    // We're the higher id: we never send opening Hellos of our own, so nothing
                    // we have could ever be "replied to". Ignore a stray/foreign reply outright.
                    return Ok(FrameOutcome::default());
                }
                // A reply to one of our own opening Hellos: only accept it if it actually
                // answers our current pending handshake, and never answer a reply with another
                // Hello -- that would turn every stray/late reply into a new round.
                let Some(p) = self.pending.get(&peer).filter(|p| p.hs.public_key() == expected) else {
                    return Ok(FrameOutcome::default());
                };
                // Don't consume the pending handshake until `complete` actually succeeds: on a
                // transient failure (e.g. a version mismatch) a subsequent, valid reply to the
                // same opening Hello should still be able to complete it.
                let session = p.hs.complete(&hello)?;
                self.pending.remove(&peer);
                self.awaiting.remove(&peer);
                self.fresh.remove(&peer);
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
                if is_lower {
                    // Only the higher id may legitimately send an opening Hello -- the lower id
                    // always initiates. Anything claiming otherwise is bogus and must not be
                    // allowed to disrupt our own outstanding handshake attempt or session.
                    return Err(ControlError::UnexpectedHello);
                }
                // Once a session is up, a new opening Hello is unexpected unless this connection
                // was (re)announced to us since then (`fresh`) -- e.g. the peer genuinely
                // restarted and we've since gotten a fresh `on_connected` for it. Anything else
                // -- a stray replay, a forged Hello, a late retransmission -- is rejected
                // outright rather than silently tearing down (and re-keying) a perfectly good
                // session.
                let fresh = self.fresh.contains(&peer);
                if self.sessions.contains_key(&peer) && !fresh {
                    return Err(ControlError::UnexpectedHello);
                }
                self.pending.remove(&peer); // defensive; the higher id never legitimately has one
                let hs = Handshake::new(self.local, self.name.clone());
                let reply = hs.hello_reply(&hello);
                let session = hs.complete(&hello)?;
                self.fresh.remove(&peer);
                self.awaiting.remove(&peer);
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
        self.fresh.remove(&peer);
        self.awaiting.remove(&peer);
        self.rt.write().remove(&peer);
        self.sessions.remove(&peer).is_some()
    }

    pub fn on_disconnected(&mut self, peer: PeerId) -> Option<ControlEvent> {
        self.drop_session(peer).then_some(ControlEvent::SessionDown(peer))
    }

    /// Peers whose handshake (started by our own `on_connected`) has been outstanding for at
    /// least `timeout_ms` as of `now_ms`. Each returned peer's timer is reset to `now_ms`, so a
    /// caller polling this on an interval gets a given stalled peer reported at most once per
    /// `timeout_ms` window rather than on every poll.
    pub fn stalled(&mut self, now_ms: u64, timeout_ms: u64) -> Vec<PeerId> {
        let mut out = Vec::new();
        for (&peer, started) in self.awaiting.iter_mut() {
            if now_ms.saturating_sub(*started) >= timeout_ms {
                out.push(peer);
                *started = now_ms;
            }
        }
        out
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
        // PeerId(1) < PeerId(2): only A (the lower id) opens with a Hello; B sends a
        // HELLO_REQUEST instead (redundant here since A's Hello is already in flight, but
        // harmless -- A just resends the same pending Hello if it ever receives it).
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).expect("lower peer id opens");
        let hb = b.on_connected(PeerId(1), 0).expect("higher peer id sends a hello request");
        assert_eq!(hb[0], FRAME_HELLO_REQUEST);
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
        let ha = a.on_connected(PeerId(2), 0).unwrap();
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
        let _stale = a.on_connected(PeerId(2), 0).unwrap();
        let ha = a.on_connected(PeerId(2), 0).expect("lower peer id opens");
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
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert!(a.has_session(PeerId(2)));
        let sas_before = a.sas(PeerId(2));
        // An unrelated, freshly generated opening Hello claiming to be from B, injected while A
        // has no fresh/pending handshake of its own for B: A must reject it (both because it's
        // an opening Hello received by the lower id, and because A's session isn't `fresh`) and
        // keep the existing session (and SAS) untouched, rather than silently re-keying.
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
        let ha = a.on_connected(PeerId(2), 0).unwrap();
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
        let ha = a.on_connected(PeerId(2), 0).unwrap();
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
        let ha = a.on_connected(PeerId(2), 0).unwrap();
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

    // -- Regressions ported from the reviewer's `review_probe` scratch module --

    #[test]
    fn probe_b_stale_opening_hello_reply_does_not_complete_current_pending() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let h1 = a.on_connected(PeerId(2), 0).unwrap();
        let h2 = a.on_connected(PeerId(2), 0).unwrap(); // lower side "reconnected"; b never told
        let r1 = b.on_frame(PeerId(1), &h1).unwrap().reply.unwrap();
        assert!(
            b.on_frame(PeerId(1), &h2).is_err(),
            "b already has a session with a and was never told of a new connection"
        );
        // r1 answers the now-superseded h1, not a's current pending (from h2): it must not
        // complete a's side, and must not disturb b's already-established session.
        let oa = a.on_frame(PeerId(2), &r1).unwrap();
        assert!(oa.event.is_none());
        assert!(!a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
    #[test]
    fn probe_c_late_connected_notification_does_not_disrupt_an_established_session() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        // b processes the Hello before ever getting its own Connected notification -- violating
        // the documented ordering invariant, but the handshake still completes fine.
        let ob = b.on_frame(PeerId(1), &ha).unwrap();
        a.on_frame(PeerId(2), &ob.reply.unwrap()).unwrap();
        assert!(matches!(ob.event, Some(ControlEvent::SessionUp { .. })));
        // A late/duplicate Connected on b's side (self-healing regression: this used to
        // unconditionally drop the session in `on_connected`) must not disrupt it.
        let _hb = b.on_connected(PeerId(1), 10);
        assert!(b.has_session(PeerId(1)), "a late Connected must not destroy a good session");
        let f = a.seal(PeerId(2), &ControlMessage::Leave { room_id: RoomId(1) }).unwrap();
        assert!(matches!(b.on_frame(PeerId(1), &f).unwrap().event, Some(ControlEvent::Message { .. })));
    }
    #[test]
    fn probe_d_restart_rejected_without_fresh_connected_but_recovers_with_one() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
        // A restarts (fresh process, no memory of the old session) and reconnects.
        let mut a2 = ControlChannel::new(PeerId(1), "A".into());
        let h = a2.on_connected(PeerId(2), 20).unwrap();
        // b's transport hasn't told it about this yet: reject rather than silently re-keying.
        assert!(b.on_frame(PeerId(1), &h).is_err());
        assert!(b.has_session(PeerId(1)));
        // Once b's transport does tell it about the (re)connection, the restart is accepted.
        assert!(b.on_connected(PeerId(1), 21).is_none(), "b already has a session: on_connected is a no-op");
        let ob = b.on_frame(PeerId(1), &h).unwrap();
        assert!(matches!(ob.event, Some(ControlEvent::SessionUp { .. })));
        a2.on_frame(PeerId(2), &ob.reply.unwrap()).unwrap();
        assert!(a2.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
    #[test]
    fn probe_e_injected_opening_hello_at_lower_side_is_rejected_and_real_handshake_still_completes() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let mallory = Handshake::new(PeerId(2), "B".into());
        // An opening Hello claiming to be from B (the higher id), injected at A (the lower id):
        // only the higher id may legitimately open a handshake, so this must be rejected
        // outright, and it must not disturb A's real outstanding attempt.
        let err = a.on_frame(PeerId(2), &mallory.hello()).unwrap_err();
        assert!(matches!(err, ControlError::UnexpectedHello));
        assert!(!a.has_session(PeerId(2)));
        let r = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        assert!(matches!(a.on_frame(PeerId(2), &r).unwrap().event, Some(ControlEvent::SessionUp { .. })));
    }

    #[test]
    fn hello_request_on_cold_peer_starts_a_handshake() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        // b (higher) tells a (lower) it's connected: a opens. b never calls on_connected itself
        // here; instead a HELLO_REQUEST arrives out of band (e.g. from a retried Connected).
        let req = vec![FRAME_HELLO_REQUEST];
        let out = a.on_frame(PeerId(2), &req).unwrap();
        assert!(out.event.is_none());
        let hello = out.reply.expect("the lower id answers a cold HELLO_REQUEST with a Hello");
        let ob = b.on_frame(PeerId(1), &hello).unwrap();
        a.on_frame(PeerId(2), &ob.reply.unwrap()).unwrap();
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
    #[test]
    fn hello_request_while_pending_resends_the_same_hello() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let out = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap();
        assert_eq!(out.reply.unwrap(), ha, "must resend the identical pending Hello, not a new one");
    }
    #[test]
    fn hello_request_on_stale_session_drops_it_and_restarts() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert!(a.has_session(PeerId(2)));
        // b lost its session (e.g. restarted) and is asking a for a fresh Hello.
        let out = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap();
        assert!(matches!(out.event, Some(ControlEvent::SessionDown(p)) if p == PeerId(2)));
        assert!(!a.has_session(PeerId(2)));
        assert!(out.reply.is_some(), "a fresh opening Hello is sent");
    }
    #[test]
    fn higher_id_never_answers_a_hello_request() {
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let out = b.on_frame(PeerId(1), &[FRAME_HELLO_REQUEST]).unwrap();
        assert!(out.event.is_none() && out.reply.is_none());
    }

    #[test]
    fn stalled_reports_a_peer_after_timeout_and_resets_its_timer() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        a.on_connected(PeerId(2), 1_000);
        assert_eq!(a.stalled(1_500, 2_000), Vec::new(), "not yet timed out");
        assert_eq!(a.stalled(3_500, 2_000), vec![PeerId(2)]);
        // The timer was reset: polling again immediately must not re-report it.
        assert_eq!(a.stalled(3_600, 2_000), Vec::new());
        assert_eq!(a.stalled(5_600, 2_000), vec![PeerId(2)]);
    }
    #[test]
    fn stalled_is_cleared_on_session_up_and_on_disconnected() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert_eq!(a.stalled(100_000, 1), Vec::new(), "a resolved handshake is not stalled");
        let mut c = ControlChannel::new(PeerId(1), "A".into());
        c.on_connected(PeerId(3), 0);
        c.on_disconnected(PeerId(3));
        assert_eq!(c.stalled(100_000, 1), Vec::new(), "a disconnected peer is not stalled");
    }
}
