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
    /// The session was torn down as part of a legitimate in-band re-open (a HELLO_REQUEST we
    /// answered after finding our side stale, or `on_disconnected`). The underlying transport
    /// connection is either already known to be gone, or still fine and about to carry a fresh
    /// handshake -- the caller should just notify the engine, not tear down the transport.
    SessionDown(PeerId),
    /// A sealed frame failed to decrypt/authenticate: our session state desynced from the
    /// peer's in a way a fresh handshake over the *same* connection can't explain away, which
    /// makes the connection itself suspect. The caller should treat this like a connection
    /// failure -- notify the engine AND disconnect the transport so a fresh `on_connected`
    /// starts a clean handshake, rather than assuming a quick re-key will fix it.
    SessionFailed(PeerId),
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
    /// Peers for whom a handshake attempt is outstanding, with its start time: `Some(ms)` once
    /// stamped, or `None` for a handshake started from `on_hello_request` (which has no `now_ms`
    /// to stamp with) until the next `stalled()` poll lazily stamps it. Cleared on `SessionUp`
    /// and on `on_disconnected`.
    awaiting: HashMap<PeerId, Option<u64>>,
    /// Peers whose most recent `on_connected` call hasn't yet been consumed by a resulting
    /// `SessionUp`. Sending an opening Hello to a peer we still believe has a session recorded
    /// this way tells `on_hello` it's allowed to accept the Hello anyway (the peer likely
    /// restarted and our own state is stale) instead of rejecting it as unexpected. One-shot:
    /// consumed the moment it's actually used to accept a Hello. Also gates whether an
    /// unsolicited HELLO_REQUEST is allowed to tear down an existing session (see
    /// `on_hello_request`) -- without a recent `on_connected`, a HELLO_REQUEST can't be trusted
    /// to mean the peer actually needs a new handshake.
    fresh: HashSet<PeerId>,
    /// The public key of the last opening Hello we accepted from each peer (higher-id role
    /// only). Lets a byte-identical retransmitted opening Hello be recognized and silently
    /// ignored instead of either erroring (log spam over a harmless network-level duplicate) or
    /// re-keying a session that's already correct.
    answered: HashMap<PeerId, [u8; 32]>,
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
            answered: HashMap::new(),
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
    ///
    /// If a handshake is already pending for `peer` (whether started by an earlier
    /// `on_connected` or by cold-answering a HELLO_REQUEST), this resends the *same* opening
    /// Hello rather than generating new key material: otherwise two outstanding attempts with
    /// different keys could never agree on one session, and a reply to the earlier attempt would
    /// be unable to complete the (now superseded) later one.
    pub fn on_connected(&mut self, peer: PeerId, now_ms: u64) -> Option<Vec<u8>> {
        self.fresh.insert(peer);
        if self.sessions.contains_key(&peer) {
            return None;
        }
        if let Some(p) = self.pending.get(&peer) {
            self.awaiting.insert(peer, Some(now_ms));
            return Some(p.hello_bytes.clone());
        }
        self.awaiting.insert(peer, Some(now_ms));
        if self.local > peer {
            return Some(vec![FRAME_HELLO_REQUEST]);
        }
        Some(self.start_handshake(peer))
    }

    pub fn on_frame(&mut self, peer: PeerId, frame: &[u8]) -> Result<FrameOutcome, ControlError> {
        match frame.first() {
            None => Err(ControlError::Empty),
            Some(&FRAME_HELLO) => self.on_hello(peer, frame),
            Some(&FRAME_HELLO_REQUEST) => {
                // A HELLO_REQUEST carries no payload: anything else riding on the tag byte is
                // malformed (or an attempt to smuggle data through it) rather than a real one.
                if frame.len() != 1 {
                    return Err(ControlError::Secure(SecureError::Malformed));
                }
                Ok(self.on_hello_request(peer))
            }
            Some(&FRAME_SEALED) => {
                let s = self.sessions.get_mut(&peer).ok_or(ControlError::NoSession)?;
                match s.open_control(frame) {
                    Ok(pt) => {
                        // An authenticated frame proves the session is genuinely current: clear
                        // any lingering `fresh` flag from an earlier (possibly redundant)
                        // `on_connected`, so a HELLO_REQUEST or opening Hello arriving long after
                        // can't exploit it to force a re-open (see `on_hello_request`/`on_hello`).
                        self.fresh.remove(&peer);
                        let msg = protocol::decode(&pt)?;
                        Ok(FrameOutcome { event: Some(ControlEvent::Message { from: peer, msg }), reply: None })
                    }
                    // A sealed frame that fails to decrypt/authenticate means our session state
                    // has desynced from the peer's in a way that isn't explained by a stray
                    // duplicate -- unlike the HELLO_REQUEST/on_disconnected paths, we don't know
                    // the connection itself is still good, so this is a harder failure: drop the
                    // session and tell the caller to disconnect the transport too.
                    Err(SecureError::Crypto) => {
                        self.drop_session(peer);
                        Ok(FrameOutcome { event: Some(ControlEvent::SessionFailed(peer)), reply: None })
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
            // We think we have a session, and our peer is asking us for a Hello. Only honor
            // that if a transport Connected was seen for this peer since the session came up
            // (`fresh`): otherwise an unsolicited single byte from anyone able to inject one
            // could tear down (and force a re-key of) an otherwise healthy, SAS-verified
            // session -- a denial-of-service/downgrade vector. Without that evidence, just
            // ignore the request.
            if !self.fresh.contains(&peer) {
                return FrameOutcome::default();
            }
            self.drop_session(peer);
            let hello = self.start_handshake(peer);
            // `on_frame` has no `now_ms` to stamp this with; `stalled()` stamps it lazily.
            self.awaiting.insert(peer, None);
            return FrameOutcome { event: Some(ControlEvent::SessionDown(peer)), reply: Some(hello) };
        }
        // Cold start: neither pending nor session -- begin a fresh handshake, likewise watched
        // lazily by the watchdog.
        let hello = self.start_handshake(peer);
        self.awaiting.insert(peer, None);
        FrameOutcome { event: None, reply: Some(hello) }
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
                // A byte-identical retransmission of the opening Hello we already answered (and
                // already have a session from) is a harmless network-level duplicate: ignore it
                // silently rather than erroring (log spam) or re-keying a session that's already
                // correct.
                if self.sessions.contains_key(&peer) && self.answered.get(&peer) == Some(&hello.public_key) {
                    return Ok(FrameOutcome::default());
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
                self.answered.insert(peer, hello.public_key);
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
        self.answered.remove(&peer);
        self.rt.write().remove(&peer);
        self.sessions.remove(&peer).is_some()
    }

    pub fn on_disconnected(&mut self, peer: PeerId) -> Option<ControlEvent> {
        self.drop_session(peer).then_some(ControlEvent::SessionDown(peer))
    }

    /// Peers whose handshake has been outstanding for at least `timeout_ms` as of `now_ms`.
    /// Each returned peer's timer is reset to `now_ms`, so a caller polling this on an interval
    /// gets a given stalled peer reported at most once per `timeout_ms` window rather than on
    /// every poll. A handshake started by `on_hello_request` (which has no `now_ms` to stamp
    /// with) is stamped lazily on its first poll here instead of being reported immediately.
    pub fn stalled(&mut self, now_ms: u64, timeout_ms: u64) -> Vec<PeerId> {
        let mut out = Vec::new();
        for (&peer, started) in self.awaiting.iter_mut() {
            match *started {
                None => *started = Some(now_ms),
                Some(t) if now_ms.saturating_sub(t) >= timeout_ms => {
                    out.push(peer);
                    *started = Some(now_ms);
                }
                Some(_) => {}
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
    fn crypto_failure_on_sealed_frame_drops_session_and_reports_failed() {
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
        // Distinct from SessionDown: the connection itself is suspect, so the caller is expected
        // to disconnect the transport too, not just notify the engine.
        assert!(matches!(outcome.event, Some(ControlEvent::SessionFailed(p)) if p == PeerId(1)));
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
        // A duplicate/spurious "connected" event on the lower side (b never told): with a
        // handshake already pending, on_connected resends the *same* Hello instead of new keys
        // (F3), so this no longer diverges from h1.
        let h2 = a.on_connected(PeerId(2), 10).unwrap();
        assert_eq!(h1, h2, "must resend the identical pending Hello, not generate new key material");
        let r1 = b.on_frame(PeerId(1), &h1).unwrap().reply.unwrap();
        // b already completed a session from h1; the byte-identical h2 is silently ignored, not
        // an UnexpectedHello (F4).
        assert!(b.on_frame(PeerId(1), &h2).unwrap().event.is_none());
        a.on_frame(PeerId(2), &r1).unwrap();
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
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
    fn hello_request_on_verified_session_without_fresh_connected_is_ignored() {
        // p1: an injected/forged HELLO_REQUEST must not be able to tear down (and force a
        // re-key of) an SAS-verified session just because it arrived -- only a genuine, locally
        // observed Connected notification (`fresh`) may authorize that.
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert!(a.has_session(PeerId(2)));
        let sas_before = a.sas(PeerId(2));
        let out = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap();
        assert!(out.event.is_none() && out.reply.is_none(), "ignored: no fresh Connected since the session came up");
        assert!(a.has_session(PeerId(2)));
        assert_eq!(a.sas(PeerId(2)), sas_before);
    }
    #[test]
    fn hello_request_on_stale_session_drops_it_and_restarts_when_fresh() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let reply = b.on_frame(PeerId(1), &ha).unwrap().reply.unwrap();
        a.on_frame(PeerId(2), &reply).unwrap();
        assert!(a.has_session(PeerId(2)));
        // A's transport tells it about a (re)connection to b: this is what authorizes accepting
        // an unsolicited HELLO_REQUEST as a legitimate request to restart the handshake (e.g.
        // b actually restarted and lost its session).
        a.on_connected(PeerId(2), 5);
        let out = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap();
        assert!(matches!(out.event, Some(ControlEvent::SessionDown(p)) if p == PeerId(2)));
        assert!(!a.has_session(PeerId(2)));
        assert!(out.reply.is_some(), "a fresh opening Hello is sent");
    }
    #[test]
    fn hello_request_with_extra_bytes_is_malformed() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        assert!(a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST, 0x00]).is_err());
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
    #[test]
    fn hello_request_cold_answer_is_lazily_watched_by_the_watchdog() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let _hello = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap().reply.unwrap();
        // The first poll just stamps the start time (no `now_ms` was available when the
        // handshake was started by on_hello_request); it must not be reported as already
        // stalled from that same poll.
        assert_eq!(a.stalled(1_000_000, 1), Vec::new(), "first poll only stamps the start time");
        assert_eq!(a.stalled(1_000_001, 1), vec![PeerId(2)], "now past the stamped start time");
    }
    #[test]
    fn on_connected_with_pending_handshake_resends_same_hello_and_refreshes_awaiting() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let h1 = a.on_connected(PeerId(2), 1_000).unwrap();
        let h2 = a.on_connected(PeerId(2), 2_000).unwrap();
        assert_eq!(h1, h2, "must resend the identical pending Hello, not generate new key material");
        // The watchdog timer is refreshed to the second call's time (2_000), not stuck at the
        // first (1_000).
        assert_eq!(a.stalled(2_999, 1_000), Vec::new(), "not yet 1s past the refreshed start time");
        assert_eq!(a.stalled(3_000, 1_000), vec![PeerId(2)], "1s past the refreshed start time");
    }

    // -- Regressions ported from the reviewer's `review_probe2`/`review_probe3` scratch modules --

    fn msg() -> ControlMessage {
        ControlMessage::Leave { room_id: RoomId(1) }
    }
    /// Brings up a session the way a real transport would when both ends observe the connect:
    /// the lower id opens with a Hello, the higher id (independently) sends a HELLO_REQUEST,
    /// and the two cross in flight -- the request arrives at the lower side before its own
    /// pending handshake's real reply does.
    fn up(a: &mut ControlChannel, b: &mut ControlChannel) {
        let ha = a.on_connected(b.local, 0).unwrap();
        let rq = b.on_connected(a.local, 0).unwrap();
        let r = b.on_frame(a.local, &ha).unwrap().reply.unwrap();
        // a gets b's request before the real reply: resends the same (still pending) hello.
        let dup = a.on_frame(b.local, &rq).unwrap().reply.unwrap();
        a.on_frame(b.local, &r).unwrap();
        // b now has a session; the duplicate opening hello (identical to the one it already
        // answered) is silently ignored rather than erroring (F4).
        assert!(b.on_frame(a.local, &dup).unwrap().event.is_none());
        assert!(a.has_session(b.local) && b.has_session(a.local));
    }
    #[test]
    fn p1_injected_hello_request_does_not_disrupt_a_verified_session() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        up(&mut a, &mut b);
        let sas = a.sas(PeerId(2));
        // Attacker injects a bare HELLO_REQUEST claiming to be from b.
        let o = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap();
        assert!(o.event.is_none() && o.reply.is_none());
        assert!(a.has_session(PeerId(2)));
        assert_eq!(a.sas(PeerId(2)), sas);
        // Normal traffic keeps flowing afterward.
        let f = b.seal(PeerId(1), &msg()).unwrap();
        assert!(matches!(a.on_frame(PeerId(2), &f).unwrap().event, Some(ControlEvent::Message { .. })));
        assert_eq!(a.stalled(10_000, 5_000), Vec::new());
        assert_eq!(b.stalled(10_000, 5_000), Vec::new());
    }
    #[test]
    fn p2_injected_hello_request_is_ignored_so_a_mitm_reply_has_nothing_to_attach_to() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        up(&mut a, &mut b);
        let sas = a.sas(PeerId(2));
        // Attacker injects a HELLO_REQUEST hoping A will answer with a fresh opening Hello it
        // can then reply to (impersonating b) and hijack. With the fix, A has nothing pending
        // and no `fresh` Connected, so the request is ignored outright -- no hello is ever
        // produced for the attacker to attach a forged reply to.
        let out = a.on_frame(PeerId(2), &[FRAME_HELLO_REQUEST]).unwrap();
        assert!(out.reply.is_none(), "the request is ignored: no fresh opening hello to attack");
        assert!(a.has_session(PeerId(2)));
        assert_eq!(a.sas(PeerId(2)), sas);
    }
    #[test]
    fn p5_lingering_fresh_flag_is_cleared_by_first_authenticated_sealed_frame() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let ha = a.on_connected(PeerId(2), 0).unwrap();
        let ob = b.on_frame(PeerId(1), &ha).unwrap(); // hello processed before b's own Connected
        a.on_frame(PeerId(2), &ob.reply.unwrap()).unwrap();
        // A late/redundant Connected on b's side sets `fresh` even though a session already
        // exists (self-healing: see probe_c above).
        b.on_connected(PeerId(1), 0);
        assert!(b.has_session(PeerId(1)));
        // An authenticated sealed frame proves the session is current: it must clear the
        // lingering fresh flag left over from the redundant on_connected above.
        let f = a.seal(PeerId(2), &msg()).unwrap();
        assert!(b.on_frame(PeerId(1), &f).is_ok());
        // Long after, an attacker injects a forged opening Hello: it must now be rejected, not
        // silently accepted via the lingering fresh flag.
        let mallory = Handshake::new(PeerId(1), "A".into());
        let err = b.on_frame(PeerId(1), &mallory.hello()).unwrap_err();
        assert!(matches!(err, ControlError::UnexpectedHello));
        assert!(b.has_session(PeerId(1)));
    }
    #[test]
    fn p6_two_opening_hellos_from_duplicate_on_connected_still_converge() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let h1 = a.on_connected(PeerId(2), 0).unwrap();
        let h2 = a.on_connected(PeerId(2), 10).unwrap();
        assert_eq!(h1, h2, "on_connected with a pending handshake must resend the same hello");
        let r1 = b.on_frame(PeerId(1), &h1).unwrap().reply.unwrap();
        assert!(b.on_frame(PeerId(1), &h2).unwrap().event.is_none(), "identical duplicate is ignored");
        a.on_frame(PeerId(2), &r1).unwrap();
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
        assert_eq!(a.stalled(5_010, 5_000), Vec::new(), "no stale watchdog entry lingers after SessionUp");
    }
    #[test]
    fn p9_hello_request_before_own_connected_then_on_connected_is_idempotent() {
        let mut a = ControlChannel::new(PeerId(1), "A".into());
        let mut b = ControlChannel::new(PeerId(2), "B".into());
        let rq = b.on_connected(PeerId(1), 0).unwrap();
        // a answers the cold request before its own Connected notification ever arrives.
        let h1 = a.on_frame(PeerId(2), &rq).unwrap().reply.unwrap();
        // a's own Connected notification arrives afterward: must reuse the same pending
        // handshake started above rather than generating new keys (F3).
        let h2 = a.on_connected(PeerId(2), 0).unwrap();
        assert_eq!(h1, h2, "must reuse the pending handshake started by the cold HELLO_REQUEST answer");
        let r1 = b.on_frame(PeerId(1), &h1).unwrap().reply.unwrap();
        assert!(b.on_frame(PeerId(1), &h2).unwrap().event.is_none(), "identical duplicate is ignored");
        a.on_frame(PeerId(2), &r1).unwrap();
        assert!(a.has_session(PeerId(2)) && b.has_session(PeerId(1)));
    }
}
