//! UniFFI surface for the Swift app. Ids cross the boundary as 16-char lowercase hex strings.
use crate::audio::device_io::{list_devices, DeviceSelector};
use crate::dsp::vad::sanitize_baseline;
use crate::engine::facade::{Core, CoreConfig, EventSink};
use crate::engine::health::{AudioHealth, HealthCounts};
use crate::engine::meter::MicMeter;
use crate::engine::metrics::PeerMetrics;
use crate::engine::realtime::RtStatus;
use crate::engine::runtime::{AudioSettings, SystemAudio};
use crate::ids::{PeerId, RoomId};
use crate::network::transport::{LocalAdvertisement, PeerTransport, TransportEvent};
use crate::room::engine::{Command, RoomError};
use crate::room::events::{ConnectionQuality, NearbyPeer, RoomEvent, RoomSnapshot};
use crate::room::state::Capabilities;
use std::sync::Arc;

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum FfiError {
    #[error("This Mac is already in a room")]
    AlreadyInRoom,
    #[error("This Mac is not in a room")]
    NotInRoom,
    #[error("The invitation is no longer valid")]
    NoSuchInvite,
    #[error("That Mac is not a member of the room")]
    NotMember,
    /// The change speaks for another Mac (its name or capabilities), which only that Mac may
    /// make.
    #[error("Only that Mac can change this")]
    NotPermitted,
    #[error("Invalid peer id")]
    InvalidPeerId,
    /// The command was not applied (the core was busy, or it was called from inside an event
    /// or transport callback). Safe to retry.
    #[error("RoomMesh is busy — try again")]
    Timeout,
    /// The core stopped working (internal failure); the app must be restarted.
    #[error("RoomMesh stopped unexpectedly — restart the app")]
    Internal,
}

impl From<RoomError> for FfiError {
    fn from(e: RoomError) -> Self {
        match e {
            RoomError::AlreadyInRoom => Self::AlreadyInRoom,
            RoomError::NotInRoom => Self::NotInRoom,
            RoomError::NoSuchInvite => Self::NoSuchInvite,
            RoomError::NotMember => Self::NotMember,
            RoomError::NotPermitted => Self::NotPermitted,
            RoomError::Timeout => Self::Timeout,
            RoomError::Internal => Self::Internal,
        }
    }
}

