//! Events surfaced to the UI (via FFI) and the local role summary consumed by the audio runtime.
use crate::ids::{Epoch, PeerId, RoomId};

#[derive(Clone, Debug, PartialEq)]
pub struct NearbyPeer {
    pub id: PeerId,
    pub name: String,
    pub connected: bool,
    pub in_my_room: bool,
    pub sas: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemberSnapshot {
    pub id: PeerId,
    pub name: String,
    pub is_local: bool,
    pub online: bool,
    pub mic_enabled: bool,
    pub is_coordinator: bool,
    pub is_speaker: bool,
    pub is_active_mic: bool,
    /// The member has the RoomMesh audio driver installed (without it, it can't coordinate).
    pub driver_installed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoomSnapshot {
    pub room_id: RoomId,
    pub name: String,
    pub epoch: Epoch,
    pub revision: u32,
    pub coordinator: PeerId,
    pub speaker: Option<PeerId>,
    /// The room's noise baseline (`None` = Automatic).
    pub noise_baseline_db: Option<f32>,
    pub active_primary: Option<PeerId>,
    pub active_secondary: Option<PeerId>,
    pub members: Vec<MemberSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionQuality {
    Excellent,
    Good,
    Degraded,
    Disconnected,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RoomEvent {
    NearbyChanged(Vec<NearbyPeer>),
    RoomChanged(Option<RoomSnapshot>),
    PeerJoined {
        peer: PeerId,
        name: String,
    },
    PeerLeft {
        peer: PeerId,
    },
    CoordinatorChanged {
        peer: PeerId,
        epoch: Epoch,
    },
    SpeakerChanged {
        peer: Option<PeerId>,
    },
    ActiveMicChanged {
        primary: Option<PeerId>,
        secondary: Option<PeerId>,
    },
    InviteReceived {
        room_id: RoomId,
        room_name: String,
        from: PeerId,
        from_name: String,
        sas: String,
    },
    InviteDeclined {
        peer: PeerId,
    },
    CoordinatorLost {
        candidates: Vec<PeerId>,
    },
    SpeakerLost {
        candidates: Vec<PeerId>,
    },
    ConnectionQualityChanged {
        peer: PeerId,
        quality: ConnectionQuality,
    },
    AecStatusChanged {
        converged: bool,
    },
    LeftRoom,
    Error {
        message: String,
    },
    /// Informational status for the user (e.g. a device recovered); not an error.
    Notice {
        message: String,
    },
}

/// What this Mac must do right now. Produced by the room engine, consumed by the audio runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalRoles {
    pub room_id: Option<RoomId>,
    pub epoch: Epoch,
    pub coordinator: Option<PeerId>,
    pub speaker: Option<PeerId>,
    pub is_coordinator: bool,
    pub is_speaker: bool,
    pub mic_enabled: bool,
    pub enabled_mics: Vec<PeerId>,
    /// The room's noise baseline (see `RoomManifest::noise_baseline_db`); `None` = Automatic,
    /// and always `None` outside a room.
    pub noise_baseline_db: Option<f32>,
    pub members: Vec<PeerId>,
}

impl LocalRoles {
    pub fn none() -> Self {
        Self {
            room_id: None,
            epoch: Epoch(0),
            coordinator: None,
            speaker: None,
            is_coordinator: false,
            is_speaker: false,
            mic_enabled: false,
            enabled_mics: vec![],
            noise_baseline_db: None,
            members: vec![],
        }
    }
}
