//! Deterministic coordinator election: every peer computes the same answer from the same
//! manifest and liveness view. Prefer peers with the virtual driver installed, then lowest id.
use crate::ids::PeerId;
use crate::room::state::RoomManifest;

pub fn elect_coordinator(m: &RoomManifest, alive: impl Fn(PeerId) -> bool) -> Option<PeerId> {
    m.members
        .iter()
        .filter(|x| x.capabilities.can_coordinate && alive(x.id))
        .min_by_key(|x| (!x.capabilities.driver_installed, x.id))
        .map(|x| x.id)
}

pub fn speaker_candidates(m: &RoomManifest, alive: impl Fn(PeerId) -> bool) -> Vec<PeerId> {
    m.members
        .iter()
        .filter(|x| x.capabilities.has_speaker && alive(x.id))
        .map(|x| x.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::room::state::*;
    fn m() -> RoomManifest {
        let mk = |id: u64, drv: bool| MemberInfo {
            id: PeerId(id),
            name: String::new(),
            mic_enabled: true,
            capabilities: Capabilities {
                driver_installed: drv,
                ..Capabilities::full()
            },
            noise_baseline_db: None,
        };
        let mut r = RoomManifest::new(RoomId(1), "R".into(), mk(5, true));
        r.upsert_member(mk(2, false));
        r.upsert_member(mk(3, true));
        r.upsert_member(mk(4, true));
        r
    }
    #[test]
    fn prefers_driver_installed_then_lowest_id() {
        assert_eq!(elect_coordinator(&m(), |_| true), Some(PeerId(3)));
        assert_eq!(elect_coordinator(&m(), |p| p != PeerId(3)), Some(PeerId(4)));
        assert_eq!(elect_coordinator(&m(), |p| p == PeerId(2)), Some(PeerId(2)));
        assert_eq!(elect_coordinator(&m(), |_| false), None);
    }
    #[test]
    fn speaker_candidates_are_alive_speakers() {
        assert_eq!(
            speaker_candidates(&m(), |p| p.0 % 2 == 0),
            vec![PeerId(2), PeerId(4)]
        );
    }
}
