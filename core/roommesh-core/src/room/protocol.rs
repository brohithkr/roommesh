//! Reliable control-channel messages (postcard-encoded, then sealed by `network::secure`).
use crate::ids::{Epoch, PeerId, RoomId};
use crate::room::state::{MemberInfo, RoomManifest};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChangeRequest {
    SetCoordinator(PeerId),
    SetSpeaker(Option<PeerId>),
    SetMicEnabled {
        peer: PeerId,
        enabled: bool,
    },
    RemoveMember(PeerId),
    Rename(String),
    /// A member refreshing its own name/capabilities; the coordinator keeps `mic_enabled` as
    /// the manifest has it.
    UpdateMember(MemberInfo),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerReport {
    pub peer: PeerId,
    pub rtt_ms: f32,
    pub jitter_ms: f32,
    pub loss_pct: f32,
    pub clock_offset_ms: f32,
    pub drift_ppm: f32,
    pub transport: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    Invite {
        manifest: RoomManifest,
        from_name: String,
    },
    InviteResponse {
        room_id: RoomId,
        accepted: bool,
    },
    JoinRequest {
        room_id: RoomId,
        member: MemberInfo,
    },
    Manifest(RoomManifest),
    Request {
        room_id: RoomId,
        epoch: Epoch,
        change: ChangeRequest,
    },
    Leave {
        room_id: RoomId,
    },
    /// Sent every second to every member. The coordinator attaches the full manifest so
    /// members that missed an update (or a healed partition) converge. `sees_coordinator`
    /// tells the receiver whether the sender currently considers the coordinator (of `epoch`)
    /// alive — true when the sender is the coordinator — so a peer whose own link to the
    /// coordinator broke doesn't start an election while the rest of the room can reach it.
    Heartbeat {
        room_id: RoomId,
        epoch: Epoch,
        revision: u32,
        manifest: Option<RoomManifest>,
        sees_coordinator: bool,
    },
    ActiveMic {
        room_id: RoomId,
        epoch: Epoch,
        primary: Option<PeerId>,
        secondary: Option<PeerId>,
    },
    PeerReport(PeerReport),
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed control message: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("trailing bytes after control message")]
    TrailingBytes,
}

pub fn encode(msg: &ControlMessage) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("control message serialization cannot fail")
}
pub fn decode(bytes: &[u8]) -> Result<ControlMessage, ProtocolError> {
    let (msg, rest) = postcard::take_from_bytes(bytes)?;
    if !rest.is_empty() {
        return Err(ProtocolError::TrailingBytes);
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::state::*;
    #[test]
    fn roundtrip_all_variants() {
        let m = RoomManifest::new(
            RoomId(1),
            "R".into(),
            MemberInfo {
                id: PeerId(1),
                name: "A".into(),
                mic_enabled: true,
                capabilities: Capabilities::full(),
            },
        );
        let msgs = vec![
            ControlMessage::Invite {
                manifest: m.clone(),
                from_name: "A".into(),
            },
            ControlMessage::InviteResponse {
                room_id: RoomId(1),
                accepted: true,
            },
            ControlMessage::JoinRequest {
                room_id: RoomId(1),
                member: m.members[0].clone(),
            },
            ControlMessage::Manifest(m.clone()),
            ControlMessage::Request {
                room_id: RoomId(1),
                epoch: Epoch(1),
                change: ChangeRequest::SetSpeaker(Some(PeerId(2))),
            },
            ControlMessage::Leave { room_id: RoomId(1) },
            ControlMessage::Request {
                room_id: RoomId(1),
                epoch: Epoch(1),
                change: ChangeRequest::UpdateMember(m.members[0].clone()),
            },
            ControlMessage::Heartbeat {
                room_id: RoomId(1),
                epoch: Epoch(1),
                revision: 0,
                manifest: Some(m.clone()),
                sees_coordinator: true,
            },
            ControlMessage::Heartbeat {
                room_id: RoomId(1),
                epoch: Epoch(1),
                revision: 0,
                manifest: None,
                sees_coordinator: false,
            },
            ControlMessage::ActiveMic {
                room_id: RoomId(1),
                epoch: Epoch(1),
                primary: Some(PeerId(1)),
                secondary: None,
            },
            ControlMessage::PeerReport(PeerReport {
                peer: PeerId(1),
                rtt_ms: 3.0,
                jitter_ms: 1.0,
                loss_pct: 0.0,
                clock_offset_ms: 0.2,
                drift_ppm: 4.0,
                transport: "awdl0".into(),
            }),
        ];
        for msg in msgs {
            let bytes = encode(&msg);
            assert_eq!(decode(&bytes).unwrap(), msg);
        }
        assert!(decode(&[0xff, 0xff, 0xff]).is_err());
    }
    #[test]
    fn decode_rejects_trailing_bytes() {
        let msg = ControlMessage::Leave { room_id: RoomId(1) };
        let mut bytes = encode(&msg);
        bytes.push(0x42);
        assert!(matches!(decode(&bytes), Err(ProtocolError::TrailingBytes)));
    }
}