pub fn parse_peer(s: &str) -> Result<PeerId, FfiError> {
    PeerId::from_hex(s).ok_or(FfiError::InvalidPeerId)
}
fn parse_room(s: &str) -> Result<RoomId, FfiError> {
    // An unparseable room id can only name an invite we never received.
    RoomId::from_hex(s).ok_or(FfiError::NoSuchInvite)
}
fn hex(p: PeerId) -> String {
    p.to_hex()
}
fn ohex(p: Option<PeerId>) -> Option<String> {
    p.map(hex)
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiNearbyPeer {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub in_my_room: bool,
    /// 6-digit short authentication string of the secure session, once established.
    pub sas: Option<String>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiMember {
    pub id: String,
    pub name: String,
    pub is_local: bool,
    pub online: bool,
    pub mic_enabled: bool,
    pub is_coordinator: bool,
    pub is_speaker: bool,
    pub is_active_mic: bool,
    /// The Mac has the RoomMesh audio driver installed; one without it can't coordinate.
    pub driver_installed: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiRoomState {
    pub room_id: String,
    pub name: String,
    pub epoch: u32,
    pub coordinator: String,
    pub speaker: Option<String>,
    /// The room's noise baseline in dBFS (after processing), applied to every Mac's mic:
    /// anything at or below it is never speech. `None` = Automatic.
    pub noise_baseline_db: Option<f32>,
    pub active_primary: Option<String>,
    pub active_secondary: Option<String>,
    pub members: Vec<FfiMember>,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiQuality {
    Excellent,
    Good,
    Degraded,
    Disconnected,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiPeerMetrics {
    pub peer_id: String,
    pub name: String,
    pub rtt_ms: Option<f32>,
    pub jitter_ms: Option<f32>,
    pub loss_pct: Option<f32>,
    pub clock_offset_ms: Option<f32>,
    pub drift_ppm: Option<f32>,
    pub buffer_ms: Option<f32>,
    pub mic_score: Option<f32>,
    pub speech_prob: Option<f32>,
    pub aec_erle_db: Option<f32>,
    pub aec_converged: bool,
    pub is_active: bool,
    pub transport: String,
    pub quality: FfiQuality,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiAudioDevice {
    pub name: String,
    pub is_input: bool,
    pub is_output: bool,
    pub is_default: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiSettings {
    /// `None` = system default device.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub allow_simultaneous_talkers: bool,
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    /// Clamped to 30..=300 ms.
    pub mic_latency_ms: u32,
    /// Clamped to 30..=300 ms.
    pub playout_delay_ms: u32,
    pub auto_elect_coordinator: bool,
    pub fallback_speaker_to_coordinator: bool,
    /// This Mac's noise baseline preference in dBFS (after processing; `None` = Automatic):
    /// the baseline a room this Mac creates starts with, and the one the mic meter uses
    /// outside a room. In a room, the room's baseline applies (`set_room_noise_baseline`).
    pub noise_baseline_db: Option<f32>,
}

/// A live reading of this Mac's mic as the room judges it (see `get_mic_meter`). dBFS.
#[derive(uniffi::Record, Clone, Copy, Debug, PartialEq)]
pub struct FfiMicMeter {
    /// Level after the room's processing (echo cancellation, noise suppression).
    pub level_db: f32,
    /// The effective noise floor: the automatic floor, raised to the baseline when one is set.
    pub floor_db: f32,
    /// `max(0, level_db - floor_db)`.
    pub perceived_db: f32,
    pub is_speech: bool,
    /// Recent peak level (decays at 20 dB/s).
    pub peak_db: f32,
    /// The floor Automatic would use.
    pub auto_floor_db: f32,
}

impl From<MicMeter> for FfiMicMeter {
    fn from(m: MicMeter) -> Self {
        Self {
            level_db: m.level_db,
            floor_db: m.floor_db,
            perceived_db: m.perceived_db,
            is_speech: m.is_speech,
            peak_db: m.peak_db,
            auto_floor_db: m.auto_floor_db,
        }
    }
}

/// How the audio thread is scheduled.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiRealtimeStatus {
    /// An ordinary thread: real-time scheduling was refused, or the system took it away.
    NormalPriority,
    /// Real-time (time-constraint) scheduling without an audio workgroup.
    TimeConstraintOnly,
    /// Real-time scheduling in an audio workgroup.
    WorkgroupAndTimeConstraint,
}

/// Mic audio one Mac's mic could not supply when the room mic needed it.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiMicMissing {
    pub peer_id: String,
    pub missing_ms: f32,
}

/// Where audio was lost over some span (see [`FfiAudioHealth`]).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiHealthCounts {
    /// Audio thread wakes counted.
    pub wakes: u64,
    /// How much later than asked the audio thread woke: the worst, and 99% of wakes within.
    pub wake_worst_ms: f32,
    pub wake_p99_ms: f32,
    /// Times the room speaker's output ran dry and played silence, and how much in total.
    pub speaker_underruns: u64,
    pub speaker_underrun_ms: f32,
    /// Meeting audio missing at its play time on the room speaker (gaps inside the stream).
    pub speaker_gap_ms: f32,
    /// Meeting audio frames (10 ms each) dropped because their play time had passed.
    pub late_meeting_frames: u64,
    /// Times the meeting audio stream from RoomMesh Speaker restarted (a discontinuity).
    pub meeting_restarts: u64,
    /// Room mic written as silence after the audio thread stalled.
    pub mic_silence_ms: f32,
    /// Per Mac, only those whose mic missed something (coordinator only).
    pub mic_missing: Vec<FfiMicMissing>,
}

/// The audio health counters (`get_audio_health`).
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct FfiAudioHealth {
    pub realtime: FfiRealtimeStatus,
    /// Times the system took real-time scheduling away from the audio thread.
    pub demotions: u32,
    /// Seconds `last_minute` covers (less than 60 right after audio started or a reset).
    pub window_secs: u32,
    pub last_minute: FfiHealthCounts,
    /// Since audio started or the counters were reset.
    pub since_start: FfiHealthCounts,
}

fn ms(ns: u64) -> f32 {
    (ns as f64 / 1e6) as f32
}

fn health_counts(c: HealthCounts) -> FfiHealthCounts {
    FfiHealthCounts {
        wakes: c.wakes,
        wake_worst_ms: ms(c.wake_worst_ns),
        wake_p99_ms: ms(c.wake_p99_ns),
        speaker_underruns: c.speaker_underruns,
        speaker_underrun_ms: ms(c.speaker_underrun_ns),
        speaker_gap_ms: ms(c.speaker_missing_ns),
        late_meeting_frames: c.farend_late_frames,
        meeting_restarts: c.farend_resets,
        mic_silence_ms: ms(c.mic_silence_slots * crate::audio::frames::FRAME_NS),
        mic_missing: c
            .mic_missing_ns
            .into_iter()
            .map(|(p, ns)| FfiMicMissing {
                peer_id: hex(p),
                missing_ms: ms(ns),
            })
            .collect(),
    }
}

pub fn audio_health(h: AudioHealth) -> FfiAudioHealth {
    FfiAudioHealth {
        realtime: match h.realtime {
            RtStatus::NormalPriority => FfiRealtimeStatus::NormalPriority,
            RtStatus::TimeConstraintOnly => FfiRealtimeStatus::TimeConstraintOnly,
            RtStatus::WorkgroupAndTimeConstraint => FfiRealtimeStatus::WorkgroupAndTimeConstraint,
        },
        demotions: h.demotions,
        window_secs: h.window_secs,
        last_minute: health_counts(h.last_minute),
        since_start: health_counts(h.since_start),
    }
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq)]
pub enum FfiEvent {
    NearbyChanged {
        peers: Vec<FfiNearbyPeer>,
    },
    RoomChanged {
        state: Option<FfiRoomState>,
    },
    PeerJoined {
        peer_id: String,
        name: String,
    },
    PeerLeft {
        peer_id: String,
    },
    CoordinatorChanged {
        peer_id: String,
        epoch: u32,
    },
    SpeakerChanged {
        peer_id: Option<String>,
    },
    ActiveMicChanged {
        primary: Option<String>,
        secondary: Option<String>,
    },
    InviteReceived {
        room_id: String,
        room_name: String,
        from_peer: String,
        from_name: String,
        sas: String,
    },
    InviteDeclined {
        peer_id: String,
    },
    CoordinatorLost {
        candidates: Vec<String>,
    },
    SpeakerLost {
        candidates: Vec<String>,
    },
    ConnectionQualityChanged {
        peer_id: String,
        quality: FfiQuality,
    },
    AecStatusChanged {
        converged: bool,
    },
    LeftRoom,
    Error {
        message: String,
    },
    /// Informational status (e.g. "Microphone recovered"); show it unobtrusively, not as an error.
    Notice {
        message: String,
    },
}

fn quality(x: ConnectionQuality) -> FfiQuality {
    match x {
        ConnectionQuality::Excellent => FfiQuality::Excellent,
        ConnectionQuality::Good => FfiQuality::Good,
        ConnectionQuality::Degraded => FfiQuality::Degraded,
        ConnectionQuality::Disconnected => FfiQuality::Disconnected,
    }
}

fn nearby(n: NearbyPeer) -> FfiNearbyPeer {
    FfiNearbyPeer {
        id: hex(n.id),
        name: n.name,
        connected: n.connected,
        in_my_room: n.in_my_room,
        sas: n.sas,
    }
}

fn room(s: RoomSnapshot) -> FfiRoomState {
    FfiRoomState {
        room_id: s.room_id.to_hex(),
        name: s.name,
        epoch: s.epoch.0,
        coordinator: hex(s.coordinator),
        speaker: ohex(s.speaker),
        noise_baseline_db: s.noise_baseline_db,
        active_primary: ohex(s.active_primary),
        active_secondary: ohex(s.active_secondary),
        members: s
            .members
            .into_iter()
            .map(|m| FfiMember {
                id: hex(m.id),
                name: m.name,
                is_local: m.is_local,
                online: m.online,
                mic_enabled: m.mic_enabled,
                is_coordinator: m.is_coordinator,
                is_speaker: m.is_speaker,
                is_active_mic: m.is_active_mic,
                driver_installed: m.driver_installed,
            })
            .collect(),
    }
}

fn metrics(m: PeerMetrics) -> FfiPeerMetrics {
    FfiPeerMetrics {
        peer_id: hex(m.peer),
        name: m.name,
        rtt_ms: m.rtt_ms,
        jitter_ms: m.jitter_ms,
        loss_pct: m.loss_pct,
        clock_offset_ms: m.clock_offset_ms,
        drift_ppm: m.drift_ppm,
        buffer_ms: m.buffer_ms,
        mic_score: m.mic_score,
        speech_prob: m.speech_prob,
        aec_erle_db: m.aec_erle_db,
        aec_converged: m.aec_converged,
        is_active: m.is_active,
        transport: m.transport,
        quality: quality(m.quality),
    }
}

fn hexes(v: Vec<PeerId>) -> Vec<String> {
    v.into_iter().map(hex).collect()
}

pub fn to_ffi_event(e: RoomEvent) -> FfiEvent {
    match e {
        RoomEvent::NearbyChanged(v) => FfiEvent::NearbyChanged {
            peers: v.into_iter().map(nearby).collect(),
        },
        RoomEvent::RoomChanged(s) => FfiEvent::RoomChanged { state: s.map(room) },
        RoomEvent::PeerJoined { peer, name } => FfiEvent::PeerJoined {
            peer_id: hex(peer),
            name,
        },
        RoomEvent::PeerLeft { peer } => FfiEvent::PeerLeft { peer_id: hex(peer) },
        RoomEvent::CoordinatorChanged { peer, epoch } => FfiEvent::CoordinatorChanged {
            peer_id: hex(peer),
            epoch: epoch.0,
        },
        RoomEvent::SpeakerChanged { peer } => FfiEvent::SpeakerChanged {
            peer_id: ohex(peer),
        },
        RoomEvent::ActiveMicChanged { primary, secondary } => FfiEvent::ActiveMicChanged {
            primary: ohex(primary),
            secondary: ohex(secondary),
        },
        RoomEvent::InviteReceived {
            room_id,
            room_name,
            from,
            from_name,
            sas,
        } => FfiEvent::InviteReceived {
            room_id: room_id.to_hex(),
            room_name,
            from_peer: hex(from),
            from_name,
            sas,
        },
        RoomEvent::InviteDeclined { peer } => FfiEvent::InviteDeclined { peer_id: hex(peer) },
        RoomEvent::CoordinatorLost { candidates } => FfiEvent::CoordinatorLost {
            candidates: hexes(candidates),
        },
        RoomEvent::SpeakerLost { candidates } => FfiEvent::SpeakerLost {
            candidates: hexes(candidates),
        },
        RoomEvent::ConnectionQualityChanged { peer, quality: q } => {
            FfiEvent::ConnectionQualityChanged {
                peer_id: hex(peer),
                quality: quality(q),
            }
        }
        RoomEvent::AecStatusChanged { converged } => FfiEvent::AecStatusChanged { converged },
        RoomEvent::LeftRoom => FfiEvent::LeftRoom,
        RoomEvent::Error { message } => FfiEvent::Error { message },
        RoomEvent::Notice { message } => FfiEvent::Notice { message },
    }
}

fn selector(name: &Option<String>) -> DeviceSelector {
    match name {
        Some(n) if !n.is_empty() => DeviceSelector::Name(n.clone()),
        _ => DeviceSelector::Default,
    }
}

/// Maps the app's settings onto the audio runtime configuration (everything not exposed in the
/// UI keeps the runtime defaults).
pub fn audio_settings(s: &FfiSettings) -> AudioSettings {
    let mut a = AudioSettings {
        input: selector(&s.input_device),
        output: selector(&s.output_device),
        noise_baseline_db: sanitize_baseline(s.noise_baseline_db),
        ..Default::default()
    };
    a.coordinator.arbitration.allow_multi = s.allow_simultaneous_talkers;
    a.coordinator.use_webrtc_aec = s.echo_cancellation;
    a.coordinator.noise_suppression = s.noise_suppression;
    a.coordinator.mic_latency_ns = u64::from(s.mic_latency_ms.clamp(30, 300)) * 1_000_000;
    a.coordinator.playout_delay_ns = u64::from(s.playout_delay_ms.clamp(30, 300)) * 1_000_000;
    a
}

fn capabilities(driver_installed: bool) -> Capabilities {
    Capabilities {
        driver_installed,
        ..Capabilities::full()
    }
}

/// Implemented in Swift by `AppleP2PTransport` (Network.framework, peer-to-peer enabled).
///
/// Threading contract (violations deadlock or stall audio):
/// - Calls arrive on Rust threads (the control and DSP threads among them). Implementations
///   must be thread-safe and must never block: enqueue the work (`queue.async`) and return.
/// - Never call a Result-returning `RoomMeshCore` command synchronously from a callback; such a
///   call from the core's control thread fails at once with `FfiError.Timeout`.
/// - `send_realtime` is also called re-entrantly from inside `on_realtime_packet` (clock pongs
///   are answered on the thread delivering the ping), i.e. on your own delivery queue: never
///   `queue.sync` onto that queue (or any queue you might be called on).
/// - `start()` and `stop()` run on the thread that called `RoomMeshCore.start()` / `.stop()`.
/// - Dropping the last `RoomMeshCore` reference stops the control thread and joins the DSP
///   thread; don't release it from a transport callback.
#[uniffi::export(foreign)]
pub trait FfiTransport: Send + Sync {
    fn start(&self, peer_id: String, name: String, protocol_version: u16);
    fn stop(&self);
    fn connect(&self, peer_id: String);
    fn disconnect(&self, peer_id: String);
    fn send_control(&self, peer_id: String, frame: Vec<u8>);
    fn send_realtime(&self, peer_id: String, packet: Vec<u8>);
    /// Human readable transport/interface currently in use (e.g. "awdl0", "en0").
    fn description(&self) -> String;
}

/// Implemented in Swift. Events are delivered in order on the core's control thread.
///
/// Threading contract: `on_event` must never block — hand the event to a serial queue / the
/// main actor and return — and must never call a Result-returning `RoomMeshCore` command
/// synchronously (it would fail at once with `FfiError.Timeout`: the control thread that runs
/// commands is the one delivering the event). Query methods (`get_room_state`, ...) are fine.
/// Never `queue.sync` onto a queue that may itself be waiting on the core.
#[uniffi::export(foreign)]
pub trait FfiEventListener: Send + Sync {
    fn on_event(&self, event: FfiEvent);
}

struct TransportAdapter(Arc<dyn FfiTransport>);
impl PeerTransport for TransportAdapter {
    fn start(&self, a: LocalAdvertisement) {
        self.0.start(a.peer_id.to_hex(), a.name, a.protocol_version);
    }
    fn stop(&self) {
        self.0.stop();
    }
    fn connect(&self, p: PeerId) {
        self.0.connect(p.to_hex());
    }
    fn disconnect(&self, p: PeerId) {
        self.0.disconnect(p.to_hex());
    }
    fn send_control(&self, p: PeerId, f: Vec<u8>) {
        self.0.send_control(p.to_hex(), f);
    }
    fn send_realtime(&self, p: PeerId, pkt: Vec<u8>) {
        self.0.send_realtime(p.to_hex(), pkt);
    }
    fn description(&self) -> String {
        self.0.description()
    }
}

struct EventAdapter(Arc<dyn FfiEventListener>);
impl EventSink for EventAdapter {
    fn on_event(&self, e: RoomEvent) {
        self.0.on_event(to_ffi_event(e));
    }
}

/// The core. Dropping the last reference stops its control thread and joins its DSP thread
/// (see [`FfiTransport`] and [`FfiEventListener`] for the callback contracts).
#[derive(uniffi::Object)]
pub struct RoomMeshCore {
    core: Core,
}

#[uniffi::export]
impl RoomMeshCore {
    #[uniffi::constructor]
    pub fn new(
        peer_id: String,
        name: String,
        driver_installed: bool,
        transport: Arc<dyn FfiTransport>,
        listener: Arc<dyn FfiEventListener>,
        settings: FfiSettings,
    ) -> Result<Arc<Self>, FfiError> {
        let mut cfg = CoreConfig::new(parse_peer(&peer_id)?, name);
        cfg.capabilities = capabilities(driver_installed);
        cfg.audio = audio_settings(&settings);
        cfg.auto_elect = settings.auto_elect_coordinator;
        cfg.fallback_speaker = settings.fallback_speaker_to_coordinator;
        cfg.noise_baseline_db = settings.noise_baseline_db;
        let core = Core::new(
            cfg,
            Arc::new(TransportAdapter(transport)),
            Arc::new(EventAdapter(listener)),
            Box::new(SystemAudio::new()),
        );
        Ok(Arc::new(Self { core }))
    }

    pub fn start(&self) {
        self.core.start();
    }
    pub fn stop(&self) {
        self.core.stop();
    }
    pub fn local_peer_id(&self) -> String {
        self.core.local_peer().to_hex()
    }

    // ---- transport -> core (malformed peer ids are dropped) ----
    pub fn on_peer_discovered(&self, peer_id: String, name: String) {
        if let Ok(peer) = parse_peer(&peer_id) {
            self.core
                .handle_transport_event(TransportEvent::Discovered { peer, name });
        }
    }
    pub fn on_peer_lost(&self, peer_id: String) {
        if let Ok(p) = parse_peer(&peer_id) {
            self.core.handle_transport_event(TransportEvent::Lost(p));
        }
    }
    pub fn on_connected(&self, peer_id: String) {
        if let Ok(p) = parse_peer(&peer_id) {
            self.core
                .handle_transport_event(TransportEvent::Connected(p));
        }
    }
    pub fn on_disconnected(&self, peer_id: String) {
        if let Ok(p) = parse_peer(&peer_id) {
            self.core
                .handle_transport_event(TransportEvent::Disconnected(p));
        }
    }
    pub fn on_control_frame(&self, peer_id: String, frame: Vec<u8>) {
        if let Ok(peer) = parse_peer(&peer_id) {
            self.core
                .handle_transport_event(TransportEvent::Control { peer, frame });
        }
    }
    pub fn on_realtime_packet(&self, packet: Vec<u8>) {
        self.core.on_realtime(&packet);
    }

    // ---- room commands ----
    pub fn create_room(&self, name: String) -> Result<(), FfiError> {
        Ok(self.core.command(Command::CreateRoom { name })?)
    }
    pub fn invite(&self, peer_id: String) -> Result<(), FfiError> {
        Ok(self.core.command(Command::Invite(parse_peer(&peer_id)?))?)
    }
    pub fn respond_to_invite(&self, room_id: String, accept: bool) -> Result<(), FfiError> {
        Ok(self.core.command(Command::RespondToInvite {
            room_id: parse_room(&room_id)?,
            accept,
        })?)
    }
    pub fn leave_room(&self) -> Result<(), FfiError> {
        Ok(self.core.command(Command::Leave)?)
    }
    pub fn set_coordinator(&self, peer_id: String) -> Result<(), FfiError> {
        Ok(self
            .core
            .command(Command::SetCoordinator(parse_peer(&peer_id)?))?)
    }
    pub fn set_speaker(&self, peer_id: Option<String>) -> Result<(), FfiError> {
        let p = peer_id.map(|s| parse_peer(&s)).transpose()?;
        Ok(self.core.command(Command::SetSpeaker(p))?)
    }
    pub fn set_peer_microphone_enabled(
        &self,
        peer_id: String,
        enabled: bool,
    ) -> Result<(), FfiError> {
        Ok(self.core.command(Command::SetMicEnabled {
            peer: parse_peer(&peer_id)?,
            enabled,
        })?)
    }
    pub fn remove_member(&self, peer_id: String) -> Result<(), FfiError> {
        Ok(self
            .core
            .command(Command::RemoveMember(parse_peer(&peer_id)?))?)
    }
    pub fn rename_room(&self, name: String) -> Result<(), FfiError> {
        Ok(self.core.command(Command::Rename(name))?)
    }
    /// Sets the room's noise baseline for every Mac in it (`None` = Automatic). Any member may.
    /// Clamped to -100..=0 dBFS; a non-finite value is ignored.
    pub fn set_room_noise_baseline(&self, baseline_db: Option<f32>) -> Result<(), FfiError> {
        Ok(self.core.command(Command::SetNoiseBaseline(baseline_db))?)
    }

    // ---- local controls ----
    pub fn set_local_mute(&self, muted: bool) {
        self.core.set_muted(muted);
    }
    pub fn is_local_muted(&self) -> bool {
        self.core.is_muted()
    }
    pub fn start_audio_engine(&self) {
        self.core.start_audio();
    }
    pub fn stop_audio_engine(&self) {
        self.core.stop_audio();
    }
    pub fn update_settings(&self, settings: FfiSettings) {
        self.core.update_audio_settings(audio_settings(&settings));
        self.core.set_policies(
            settings.auto_elect_coordinator,
            settings.fallback_speaker_to_coordinator,
        );
        self.core.set_noise_baseline(settings.noise_baseline_db);
    }
    /// Runs the live mic meter for the Settings UI; turn it off when the UI goes away (it costs
    /// an extra echo canceller). While on, the mic is captured even outside a room.
    pub fn set_mic_meter_enabled(&self, enabled: bool) {
        self.core.set_mic_meter_enabled(enabled);
    }
    /// The meter's latest reading (updated every 10 ms): `None` while the meter is off or
    /// before the mic delivers audio.
    pub fn get_mic_meter(&self) -> Option<FfiMicMeter> {
        self.core.mic_meter().map(FfiMicMeter::from)
    }
    /// Where audio was lost over the last minute and since audio started, and how the audio
    /// thread is scheduled. Updated once a second.
    pub fn get_audio_health(&self) -> FfiAudioHealth {
        audio_health(self.core.audio_health())
    }
    /// Clears the audio health counters.
    pub fn reset_audio_health(&self) {
        self.core.reset_audio_health();
    }
    pub fn set_local_info(&self, name: String, driver_installed: bool) {
        self.core
            .set_local_info(name, capabilities(driver_installed));
    }

    // ---- queries ----
    pub fn get_room_state(&self) -> Option<FfiRoomState> {
        self.core.room_snapshot().map(room)
    }
    pub fn get_nearby_peers(&self) -> Vec<FfiNearbyPeer> {
        self.core.nearby().into_iter().map(nearby).collect()
    }
    pub fn get_peer_metrics(&self) -> Vec<FfiPeerMetrics> {
        self.core.metrics().into_iter().map(metrics).collect()
    }
    pub fn get_active_microphones(&self) -> Vec<String> {
        self.core
            .room_snapshot()
            .map(|s| {
                [s.active_primary, s.active_secondary]
                    .into_iter()
                    .flatten()
                    .map(hex)
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn virtual_device_available(&self) -> bool {
        self.core.virtual_device_ok()
    }
}

#[uniffi::export]
pub fn list_audio_devices() -> Vec<FfiAudioDevice> {
    list_devices()
        .into_iter()
        .map(|d| FfiAudioDevice {
            name: d.name,
            is_input: d.is_input,
            is_output: d.is_output,
            is_default: d.is_default,
        })
        .collect()
}

#[uniffi::export]
pub fn generate_peer_id() -> String {
    PeerId::random().to_hex()
}

#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::events::*;
    #[test]
    fn settings_map_to_audio_config() {
        let s = FfiSettings {
            input_device: Some("MacBook Pro Microphone".into()),
            output_device: None,
            allow_simultaneous_talkers: true,
            echo_cancellation: false,
            noise_suppression: true,
            mic_latency_ms: 70,
            playout_delay_ms: 90,
            auto_elect_coordinator: false,
            fallback_speaker_to_coordinator: true,
            noise_baseline_db: None,
        };
        let a = audio_settings(&s);
        assert_eq!(
            a.input,
            DeviceSelector::Name("MacBook Pro Microphone".into())
        );
        assert_eq!(a.output, DeviceSelector::Default);
        assert!(a.coordinator.arbitration.allow_multi);
        assert!(!a.coordinator.use_webrtc_aec);
        assert_eq!(a.noise_baseline_db, None);
        assert_eq!(a.coordinator.mic_latency_ns, 70_000_000);
        assert_eq!(a.coordinator.playout_delay_ns, 90_000_000);
    }
    #[test]
    fn audio_health_maps_to_ms_and_hex_ids() {
        let counts = HealthCounts {
            wakes: 30_000,
            wake_worst_ns: 1_800_000,
            wake_p99_ns: 400_000,
            speaker_underruns: 2,
            speaker_underrun_ns: 12_500_000,
            speaker_missing_ns: 20_000_000,
            farend_late_frames: 3,
            farend_resets: 1,
            mic_silence_slots: 4,
            mic_missing_ns: vec![(PeerId(0xab), 30_000_000)],
        };
        let h = audio_health(AudioHealth {
            realtime: RtStatus::WorkgroupAndTimeConstraint,
            demotions: 1,
            window_secs: 60,
            last_minute: counts.clone(),
            since_start: HealthCounts::default(),
        });
        assert_eq!(h.realtime, FfiRealtimeStatus::WorkgroupAndTimeConstraint);
        assert_eq!((h.demotions, h.window_secs), (1, 60));
        let m = &h.last_minute;
        assert_eq!(m.wakes, 30_000);
        assert_eq!(m.wake_worst_ms, 1.8);
        assert_eq!(m.wake_p99_ms, 0.4);
        assert_eq!((m.speaker_underruns, m.speaker_underrun_ms), (2, 12.5));
        assert_eq!(m.speaker_gap_ms, 20.0);
        assert_eq!((m.late_meeting_frames, m.meeting_restarts), (3, 1));
        assert_eq!(m.mic_silence_ms, 40.0);
        assert_eq!(
            m.mic_missing,
            vec![FfiMicMissing {
                peer_id: "00000000000000ab".into(),
                missing_ms: 30.0
            }]
        );
        assert_eq!(h.since_start.wakes, 0);
        assert!(h.since_start.mic_missing.is_empty());
        for (rt, ffi) in [
            (RtStatus::NormalPriority, FfiRealtimeStatus::NormalPriority),
            (
                RtStatus::TimeConstraintOnly,
                FfiRealtimeStatus::TimeConstraintOnly,
            ),
        ] {
            let h = audio_health(AudioHealth {
                realtime: rt,
                ..Default::default()
            });
            assert_eq!(h.realtime, ffi);
        }
    }

    #[test]
    fn events_convert_with_hex_ids() {
        let e = to_ffi_event(RoomEvent::ActiveMicChanged {
            primary: Some(PeerId(0xab)),
            secondary: None,
        });
        match e {
            FfiEvent::ActiveMicChanged { primary, secondary } => {
                assert_eq!(primary.as_deref(), Some("00000000000000ab"));
                assert!(secondary.is_none());
            }
            _ => panic!(),
        }
        assert!(matches!(
            to_ffi_event(RoomEvent::LeftRoom),
            FfiEvent::LeftRoom
        ));
        assert_eq!(
            to_ffi_event(RoomEvent::Notice {
                message: "Microphone recovered".into()
            }),
            FfiEvent::Notice {
                message: "Microphone recovered".into()
            }
        );
        assert!(parse_peer("nothex").is_err());
    }
    #[test]
    fn settings_clamp_and_errors_map() {
        let s = FfiSettings {
            input_device: Some(String::new()),
            output_device: Some("Studio Display".into()),
            allow_simultaneous_talkers: false,
            echo_cancellation: true,
            noise_suppression: false,
            mic_latency_ms: 5,
            playout_delay_ms: 10_000,
            auto_elect_coordinator: true,
            fallback_speaker_to_coordinator: false,
            noise_baseline_db: Some(-55.0),
        };
        let a = audio_settings(&s);
        assert_eq!(a.input, DeviceSelector::Default);
        assert_eq!(a.output, DeviceSelector::Name("Studio Display".into()));
        assert_eq!(a.coordinator.mic_latency_ns, 30_000_000);
        assert_eq!(a.coordinator.playout_delay_ns, 300_000_000);
        assert!(!a.coordinator.noise_suppression);
        assert_eq!(a.noise_baseline_db, Some(-55.0));
        let nan = FfiSettings {
            noise_baseline_db: Some(f32::NAN),
            ..s.clone()
        };
        assert_eq!(audio_settings(&nan).noise_baseline_db, None);
        assert!(matches!(
            FfiError::from(RoomError::NotMember),
            FfiError::NotMember
        ));
        assert!(matches!(
            FfiError::from(RoomError::NotPermitted),
            FfiError::NotPermitted
        ));
        assert!(matches!(
            FfiError::from(RoomError::Timeout),
            FfiError::Timeout
        ));
        assert_eq!(
            FfiError::Timeout.to_string(),
            "RoomMesh is busy — try again"
        );
        assert!(matches!(
            FfiError::from(RoomError::Internal),
            FfiError::Internal
        ));
        assert_eq!(
            FfiError::Internal.to_string(),
            "RoomMesh stopped unexpectedly — restart the app"
        );
        let member = |id, driver_installed| crate::room::events::MemberSnapshot {
            id: PeerId(id),
            name: format!("Mac {id}"),
            is_local: id == 1,
            online: true,
            mic_enabled: true,
            is_coordinator: id == 1,
            is_speaker: false,
            is_active_mic: false,
            driver_installed,
        };
        let r = room(RoomSnapshot {
            room_id: RoomId(9),
            name: "R".into(),
            epoch: crate::ids::Epoch(1),
            revision: 0,
            coordinator: PeerId(1),
            speaker: None,
            noise_baseline_db: Some(-44.5),
            active_primary: None,
            active_secondary: None,
            members: vec![member(1, true), member(2, false)],
        });
        assert_eq!(r.noise_baseline_db, Some(-44.5));
        assert_eq!(
            r.members
                .iter()
                .map(|m| m.driver_installed)
                .collect::<Vec<_>>(),
            vec![true, false]
        );
        assert!(parse_peer("00000000000000AB").is_err());
        assert_eq!(parse_peer("00000000000000ab").unwrap(), PeerId(0xab));
        let e = to_ffi_event(RoomEvent::CoordinatorLost {
            candidates: vec![PeerId(1), PeerId(2)],
        });
        assert_eq!(
            e,
            FfiEvent::CoordinatorLost {
                candidates: vec!["0000000000000001".into(), "0000000000000002".into()]
            }
        );
    }
}
