//! Room state machine. Pure and deterministic: callers feed inputs stamped with the current time
//! (ms) and drain `Output`s (messages, connection requests, UI events, role changes).
//!
//! Coordinator liveness is judged through the whole room, not just the local link: every
//! heartbeat carries `sees_coordinator` (the sender's own view), and a peer only declares the
//! coordinator lost when it can't reach it itself *and* no other live member currently reports
//! reaching it. Trade-off: with one broken link (A↔C cut, B sees both) C stays in A's room with a
//! single coordinator instead of splitting the room — but C's mic can't reach the coordinator
//! (and C sees it as offline) until the link heals. Control requests from C are relayed through
//! a member that can see the coordinator.
//!
//! Automatic election also needs a quorum: a peer only elects while it sees at least half the
//! room alive, so a peer that was offline or asleep doesn't come back as a phantom coordinator.
//! Trade-off: when a room splits into a minority and a majority, the minority waits for a
//! manual pick.
use crate::ids::{PeerId, RoomId};
use crate::room::election::{elect_coordinator, speaker_candidates};
use crate::room::events::*;
use crate::room::protocol::{ChangeRequest, ControlMessage};
use crate::room::state::{
    evaluate_manifest, is_valid_coordinator_command, Acceptance, MemberInfo, RoomManifest,
};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct RoomConfig {
    pub local: MemberInfo,
    pub heartbeat_ms: u64,
    pub peer_timeout_ms: u64,
    pub election_grace_ms: u64,
    pub join_timeout_ms: u64,
    pub auto_elect: bool,
    pub fallback_speaker_to_coordinator: bool,
}
impl RoomConfig {
    pub fn new(local: MemberInfo) -> Self {
        Self {
            local,
            heartbeat_ms: 1_000,
            peer_timeout_ms: 4_000,
            election_grace_ms: 1_500,
            join_timeout_ms: 10_000,
            auto_elect: true,
            fallback_speaker_to_coordinator: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    CreateRoom { name: String },
    Invite(PeerId),
    RespondToInvite { room_id: RoomId, accept: bool },
    Leave,
    SetCoordinator(PeerId),
    SetSpeaker(Option<PeerId>),
    SetMicEnabled { peer: PeerId, enabled: bool },
    RemoveMember(PeerId),
    Rename(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoomError {
    #[error("already in a room")]
    AlreadyInRoom,
    #[error("not in a room")]
    NotInRoom,
    #[error("no such invite")]
    NoSuchInvite,
    #[error("peer is not a room member")]
    NotMember,
    /// The core's control thread did not take the command in time (busy, or called
    /// re-entrantly from an event callback). The command was not applied.
    #[error("timed out")]
    Timeout,
    /// The core's control thread has stopped (it panicked); nothing can be applied any more.
    #[error("internal error: room control stopped")]
    Internal,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Send { to: PeerId, msg: ControlMessage },
    Connect(PeerId),
    Event(RoomEvent),
    Roles(LocalRoles),
}

struct Nearby {
    name: String,
    connected: bool,
    sas: Option<String>,
}
struct PendingInvite {
    manifest: RoomManifest,
    from: PeerId,
}

/// The room a control message belongs to (None for room-independent messages).
fn message_room(msg: &ControlMessage) -> Option<RoomId> {
    match msg {
        ControlMessage::Invite { manifest, .. } => Some(manifest.room_id),
        ControlMessage::Manifest(m) => Some(m.room_id),
        ControlMessage::InviteResponse { room_id, .. }
        | ControlMessage::JoinRequest { room_id, .. }
        | ControlMessage::Request { room_id, .. }
        | ControlMessage::Leave { room_id }
        | ControlMessage::Heartbeat { room_id, .. }
        | ControlMessage::ActiveMic { room_id, .. } => Some(*room_id),
        ControlMessage::PeerReport(_) => None,
    }
}

pub struct RoomEngine {
    cfg: RoomConfig,
    now: u64,
    manifest: Option<RoomManifest>,
    nearby: BTreeMap<PeerId, Nearby>,
    sessions: HashSet<PeerId>,
    last_seen: HashMap<PeerId, u64>,
    invites_in: HashMap<RoomId, PendingInvite>,
    joining: Option<(RoomId, u64)>,
    queued: HashMap<PeerId, Vec<ControlMessage>>,
    /// Latest `sees_coordinator` report per member for the current epoch: (received at, value).
    coord_reports: HashMap<PeerId, (u64, bool)>,
    coordinator_lost_since: Option<u64>,
    coordinator_lost_prompted: bool,
    speaker_lost_prompted: bool,
    last_heartbeat: u64,
    active: (Option<PeerId>, Option<PeerId>),
    last_roles: Option<LocalRoles>,
    /// Members reported online by the last `RoomChanged`.
    last_online: Vec<PeerId>,
    /// Peers a `Connect` was emitted for since the last `take_outputs` (dedupe).
    connects: HashSet<PeerId>,
    out: Vec<Output>,
}

impl RoomEngine {
    pub fn new(cfg: RoomConfig) -> Self {
        Self {
            cfg,
            now: 0,
            manifest: None,
            nearby: BTreeMap::new(),
            sessions: HashSet::new(),
            last_seen: HashMap::new(),
            invites_in: HashMap::new(),
            joining: None,
            queued: HashMap::new(),
            coord_reports: HashMap::new(),
            coordinator_lost_since: None,
            coordinator_lost_prompted: false,
            speaker_lost_prompted: false,
            last_heartbeat: 0,
            active: (None, None),
            last_roles: None,
            last_online: vec![],
            connects: HashSet::new(),
            out: vec![],
        }
    }

    // ---------- accessors ----------
    pub fn local(&self) -> PeerId {
        self.cfg.local.id
    }
    pub fn manifest(&self) -> Option<&RoomManifest> {
        self.manifest.as_ref()
    }
    pub fn take_outputs(&mut self) -> Vec<Output> {
        self.connects.clear();
        std::mem::take(&mut self.out)
    }
    pub fn set_auto_elect(&mut self, v: bool) {
        self.cfg.auto_elect = v;
    }
    pub fn set_fallback_speaker(&mut self, v: bool) {
        self.cfg.fallback_speaker_to_coordinator = v;
    }
    /// Updates the local member info; when in a room and the name or capabilities differ from
    /// the manifest, the change is propagated through the coordinator (`mic_enabled` stays as
    /// the room has it).
    pub fn set_local_info(&mut self, info: MemberInfo) {
        self.cfg.local = info;
        let now = self.now;
        self.sync_local_info(now, true);
    }
    pub fn is_coordinator(&self) -> bool {
        self.manifest
            .as_ref()
            .is_some_and(|m| m.coordinator == self.local())
    }

    pub fn is_alive(&self, now: u64, p: PeerId) -> bool {
        p == self.local()
            || self
                .last_seen
                .get(&p)
                .is_some_and(|t| now.saturating_sub(*t) <= self.cfg.peer_timeout_ms)
    }

    /// Online as shown to the user: a live session and recent traffic (local always online).
    fn is_online(&self, now: u64, p: PeerId) -> bool {
        p == self.local() || (self.sessions.contains(&p) && self.is_alive(now, p))
    }

    /// Member `p`'s latest `sees_coordinator` report, if it's recent enough to go by.
    fn coordinator_report(&self, now: u64, p: PeerId) -> Option<bool> {
        self.coord_reports
            .get(&p)
            .filter(|(t, _)| now.saturating_sub(*t) <= self.cfg.peer_timeout_ms)
            .map(|(_, sees)| *sees)
    }

    /// Another live member recently reported that it can reach the current coordinator.
    fn coordinator_seen_by_others(&self, now: u64, m: &RoomManifest) -> bool {
        let local = self.local();
        m.members.iter().any(|x| {
            x.id != local
                && x.id != m.coordinator
                && self.coordinator_report(now, x.id) == Some(true)
        })
    }

    /// The coordinator is gone from the whole room's point of view (as far as we can tell).
    fn coordinator_lost(&self, now: u64, m: &RoomManifest) -> bool {
        m.coordinator != self.local()
            && !self.is_alive(now, m.coordinator)
            && !self.coordinator_seen_by_others(now, m)
    }

    pub fn roles(&self) -> LocalRoles {
        let Some(m) = &self.manifest else {
            return LocalRoles::none();
        };
        let local = self.local();
        LocalRoles {
            room_id: Some(m.room_id),
            epoch: m.epoch,
            coordinator: Some(m.coordinator),
            speaker: m.speaker,
            is_coordinator: m.coordinator == local,
            is_speaker: m.speaker == Some(local),
            mic_enabled: m.member(local).is_some_and(|x| x.mic_enabled),
            enabled_mics: m.enabled_mics(),
            members: m.member_ids(),
        }
    }

    pub fn snapshot(&self) -> Option<RoomSnapshot> {
        let m = self.manifest.as_ref()?;
        let local = self.local();
        Some(RoomSnapshot {
            room_id: m.room_id,
            name: m.name.clone(),
            epoch: m.epoch,
            revision: m.revision,
            coordinator: m.coordinator,
            speaker: m.speaker,
            active_primary: self.active.0,
            active_secondary: self.active.1,
            members: m
                .members
                .iter()
                .map(|x| MemberSnapshot {
                    id: x.id,
                    name: x.name.clone(),
                    is_local: x.id == local,
                    online: self.is_online(self.now, x.id),
                    mic_enabled: x.mic_enabled,
                    is_coordinator: x.id == m.coordinator,
                    is_speaker: m.speaker == Some(x.id),
                    is_active_mic: self.active.0 == Some(x.id) || self.active.1 == Some(x.id),
                })
                .collect(),
        })
    }

    fn online_members(&self, now: u64) -> Vec<PeerId> {
        self.manifest.as_ref().map_or(vec![], |m| {
            m.members
                .iter()
                .map(|x| x.id)
                .filter(|&id| self.is_online(now, id))
                .collect()
        })
    }

    pub fn nearby_peers(&self) -> Vec<NearbyPeer> {
        self.nearby
            .iter()
            .map(|(id, n)| NearbyPeer {
                id: *id,
                name: n.name.clone(),
                connected: n.connected,
                in_my_room: self.manifest.as_ref().is_some_and(|m| m.is_member(*id)),
                sas: n.sas.clone(),
            })
            .collect()
    }

    pub fn sas(&self, peer: PeerId) -> Option<String> {
        self.nearby.get(&peer).and_then(|n| n.sas.clone())
    }

    // ---------- output helpers ----------
    fn event(&mut self, e: RoomEvent) {
        self.out.push(Output::Event(e));
    }
    fn emit_room(&mut self) {
        let s = self.snapshot();
        self.last_online = self.online_members(self.now);
        self.event(RoomEvent::RoomChanged(s));
    }
    fn emit_nearby(&mut self) {
        let n = self.nearby_peers();
        self.event(RoomEvent::NearbyChanged(n));
    }
    fn emit_roles(&mut self) {
        let r = self.roles();
        if self.last_roles.as_ref() != Some(&r) {
            self.last_roles = Some(r.clone());
            self.out.push(Output::Roles(r));
        }
    }
    /// At most one `Connect` per peer per drain of the outputs.
    fn connect(&mut self, p: PeerId) {
        if p != self.local() && self.connects.insert(p) {
            self.out.push(Output::Connect(p));
        }
    }
    /// Sends now, or queues until the session comes up (and asks for a connection).
    fn send(&mut self, to: PeerId, msg: ControlMessage) {
        if to == self.local() {
            return;
        }
        if self.sessions.contains(&to) {
            self.out.push(Output::Send { to, msg });
        } else {
            let q = self.queued.entry(to).or_default();
            if q.len() >= 32 {
                q.remove(0);
            } // unreachable peers must not grow memory
            q.push(msg);
            self.connect(to);
        }
    }
    /// Sends only over an established session; for realtime-ish messages that are useless late.
    fn send_if_connected(&mut self, to: PeerId, msg: ControlMessage) {
        if to != self.local() && self.sessions.contains(&to) {
            self.out.push(Output::Send { to, msg });
        }
    }
    fn broadcast_to(&mut self, members: &[PeerId], msg: ControlMessage) {
        for &id in members {
            if id != self.local() {
                self.send(id, msg.clone());
            }
        }
    }
    /// Where to send something meant for the coordinator over a live session: the coordinator
    /// itself, else a connected member that reports seeing it (it relays).
    fn coordinator_route(&self, now: u64, m: &RoomManifest) -> Option<PeerId> {
        if self.sessions.contains(&m.coordinator) {
            return Some(m.coordinator);
        }
        let local = self.local();
        m.members.iter().map(|x| x.id).find(|&id| {
            id != local
                && id != m.coordinator
                && self.sessions.contains(&id)
                && self.coordinator_report(now, id) == Some(true)
        })
    }
    /// Sends to the coordinator (directly or relayed), queueing for it when neither is possible.
    fn send_to_coordinator(&mut self, now: u64, m: &RoomManifest, msg: ControlMessage) {
        let to = self.coordinator_route(now, m).unwrap_or(m.coordinator);
        self.send(to, msg);
    }
    fn drop_queued_for_room(&mut self, room_id: RoomId) {
        for q in self.queued.values_mut() {
            q.retain(|msg| message_room(msg) != Some(room_id));
        }
        self.queued.retain(|_, q| !q.is_empty());
    }
    /// A peer that thinks we're in `room_id` (we're not, and aren't joining it) is told we left.
    fn reply_not_in_room(&mut self, from: PeerId, room_id: RoomId) {
        if self.joining.map(|(r, _)| r) != Some(room_id) {
            self.send(from, ControlMessage::Leave { room_id });
        }
    }

    // ---------- transport inputs ----------
    pub fn on_discovered(&mut self, peer: PeerId, name: String) {
        let e = self.nearby.entry(peer).or_insert(Nearby {
            name: name.clone(),
            connected: false,
            sas: None,
        });
        e.name = name;
        self.emit_nearby();
    }
    pub fn on_lost(&mut self, peer: PeerId) {
        if self.nearby.get(&peer).is_some_and(|n| !n.connected) {
            self.nearby.remove(&peer);
        }
        self.emit_nearby();
    }
    pub fn on_session_up(&mut self, now: u64, peer: PeerId, name: String, sas: String) {
        self.now = now;
        self.sessions.insert(peer);
        self.last_seen.insert(peer, now);
        let e = self.nearby.entry(peer).or_insert(Nearby {
            name: name.clone(),
            connected: false,
            sas: None,
        });
        e.connected = true;
        e.sas = Some(sas);
        if !name.is_empty() {
            e.name = name;
        }
        if let Some(q) = self.queued.remove(&peer) {
            for msg in q {
                self.out.push(Output::Send { to: peer, msg });
            }
        }
        if let Some(m) = self.manifest.clone() {
            if m.coordinator == self.local() && m.is_member(peer) {
                self.out.push(Output::Send {
                    to: peer,
                    msg: ControlMessage::Manifest(m),
                });
            }
        }
        self.emit_nearby();
        self.emit_room();
    }
    pub fn on_session_down(&mut self, now: u64, peer: PeerId) {
        self.now = now;
        self.sessions.remove(&peer);
        if let Some(n) = self.nearby.get_mut(&peer) {
            n.connected = false;
            n.sas = None;
        }
        self.emit_nearby();
        self.emit_room();
    }

    // ---------- user commands ----------
    pub fn command(&mut self, now: u64, cmd: Command) -> Result<(), RoomError> {
        self.now = now;
        match cmd {
            Command::CreateRoom { name } => {
                if self.manifest.is_some() {
                    return Err(RoomError::AlreadyInRoom);
                }
                let m = RoomManifest::new(RoomId::random(), name, self.cfg.local.clone());
                self.install(now, m);
                Ok(())
            }
            Command::Invite(peer) => {
                let m = self.manifest.clone().ok_or(RoomError::NotInRoom)?;
                if !m.is_member(peer) {
                    self.send(
                        peer,
                        ControlMessage::Invite {
                            manifest: m,
                            from_name: self.cfg.local.name.clone(),
                        },
                    );
                }
                Ok(())
            }
            Command::RespondToInvite { room_id, accept } => {
                let inv = self
                    .invites_in
                    .remove(&room_id)
                    .ok_or(RoomError::NoSuchInvite)?;
                self.send(
                    inv.from,
                    ControlMessage::InviteResponse {
                        room_id,
                        accepted: accept,
                    },
                );
                if accept {
                    if self.manifest.is_some() {
                        self.leave(now);
                    }
                    self.joining = Some((room_id, now));
                    self.send(
                        inv.manifest.coordinator,
                        ControlMessage::JoinRequest {
                            room_id,
                            member: self.cfg.local.clone(),
                        },
                    );
                }
                Ok(())
            }
            Command::Leave => {
                if self.manifest.is_none() {
                    return Err(RoomError::NotInRoom);
                }
                self.leave(now);
                Ok(())
            }
            Command::SetCoordinator(p) => self.change(now, ChangeRequest::SetCoordinator(p)),
            Command::SetSpeaker(p) => self.change(now, ChangeRequest::SetSpeaker(p)),
            Command::SetMicEnabled { peer, enabled } => {
                self.change(now, ChangeRequest::SetMicEnabled { peer, enabled })
            }
            Command::RemoveMember(p) => self.change(now, ChangeRequest::RemoveMember(p)),
            Command::Rename(n) => self.change(now, ChangeRequest::Rename(n)),
        }
    }

    fn change(&mut self, now: u64, change: ChangeRequest) -> Result<(), RoomError> {
        let m = self.manifest.clone().ok_or(RoomError::NotInRoom)?;
        let target = match &change {
            ChangeRequest::SetCoordinator(p) | ChangeRequest::RemoveMember(p) => Some(*p),
            ChangeRequest::SetSpeaker(p) => *p,
            ChangeRequest::SetMicEnabled { peer, .. } => Some(*peer),
            ChangeRequest::UpdateMember(info) => Some(info.id),
            ChangeRequest::Rename(_) => None,
        };
        if target.is_some_and(|p| !m.is_member(p)) {
            return Err(RoomError::NotMember);
        }
        if m.coordinator == self.local() {
            self.apply_change(now, change);
        } else {
            match change {
                ChangeRequest::SetCoordinator(p) if self.coordinator_lost(now, &m) => {
                    self.propose_coordinator(now, p)
                }
                other => {
                    let msg = ControlMessage::Request {
                        room_id: m.room_id,
                        epoch: m.epoch,
                        change: other,
                    };
                    self.send_to_coordinator(now, &m, msg)
                }
            }
        }
        Ok(())
    }

    /// Pushes our name/capabilities into the manifest if they differ from it. `queue` = may
    /// queue the request when no session is up (periodic retries only send over live sessions).
    fn sync_local_info(&mut self, now: u64, queue: bool) {
        let Some(m) = self.manifest.clone() else {
            return;
        };
        let info = self.cfg.local.clone();
        let Some(me) = m.member(info.id) else {
            return;
        };
        if me.name == info.name && me.capabilities == info.capabilities {
            return;
        }
        if m.coordinator == info.id {
            self.apply_change(now, ChangeRequest::UpdateMember(info));
            return;
        }
        let msg = ControlMessage::Request {
            room_id: m.room_id,
            epoch: m.epoch,
            change: ChangeRequest::UpdateMember(info),
        };
        if queue {
            self.send_to_coordinator(now, &m, msg);
        } else if let Some(to) = self.coordinator_route(now, &m) {
            self.send_if_connected(to, msg);
        }
    }

    /// Coordinator only: removes a member; a removed speaker falls back to the coordinator
    /// when that setting is on.
    fn drop_member(&self, m: &mut RoomManifest, p: PeerId) {
        let was_speaker = m.speaker == Some(p);
        m.remove_member(p);
        if was_speaker && self.cfg.fallback_speaker_to_coordinator {
            m.speaker = Some(m.coordinator);
        }
    }

    /// Coordinator only.
    fn apply_change(&mut self, now: u64, change: ChangeRequest) {
        let Some(mut m) = self.manifest.clone() else {
            return;
        };
        let mut removed = None;
        match change {
            ChangeRequest::SetCoordinator(p) => {
                if !m.is_member(p) || p == m.coordinator {
                    return;
                }
                m.epoch = m.epoch.next();
                m.revision = 0;
                m.coordinator = p;
            }
            ChangeRequest::SetSpeaker(p) => {
                if p.is_some_and(|p| !m.is_member(p)) || m.speaker == p {
                    return;
                }
                m.speaker = p;
                m.revision += 1;
            }
            ChangeRequest::SetMicEnabled { peer, enabled } => {
                match m.member_mut(peer) {
                    Some(x) if x.mic_enabled != enabled => x.mic_enabled = enabled,
                    _ => return,
                }
                m.revision += 1;
            }
            ChangeRequest::RemoveMember(p) => {
                if !m.is_member(p) || p == m.coordinator {
                    return;
                }
                self.drop_member(&mut m, p);
                m.revision += 1;
                removed = Some(p);
            }
            ChangeRequest::Rename(n) => {
                m.name = n;
                m.revision += 1;
            }
            ChangeRequest::UpdateMember(info) => {
                match m.member_mut(info.id) {
                    Some(x) if x.name != info.name || x.capabilities != info.capabilities => {
                        x.name = info.name;
                        x.capabilities = info.capabilities;
                    }
                    _ => return,
                }
                m.revision += 1;
            }
        }
        self.publish(now, m.clone());
        if let Some(p) = removed {
            self.send(p, ControlMessage::Manifest(m));
        }
    }

    fn publish(&mut self, now: u64, m: RoomManifest) {
        let members = m.member_ids();
        self.install(now, m.clone());
        self.broadcast_to(&members, ControlMessage::Manifest(m));
    }

    fn propose_coordinator(&mut self, now: u64, p: PeerId) {
        let Some(mut m) = self.manifest.clone() else {
            return;
        };
        let old = m.coordinator;
        m.epoch = m.epoch.next();
        m.revision = 0;
        m.coordinator = p;
        if m.speaker == Some(old) && self.cfg.fallback_speaker_to_coordinator {
            m.speaker = Some(p);
        }
        self.publish(now, m);
    }

    fn leave(&mut self, now: u64) {
        let Some(m) = self.manifest.clone() else {
            return;
        };
        self.drop_queued_for_room(m.room_id);
        let local = self.local();
        if m.coordinator == local {
            let mut next = m.clone();
            next.remove_member(local);
            if let Some(succ) = elect_coordinator(&next, |p| self.is_alive(now, p)) {
                next.epoch = next.epoch.next();
                next.revision = 0;
                next.coordinator = succ;
                if next.speaker.is_none() && self.cfg.fallback_speaker_to_coordinator {
                    next.speaker = Some(succ);
                }
                let ids = next.member_ids();
                self.broadcast_to(&ids, ControlMessage::Manifest(next));
            }
        } else {
            let ids = m.member_ids();
            self.broadcast_to(&ids, ControlMessage::Leave { room_id: m.room_id });
        }
        self.manifest = None;
        self.reset_room_flags();
        self.event(RoomEvent::LeftRoom);
        self.emit_room();
        self.emit_roles();
    }

    fn reset_room_flags(&mut self) {
        self.active = (None, None);
        self.coord_reports.clear();
        self.coordinator_lost_since = None;
        self.coordinator_lost_prompted = false;
        self.speaker_lost_prompted = false;
    }

    /// Replace the manifest, emitting diff events and role changes.
    fn install(&mut self, now: u64, new: RoomManifest) {
        let local = self.local();
        let old = self.manifest.take();
        if let Some(o) = old.as_ref().filter(|o| o.room_id != new.room_id) {
            let rid = o.room_id;
            self.drop_queued_for_room(rid);
        }
        let old = old.filter(|o| o.room_id == new.room_id);
        if !new.is_member(local) {
            self.drop_queued_for_room(new.room_id);
            self.reset_room_flags();
            self.event(RoomEvent::LeftRoom);
            self.emit_room();
            self.emit_roles();
            return;
        }
        match &old {
            None => {
                self.event(RoomEvent::CoordinatorChanged {
                    peer: new.coordinator,
                    epoch: new.epoch,
                });
                self.event(RoomEvent::SpeakerChanged { peer: new.speaker });
                for x in &new.members {
                    if x.id != local {
                        self.event(RoomEvent::PeerJoined {
                            peer: x.id,
                            name: x.name.clone(),
                        });
                    }
                }
            }
            Some(o) => {
                for x in &new.members {
                    if !o.is_member(x.id) && x.id != local {
                        self.event(RoomEvent::PeerJoined {
                            peer: x.id,
                            name: x.name.clone(),
                        });
                    }
                }
                for x in &o.members {
                    if !new.is_member(x.id) {
                        self.event(RoomEvent::PeerLeft { peer: x.id });
                    }
                }
                if o.coordinator != new.coordinator {
                    self.event(RoomEvent::CoordinatorChanged {
                        peer: new.coordinator,
                        epoch: new.epoch,
                    });
                }
                if o.speaker != new.speaker {
                    self.event(RoomEvent::SpeakerChanged { peer: new.speaker });
                    // The speaker left or was removed (not an explicit "no speaker" choice).
                    if new.speaker.is_none() && o.speaker.is_some_and(|s| !new.is_member(s)) {
                        let candidates = speaker_candidates(&new, |p| self.is_alive(now, p));
                        self.event(RoomEvent::SpeakerLost { candidates });
                    }
                }
            }
        }
        let coord_changed =
            old.as_ref().map(|o| (o.coordinator, o.epoch)) != Some((new.coordinator, new.epoch));
        if coord_changed {
            self.coord_reports.clear();
            self.coordinator_lost_since = None;
            self.coordinator_lost_prompted = false;
            self.active = (None, None);
        }
        if old.as_ref().map(|o| o.speaker) != Some(new.speaker) {
            self.speaker_lost_prompted = false;
        }
        for id in new.member_ids() {
            if id == local {
                continue;
            }
            // Newly (re)added members get a fresh liveness grace period.
            if !old.as_ref().is_some_and(|o| o.is_member(id)) {
                self.last_seen.insert(id, now);
            }
            if !self.sessions.contains(&id) {
                self.connect(id);
            }
        }
        self.manifest = Some(new);
        self.joining = None;
        self.emit_room();
        self.emit_roles();
    }

    // ---------- network messages ----------
    pub fn on_message(&mut self, now: u64, from: PeerId, msg: ControlMessage) {
        self.now = now;
        self.last_seen.insert(from, now);
        let local = self.local();
        match msg {
            ControlMessage::Invite {
                manifest,
                from_name,
            } => {
                if self
                    .manifest
                    .as_ref()
                    .is_some_and(|m| m.room_id == manifest.room_id)
                {
                    return;
                }
                let sas = self.sas(from).unwrap_or_default();
                let (room_id, room_name) = (manifest.room_id, manifest.name.clone());
                self.invites_in
                    .insert(room_id, PendingInvite { manifest, from });
                self.event(RoomEvent::InviteReceived {
                    room_id,
                    room_name,
                    from,
                    from_name,
                    sas,
                });
            }
            ControlMessage::InviteResponse { accepted, .. } => {
                if !accepted {
                    self.event(RoomEvent::InviteDeclined { peer: from });
                }
            }
            ControlMessage::JoinRequest { room_id, member } => {
                let Some(m) = self.manifest.clone() else {
                    return;
                };
                // Direct from the joiner, or relayed by a member for a joiner that isn't one yet
                // (e.g. it was invited by a peer that has since stopped being the coordinator).
                let relayed = member.id != from;
                if m.room_id != room_id
                    || (relayed && (!m.is_member(from) || m.is_member(member.id)))
                {
                    return;
                }
                if m.coordinator == local {
                    let mut next = m;
                    let mut member = member;
                    // A member re-joining keeps the room's mic setting for it.
                    if let Some(existing) = next.member(member.id) {
                        member.mic_enabled = existing.mic_enabled;
                    }
                    next.upsert_member(member);
                    next.revision += 1;
                    self.publish(now, next);
                } else if m.coordinator != from {
                    // One relay hop at most: the originator may pick a relayer, but a relayer
                    // sends straight to the coordinator (queued if there's no session).
                    self.send(
                        m.coordinator,
                        ControlMessage::JoinRequest { room_id, member },
                    );
                }
            }
            ControlMessage::Manifest(m) => self.receive_manifest(now, from, m),
            ControlMessage::Request {
                room_id,
                epoch,
                change,
            } => {
                let Some(cur) = self.manifest.clone() else {
                    return;
                };
                if cur.room_id != room_id || !cur.is_member(from) {
                    return;
                }
                if cur.coordinator == local {
                    // A request made under an older coordinator term is stale.
                    if epoch >= cur.epoch {
                        self.apply_change(now, change);
                    }
                } else if cur.coordinator != from {
                    // Relay unchanged (keeping the requester's epoch), never back to its sender,
                    // and straight to the coordinator: one relay hop at most, so stale
                    // `sees_coordinator` reports can't bounce it between members.
                    let msg = ControlMessage::Request {
                        room_id,
                        epoch,
                        change,
                    };
                    self.send(cur.coordinator, msg);
                }
            }
            ControlMessage::Leave { room_id } => {
                let Some(cur) = self.manifest.clone() else {
                    return;
                };
                if cur.room_id != room_id {
                    return;
                }
                self.last_seen.remove(&from);
                self.coord_reports.remove(&from);
                if cur.coordinator == local && cur.is_member(from) {
                    let mut next = cur;
                    self.drop_member(&mut next, from);
                    next.revision += 1;
                    self.publish(now, next);
                }
            }
            ControlMessage::Heartbeat {
                room_id,
                epoch,
                revision,
                manifest,
                sees_coordinator,
            } => {
                let Some(cur) = self.manifest.clone().filter(|m| m.room_id == room_id) else {
                    return self.reply_not_in_room(from, room_id);
                };
                if !cur.is_member(from) {
                    // e.g. a removed peer that missed its removal: show it the manifest.
                    if cur.coordinator == local {
                        self.send(from, ControlMessage::Manifest(cur));
                    }
                    return;
                }
                if let Some(m) = manifest {
                    self.receive_manifest(now, from, m);
                } else if cur.coordinator == local && (epoch, revision) < cur.version() {
                    self.send(from, ControlMessage::Manifest(cur));
                }
                if self.manifest.as_ref().is_some_and(|m| m.epoch == epoch) {
                    self.coord_reports.insert(from, (now, sees_coordinator));
                }
            }
            ControlMessage::ActiveMic {
                room_id,
                epoch,
                primary,
                secondary,
            } => {
                let Some(cur) = self.manifest.as_ref() else {
                    return;
                };
                if cur.room_id == room_id
                    && is_valid_coordinator_command(cur, from, epoch)
                    && self.active != (primary, secondary)
                {
                    self.active = (primary, secondary);
                    self.event(RoomEvent::ActiveMicChanged { primary, secondary });
                    self.emit_room();
                }
            }
            ControlMessage::PeerReport(_) => {} // handled by the metrics layer (engine::facade)
        }
    }

    fn receive_manifest(&mut self, now: u64, from: PeerId, m: RoomManifest) {
        let local = self.local();
        if let Some((rid, _)) = self.joining {
            if m.room_id == rid && m.is_member(local) && from == m.coordinator {
                self.install(now, m);
                return;
            }
        }
        let Some(cur) = self.manifest.clone().filter(|c| c.room_id == m.room_id) else {
            return self.reply_not_in_room(from, m.room_id);
        };
        match evaluate_manifest(Some(&cur), &m, from) {
            Acceptance::Accept => {
                // A proposal naming us, relayed by someone else (manual pick, leave hand-off,
                // election by a peer): republish it as its coordinator so racing proposals for
                // the same epoch converge on our version.
                let relayed = m.coordinator == local && from != local;
                self.install(now, m);
                if relayed {
                    if let Some(mut adopted) = self.manifest.clone() {
                        adopted.revision += 1;
                        self.publish(now, adopted);
                    }
                }
            }
            Acceptance::Reject(_) if !cur.is_member(from) => {}
            Acceptance::Reject(_)
                if m.epoch == cur.epoch
                    && m.coordinator == cur.coordinator
                    && m.revision > cur.revision
                    && !self.is_alive(now, cur.coordinator) =>
            {
                // Our coordinator's newer manifest, relayed by a member while our own link to
                // the coordinator is down.
                self.install(now, m);
            }
            Acceptance::Reject(_)
                if m.epoch == cur.epoch
                    && m.coordinator == local
                    && m.coordinator < cur.coordinator
                    && m.is_member(local) =>
            {
                // A same-epoch proposal naming us beats the one we hold; as the coordinator it
                // names, we vouch for it.
                let mut adopted = m;
                adopted.revision += 1;
                self.publish(now, adopted);
            }
            Acceptance::Reject(_) => {
                // Bring the sender up to date: we're its coordinator, or it's behind by an epoch.
                if cur.coordinator == local || m.epoch < cur.epoch {
                    self.send(from, ControlMessage::Manifest(cur));
                }
            }
        }
    }

    /// Called by the audio runtime when the local coordinator's arbitration result changes.
    pub fn on_local_selection(
        &mut self,
        now: u64,
        primary: Option<PeerId>,
        secondary: Option<PeerId>,
    ) {
        self.now = now;
        let Some(m) = self.manifest.clone() else {
            return;
        };
        if m.coordinator != self.local() || self.active == (primary, secondary) {
            return;
        }
        self.active = (primary, secondary);
        self.event(RoomEvent::ActiveMicChanged { primary, secondary });
        self.emit_room();
        let msg = ControlMessage::ActiveMic {
            room_id: m.room_id,
            epoch: m.epoch,
            primary,
            secondary,
        };
        for id in m.member_ids() {
            self.send_if_connected(id, msg.clone()); // stale by the time a session comes up
        }
    }

    // ---------- timers ----------
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if let Some((rid, since)) = self.joining {
            if now.saturating_sub(since) > self.cfg.join_timeout_ms {
                self.joining = None;
                self.drop_queued_for_room(rid);
                self.event(RoomEvent::Error {
                    message: "Could not join the room: the coordinator did not answer.".into(),
                });
            }
        }
        let Some(m) = self.manifest.clone() else {
            return;
        };
        let local = self.local();
        let grace = self.cfg.election_grace_ms;

        if now.saturating_sub(self.last_heartbeat) >= self.cfg.heartbeat_ms {
            self.last_heartbeat = now;
            let hb = ControlMessage::Heartbeat {
                room_id: m.room_id,
                epoch: m.epoch,
                revision: m.revision,
                manifest: (m.coordinator == local).then(|| m.clone()),
                sees_coordinator: self.is_alive(now, m.coordinator),
            };
            // A member that can't reach the coordinator gets its manifest relayed by us.
            let can_relay = m.coordinator != local && self.is_alive(now, m.coordinator);
            for id in m.member_ids() {
                if id == local {
                    continue;
                }
                if self.sessions.contains(&id) {
                    let mut msg = hb.clone();
                    if can_relay && self.coordinator_report(now, id) == Some(false) {
                        if let ControlMessage::Heartbeat { manifest, .. } = &mut msg {
                            *manifest = Some(m.clone());
                        }
                    }
                    self.out.push(Output::Send { to: id, msg });
                } else {
                    self.connect(id);
                }
            }
            self.sync_local_info(now, false);
        }

        if self.online_members(now) != self.last_online {
            self.emit_room();
        }

        if self.coordinator_lost(now, &m) {
            let since = *self.coordinator_lost_since.get_or_insert(now);
            let waited = now.saturating_sub(since);
            // Only elect while we see at least half the room: a peer that was offline/asleep (and
            // so sees nobody) must not come back as a phantom coordinator or take over the room
            // with a stale manifest. Manual SetCoordinator stays ungated.
            let alive_n = m
                .members
                .iter()
                .filter(|x| self.is_alive(now, x.id))
                .count();
            let quorum = alive_n * 2 >= m.members.len();
            if self.cfg.auto_elect {
                if waited >= grace && quorum {
                    let first = elect_coordinator(&m, |p| self.is_alive(now, p));
                    let winner = if waited >= 3 * grace && first != Some(local) {
                        elect_coordinator(&m, |p| self.is_alive(now, p) && Some(p) != first)
                    } else {
                        first
                    };
                    if winner == Some(local) {
                        self.propose_coordinator(now, local);
                        return;
                    }
                }
            } else if !self.coordinator_lost_prompted && waited >= grace {
                self.coordinator_lost_prompted = true;
                let candidates = m
                    .members
                    .iter()
                    .filter(|x| {
                        x.id != m.coordinator
                            && x.capabilities.can_coordinate
                            && self.is_alive(now, x.id)
                    })
                    .map(|x| x.id)
                    .collect();
                self.event(RoomEvent::CoordinatorLost { candidates });
            }
        } else {
            self.coordinator_lost_since = None;
            self.coordinator_lost_prompted = false;
        }

        // Only a speaker that was chosen and went silent is "lost"; an explicit "no speaker"
        // (None) is left alone.
        match m.speaker {
            Some(s) if !self.is_alive(now, s) => {
                if m.coordinator == local && self.cfg.fallback_speaker_to_coordinator {
                    self.apply_change(now, ChangeRequest::SetSpeaker(Some(local)));
                } else if !self.speaker_lost_prompted {
                    self.speaker_lost_prompted = true;
                    let candidates = speaker_candidates(&m, |p| self.is_alive(now, p));
                    self.event(RoomEvent::SpeakerLost { candidates });
                }
            }
            _ => self.speaker_lost_prompted = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Epoch;
    use crate::room::state::Capabilities;
    use std::collections::{BTreeMap, HashSet};

    fn k(a: PeerId, b: PeerId) -> (PeerId, PeerId) {
        if a < b {
            (a, b)
        } else {
            (b, a)
        }
    }

    struct Net {
        engines: BTreeMap<PeerId, RoomEngine>,
        down: HashSet<PeerId>,
        cut: HashSet<(PeerId, PeerId)>,
        links: HashSet<(PeerId, PeerId)>,
        now: u64,
        events: BTreeMap<PeerId, Vec<RoomEvent>>,
    }

    impl Net {
        fn new(ids: &[u64]) -> Self {
            let engines = ids
                .iter()
                .map(|&i| {
                    let info = MemberInfo {
                        id: PeerId(i),
                        name: format!("Mac {i}"),
                        mic_enabled: true,
                        capabilities: Capabilities::full(),
                    };
                    (PeerId(i), RoomEngine::new(RoomConfig::new(info)))
                })
                .collect();
            Net {
                engines,
                down: HashSet::new(),
                cut: HashSet::new(),
                links: HashSet::new(),
                now: 1_000,
                events: BTreeMap::new(),
            }
        }
        fn can_talk(&self, a: PeerId, b: PeerId) -> bool {
            !self.down.contains(&a)
                && !self.down.contains(&b)
                && !self.cut.contains(&k(a, b))
                && self.engines.contains_key(&b)
        }
        fn pump(&mut self) {
            for _ in 0..10_000 {
                let mut work = vec![];
                for (id, e) in self.engines.iter_mut() {
                    let outs = e.take_outputs();
                    if !self.down.contains(id) {
                        work.extend(outs.into_iter().map(|o| (*id, o)));
                    }
                }
                if work.is_empty() {
                    return;
                }
                for (from, o) in work {
                    let now = self.now;
                    match o {
                        Output::Send { to, msg } => {
                            if self.can_talk(from, to) && self.links.contains(&k(from, to)) {
                                self.engines
                                    .get_mut(&to)
                                    .unwrap()
                                    .on_message(now, from, msg);
                            }
                        }
                        Output::Connect(to) => {
                            if self.can_talk(from, to) && !self.links.contains(&k(from, to)) {
                                self.links.insert(k(from, to));
                                self.engines.get_mut(&from).unwrap().on_session_up(
                                    now,
                                    to,
                                    format!("Mac {}", to.0),
                                    "123 456".into(),
                                );
                                self.engines.get_mut(&to).unwrap().on_session_up(
                                    now,
                                    from,
                                    format!("Mac {}", from.0),
                                    "123 456".into(),
                                );
                            }
                        }
                        Output::Event(ev) => self.events.entry(from).or_default().push(ev),
                        Output::Roles(_) => {}
                    }
                }
            }
            panic!("message storm");
        }
        fn advance(&mut self, ms: u64) {
            let end = self.now + ms;
            while self.now < end {
                self.now += 100;
                let now = self.now;
                for (id, e) in self.engines.iter_mut() {
                    if !self.down.contains(id) {
                        e.tick(now);
                    }
                }
                self.pump();
            }
        }
        fn cmd(&mut self, id: u64, c: Command) {
            let now = self.now;
            self.engines
                .get_mut(&PeerId(id))
                .unwrap()
                .command(now, c)
                .unwrap();
            self.pump();
        }
        fn drop_link(&mut self, a: PeerId, b: PeerId) {
            if self.links.remove(&k(a, b)) {
                let now = self.now;
                if let Some(e) = self.engines.get_mut(&a) {
                    e.on_session_down(now, b);
                }
                if let Some(e) = self.engines.get_mut(&b) {
                    e.on_session_down(now, a);
                }
            }
        }
        fn kill(&mut self, id: u64) {
            self.down.insert(PeerId(id));
            let links: Vec<_> = self
                .links
                .iter()
                .copied()
                .filter(|(a, b)| a.0 == id || b.0 == id)
                .collect();
            for (a, b) in links {
                self.drop_link(a, b);
            }
        }
        fn partition(&mut self, left: &[u64], right: &[u64]) {
            for &a in left {
                for &b in right {
                    self.cut.insert(k(PeerId(a), PeerId(b)));
                    self.drop_link(PeerId(a), PeerId(b));
                }
            }
        }
        fn manifest(&self, id: u64) -> Option<RoomManifest> {
            self.engines[&PeerId(id)].manifest().cloned()
        }
        fn invite_for(&self, id: u64) -> RoomId {
            self.events[&PeerId(id)]
                .iter()
                .rev()
                .find_map(|e| match e {
                    RoomEvent::InviteReceived { room_id, .. } => Some(*room_id),
                    _ => None,
                })
                .expect("no invite")
        }
        fn has_event(&self, id: u64, f: impl Fn(&RoomEvent) -> bool) -> bool {
            self.events
                .get(&PeerId(id))
                .is_some_and(|v| v.iter().any(f))
        }
        fn converged(&self, ids: &[u64]) -> RoomManifest {
            let m0 = self.manifest(ids[0]).expect("not in room");
            for &i in ids {
                assert_eq!(self.manifest(i).as_ref(), Some(&m0), "peer {i} diverged");
            }
            m0
        }
    }

    fn room(ids: &[u64]) -> Net {
        let mut n = Net::new(ids);
        n.cmd(
            ids[0],
            Command::CreateRoom {
                name: "Conference Room".into(),
            },
        );
        for &id in &ids[1..] {
            n.cmd(ids[0], Command::Invite(PeerId(id)));
            let room_id = n.invite_for(id);
            n.cmd(
                id,
                Command::RespondToInvite {
                    room_id,
                    accept: true,
                },
            );
        }
        n.advance(300);
        n
    }

    #[test]
    fn create_invite_join_converges() {
        let n = room(&[1, 2, 3]);
        let m = n.converged(&[1, 2, 3]);
        assert_eq!(m.members.len(), 3);
        assert_eq!(m.coordinator, PeerId(1));
        assert_eq!(m.speaker, Some(PeerId(1)));
        assert!(n.has_event(
            2,
            |e| matches!(e, RoomEvent::PeerJoined { peer, .. } if *peer == PeerId(3))
        ));
        assert!(n.engines[&PeerId(1)].roles().is_coordinator);
    }
    #[test]
    fn invite_carries_sas_and_decline_is_reported() {
        let mut n = Net::new(&[1, 2]);
        n.cmd(1, Command::CreateRoom { name: "R".into() });
        n.cmd(1, Command::Invite(PeerId(2)));
        assert!(n.has_event(
            2,
            |e| matches!(e, RoomEvent::InviteReceived { sas, .. } if sas == "123 456")
        ));
        let room_id = n.invite_for(2);
        n.cmd(
            2,
            Command::RespondToInvite {
                room_id,
                accept: false,
            },
        );
        assert!(n.has_event(
            1,
            |e| matches!(e, RoomEvent::InviteDeclined { peer } if *peer == PeerId(2))
        ));
        assert!(n.manifest(2).is_none());
    }
    #[test]
    fn non_coordinator_changes_speaker_and_mics() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(3, Command::SetSpeaker(Some(PeerId(2))));
        n.cmd(
            3,
            Command::SetMicEnabled {
                peer: PeerId(3),
                enabled: false,
            },
        );
        let m = n.converged(&[1, 2, 3]);
        assert_eq!(m.speaker, Some(PeerId(2)));
        assert_eq!(m.enabled_mics(), vec![PeerId(1), PeerId(2)]);
        assert!(n.engines[&PeerId(2)].roles().is_speaker);
    }
    #[test]
    fn coordinator_change_bumps_epoch() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(2, Command::SetCoordinator(PeerId(3)));
        let m = n.converged(&[1, 2, 3]);
        assert_eq!((m.coordinator, m.epoch), (PeerId(3), Epoch(2)));
        assert!(n.engines[&PeerId(3)].roles().is_coordinator);
        assert!(!n.engines[&PeerId(1)].roles().is_coordinator);
    }
    #[test]
    fn stale_coordinator_is_ignored() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(1, Command::SetCoordinator(PeerId(2)));
        let cur = n.converged(&[1, 2, 3]);
        let mut old = cur.clone();
        old.epoch = Epoch(1);
        old.coordinator = PeerId(1);
        old.revision = 99;
        let now = n.now;
        let e3 = n.engines.get_mut(&PeerId(3)).unwrap();
        e3.on_message(now, PeerId(1), ControlMessage::Manifest(old));
        e3.on_message(
            now,
            PeerId(1),
            ControlMessage::ActiveMic {
                room_id: cur.room_id,
                epoch: Epoch(1),
                primary: Some(PeerId(1)),
                secondary: None,
            },
        );
        assert_eq!(e3.manifest(), Some(&cur));
        assert!(!e3
            .take_outputs()
            .iter()
            .any(|o| matches!(o, Output::Event(RoomEvent::ActiveMicChanged { .. }))));
    }
    #[test]
    fn active_mic_is_broadcast_by_coordinator() {
        let mut n = room(&[1, 2]);
        let now = n.now;
        n.engines
            .get_mut(&PeerId(1))
            .unwrap()
            .on_local_selection(now, Some(PeerId(2)), None);
        n.pump();
        assert!(n.has_event(
            2,
            |e| matches!(e, RoomEvent::ActiveMicChanged { primary: Some(p), .. } if *p == PeerId(2))
        ));
        let snap = n.engines[&PeerId(2)].snapshot().unwrap();
        assert!(
            snap.members
                .iter()
                .find(|m| m.id == PeerId(2))
                .unwrap()
                .is_active_mic
        );
    }
    #[test]
    fn automatic_failover_elects_lowest_alive() {
        let mut n = room(&[1, 2, 3]);
        n.kill(1);
        n.advance(7_000);
        let m = n.converged(&[2, 3]);
        assert_eq!((m.coordinator, m.epoch), (PeerId(2), Epoch(2)));
    }
    #[test]
    fn manual_failover_prompts_then_user_picks() {
        let mut n = room(&[1, 2, 3]);
        for e in n.engines.values_mut() {
            e.set_auto_elect(false);
        }
        n.kill(1);
        n.advance(7_000);
        assert!(n.has_event(2, |e| matches!(e, RoomEvent::CoordinatorLost { candidates } if candidates.contains(&PeerId(3)))));
        assert_eq!(n.converged(&[2, 3]).coordinator, PeerId(1));
        n.cmd(3, Command::SetCoordinator(PeerId(3)));
        assert_eq!(n.converged(&[2, 3]).coordinator, PeerId(3));
    }
    #[test]
    fn speaker_loss_prompts_without_fallback() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(1, Command::SetSpeaker(Some(PeerId(3))));
        n.kill(3);
        n.advance(6_000);
        assert!(n.has_event(1, |e| matches!(e, RoomEvent::SpeakerLost { candidates } if !candidates.contains(&PeerId(3)))));
        assert_eq!(
            n.converged(&[1, 2]).speaker,
            Some(PeerId(3)),
            "never auto-switch without the setting"
        );
    }
    #[test]
    fn speaker_loss_falls_back_to_coordinator_when_enabled() {
        let mut n = room(&[1, 2, 3]);
        for e in n.engines.values_mut() {
            e.set_fallback_speaker(true);
        }
        n.cmd(1, Command::SetSpeaker(Some(PeerId(3))));
        n.kill(3);
        n.advance(6_000);
        assert_eq!(n.converged(&[1, 2]).speaker, Some(PeerId(1)));
    }
    #[test]
    fn coordinator_leave_hands_off_and_member_leave_removes() {
        let mut n = room(&[1, 2, 3, 4]);
        n.cmd(1, Command::Leave);
        n.advance(200);
        assert!(n.manifest(1).is_none());
        let m = n.converged(&[2, 3, 4]);
        assert_eq!(m.coordinator, PeerId(2));
        assert!(!m.is_member(PeerId(1)));
        n.cmd(4, Command::Leave);
        n.advance(200);
        assert_eq!(n.converged(&[2, 3]).members.len(), 2);
    }
    #[test]
    fn split_brain_heals_to_higher_epoch() {
        let mut n = room(&[1, 2, 3, 4]);
        n.partition(&[1, 2], &[3, 4]);
        n.advance(8_000);
        assert_eq!(n.converged(&[1, 2]).coordinator, PeerId(1));
        let right = n.converged(&[3, 4]);
        assert_eq!((right.coordinator, right.epoch), (PeerId(3), Epoch(2)));
        n.cut.clear();
        n.advance(3_000);
        assert_eq!(n.converged(&[1, 2, 3, 4]).coordinator, PeerId(3));
    }

    // ---------- review regressions ----------
    fn coordinators(n: &Net, ids: &[u64]) -> Vec<u64> {
        ids.iter()
            .copied()
            .filter(|&i| n.engines[&PeerId(i)].roles().is_coordinator)
            .collect()
    }

    #[test]
    fn join_request_to_former_coordinator_is_relayed() {
        let mut n = Net::new(&[1, 2, 3, 4]);
        n.cmd(1, Command::CreateRoom { name: "R".into() });
        for id in [2, 3] {
            n.cmd(1, Command::Invite(PeerId(id)));
            let room_id = n.invite_for(id);
            n.cmd(
                id,
                Command::RespondToInvite {
                    room_id,
                    accept: true,
                },
            );
        }
        n.advance(300);
        n.cmd(1, Command::Invite(PeerId(4)));
        let room_id = n.invite_for(4);
        n.cmd(1, Command::SetCoordinator(PeerId(2)));
        n.cmd(
            4,
            Command::RespondToInvite {
                room_id,
                accept: true,
            },
        );
        n.advance(11_000);
        assert!(!n.has_event(4, |e| matches!(e, RoomEvent::Error { .. })));
        let m = n.converged(&[1, 2, 3, 4]);
        assert_eq!(m.coordinator, PeerId(2));
        assert!(m.is_member(PeerId(4)));
    }

    #[test]
    fn concurrent_manual_coordinator_picks_converge() {
        let mut n = room(&[1, 2, 3, 4, 5]);
        for e in n.engines.values_mut() {
            e.set_auto_elect(false);
        }
        n.kill(1);
        n.advance(7_000);
        let now = n.now;
        n.engines
            .get_mut(&PeerId(2))
            .unwrap()
            .command(now, Command::SetCoordinator(PeerId(5)))
            .unwrap();
        n.engines
            .get_mut(&PeerId(3))
            .unwrap()
            .command(now, Command::SetCoordinator(PeerId(4)))
            .unwrap();
        n.pump();
        n.advance(5_000);
        let m = n.converged(&[2, 3, 4, 5]);
        assert_eq!(m.coordinator, PeerId(4));
        assert_eq!(coordinators(&n, &[2, 3, 4, 5]), vec![4]);
    }

    #[test]
    fn relayed_proposals_with_different_fallback_settings_converge() {
        let mut n = room(&[1, 2, 3, 4]);
        for e in n.engines.values_mut() {
            e.set_auto_elect(false);
        }
        n.engines
            .get_mut(&PeerId(2))
            .unwrap()
            .set_fallback_speaker(true);
        n.kill(1);
        n.advance(7_000);
        let now = n.now;
        for id in [2, 3] {
            n.engines
                .get_mut(&PeerId(id))
                .unwrap()
                .command(now, Command::SetCoordinator(PeerId(4)))
                .unwrap();
        }
        n.pump();
        n.advance(5_000);
        let m = n.converged(&[2, 3, 4]);
        assert_eq!(m.coordinator, PeerId(4));
    }

    #[test]
    fn leave_hand_off_is_republished_by_successor() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(1, Command::Leave);
        let m = n.converged(&[2, 3]);
        assert_eq!(m.coordinator, PeerId(2));
        assert!(m.revision >= 1, "successor vouches for the hand-off");
    }

