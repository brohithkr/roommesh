//! Replicated room manifest and the rules deciding which manifest wins.
use crate::ids::{Epoch, PeerId, RoomId};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub can_coordinate: bool,
    pub has_mic: bool,
    pub has_speaker: bool,
    pub driver_installed: bool,
}
impl Capabilities {
    pub fn full() -> Self {
        Self { can_coordinate: true, has_mic: true, has_speaker: true, driver_installed: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub id: PeerId,
    pub name: String,
    pub mic_enabled: bool,
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomManifest {
    pub room_id: RoomId,
    pub name: String,
    /// Coordinator term; bumps on every coordinator change.
    pub epoch: Epoch,
    /// Changes published by the coordinator within one epoch.
    pub revision: u32,
    pub protocol_version: u16,
    /// Sorted by id, unique.
    pub members: Vec<MemberInfo>,
    pub coordinator: PeerId,
    pub speaker: Option<PeerId>,
}

impl RoomManifest {
    pub fn new(room_id: RoomId, name: String, creator: MemberInfo) -> Self {
        let id = creator.id;
        Self {
            room_id,
            name,
            epoch: Epoch(1),
            revision: 0,
            protocol_version: PROTOCOL_VERSION,
            members: vec![creator],
            coordinator: id,
            speaker: Some(id),
        }
    }
    pub fn version(&self) -> (Epoch, u32) {
        (self.epoch, self.revision)
    }
    pub fn is_member(&self, id: PeerId) -> bool {
        self.members.iter().any(|m| m.id == id)
    }
    pub fn member(&self, id: PeerId) -> Option<&MemberInfo> {
        self.members.iter().find(|m| m.id == id)
    }
    pub fn member_mut(&mut self, id: PeerId) -> Option<&mut MemberInfo> {
        self.members.iter_mut().find(|m| m.id == id)
    }
    pub fn upsert_member(&mut self, info: MemberInfo) {
        match self.members.binary_search_by_key(&info.id, |m| m.id) {
            Ok(i) => self.members[i] = info,
            Err(i) => self.members.insert(i, info),
        }
    }
    pub fn remove_member(&mut self, id: PeerId) {
        self.members.retain(|m| m.id != id);
        if self.speaker == Some(id) {
            self.speaker = None;
        }
    }
    pub fn enabled_mics(&self) -> Vec<PeerId> {
        self.members.iter().filter(|m| m.mic_enabled && m.capabilities.has_mic).map(|m| m.id).collect()
    }
    pub fn member_ids(&self) -> Vec<PeerId> {
        self.members.iter().map(|m| m.id).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    WrongRoom,
    StaleEpoch,
    StaleRevision,
    NotCoordinator,
    NotMember,
    LostTieBreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acceptance {
    Accept,
    Reject(RejectReason),
}

/// Decides whether `incoming` (sent by authenticated `sender`) replaces `current`.
/// Order: (epoch, revision). Same-epoch conflicting coordinators → lowest coordinator id wins.
pub fn evaluate_manifest(current: Option<&RoomManifest>, incoming: &RoomManifest, sender: PeerId) -> Acceptance {
    use Acceptance::*;
    use RejectReason::*;
    let Some(cur) = current else { return Accept };
    if cur.room_id != incoming.room_id {
        return Reject(WrongRoom);
    }
    if incoming.epoch < cur.epoch {
        return Reject(StaleEpoch);
    }
    if incoming.epoch > cur.epoch {
        return if cur.is_member(sender) { Accept } else { Reject(NotMember) };
    }
    // same epoch, conflicting coordinators: lowest id wins the tie-break. Only a current member
    // may assert this (an outsider can't force a coordinator change), and the manifest must
    // actually be vouched for by the coordinator it names.
    if incoming.coordinator != cur.coordinator {
        if !cur.is_member(sender) {
            return Reject(NotMember);
        }
        return if incoming.coordinator < cur.coordinator {
            if sender == incoming.coordinator { Accept } else { Reject(NotCoordinator) }
        } else {
            Reject(LostTieBreak)
        };
    }
    if sender != cur.coordinator {
        return Reject(NotCoordinator);
    }
    if incoming.revision > cur.revision {
        Accept
    } else {
        Reject(StaleRevision)
    }
}

/// A realtime/control command claiming coordinator authority is valid only for the
/// current (epoch, coordinator) pair.
pub fn is_valid_coordinator_command(current: &RoomManifest, sender: PeerId, epoch: Epoch) -> bool {
    epoch == current.epoch && sender == current.coordinator
}

#[cfg(test)]
mod tests {
    use super::*;
    fn member(id: u64) -> MemberInfo {
        MemberInfo { id: PeerId(id), name: format!("Mac {id}"), mic_enabled: true, capabilities: Capabilities::full() }
    }
    fn room() -> RoomManifest {
        RoomManifest::new(RoomId(7), "Conference Room".into(), member(1))
    }

    #[test]
    fn new_room_has_creator_as_coordinator_and_speaker() {
        let m = room();
        assert_eq!(m.epoch, Epoch(1));
        assert_eq!(m.coordinator, PeerId(1));
        assert_eq!(m.speaker, Some(PeerId(1)));
        assert!(m.is_member(PeerId(1)));
    }
    #[test]
    fn upsert_keeps_members_sorted_and_unique() {
        let mut m = room();
        m.upsert_member(member(9));
        m.upsert_member(member(3));
        m.upsert_member(member(3));
        let ids: Vec<u64> = m.members.iter().map(|x| x.id.0).collect();
        assert_eq!(ids, vec![1, 3, 9]);
    }
    #[test]
    fn remove_member_clears_speaker() {
        let mut m = room();
        m.upsert_member(member(2));
        m.speaker = Some(PeerId(2));
        m.remove_member(PeerId(2));
        assert_eq!(m.speaker, None);
    }
    #[test]
    fn stale_epoch_rejected() {
        let mut cur = room();
        cur.upsert_member(member(2));
        cur.epoch = Epoch(42);
        cur.coordinator = PeerId(2);
        let mut old = cur.clone();
        old.epoch = Epoch(41);
        old.coordinator = PeerId(1);
        assert_eq!(evaluate_manifest(Some(&cur), &old, PeerId(1)), Acceptance::Reject(RejectReason::StaleEpoch));
    }
    #[test]
    fn higher_epoch_from_member_accepted() {
        let mut cur = room();
        cur.upsert_member(member(4));
        let mut next = cur.clone();
        next.epoch = Epoch(2);
        next.coordinator = PeerId(4);
        assert_eq!(evaluate_manifest(Some(&cur), &next, PeerId(4)), Acceptance::Accept);
        assert_eq!(evaluate_manifest(Some(&cur), &next, PeerId(99)), Acceptance::Reject(RejectReason::NotMember));
    }
    #[test]
    fn same_epoch_revision_only_from_coordinator() {
        let cur = room();
        let mut next = cur.clone();
        next.revision = 1;
        assert_eq!(evaluate_manifest(Some(&cur), &next, PeerId(1)), Acceptance::Accept);
        assert_eq!(evaluate_manifest(Some(&cur), &next, PeerId(2)), Acceptance::Reject(RejectReason::NotCoordinator));
        assert_eq!(evaluate_manifest(Some(&next), &cur, PeerId(1)), Acceptance::Reject(RejectReason::StaleRevision));
    }
    #[test]
    fn same_epoch_conflict_lowest_coordinator_wins() {
        let mut base = room();
        base.upsert_member(member(2));
        base.upsert_member(member(3));
        let mut a = base.clone();
        a.epoch = Epoch(5);
        a.coordinator = PeerId(3);
        let mut b = base.clone();
        b.epoch = Epoch(5);
        b.coordinator = PeerId(2);
        assert_eq!(evaluate_manifest(Some(&a), &b, PeerId(2)), Acceptance::Accept);
        assert_eq!(evaluate_manifest(Some(&b), &a, PeerId(3)), Acceptance::Reject(RejectReason::LostTieBreak));
    }
    #[test]
    fn same_epoch_conflict_from_non_member_rejected() {
        let mut base = room();
        base.upsert_member(member(2));
        base.upsert_member(member(3));
        let mut a = base.clone();
        a.epoch = Epoch(5);
        a.coordinator = PeerId(3);
        let mut b = base.clone();
        b.epoch = Epoch(5);
        b.coordinator = PeerId(2);
        // PeerId(2) has the lower coordinator id (would normally win the tie-break), but the
        // manifest is claimed to come from PeerId(99), who isn't even a member of `a`.
        assert_eq!(evaluate_manifest(Some(&a), &b, PeerId(99)), Acceptance::Reject(RejectReason::NotMember));
    }
    #[test]
    fn same_epoch_conflict_not_vouched_by_named_coordinator_rejected() {
        let mut base = room();
        base.upsert_member(member(2));
        base.upsert_member(member(3));
        let mut a = base.clone();
        a.epoch = Epoch(5);
        a.coordinator = PeerId(3);
        let mut b = base.clone();
        b.epoch = Epoch(5);
        b.coordinator = PeerId(2);
        // PeerId(3) is a member and would win nothing here; the manifest names PeerId(2) as
        // coordinator but is sent by PeerId(3), who isn't that coordinator.
        assert_eq!(evaluate_manifest(Some(&a), &b, PeerId(3)), Acceptance::Reject(RejectReason::NotCoordinator));
    }
    #[test]
    fn coordinator_commands_validated_by_epoch() {
        let mut m = room();
        m.epoch = Epoch(42);
        m.coordinator = PeerId(4);
        assert!(is_valid_coordinator_command(&m, PeerId(4), Epoch(42)));
        assert!(!is_valid_coordinator_command(&m, PeerId(1), Epoch(41)));
        assert!(!is_valid_coordinator_command(&m, PeerId(4), Epoch(41)));
    }
}