    #[test]
    fn removed_member_that_missed_its_removal_leaves() {
        let mut n = room(&[1, 2, 3]);
        n.cut.insert(k(PeerId(1), PeerId(3))); // 1<->3 messages lost, session stays up
        n.cmd(1, Command::RemoveMember(PeerId(3)));
        n.advance(200);
        n.cut.clear();
        n.advance(10_000);
        assert!(n.manifest(3).is_none());
        assert!(n.has_event(3, |e| matches!(e, RoomEvent::LeftRoom)));
        assert!(!n.engines[&PeerId(3)].roles().is_coordinator);
        assert_eq!(
            n.converged(&[1, 2]).member_ids(),
            vec![PeerId(1), PeerId(2)]
        );
    }

    #[test]
    fn lost_leave_is_repaired_by_the_coordinator() {
        let mut n = room(&[1, 2, 3]);
        n.cut.insert(k(PeerId(1), PeerId(2)));
        n.cmd(2, Command::Leave);
        n.advance(200);
        n.cut.clear();
        n.advance(10_000);
        assert!(n.manifest(2).is_none());
        let m = n.converged(&[1, 3]);
        assert_eq!(m.member_ids(), vec![PeerId(1), PeerId(3)]);
    }

    #[test]
    fn peer_outside_the_room_answers_heartbeats_with_leave() {
        let n = room(&[1, 2]);
        let m = n.converged(&[1, 2]);
        let now = n.now;
        let mut stranger = Net::new(&[7]).engines.remove(&PeerId(7)).unwrap();
        stranger.on_session_up(now, PeerId(1), "Mac 1".into(), "123 456".into());
        stranger.take_outputs();
        stranger.on_message(
            now,
            PeerId(1),
            ControlMessage::Heartbeat {
                room_id: m.room_id,
                epoch: m.epoch,
                revision: m.revision,
                manifest: Some(m.clone()),
                sees_coordinator: true,
            },
        );
        assert_eq!(
            stranger.take_outputs(),
            vec![Output::Send {
                to: PeerId(1),
                msg: ControlMessage::Leave { room_id: m.room_id }
            }]
        );
    }

    #[test]
    fn coordinator_rejects_request_from_older_epoch() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(1, Command::SetCoordinator(PeerId(2)));
        let cur = n.converged(&[1, 2, 3]);
        let now = n.now;
        let e2 = n.engines.get_mut(&PeerId(2)).unwrap();
        e2.on_message(
            now,
            PeerId(3),
            ControlMessage::Request {
                room_id: cur.room_id,
                epoch: Epoch(1),
                change: ChangeRequest::SetSpeaker(Some(PeerId(3))),
            },
        );
        assert_eq!(e2.manifest(), Some(&cur));
        e2.on_message(
            now,
            PeerId(3),
            ControlMessage::Request {
                room_id: cur.room_id,
                epoch: cur.epoch,
                change: ChangeRequest::SetSpeaker(Some(PeerId(3))),
            },
        );
        assert_eq!(e2.manifest().unwrap().speaker, Some(PeerId(3)));
    }

    #[test]
    fn one_broken_link_keeps_a_single_coordinator() {
        let mut n = room(&[1, 2, 3]);
        n.partition(&[1], &[3]); // 2 still sees both
        for _ in 0..60 {
            n.advance(500);
            for i in 1..=3 {
                let m = n.manifest(i).expect("still in the room");
                assert_eq!((m.coordinator, m.epoch), (PeerId(1), Epoch(1)), "peer {i}");
            }
            assert_eq!(coordinators(&n, &[1, 2, 3]), vec![1]);
        }
        assert!(!n.has_event(3, |e| matches!(e, RoomEvent::CoordinatorLost { .. })));
        // Requests from the cut-off peer are relayed by a member that sees the coordinator, and
        // that member relays the coordinator's manifest back.
        n.cmd(3, Command::SetSpeaker(Some(PeerId(3))));
        n.advance(1_500);
        assert_eq!(n.converged(&[1, 2, 3]).speaker, Some(PeerId(3)));
        assert!(n.engines[&PeerId(3)].roles().is_speaker);
    }

    #[test]
    fn coordinator_still_fails_over_when_nobody_sees_it() {
        let mut n = room(&[1, 2, 3]);
        n.partition(&[1], &[3]);
        n.advance(3_000);
        n.kill(1);
        n.advance(8_000);
        let m = n.converged(&[2, 3]);
        assert_eq!((m.coordinator, m.epoch), (PeerId(2), Epoch(2)));
    }

    #[test]
    fn speaker_lost_prompts_again_after_speaker_recovers() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(1, Command::SetSpeaker(Some(PeerId(3))));
        n.kill(3);
        n.advance(6_000);
        n.down.remove(&PeerId(3));
        n.advance(3_000);
        n.kill(3);
        n.advance(6_000);
        let prompts = n.events[&PeerId(1)]
            .iter()
            .filter(|e| matches!(e, RoomEvent::SpeakerLost { .. }))
            .count();
        assert_eq!(prompts, 2);
    }

    #[test]
    fn room_snapshot_marks_silent_member_offline() {
        let mut n = room(&[1, 2, 3]);
        // Session stays up but nothing arrives from 3 any more.
        n.cut.insert(k(PeerId(1), PeerId(3)));
        n.advance(8_000);
        let last = n.events[&PeerId(1)]
            .iter()
            .rev()
            .find_map(|e| match e {
                RoomEvent::RoomChanged(Some(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap();
        let online = |id| {
            last.members
                .iter()
                .find(|m| m.id == PeerId(id))
                .unwrap()
                .online
        };
        assert!(!online(3));
        assert!(online(1) && online(2));
        n.cut.clear();
        n.advance(2_000);
        let snap = n.engines[&PeerId(1)].snapshot().unwrap();
        assert!(snap.members.iter().all(|m| m.online));
        assert!(n.events[&PeerId(1)].iter().rev().any(|e| matches!(e,
            RoomEvent::RoomChanged(Some(s)) if s.members.iter().all(|m| m.online))));
    }

    #[test]
    fn set_local_info_propagates_through_coordinator() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(
            1,
            Command::SetMicEnabled {
                peer: PeerId(3),
                enabled: false,
            },
        );
        n.engines
            .get_mut(&PeerId(3))
            .unwrap()
            .set_local_info(MemberInfo {
                id: PeerId(3),
                name: "Board Room".into(),
                mic_enabled: true,
                capabilities: Capabilities {
                    driver_installed: false,
                    ..Capabilities::full()
                },
            });
        n.pump();
        let m = n.converged(&[1, 2, 3]);
        let me = m.member(PeerId(3)).unwrap();
        assert_eq!(me.name, "Board Room");
        assert!(!me.capabilities.driver_installed);
        assert!(!me.mic_enabled, "mic setting stays as the room has it");
        // The coordinator applies its own change directly.
        n.engines
            .get_mut(&PeerId(1))
            .unwrap()
            .set_local_info(MemberInfo {
                id: PeerId(1),
                name: "Host".into(),
                mic_enabled: true,
                capabilities: Capabilities::full(),
            });
        n.pump();
        assert_eq!(
            n.converged(&[1, 2, 3]).member(PeerId(1)).unwrap().name,
            "Host"
        );
        // Unchanged info publishes nothing.
        let v = n.converged(&[1, 2, 3]).version();
        let info = n.manifest(2).unwrap().member(PeerId(2)).unwrap().clone();
        n.engines.get_mut(&PeerId(2)).unwrap().set_local_info(info);
        n.advance(2_000);
        assert_eq!(n.converged(&[1, 2, 3]).version(), v);
    }

    #[test]
    fn set_local_info_while_coordinator_unreachable_is_retried() {
        let mut n = room(&[1, 2]);
        n.cut.insert(k(PeerId(1), PeerId(2))); // request is lost
        n.engines
            .get_mut(&PeerId(2))
            .unwrap()
            .set_local_info(MemberInfo {
                id: PeerId(2),
                name: "Renamed".into(),
                mic_enabled: true,
                capabilities: Capabilities::full(),
            });
        n.pump();
        n.cut.clear();
        n.advance(2_000);
        assert_eq!(
            n.converged(&[1, 2]).member(PeerId(2)).unwrap().name,
            "Renamed"
        );
    }

    #[test]
    fn active_mic_is_not_queued_for_unconnected_members() {
        let mut n = room(&[1, 2, 3]);
        n.drop_link(PeerId(1), PeerId(3));
        let now = n.now;
        let e1 = n.engines.get_mut(&PeerId(1)).unwrap();
        e1.take_outputs();
        e1.on_local_selection(now, Some(PeerId(2)), None);
        let outs = e1.take_outputs();
        assert!(outs.iter().any(|o| matches!(o,
            Output::Send { to, msg: ControlMessage::ActiveMic { .. } } if *to == PeerId(2))));
        assert!(!outs.iter().any(|o| matches!(o,
            Output::Send { to, .. } | Output::Connect(to) if *to == PeerId(3))));
        assert!(e1.queued.is_empty());
    }

    #[test]
    fn leaving_drops_queued_messages_for_the_room() {
        let mut n = room(&[1, 2, 3]);
        n.cut.insert(k(PeerId(2), PeerId(3)));
        n.drop_link(PeerId(2), PeerId(3));
        let now = n.now;
        let e2 = n.engines.get_mut(&PeerId(2)).unwrap();
        e2.command(now, Command::Invite(PeerId(9))).unwrap(); // queued for an unknown peer
        assert!(!e2.queued.is_empty());
        e2.command(now, Command::Leave).unwrap();
        // Only the Leave notice for the unreachable member remains queued.
        let left: Vec<_> = e2.queued.values().flatten().collect();
        assert_eq!(left.len(), 1);
        assert!(matches!(left[0], ControlMessage::Leave { .. }));
    }

    #[test]
    fn join_timeout_drops_queued_join_request() {
        let mut n = Net::new(&[1, 2]);
        n.cmd(1, Command::CreateRoom { name: "R".into() });
        n.cmd(1, Command::Invite(PeerId(2)));
        let room_id = n.invite_for(2);
        n.kill(1);
        n.cmd(
            2,
            Command::RespondToInvite {
                room_id,
                accept: true,
            },
        );
        assert!(!n.engines[&PeerId(2)].queued.is_empty());
        n.advance(11_000);
        assert!(n.has_event(2, |e| matches!(e, RoomEvent::Error { .. })));
        assert!(n.engines[&PeerId(2)].queued.is_empty());
    }

    #[test]
    fn explicit_no_speaker_neither_prompts_nor_falls_back() {
        let mut n = room(&[1, 2, 3]);
        for e in n.engines.values_mut() {
            e.set_fallback_speaker(true);
        }
        n.cmd(2, Command::SetSpeaker(None));
        n.advance(6_000);
        assert_eq!(n.converged(&[1, 2, 3]).speaker, None);
        for i in 1..=3 {
            assert!(!n.has_event(i, |e| matches!(e, RoomEvent::SpeakerLost { .. })));
        }
    }

    #[test]
    fn removing_the_speaker_prompts_or_falls_back() {
        let mut n = room(&[1, 2, 3]);
        n.cmd(1, Command::SetSpeaker(Some(PeerId(3))));
        n.cmd(1, Command::RemoveMember(PeerId(3)));
        assert_eq!(n.converged(&[1, 2]).speaker, None);
        assert!(n.has_event(2, |e| matches!(e, RoomEvent::SpeakerLost { .. })));

        let mut n = room(&[1, 2, 3]);
        n.engines
            .get_mut(&PeerId(1))
            .unwrap()
            .set_fallback_speaker(true);
        n.cmd(1, Command::SetSpeaker(Some(PeerId(3))));
        n.cmd(3, Command::Leave);
        assert_eq!(n.converged(&[1, 2]).speaker, Some(PeerId(1)));
    }

    #[test]
    fn connect_is_emitted_once_per_peer_per_drain() {
        let mut n = room(&[1, 2, 3]);
        n.drop_link(PeerId(1), PeerId(3));
        n.cut.insert(k(PeerId(1), PeerId(3)));
        let now = n.now;
        let e1 = n.engines.get_mut(&PeerId(1)).unwrap();
        e1.take_outputs();
        e1.command(now, Command::Rename("A".into())).unwrap();
        e1.command(now, Command::SetSpeaker(Some(PeerId(2))))
            .unwrap();
        e1.tick(now + 1_000);
        let connects = e1
            .take_outputs()
            .into_iter()
            .filter(|o| matches!(o, Output::Connect(p) if *p == PeerId(3)))
            .count();
        assert_eq!(connects, 1);
    }

    #[test]
    fn relayed_request_takes_at_most_one_extra_hop() {
        let mut n = room(&[1, 2, 3]);
        n.kill(1); // sessions to 1 drop; 2 and 3 still report sees_coordinator=true for ~4 s
        let now = n.now;
        n.engines
            .get_mut(&PeerId(2))
            .unwrap()
            .command(now, Command::SetSpeaker(Some(PeerId(3))))
            .unwrap();
        let mut hops = 0;
        for _ in 0..50 {
            let mut work = vec![];
            for (id, e) in n.engines.iter_mut() {
                let outs = e.take_outputs();
                if !n.down.contains(id) {
                    work.extend(outs.into_iter().map(|o| (*id, o)));
                }
            }
            for (from, o) in work {
                if let Output::Send { to, msg } = o {
                    if matches!(msg, ControlMessage::Request { .. }) {
                        hops += 1;
                    }
                    if n.can_talk(from, to) && n.links.contains(&k(from, to)) {
                        n.engines.get_mut(&to).unwrap().on_message(now, from, msg);
                    }
                }
            }
        }
        assert!(hops <= 2, "request bounced {hops} times");
    }

    #[test]
    fn waking_peer_does_not_take_over_the_room() {
        let mut n = room(&[1, 2, 3]);
        n.kill(3); // asleep: no ticks
        n.advance(20_000);
        n.cmd(1, Command::SetSpeaker(Some(PeerId(2))));
        n.down.remove(&PeerId(3));
        n.partition(&[3], &[1, 2]); // wakes; sessions take a few seconds to come back
        n.advance(3_000);
        n.cut.clear();
        n.advance(5_000);
        let m = n.converged(&[1, 2, 3]);
        assert_eq!((m.coordinator, m.epoch), (PeerId(1), Epoch(1)));
        assert_eq!(m.speaker, Some(PeerId(2)), "changes made meanwhile survive");
    }

    #[test]
    fn isolated_peer_does_not_self_elect_and_leaves_after_removal() {
        let mut n = room(&[1, 2, 3]);
        n.partition(&[3], &[1, 2]);
        n.advance(8_000);
        assert!(!n.engines[&PeerId(3)].roles().is_coordinator);
        n.cmd(1, Command::RemoveMember(PeerId(3)));
        n.cut.clear();
        n.advance(10_000);
        assert!(n.manifest(3).is_none());
        assert_eq!(
            n.converged(&[1, 2]).member_ids(),
            vec![PeerId(1), PeerId(2)]
        );
    }

    #[test]
    fn relayed_join_request_cannot_overwrite_a_member() {
        let mut n = room(&[1, 2, 3]);
        let cur = n.converged(&[1, 2, 3]);
        let now = n.now;
        let mut forged = cur.member(PeerId(3)).unwrap().clone();
        forged.name = "Forged".into();
        let e1 = n.engines.get_mut(&PeerId(1)).unwrap();
        e1.on_message(
            now,
            PeerId(2),
            ControlMessage::JoinRequest {
                room_id: cur.room_id,
                member: forged,
            },
        );
        assert_eq!(e1.manifest(), Some(&cur));
    }

    #[test]
    fn member_rejoin_keeps_room_mic_setting() {
        let mut n = room(&[1, 2]);
        n.cmd(
            1,
            Command::SetMicEnabled {
                peer: PeerId(2),
                enabled: false,
            },
        );
        let cur = n.converged(&[1, 2]);
        let now = n.now;
        let mut me = cur.member(PeerId(2)).unwrap().clone();
        me.mic_enabled = true;
        let e1 = n.engines.get_mut(&PeerId(1)).unwrap();
        e1.on_message(
            now,
            PeerId(2),
            ControlMessage::JoinRequest {
                room_id: cur.room_id,
                member: me,
            },
        );
        assert!(
            !e1.manifest()
                .unwrap()
                .member(PeerId(2))
                .unwrap()
                .mic_enabled
        );
    }
}
