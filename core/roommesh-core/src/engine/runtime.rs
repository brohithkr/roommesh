//! The audio runtime: owns devices and pipelines on one DSP thread and follows `LocalRoles`.
//!
//! One thread, woken every 2 ms or by a message. Per step: virtual-device monitor → device
//! reconcile/watchdog → clock-sync pings (non-coordinators) → drain capture → uplink or local
//! mic → (coordinator) far-end in from the speaker ring, schedule to the room speaker, produce
//! mic frames on a host-time 10 ms grid into the mic ring → (speaker) keep the physical output
//! queue topped up → metrics every 250 ms.
//!
//! The DSP thread never waits on device opens (they are requested and polled), never blocks on
//! the metrics book (`try_lock`), and only takes the short-held sessions read lock.
//!
//! Realtime sequence numbers: the ChaCha20-Poly1305 nonce of a realtime packet is
//! `(kind, epoch, stream, sequence)` under the per-connection key, so a sequence number must
//! never repeat for the same (kind, stream) within one session + epoch. Every realtime packet
//! this thread sends (clock pings, mic uplink, far-end playback) takes its sequence number from
//! a counter owned by [`Dsp`] that only ever increases for the lifetime of the runtime; pipeline
//! objects (`MicUplink`, `CoordinatorPipeline`) may be recreated freely (role changes, settings
//! changes, audio disabled/enabled within one epoch) without restarting the numbering.
use crate::audio::device_clock::DeviceClock;
use crate::audio::device_io::{
    CaptureHandle, DeviceError, DeviceHost, DeviceSelector, PlaybackClock, PlaybackHandle,
    CAPTURE_STREAM_ERROR, PLAYBACK_STREAM_ERROR,
};
use crate::audio::frames::{FRAME_NS, NS_PER_SAMPLE, SAMPLE_RATE};
use crate::audio::shared_layout::SHM_NAME;
use crate::audio::virtual_device::{MicWriter, SharedRegion, SpeakerReader, VirtualDeviceError};
use crate::dsp::arbitration::Selection;
use crate::engine::coordinator::{CoordinatorConfig, CoordinatorPipeline};
use crate::engine::metrics::{stream_jitter_ms, stream_loss_pct, MetricsBook};
use crate::engine::speaker::SpeakerPipeline;
use crate::engine::uplink::{FrameAssembler, MicUplink};
use crate::ids::{Epoch, PeerId, StreamId};
use crate::network::clock_sync::{ClockEstimator, ClockSample};
use crate::network::control::RtSessions;
use crate::network::realtime::{encode_times, PacketKind, RtHeader};
use crate::network::transport::PeerTransport;
use crate::room::events::LocalRoles;
use crate::room::protocol::PeerReport;
use crate::time::now_ns;
use crossbeam_channel::{bounded, unbounded, Receiver, Select, Sender, TryRecvError};
use parking_lot::Mutex;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(2);
const METRICS_INTERVAL_NS: u64 = 250_000_000;
/// How often the virtual-device monitor looks at the driver region.
const VDEV_CHECK_NS: u64 = 1_000_000_000;
/// Backoff between attempts to open a missing driver region.
const VDEV_RETRY_NS: u64 = 2_000_000_000;
/// Backoff between attempts to open a wanted-but-missing physical device.
const DEVICE_RETRY_NS: u64 = 3_000_000_000;
/// A requested open that has not completed by then is abandoned (and retried).
const DEVICE_OPEN_TIMEOUT_NS: u64 = 5_000_000_000;
/// A running device that delivers no capture blocks / playback reports for this long is
/// considered dead and restarted…
const DEVICE_STALL_NS: u64 = 1_000_000_000;
/// …except right after opening: some devices take seconds to deliver their first callback.
const DEVICE_FIRST_ACTIVITY_NS: u64 = 5_000_000_000;
/// Delay before reopening a stalled device, doubling per consecutive stall up to the max (reset
/// once the device has been delivering for that long).
const DEVICE_RESTART_BACKOFF_MIN_NS: u64 = 1_000_000_000;
const DEVICE_RESTART_BACKOFF_MAX_NS: u64 = 10_000_000_000;
/// Every frame dropped as too old for this long: the input latency alone exceeds the mic budget.
const MIC_OVER_BUDGET_NS: u64 = 1_000_000_000;
const MIC_OVER_BUDGET_MSG: &str =
    "Microphone latency exceeds the room mic budget; raise Mic latency in Advanced settings";
/// Mic-grid lag beyond which missed 10 ms slots are skipped (written as silence) instead of
/// being run through the pipeline.
///
/// The driver serves "RoomMesh Microphone" from a read head anchored to its own sample clock a
/// fixed distance behind our write edge, so the mic ring's write position must advance 1:1
/// with host time. After a DSP stall the missed slots' audio is already too late to be useful,
/// and producing it all in a burst would only prolong the stall; instead the missed slots are
/// written as zeros, keeping the write edge on time. (Leaving `write_pos` behind time would put
/// the driver's reader permanently ahead of the edge and zero-fill most cycles; bursting ahead
/// of time would ratchet up the meeting's mic latency.) The threshold only needs to absorb
/// normal wake-up jitter of the 2 ms loop.
const MAX_GRID_LAG_NS: u64 = 25_000_000;
/// Longest stall that is zero-filled slot by slot. Beyond the driver's 200 ms heartbeat window
/// it has gone silent and re-anchors to the write edge anyway.
const MAX_ZERO_FILL_SLOTS: u64 = 100;
const SILENT_FRAME: [f32; crate::audio::frames::FRAME_SAMPLES] =
    [0.0; crate::audio::frames::FRAME_SAMPLES];
/// Minimum audio queued on the physical room-speaker output…
const PLAYBACK_QUEUE_MIN_NS: u64 = 30_000_000;
/// …and the margin kept above one device callback's worth.
const PLAYBACK_QUEUE_MARGIN_NS: u64 = 15_000_000;
/// Clock-ping interval while unsynced / once synced.
const PING_FAST_NS: u64 = 50_000_000;
const PING_SLOW_NS: u64 = 250_000_000;
/// Outstanding clock pings remembered for pong matching (replay protection), and how long.
const MAX_OUTSTANDING_PINGS: usize = 32;
const PING_TTL_NS: u64 = 1_000_000_000;
/// Capacity of the lossy realtime-packet queue into the DSP thread.
const PACKET_QUEUE: usize = 4096;
/// Realtime packets handled per wake before the next `step()`, so a flood can't starve the
/// audio cadence (the rest wait for the next wake, which comes at once).
const MAX_PACKETS_PER_WAKE: usize = 256;
/// Off the coordinator, a captured frame older than this is not worth uplinking. Fixed and
/// generous (the largest mic budget a coordinator may use): a member doesn't know the
/// coordinator's actual budget, which judges lateness itself.
const MEMBER_UPLINK_BUDGET_NS: u64 = 300_000_000;
/// A far-end chunk whose time disagrees with the previous chunk's end by more than this is a
/// discontinuity (playback paused/restarted, reader resynced).
const FAREND_JUMP_NS: u64 = 20_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    pub input: DeviceSelector,
    pub output: DeviceSelector,
    pub coordinator: CoordinatorConfig,
    pub mic_bitrate_bps: i32,
}
impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input: DeviceSelector::Default,
            output: DeviceSelector::Default,
            coordinator: CoordinatorConfig::default(),
            mic_bitrate_bps: 48_000,
        }
    }
}

/// A device-open result delivered later (or immediately, for synchronous backends).
pub type PendingOpen<H> = Receiver<Result<H, DeviceError>>;

fn ready<H>(r: Result<H, DeviceError>) -> PendingOpen<H> {
    let (tx, rx) = bounded(1);
    let _ = tx.send(r);
    rx
}

/// Device access, swappable for tests.
pub trait AudioBackend: Send {
    fn start_capture(&mut self, sel: &DeviceSelector) -> Result<CaptureHandle, DeviceError>;
    fn stop_capture(&mut self);
    fn start_playback(&mut self, sel: &DeviceSelector) -> Result<PlaybackHandle, DeviceError>;
    fn stop_playback(&mut self);
    fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError>;
    /// Starts opening the capture device without blocking the caller. A later `stop_capture`
    /// or request supersedes it. Default: synchronous [`start_capture`](Self::start_capture).
    fn request_capture(&mut self, sel: &DeviceSelector) -> PendingOpen<CaptureHandle> {
        ready(self.start_capture(sel))
    }
    /// Non-blocking counterpart of [`start_playback`](Self::start_playback).
    fn request_playback(&mut self, sel: &DeviceSelector) -> PendingOpen<PlaybackHandle> {
        ready(self.start_playback(sel))
    }
    /// Stream errors since the last call; messages start with `CAPTURE_STREAM_ERROR` or
    /// `PLAYBACK_STREAM_ERROR` when they concern one stream.
    fn take_device_errors(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// Real devices: cpal (via [`DeviceHost`]) and the driver's shared memory.
pub struct SystemAudio {
    host: DeviceHost,
}
impl SystemAudio {
    pub fn new() -> Self {
        Self {
            host: DeviceHost::spawn(),
        }
    }
}
impl Default for SystemAudio {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioBackend for SystemAudio {
    fn start_capture(&mut self, sel: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
        self.host.start_capture(sel.clone())
    }
    fn stop_capture(&mut self) {
        self.host.stop_capture();
    }
    fn start_playback(&mut self, sel: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
        self.host.start_playback(sel.clone())
    }
    fn stop_playback(&mut self) {
        self.host.stop_playback();
    }
    fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
        SharedRegion::open(SHM_NAME).map(Arc::new)
    }
    fn request_capture(&mut self, sel: &DeviceSelector) -> PendingOpen<CaptureHandle> {
        self.host.request_capture(sel.clone())
    }
    fn request_playback(&mut self, sel: &DeviceSelector) -> PendingOpen<PlaybackHandle> {
        self.host.request_playback(sel.clone())
    }
    fn take_device_errors(&mut self) -> Vec<String> {
        self.host.take_errors()
    }
}

/// No devices (control-plane tests, or audio disabled).
pub struct NullAudio;
impl AudioBackend for NullAudio {
    fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
        Err(DeviceError::NotFound)
    }
    fn stop_capture(&mut self) {}
    fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
        Err(DeviceError::NotFound)
    }
    fn stop_playback(&mut self) {}
    fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
        Err(VirtualDeviceError::NotFound(libc::ENOENT))
    }
}

/// Messages into the DSP thread. `Packet` and `Pong` are realtime traffic and travel on a
/// bounded, lossy queue; every other message is control and travels on a separate unbounded
/// queue that the DSP thread drains first, so it is never dropped (see [`AudioRuntime::send`]).
pub enum RuntimeMsg {
    Roles(LocalRoles),
    Settings(AudioSettings),
    Mute(bool),
    SetEnabled(bool),
    Packet {
        header: RtHeader,
        payload: Vec<u8>,
        arrival_ns: u64,
    },
    Pong {
        sample: ClockSample,
        from: PeerId,
        /// Sequence number of the ping this pong answers (echoed in the pong payload).
        ping_seq: u32,
    },
    Shutdown,
}

impl RuntimeMsg {
    /// Realtime traffic (lossy under overload) rather than control.
    fn is_realtime(&self) -> bool {
        matches!(self, RuntimeMsg::Packet { .. } | RuntimeMsg::Pong { .. })
    }
}

/// Cloneable sending side of an [`AudioRuntime`]: routes control messages to the lossless
/// queue and realtime traffic to the bounded lossy one.
#[derive(Clone)]
pub struct RuntimeSender {
    control: Sender<RuntimeMsg>,
    packets: Sender<RuntimeMsg>,
}

impl RuntimeSender {
    /// Never blocks. Control messages are never dropped (while the DSP thread lives); a
    /// realtime message is dropped when its queue is full.
    pub fn send(&self, m: RuntimeMsg) {
        if m.is_realtime() {
            let _ = self.packets.try_send(m);
        } else {
            let _ = self.control.send(m);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeEvent {
    Selection {
        primary: Option<PeerId>,
        secondary: Option<PeerId>,
    },
    AecStatus(bool),
    Report(PeerReport),
    /// User-visible device/pipeline failure.
    Error(String),
    /// User-visible informational status, e.g. a recovery ("Microphone recovered", "RoomMesh
    /// virtual devices reconnected"). Not an error.
    Notice(String),
}

/// State the DSP thread publishes for other threads. The DSP thread only ever holds these locks
/// briefly and uses `try_lock` for the metrics book in its steady-state loop, so readers can
/// never stall audio.
#[derive(Default)]
pub struct RuntimeShared {
    pub is_coordinator: AtomicBool,
    pub metrics: Mutex<MetricsBook>,
    pub local_report: Mutex<Option<PeerReport>>,
    /// The driver region is mapped and its heartbeat is fresh (maintained on every Mac).
    pub virtual_device_ok: AtomicBool,
}

pub struct AudioRuntime {
    tx: RuntimeSender,
    shared: Arc<RuntimeShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AudioRuntime {
    pub fn spawn(
        local: PeerId,
        backend: Box<dyn AudioBackend>,
        transport: Arc<dyn PeerTransport>,
        sessions: RtSessions,
        events: Sender<RuntimeEvent>,
        settings: AudioSettings,
    ) -> Self {
        let (ctl_tx, ctl_rx) = unbounded::<RuntimeMsg>();
        let (pkt_tx, pkt_rx) = bounded::<RuntimeMsg>(PACKET_QUEUE);
        let shared = Arc::new(RuntimeShared::default());
        let panic_events = events.clone();
        let dsp = Dsp::new(
            local,
            backend,
            transport,
            sessions,
            events,
            settings,
            shared.clone(),
        );
        let join = std::thread::Builder::new()
            .name("roommesh-dsp".into())
            .spawn(move || {
                raise_thread_qos();
                let run = std::panic::AssertUnwindSafe(move || dsp.run(ctl_rx, pkt_rx));
                if std::panic::catch_unwind(run).is_err() {
                    log::error!("roommesh-dsp panicked; audio has stopped");
                    let _ = panic_events.send(RuntimeEvent::Error(
                        "RoomMesh audio stopped working (internal error). Restart the app.".into(),
                    ));
                }
            })
            .expect("spawn dsp");
        Self {
            tx: RuntimeSender {
                control: ctl_tx,
                packets: pkt_tx,
            },
            shared,
            join: Some(join),
        }
    }
    /// Never blocks. Control messages are lossless; a realtime packet or pong is dropped if
    /// its queue is full (4096 entries).
    pub fn send(&self, m: RuntimeMsg) {
        self.tx.send(m);
    }
    pub fn sender(&self) -> RuntimeSender {
        self.tx.clone()
    }
    pub fn shared(&self) -> Arc<RuntimeShared> {
        self.shared.clone()
    }
    pub fn shutdown(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        self.tx.send(RuntimeMsg::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Best effort: run the DSP thread at user-interactive QoS.
fn raise_thread_qos() {
    #[cfg(target_os = "macos")]
    unsafe {
        let _ =
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

fn send_rt(
    transport: &Arc<dyn PeerTransport>,
    sessions: &RtSessions,
    to: PeerId,
    h: &RtHeader,
    payload: &[u8],
) {
    let cipher = sessions.read().get(&to).cloned();
    if let Some(c) = cipher {
        transport.send_realtime(to, c.seal(h, payload));
    }
}

/// Returns the next value of a runtime-lifetime sequence counter.
fn next_seq(counter: &mut u32) -> u32 {
    let s = *counter;
    *counter = counter.wrapping_add(1);
    s
}

/// No coordinator pipeline: drop mic metrics. Off the coordinator role, reports are stale too
/// (they are only ever sent to the coordinator), so only names are kept.
fn clear_mic_metrics(book: &mut MetricsBook, roles: &LocalRoles) {
    if roles.is_coordinator {
        book.clear_all_mic();
    } else {
        book.reset_to_names();
    }
}

fn emit(events: &Sender<RuntimeEvent>, msg: String) {
    log::warn!("{msg}");
    let _ = events.try_send(RuntimeEvent::Error(msg));
}

fn notice(events: &Sender<RuntimeEvent>, msg: String) {
    log::info!("{msg}");
    let _ = events.try_send(RuntimeEvent::Notice(msg));
}

// ---------------------------------------------------------------------------------------------
// Virtual device monitor (every Mac)

enum VdevEvent {
    Unavailable(String),
    Reconnected,
}

/// Keeps the driver's shared-memory region mapped and follows driver restarts: a restarted
/// driver recreates the region with a new `generation`, and our old mapping goes dead.
struct VdevMonitor {
    region: Option<Arc<SharedRegion>>,
    next_check_ns: u64,
    next_open_ns: u64,
    unavailable_reported: bool,
}

impl VdevMonitor {
    fn new() -> Self {
        Self {
            region: None,
            next_check_ns: 0,
            next_open_ns: 0,
            unavailable_reported: false,
        }
    }
    fn ok(&self, now: u64) -> bool {
        self.region.as_ref().is_some_and(|r| r.driver_alive(now))
    }
    /// `report`: whether an outage should be reported now (it is reported once per outage, and
    /// only while audio is active).
    fn tick(
        &mut self,
        now: u64,
        backend: &mut dyn AudioBackend,
        report: bool,
    ) -> Option<VdevEvent> {
        if now < self.next_check_ns {
            return None;
        }
        self.next_check_ns = now + VDEV_CHECK_NS;
        let failed = |m: &mut Self, e: VirtualDeviceError| {
            m.region = None;
            m.next_open_ns = now + VDEV_RETRY_NS;
            if report && !m.unavailable_reported {
                m.unavailable_reported = true;
                return Some(VdevEvent::Unavailable(format!(
                    "RoomMesh virtual devices unavailable: {e}"
                )));
            }
            None
        };
        match self.region.as_ref() {
            None => {
                if now < self.next_open_ns {
                    return None;
                }
                match backend.open_virtual_device() {
                    Ok(r) => {
                        self.region = Some(r);
                        if std::mem::take(&mut self.unavailable_reported) {
                            return Some(VdevEvent::Reconnected);
                        }
                        None
                    }
                    Err(e) => failed(self, e),
                }
            }
            Some(r) if !r.driver_alive(now) => match backend.open_virtual_device() {
                Ok(n) if n.generation() != r.generation() => {
                    self.region = Some(n);
                    self.unavailable_reported = false;
                    Some(VdevEvent::Reconnected)
                }
                // Same driver instance, merely idle (no client doing IO): keep the mapping.
                Ok(_) => None,
                Err(e) => failed(self, e),
            },
            Some(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Physical devices

/// One physical device the runtime wants open: running, being opened, or waiting to retry.
struct Slot<T, H> {
    running: Option<T>,
    pending: Option<(PendingOpen<H>, u64)>,
    retry_at: u64,
    last_activity_ns: u64,
    /// When the running device was opened, and whether it has delivered anything since.
    opened_ns: u64,
    delivered: bool,
    /// Delay before the next stall restart's reopen (0: not backing off).
    restart_backoff_ns: u64,
    /// Last failure message reported (deduplicates reports; cleared on recovery).
    reported: Option<String>,
    /// Opened after a reported failure; recovery is announced once audio actually flows.
    confirming: bool,
}

impl<T, H> Slot<T, H> {
    fn new() -> Self {
        Self {
            running: None,
            pending: None,
            retry_at: 0,
            last_activity_ns: 0,
            opened_ns: 0,
            delivered: false,
            restart_backoff_ns: 0,
            reported: None,
            confirming: false,
        }
    }
    /// Drops the device/pending open. Returns whether the backend must be told to stop.
    fn clear(&mut self) -> bool {
        let running = self.running.take().is_some();
        let pending = self.pending.take().is_some();
        running || pending
    }
    /// Not wanted any more: forget failures and backoff.
    fn reset(&mut self) {
        self.retry_at = 0;
        self.reported = None;
        self.confirming = false;
        self.restart_backoff_ns = 0;
    }
    fn opened(&mut self, dev: T, now: u64) {
        self.running = Some(dev);
        self.last_activity_ns = now;
        self.opened_ns = now;
        self.delivered = false;
        self.confirming = self.reported.is_some();
    }
    /// Running but silent for too long: a generous grace until the first delivery, then 1 s.
    fn stalled(&self, now: u64) -> bool {
        let limit = if self.delivered {
            DEVICE_STALL_NS
        } else {
            DEVICE_FIRST_ACTIVITY_NS
        };
        self.running.is_some() && now.saturating_sub(self.last_activity_ns) > limit
    }
    /// The next stall restart's reopen delay (exponential backoff).
    fn next_restart_delay(&mut self) -> u64 {
        let d = self.restart_backoff_ns.max(DEVICE_RESTART_BACKOFF_MIN_NS);
        self.restart_backoff_ns = (d * 2).min(DEVICE_RESTART_BACKOFF_MAX_NS);
        d
    }
    fn fail(&mut self, events: &Sender<RuntimeEvent>, msg: String, retry_at: u64) {
        self.retry_at = retry_at;
        self.confirming = false;
        if self.reported.as_deref() != Some(msg.as_str()) {
            emit(events, msg.clone());
            self.reported = Some(msg);
        }
    }
    fn activity(&mut self, now: u64, events: &Sender<RuntimeEvent>, what: &str) {
        self.last_activity_ns = now;
        self.delivered = true;
        if now.saturating_sub(self.opened_ns) >= DEVICE_RESTART_BACKOFF_MAX_NS {
            self.restart_backoff_ns = 0; // healthy for a while: the next stall restarts quickly
        }
        if std::mem::take(&mut self.confirming) && self.reported.take().is_some() {
            notice(events, format!("{what} recovered"));
        }
    }
}

struct Capture {
    h: CaptureHandle,
    fa: FrameAssembler,
    /// Since when even the freshest frame has been too old for the mic budget; reported once
    /// per open (as `MIC_OVER_BUDGET_MSG`) when that lasts `MIC_OVER_BUDGET_NS`.
    over_budget_since: Option<u64>,
    over_budget_reported: bool,
}

struct Playback {
    h: PlaybackHandle,
    clock: PlaybackClock,
    pushed: u64,
    last_output_frames: Option<u64>,
    callback_frames: u64,
}

// ---------------------------------------------------------------------------------------------
// Coordinator

struct VdevLink {
    mic: MicWriter,
    spk: SpeakerReader,
}

struct CoordState {
    pipe: CoordinatorPipeline,
    /// Writers on the monitor's current region (the old writer's drop zeroes the old region's
    /// app heartbeat, so its driver goes silent immediately).
    vdev: Option<VdevLink>,
    farend: FrameAssembler,
    farend_clock: DeviceClock,
    /// (ring position, host time) where the next far-end chunk should start.
    farend_expect: Option<(u64, u64)>,
    /// Last (write_pos, write_host_ns) fed to `farend_clock`.
    farend_last_wt: Option<(u64, u64)>,
    next_out_ns: u64,
    selection: Selection,
}

impl CoordState {
    /// Follows the monitor's region; ring positions restart with a new region.
    fn sync_vdev(&mut self, region: Option<&Arc<SharedRegion>>) {
        let same = match (&self.vdev, region) {
            (Some(l), Some(r)) => Arc::ptr_eq(l.mic.region(), r),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.vdev = region.map(|r| VdevLink {
            mic: MicWriter::new(r.clone()),
            spk: SpeakerReader::new(r.clone()),
        });
        self.farend = FrameAssembler::new(SAMPLE_RATE);
        self.farend_clock.reset();
        self.farend_expect = None;
        self.farend_last_wt = None;
    }
}

struct Dsp {
    local: PeerId,
    backend: Box<dyn AudioBackend>,
    transport: Arc<dyn PeerTransport>,
    sessions: RtSessions,
    events: Sender<RuntimeEvent>,
    settings: AudioSettings,
    shared: Arc<RuntimeShared>,
    enabled: bool,
    muted: bool,
    roles: LocalRoles,
    clock: ClockEstimator,
    // Realtime sequence counters: monotonic for the lifetime of the Dsp (see module docs).
    ping_seq: u32,
    mic_seq: u32,
    farend_seq: u32,
    next_ping_ns: u64,
    /// Pings awaiting their pong: (sequence, t1). A pong is accepted only for an outstanding
    /// ping with the same t1, once (replayed or forged-late pongs are ignored).
    outstanding_pings: VecDeque<(u32, u64)>,
    vdev: VdevMonitor,
    capture: Slot<Capture, CaptureHandle>,
    playback: Slot<Playback, PlaybackHandle>,
    uplink: Option<MicUplink>,
    speaker: Option<SpeakerPipeline>,
    coord: Option<CoordState>,
    last_metrics_ns: u64,
    last_aec: bool,
    /// Peers whose mic metrics this runtime wrote into the book last round.
    mic_metric_peers: BTreeSet<PeerId>,
    play_buf: Vec<f32>,
}

impl Dsp {
    fn new(
        local: PeerId,
        backend: Box<dyn AudioBackend>,
        transport: Arc<dyn PeerTransport>,
        sessions: RtSessions,
        events: Sender<RuntimeEvent>,
        settings: AudioSettings,
        shared: Arc<RuntimeShared>,
    ) -> Self {
        let uplink = match MicUplink::new(local, settings.mic_bitrate_bps) {
            Ok(u) => Some(u),
            Err(e) => {
                emit(&events, format!("Microphone encoder unavailable: {e}"));
                None
            }
        };
        let speaker = match SpeakerPipeline::new(Epoch(0), local) {
            Ok(s) => Some(s),
            Err(e) => {
                emit(&events, format!("Room speaker decoder unavailable: {e}"));
                None
            }
        };
        Self {
            local,
            backend,
            transport,
            sessions,
            events,
            shared,
            settings,
            enabled: false,
            muted: false,
            roles: LocalRoles::none(),
            clock: ClockEstimator::new(),
            ping_seq: 0,
            mic_seq: 0,
            farend_seq: 0,
            next_ping_ns: 0,
            outstanding_pings: VecDeque::new(),
            vdev: VdevMonitor::new(),
            capture: Slot::new(),
            playback: Slot::new(),
            uplink,
            speaker,
            coord: None,
            last_metrics_ns: 0,
            last_aec: false,
            mic_metric_peers: BTreeSet::new(),
            play_buf: Vec::new(),
        }
    }

    fn error(&self, msg: String) {
        emit(&self.events, msg);
    }

    fn active(&self) -> bool {
        self.enabled && self.roles.room_id.is_some()
    }

    /// `control`: lossless control queue, always drained before `packets` (realtime traffic).
    fn run(mut self, control: Receiver<RuntimeMsg>, packets: Receiver<RuntimeMsg>) {
        'run: loop {
            // Wake on either queue or the tick; messages are then handled in priority order.
            {
                let mut sel = Select::new();
                sel.recv(&control);
                sel.recv(&packets);
                let _ = sel.ready_timeout(TICK);
            }
            if !self.drain(&control, &packets) {
                break 'run;
            }
            self.step(now_ns());
        }
        self.stop_all();
    }

    /// Handles the queued messages. Control first, fully: a role/settings change queued behind
    /// packets applies before them, and between any two packets. At most
    /// `MAX_PACKETS_PER_WAKE` packets, though. Returns false on shutdown.
    fn drain(&mut self, control: &Receiver<RuntimeMsg>, packets: &Receiver<RuntimeMsg>) -> bool {
        let mut handled = 0;
        loop {
            loop {
                match control.try_recv() {
                    Ok(m) => {
                        if !self.handle(m) {
                            return false;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return false,
                }
            }
            if handled == MAX_PACKETS_PER_WAKE {
                return true;
            }
            match packets.try_recv() {
                Ok(m) => {
                    handled += 1;
                    if !self.handle(m) {
                        return false;
                    }
                }
                Err(_) => return true,
            }
        }
    }

    fn handle(&mut self, m: RuntimeMsg) -> bool {
        match m {
            RuntimeMsg::Roles(r) => self.apply_roles(r),
            RuntimeMsg::Settings(s) => self.apply_settings(s),
            RuntimeMsg::Mute(m) => self.muted = m,
            RuntimeMsg::SetEnabled(e) => {
                if e != self.enabled {
                    self.enabled = e;
                    if !e {
                        self.stop_all();
                    }
                    self.reconcile(now_ns());
                }
            }
            RuntimeMsg::Packet {
                header,
                payload,
                arrival_ns,
            } => match header.kind {
                PacketKind::Mic => {
                    if let Some(cs) = self.coord.as_mut() {
                        cs.pipe.push_remote_mic(header, payload, arrival_ns);
                    }
                }
                PacketKind::Playback if self.roles.is_speaker => {
                    if let Some(sp) = self.speaker.as_mut() {
                        sp.push_packet(header, payload, arrival_ns);
                    }
                }
                _ => {}
            },
            RuntimeMsg::Pong {
                sample,
                from,
                ping_seq,
            } => {
                if self.roles.coordinator == Some(from)
                    && self.take_ping(ping_seq, sample.t1, sample.t4)
                {
                    self.clock.add_sample(sample);
                }
            }
            RuntimeMsg::Shutdown => return false,
        }
        true
    }

    /// Consumes the outstanding ping `seq` if it was sent at `t1` and has not expired.
    /// `t4`: when the pong arrived (local clock).
    fn take_ping(&mut self, seq: u32, t1: u64, t4: u64) -> bool {
        match self.outstanding_pings.iter().position(|&(s, _)| s == seq) {
            Some(i)
                if self.outstanding_pings[i].1 == t1 && t4.saturating_sub(t1) <= PING_TTL_NS =>
            {
                self.outstanding_pings.remove(i);
                true
            }
            _ => false,
        }
    }

    fn apply_settings(&mut self, s: AudioSettings) {
        let devices_changed = s.input != self.settings.input || s.output != self.settings.output;
        // Anything but arbitration needs a fresh pipeline (AEC instances, latencies, encoder).
        let pipeline_changed = CoordinatorConfig {
            arbitration: self.settings.coordinator.arbitration.clone(),
            ..s.coordinator.clone()
        } != self.settings.coordinator;
        if s.mic_bitrate_bps != self.settings.mic_bitrate_bps {
            // Safe to recreate: mic sequence numbers come from `self.mic_seq`, not the uplink.
            self.uplink = match MicUplink::new(self.local, s.mic_bitrate_bps) {
                Ok(u) => Some(u),
                Err(e) => {
                    self.error(format!("Microphone encoder unavailable: {e}"));
                    None
                }
            };
        }
        if let Some(cs) = self.coord.as_mut() {
            cs.pipe.set_arbitration(s.coordinator.arbitration.clone());
        }
        if s.coordinator.mic_latency_ns != self.settings.coordinator.mic_latency_ns {
            if let Some(c) = self.capture.running.as_mut() {
                // New budget: judge (and report) the input latency afresh.
                c.over_budget_since = None;
                c.over_budget_reported = false;
            }
        }
        self.settings = s;
        if devices_changed {
            self.stop_devices();
        }
        if pipeline_changed && self.coord.is_some() {
            self.drop_coord();
        }
        self.reconcile(now_ns());
    }

    /// Closes both physical devices; `reconcile` reopens whatever is still wanted.
    fn stop_devices(&mut self) {
        if self.capture.clear() {
            self.backend.stop_capture();
        }
        if self.playback.clear() {
            self.backend.stop_playback();
        }
        self.capture.retry_at = 0;
        self.playback.retry_at = 0;
    }

    fn stop_all(&mut self) {
        self.stop_devices();
        self.drop_coord();
    }

    /// Coordinator teardown: the mic writer's drop silences RoomMesh Microphone at once.
    fn drop_coord(&mut self) {
        if self.coord.take().is_some() {
            clear_mic_metrics(&mut self.shared.metrics.lock(), &self.roles);
            self.mic_metric_peers.clear();
        }
        if std::mem::take(&mut self.last_aec) {
            let _ = self.events.try_send(RuntimeEvent::AecStatus(false));
        }
        self.shared.is_coordinator.store(false, Ordering::Relaxed);
    }

    fn apply_roles(&mut self, new: LocalRoles) {
        let old = std::mem::replace(&mut self.roles, new);
        let new = &self.roles;
        if (old.coordinator, old.epoch) != (new.coordinator, new.epoch) {
            self.clock.reset();
            self.next_ping_ns = 0;
            self.outstanding_pings.clear();
            // It describes our link to the previous coordinator (and `metrics_step`, which
            // would replace it, doesn't run while audio is off).
            *self.shared.local_report.lock() = None;
            let authority = new.coordinator.unwrap_or(self.local);
            if let Some(sp) = self.speaker.as_mut() {
                sp.set_authority(new.epoch, authority);
            }
        }
        if let Some(cs) = self.coord.as_mut() {
            cs.pipe.set_epoch(self.roles.epoch);
            cs.pipe.set_enabled_mics(&self.roles.enabled_mics);
        }
        self.reconcile(now_ns());
    }

    /// Brings pipelines and devices in line with `enabled` + roles.
    fn reconcile(&mut self, now: u64) {
        self.reconcile_coordinator();
        self.reconcile_devices(now);
    }

    fn reconcile_coordinator(&mut self) {
        if self.active() && self.roles.is_coordinator {
            if self.coord.is_none() {
                match CoordinatorPipeline::new(
                    self.local,
                    self.roles.epoch,
                    self.settings.coordinator.clone(),
                ) {
                    Ok(mut pipe) => {
                        pipe.set_enabled_mics(&self.roles.enabled_mics);
                        self.coord = Some(CoordState {
                            pipe,
                            vdev: None,
                            farend: FrameAssembler::new(SAMPLE_RATE),
                            farend_clock: DeviceClock::new(SAMPLE_RATE as f64, 0),
                            farend_expect: None,
                            farend_last_wt: None,
                            next_out_ns: 0,
                            selection: Selection::default(),
                        });
                    }
                    Err(e) => self.error(format!("Coordinator pipeline failed: {e}")),
                }
            }
        } else if self.coord.is_some() {
            self.drop_coord(); // RoomMesh Microphone now serves silence
        }
        self.shared
            .is_coordinator
            .store(self.coord.is_some(), Ordering::Relaxed);
    }

    fn reconcile_devices(&mut self, now: u64) {
        self.reconcile_capture(now);
        self.reconcile_playback(now);
    }

    fn reconcile_capture(&mut self, now: u64) {
        let want = self.active() && self.roles.mic_enabled;
        let slot = &mut self.capture;
        if !want {
            if slot.clear() {
                self.backend.stop_capture();
            }
            slot.reset();
            return;
        }
        // Undrained blocks are activity too (the DSP thread itself may have stalled).
        if slot
            .running
            .as_ref()
            .is_some_and(|c| c.h.blocks.slots() > 0)
        {
            slot.activity(now, &self.events, "Microphone");
        }
        if slot.stalled(now) {
            slot.clear();
            self.backend.stop_capture();
            let retry_at = now + slot.next_restart_delay();
            slot.fail(
                &self.events,
                "Microphone stopped delivering audio; restarting it".into(),
                retry_at,
            );
        }
        if slot.running.is_none() && slot.pending.is_none() && now >= slot.retry_at {
            slot.pending = Some((self.backend.request_capture(&self.settings.input), now));
        }
        let Some((rx, since)) = slot.pending.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(h)) => {
                slot.pending = None;
                let fa = FrameAssembler::new(h.sample_rate);
                slot.opened(
                    Capture {
                        h,
                        fa,
                        over_budget_since: None,
                        over_budget_reported: false,
                    },
                    now,
                );
            }
            Ok(Err(e)) => {
                slot.pending = None;
                slot.fail(
                    &self.events,
                    format!("Microphone unavailable: {e}"),
                    now + DEVICE_RETRY_NS,
                );
            }
            Err(TryRecvError::Empty) if now.saturating_sub(*since) > DEVICE_OPEN_TIMEOUT_NS => {
                slot.pending = None;
                self.backend.stop_capture();
                slot.fail(
                    &self.events,
                    "Microphone unavailable: opening the device timed out".into(),
                    now + DEVICE_RETRY_NS,
                );
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                slot.pending = None;
                slot.fail(
                    &self.events,
                    "Microphone unavailable: audio device host stopped".into(),
                    now + DEVICE_RETRY_NS,
                );
            }
        }
    }

    fn reconcile_playback(&mut self, now: u64) {
        let want = self.active() && self.roles.is_speaker;
        let slot = &mut self.playback;
        if !want {
            if slot.clear() {
                self.backend.stop_playback();
            }
            slot.reset();
            return;
        }
        // Undrained reports are activity too (the DSP thread itself may have stalled).
        if slot
            .running
            .as_ref()
            .is_some_and(|p| p.h.reports.slots() > 0)
        {
            slot.activity(now, &self.events, "Speaker");
        }
        if slot.stalled(now) {
            slot.clear();
            self.backend.stop_playback();
            let retry_at = now + slot.next_restart_delay();
            slot.fail(
                &self.events,
                "Speaker stopped playing; restarting it".into(),
                retry_at,
            );
        }
        if slot.running.is_none() && slot.pending.is_none() && now >= slot.retry_at {
            slot.pending = Some((self.backend.request_playback(&self.settings.output), now));
        }
        let Some((rx, since)) = slot.pending.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(h)) => {
                slot.pending = None;
                let clock = PlaybackClock::new(h.sample_rate as f64);
                slot.opened(
                    Playback {
                        h,
                        clock,
                        pushed: 0,
                        last_output_frames: None,
                        callback_frames: 0,
                    },
                    now,
                );
            }
            Ok(Err(e)) => {
                slot.pending = None;
                slot.fail(
                    &self.events,
                    format!("Speaker unavailable: {e}"),
                    now + DEVICE_RETRY_NS,
                );
            }
            Err(TryRecvError::Empty) if now.saturating_sub(*since) > DEVICE_OPEN_TIMEOUT_NS => {
                slot.pending = None;
                self.backend.stop_playback();
                slot.fail(
                    &self.events,
                    "Speaker unavailable: opening the device timed out".into(),
                    now + DEVICE_RETRY_NS,
                );
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                slot.pending = None;
                slot.fail(
                    &self.events,
                    "Speaker unavailable: audio device host stopped".into(),
                    now + DEVICE_RETRY_NS,
                );
            }
        }
    }

    /// cpal stream errors: restart the affected stream (reported once per distinct message).
    fn device_errors(&mut self, now: u64) {
        for e in self.backend.take_device_errors() {
            if e.starts_with(CAPTURE_STREAM_ERROR) {
                if self.capture.clear() {
                    self.backend.stop_capture();
                }
                self.capture
                    .fail(&self.events, format!("Microphone error: {e}"), now);
            } else if e.starts_with(PLAYBACK_STREAM_ERROR) {
                if self.playback.clear() {
                    self.backend.stop_playback();
                }
                self.playback
                    .fail(&self.events, format!("Speaker error: {e}"), now);
            } else {
                self.error(format!("Audio device error: {e}"));
            }
        }
    }

    fn step(&mut self, now: u64) {
        let active = self.active();
        if let Some(ev) = self.vdev.tick(now, &mut *self.backend, active) {
            match ev {
                VdevEvent::Unavailable(msg) => self.error(msg),
                VdevEvent::Reconnected => {
                    notice(&self.events, "RoomMesh virtual devices reconnected".into())
                }
            }
        }
        self.shared
            .virtual_device_ok
            .store(self.vdev.ok(now), Ordering::Relaxed);
        if !active {
            return;
        }
        self.device_errors(now);
        self.reconcile_devices(now);
        self.clock_sync(now);
        self.capture_step(now);
        self.coordinator_step(now);
        self.speaker_step(now);
        if now.saturating_sub(self.last_metrics_ns) >= METRICS_INTERVAL_NS {
            self.last_metrics_ns = now;
            self.metrics_step();
        }
    }

    fn clock_sync(&mut self, now: u64) {
        let Some(c) = self.roles.coordinator else {
            return;
        };
        if self.roles.is_coordinator || now < self.next_ping_ns {
            return;
        }
        let h = RtHeader {
            kind: PacketKind::ClockPing,
            epoch: self.roles.epoch,
            stream: StreamId::CLOCK,
            sender: self.local,
            sequence: next_seq(&mut self.ping_seq),
            sample_index: 0,
            timestamp_ns: now,
            frame_count: 0,
        };
        self.outstanding_pings
            .retain(|&(_, t1)| now.saturating_sub(t1) <= PING_TTL_NS);
        if self.outstanding_pings.len() >= MAX_OUTSTANDING_PINGS {
            self.outstanding_pings.pop_front();
        }
        self.outstanding_pings.push_back((h.sequence, now));
        send_rt(
            &self.transport,
            &self.sessions,
            c,
            &h,
            &encode_times(&[now]),
        );
        self.next_ping_ns = now
            + if self.clock.is_synced() {
                PING_SLOW_NS
            } else {
                PING_FAST_NS
            };
    }

    fn capture_step(&mut self, now: u64) {
        let Some(cap) = self.capture.running.as_mut() else {
            return;
        };
        let mut got = false;
        while let Ok(b) = cap.h.blocks.pop() {
            got = true;
            cap.fa
                .push(b.first_frame, b.capture_ns, &b.samples[..b.len as usize]);
        }
        if got {
            self.capture.activity(now, &self.events, "Microphone");
        }
        let Some(cap) = self.capture.running.as_mut() else {
            return;
        };
        // Frames older than the mic budget can no longer be used; dropping them avoids a
        // stale-backlog burst after a mic toggle or a stall. A frame is only complete FRAME_NS
        // after its start, hence the extra frame. Only the coordinator knows the room's budget
        // (its own setting); a member uplinks by a fixed generous one.
        let is_coord = self.coord.is_some();
        let budget = if is_coord {
            self.settings.coordinator.mic_latency_ns
        } else {
            MEMBER_UPLINK_BUDGET_NS
        };
        let oldest = now.saturating_sub(budget + FRAME_NS);
        let mut newest_stale = None;
        while let Some(frame) = cap.fa.pop_frame() {
            let stale = frame.timestamp_ns < oldest;
            newest_stale = Some(stale);
            if self.muted || stale {
                continue;
            }
            if let Some(cs) = self.coord.as_mut() {
                cs.pipe.push_local_mic(&frame);
                continue;
            }
            let (Some(c), Some(up)) = (self.roles.coordinator, self.uplink.as_mut()) else {
                continue;
            };
            if !self.clock.is_synced() {
                continue;
            }
            let Some(ts) = self.clock.to_coord(frame.timestamp_ns) else {
                continue;
            };
            if let Ok((mut hdr, payload)) = up.packetize(&frame, self.roles.epoch, ts) {
                hdr.sequence = next_seq(&mut self.mic_seq);
                send_rt(&self.transport, &self.sessions, c, &hdr, &payload);
            }
        }
        // A backlog after a stall drains stale frames too, but its newest frame is fresh; only
        // when even the newest is too old does the input latency itself exceed the budget. The
        // advice (raise Mic latency) only applies to the coordinator's own setting.
        match newest_stale {
            Some(true) => {
                let since = *cap.over_budget_since.get_or_insert(now);
                if is_coord
                    && !cap.over_budget_reported
                    && now.saturating_sub(since) >= MIC_OVER_BUDGET_NS
                {
                    cap.over_budget_reported = true;
                    emit(&self.events, MIC_OVER_BUDGET_MSG.into());
                }
            }
            Some(false) => cap.over_budget_since = None,
            None => {}
        }
    }

    // Borrow note: `cs` borrows `self.coord`; everything else is reached through disjoint
    // fields — never call a `&mut self` method while `cs` is alive.
    fn coordinator_step(&mut self, now: u64) {
        let Some(cs) = self.coord.as_mut() else {
            return;
        };
        cs.sync_vdev(self.vdev.region.as_ref());
        // far-end: what the meeting app plays into "RoomMesh Speaker"
        if let Some(link) = cs.vdev.as_mut() {
            while let Some(chunk) = link.spk.read(4096) {
                // Same write position with a different host time is a torn read of the
                // driver's (position, time) pair: keep the clock on the previous sample.
                let (w, t) = (chunk.write_pos, chunk.write_host_ns);
                if cs.farend_last_wt.is_none_or(|(lw, _)| lw != w) {
                    cs.farend_clock.report(w, t);
                    cs.farend_last_wt = Some((w, t));
                }
                let ts = cs.farend_clock.time_of(chunk.first_pos).unwrap_or(now);
                if let Some((pos, t)) = cs.farend_expect {
                    if pos != chunk.first_pos || ts.abs_diff(t) > FAREND_JUMP_NS {
                        cs.farend.reset();
                    }
                }
                let len = chunk.samples.len() as u64;
                cs.farend_expect = Some((
                    chunk.first_pos + len,
                    ts + (len as f64 * NS_PER_SAMPLE) as u64,
                ));
                cs.farend.push(chunk.first_pos, ts, &chunk.samples);
            }
        }
        while let Some(f) = cs.farend.pop_frame() {
            // Always through the pipeline: it is also the AEC reference.
            let mut pb = cs.pipe.push_farend(&f);
            if pb.header.timestamp_ns < now {
                continue; // would play in the past
            }
            match self.roles.speaker {
                Some(s) if s == self.local => {
                    if let Some(sp) = self.speaker.as_mut() {
                        sp.push_local(&pb.local);
                    }
                }
                Some(s) => {
                    // The pipeline's own far-end counter restarts whenever a pipeline is
                    // recreated, which can happen within one epoch (audio toggled, settings
                    // changed); the runtime-lifetime counter keeps (epoch, seq) unique.
                    pb.header.sequence = next_seq(&mut self.farend_seq);
                    send_rt(&self.transport, &self.sessions, s, &pb.header, &pb.payload);
                }
                None => {}
            }
        }
        // room microphone: one frame per 10 ms of host time, one frame ahead of "now";
        // after a stall, slots entirely in the past are written as silence (see MAX_GRID_LAG_NS)
        if cs.next_out_ns == 0 {
            cs.next_out_ns = now;
        }
        let lag = now.saturating_sub(cs.next_out_ns);
        if lag > MAX_GRID_LAG_NS {
            let missed = lag / FRAME_NS;
            if let Some(link) = cs.vdev.as_ref() {
                for _ in 0..missed.min(MAX_ZERO_FILL_SLOTS) {
                    link.mic.write(&SILENT_FRAME, now);
                }
            }
            cs.next_out_ns += missed * FRAME_NS;
        }
        while cs.next_out_ns <= now + FRAME_NS {
            let pf = cs.pipe.produce(cs.next_out_ns, now);
            if let Some(link) = cs.vdev.as_ref() {
                link.mic.write(&pf.samples, now);
            }
            cs.selection = pf.selection;
            if pf.selection_changed {
                let _ = self.events.try_send(RuntimeEvent::Selection {
                    primary: pf.selection.primary,
                    secondary: pf.selection.secondary,
                });
            }
            cs.next_out_ns += FRAME_NS;
        }
    }

    fn speaker_step(&mut self, now: u64) {
        let Some(pb) = self.playback.running.as_mut() else {
            return;
        };
        // Reports are drained even without a speaker pipeline: undrained ones count as device
        // activity (see `reconcile_playback`) and would hide a dead device.
        let mut got = false;
        while let Ok(r) = pb.h.reports.pop() {
            got = true;
            if let Some(prev) = pb.last_output_frames {
                let d = r.output_frames.saturating_sub(prev);
                if d > 0 {
                    pb.callback_frames = d;
                }
            }
            pb.last_output_frames = Some(r.output_frames);
            pb.clock.report(r);
        }
        if got {
            self.playback.activity(now, &self.events, "Speaker");
        }
        let (Some(pb), Some(sp)) = (self.playback.running.as_mut(), self.speaker.as_mut()) else {
            return;
        };
        let rate = pb.h.sample_rate as f64;
        let ns_to_frames = |ns: u64| (rate * ns as f64 / 1e9) as u64;
        let target = ns_to_frames(PLAYBACK_QUEUE_MIN_NS)
            .max(pb.callback_frames + ns_to_frames(PLAYBACK_QUEUE_MARGIN_NS));
        let chunk = (rate * 0.01) as usize;
        let is_coord = self.roles.is_coordinator;
        let clock = &self.clock;
        let to_local = |c: u64| if is_coord { Some(c) } else { clock.to_local(c) };
        let to_coord = |l: u64| if is_coord { Some(l) } else { clock.to_coord(l) };
        let buf = &mut self.play_buf;
        buf.resize(chunk, 0.0);
        while pb.pushed.saturating_sub(pb.clock.popped()) < target {
            match pb.clock.play_time_of(pb.pushed) {
                Some(t) => {
                    sp.render(t, 1e9 / rate, buf, now, to_local, to_coord);
                }
                None => buf.fill(0.0), // no timing yet: prime with silence
            }
            let mut n = 0;
            for s in buf.iter() {
                if pb.h.samples.push(*s).is_err() {
                    break;
                }
                n += 1;
            }
            pb.pushed += n as u64;
            if n < chunk {
                break;
            }
        }
    }

    fn metrics_step(&mut self) {
        if let Some(cs) = self.coord.as_ref() {
            // Skip this round rather than wait if a reader holds the book.
            if let Some(mut book) = self.shared.metrics.try_lock() {
                let statuses = cs.pipe.statuses();
                let mut seen = BTreeSet::new();
                for s in &statuses {
                    let active =
                        self.roles.members.contains(&s.peer) && cs.selection.contains(s.peer);
                    book.apply_mic_status(s, active);
                    seen.insert(s.peer);
                }
                for p in self.mic_metric_peers.difference(&seen) {
                    book.clear_mic(*p);
                }
                self.mic_metric_peers = seen;
            }
            let conv = cs.pipe.aec_converged();
            if conv != self.last_aec {
                self.last_aec = conv;
                let _ = self.events.try_send(RuntimeEvent::AecStatus(conv));
            }
        } else if let Some(mut book) = self.shared.metrics.try_lock() {
            clear_mic_metrics(&mut book, &self.roles);
            self.mic_metric_peers.clear();
        }
        if !self.roles.is_coordinator {
            if !self.clock.is_synced() {
                // No RTT/offset yet: an all-zero report would show as a perfect link.
                *self.shared.local_report.lock() = None;
                return;
            }
            let jitter = self
                .speaker
                .as_ref()
                .filter(|_| self.roles.is_speaker)
                .map(|s| s.stats());
            let r = PeerReport {
                peer: self.local,
                rtt_ms: self
                    .clock
                    .min_rtt_ns()
                    .map(|v| v as f32 / 1e6)
                    .unwrap_or(0.0),
                jitter_ms: jitter.as_ref().map(stream_jitter_ms).unwrap_or(0.0),
                loss_pct: jitter.as_ref().map(stream_loss_pct).unwrap_or(0.0),
                clock_offset_ms: self
                    .clock
                    .offset_ns()
                    .map(|v| v as f32 / 1e6)
                    .unwrap_or(0.0),
                drift_ppm: self.clock.drift_ppm().unwrap_or(0.0) as f32,
                transport: self.transport.description(),
            };
            *self.shared.local_report.lock() = Some(r.clone());
            let _ = self.events.try_send(RuntimeEvent::Report(r));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RoomId;
    use crate::network::loopback::LoopbackNetwork;
    use crossbeam_channel::unbounded;

    const MS: u64 = 1_000_000;

    fn roles(coord: u64, speaker: u64, local: u64) -> LocalRoles {
        LocalRoles {
            room_id: Some(RoomId(1)),
            epoch: Epoch(1),
            coordinator: Some(PeerId(coord)),
            speaker: Some(PeerId(speaker)),
            is_coordinator: coord == local,
            is_speaker: speaker == local,
            mic_enabled: true,
            enabled_mics: vec![PeerId(1), PeerId(2)],
            members: vec![PeerId(1), PeerId(2)],
        }
    }
    fn transport(local: u64) -> Arc<dyn PeerTransport> {
        let net = LoopbackNetwork::new();
        let (tsink, _trx) = unbounded();
        net.transport(PeerId(local), tsink)
    }
    fn no_aec() -> AudioSettings {
        let mut s = AudioSettings::default();
        s.coordinator.use_webrtc_aec = false;
        s
    }
    fn spawn_with(
        local: u64,
        backend: Box<dyn AudioBackend>,
        settings: AudioSettings,
    ) -> (AudioRuntime, crossbeam_channel::Receiver<RuntimeEvent>) {
        let (ev_tx, ev_rx) = unbounded();
        let rt = AudioRuntime::spawn(
            PeerId(local),
            backend,
            transport(local),
            Default::default(),
            ev_tx,
            settings,
        );
        (rt, ev_rx)
    }
    fn spawn(local: u64) -> (AudioRuntime, crossbeam_channel::Receiver<RuntimeEvent>) {
        spawn_with(local, Box::new(NullAudio), AudioSettings::default())
    }
    /// A Dsp driven synchronously with synthetic time.
    fn dsp(
        local: u64,
        backend: Box<dyn AudioBackend>,
    ) -> (Dsp, crossbeam_channel::Receiver<RuntimeEvent>) {
        let (ev_tx, ev_rx) = unbounded();
        let d = Dsp::new(
            PeerId(local),
            backend,
            transport(local),
            Default::default(),
            ev_tx,
            no_aec(),
            Arc::default(),
        );
        (d, ev_rx)
    }
    /// (errors, notices) queued so far.
    fn split_events(ev: &crossbeam_channel::Receiver<RuntimeEvent>) -> (Vec<String>, Vec<String>) {
        let (mut errs, mut notes) = (Vec::new(), Vec::new());
        for e in ev.try_iter() {
            match e {
                RuntimeEvent::Error(m) => errs.push(m),
                RuntimeEvent::Notice(m) => notes.push(m),
                _ => {}
            }
        }
        (errs, notes)
    }
    fn errors(ev: &crossbeam_channel::Receiver<RuntimeEvent>) -> Vec<String> {
        ev.try_iter()
            .filter_map(|e| match e {
                RuntimeEvent::Error(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn becomes_and_stops_being_coordinator() {
        let (rt, ev) = spawn(1);
        rt.send(RuntimeMsg::SetEnabled(true));
        rt.send(RuntimeMsg::Roles(roles(1, 1, 1)));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(rt.shared().is_coordinator.load(Ordering::Relaxed));
        // NullAudio has no devices → errors are reported as events, never panics
        assert!(ev.try_iter().any(|e| matches!(e, RuntimeEvent::Error(_))));
        rt.send(RuntimeMsg::Roles(roles(2, 2, 1)));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!rt.shared().is_coordinator.load(Ordering::Relaxed));
        rt.shutdown();
    }

    /// No physical devices; hands out the given driver regions in order (repeating the last).
    struct VdevOnly(Vec<Arc<SharedRegion>>, usize);
    impl VdevOnly {
        fn new(regions: &[&Arc<SharedRegion>]) -> Box<Self> {
            Box::new(Self(regions.iter().map(|r| (*r).clone()).collect(), 0))
        }
    }
    impl AudioBackend for VdevOnly {
        fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_capture(&mut self) {}
        fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_playback(&mut self) {}
        fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
            let r = self.0[self.1.min(self.0.len() - 1)].clone();
            self.1 += 1;
            Ok(r)
        }
    }

    #[test]
    fn coordinator_writes_mic_ring_on_host_time_grid_without_mics() {
        let region = Arc::new(SharedRegion::create_for_test_with(1, now_ns()).unwrap());
        let (rt, _ev) = spawn_with(1, VdevOnly::new(&[&region]), no_aec());
        rt.send(RuntimeMsg::SetEnabled(true));
        rt.send(RuntimeMsg::Roles(roles(1, 1, 1)));
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(rt.shared().virtual_device_ok.load(Ordering::Relaxed));
        let (p0, t0) = (region.test_mic_write_pos(), now_ns());
        assert!(p0 > 0, "coordinator should already be writing the mic ring");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (p1, t1) = (region.test_mic_write_pos(), now_ns());
        let per_ms = (p1 - p0) as f64 / ((t1 - t0) as f64 / 1e6);
        assert!(
            (36.0..=60.0).contains(&per_ms),
            "mic ring should advance ~48 frames/ms, got {per_ms:.1}"
        );
        assert_eq!(p1 % 480, 0, "whole 10 ms frames only");
        let hb = region.header().app_heartbeat_ns.load(Ordering::Relaxed);
        let age = now_ns().saturating_sub(hb);
        assert!(age < 100_000_000, "app heartbeat is stale ({age} ns)");
        rt.shutdown();
    }

    #[test]
    fn mic_ring_tracks_host_time_across_a_stall() {
        let t0 = now_ns();
        let region = Arc::new(SharedRegion::create_for_test_with(1, t0 + 60_000 * MS).unwrap());
        let (mut d, _ev) = dsp(1, VdevOnly::new(&[&region]));
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 1)));
        // The grid starts at t0 and runs one frame ahead: by time t, the slots
        // t0, t0 + 10 ms, … ≤ t + 10 ms have been written.
        let expected = |t: u64| ((t - t0) / FRAME_NS + 2) * 480;
        let mut t = t0;
        for _ in 0..25 {
            d.step(t);
            assert_eq!(region.test_mic_write_pos(), expected(t));
            t += 2 * MS;
        }
        let (before, t_before) = (region.test_mic_write_pos(), t - 2 * MS);
        t += 100 * MS; // DSP stalled for 100 ms
        d.step(t);
        let after = region.test_mic_write_pos();
        // write_pos keeps advancing with time (missed slots zero-filled): the driver's read head
        // stays behind the write edge, and nothing beyond "now + one frame" is queued.
        assert_eq!(
            after,
            expected(t),
            "write_pos must track host time after a stall"
        );
        let per_ms = (after - before) as f64 / ((t - t_before) as f64 / 1e6);
        assert!((40.0..=56.0).contains(&per_ms), "{per_ms:.1} frames/ms");
        for _ in 0..10 {
            t += 2 * MS;
            d.step(t);
            assert_eq!(region.test_mic_write_pos(), expected(t));
        }
    }

    #[test]
    fn coordinator_teardown_zeroes_app_heartbeat() {
        let t0 = now_ns();
        let region = Arc::new(SharedRegion::create_for_test_with(1, t0 + 60_000 * MS).unwrap());
        let (mut d, _ev) = dsp(1, VdevOnly::new(&[&region]));
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 1)));
        for i in 0..5 {
            d.step(t0 + i * 2 * MS);
        }
        let hb = || region.header().app_heartbeat_ns.load(Ordering::Acquire);
        assert!(hb() >= t0);
        d.handle(RuntimeMsg::Roles(roles(2, 2, 1)));
        assert_eq!(
            hb(),
            0,
            "driver must go silent as soon as we stop coordinating"
        );
        assert!(!d.shared.is_coordinator.load(Ordering::Relaxed));
        let p = region.test_mic_write_pos();
        d.step(t0 + 20 * MS);
        assert_eq!(region.test_mic_write_pos(), p);
        // and on disable while coordinating
        d.handle(RuntimeMsg::Roles(roles(1, 1, 1)));
        d.step(t0 + 22 * MS);
        assert!(hb() > 0);
        d.handle(RuntimeMsg::SetEnabled(false));
        assert_eq!(hb(), 0);
    }

    #[test]
    fn follows_driver_restart_to_new_region_generation() {
        let t0 = now_ns();
        let a = Arc::new(SharedRegion::create_for_test_with(1, t0).unwrap());
        let b = Arc::new(SharedRegion::create_for_test_with(2, t0 + 60_000 * MS).unwrap());
        let (mut d, ev) = dsp(1, VdevOnly::new(&[&a, &b]));
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 1)));
        let mut t = t0;
        for _ in 0..10 {
            d.step(t);
            t += 2 * MS;
        }
        assert!(a.test_mic_write_pos() > 0);
        assert!(d.shared.virtual_device_ok.load(Ordering::Relaxed));
        let _ = errors(&ev);
        // A's driver dies (heartbeat 3 s old); the next check reopens and finds generation 2.
        t = t0 + 3_000 * MS;
        d.step(t);
        assert!(d.shared.virtual_device_ok.load(Ordering::Relaxed));
        for _ in 0..10 {
            t += 2 * MS;
            d.step(t);
        }
        assert!(
            b.test_mic_write_pos() >= 4 * 480,
            "B's mic ring should advance"
        );
        assert_eq!(a.header().app_heartbeat_ns.load(Ordering::Acquire), 0);
        let a_pos = a.test_mic_write_pos();
        d.step(t + 10 * MS);
        assert_eq!(
            a.test_mic_write_pos(),
            a_pos,
            "nothing more is written to A"
        );
        let (errs, notes) = split_events(&ev);
        assert_eq!(
            notes.iter().filter(|m| m.contains("reconnected")).count(),
            1,
            "{notes:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("reconnected")),
            "a recovery is a notice, not an error: {errs:?}"
        );
    }

    /// Capture always fails; counts open attempts.
    struct FailingMic(Arc<std::sync::atomic::AtomicUsize>);
    impl AudioBackend for FailingMic {
        fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(DeviceError::NotFound)
        }
        fn stop_capture(&mut self) {}
        fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_playback(&mut self) {}
        fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
            Err(VirtualDeviceError::NotFound(libc::ENOENT))
        }
    }

    #[test]
    fn missing_mic_is_retried_and_reported_once() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (mut d, ev) = dsp(2, Box::new(FailingMic(attempts.clone())));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2)));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        for i in 0..20 {
            d.step(t0 + i * 100 * MS); // 2 s
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 1, "backoff ~3 s");
        d.step(t0 + 3_500 * MS);
        d.step(t0 + 7_000 * MS);
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
        let msgs = errors(&ev);
        let mic: Vec<_> = msgs
            .iter()
            .filter(|m| m.starts_with("Microphone"))
            .collect();
        assert_eq!(mic.len(), 1, "{msgs:?}");
        let vdev: Vec<_> = msgs
            .iter()
            .filter(|m| m.contains("virtual devices"))
            .collect();
        assert_eq!(vdev.len(), 1, "{msgs:?}");
        // A repeated SetEnabled(true) is a no-op (no reopen).
        d.handle(RuntimeMsg::SetEnabled(true));
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    /// Blocks the DSP thread inside its first virtual-device open until released.
    struct GatedVdev {
        entered: Sender<()>,
        release: Receiver<()>,
    }
    impl AudioBackend for GatedVdev {
        fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_capture(&mut self) {}
        fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_playback(&mut self) {}
        fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
            let _ = self.entered.try_send(());
            let _ = self.release.recv_timeout(std::time::Duration::from_secs(5));
            Err(VirtualDeviceError::NotFound(libc::ENOENT))
        }
    }

    #[test]
    fn control_messages_survive_a_full_packet_queue() {
        let (entered_tx, entered) = bounded(1);
        let (release, release_rx) = bounded(1);
        let (rt, _ev) = spawn_with(
            1,
            Box::new(GatedVdev {
                entered: entered_tx,
                release: release_rx,
            }),
            no_aec(),
        );
        entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("DSP thread reached the device open");
        // The DSP thread is stuck: overflow the packet queue, then queue control behind it.
        for i in 0..(PACKET_QUEUE as u32 + 1000) {
            rt.send(RuntimeMsg::Packet {
                header: RtHeader {
                    kind: PacketKind::Mic,
                    epoch: Epoch(1),
                    stream: StreamId::MIC,
                    sender: PeerId(2),
                    sequence: i,
                    sample_index: 0,
                    timestamp_ns: 0,
                    frame_count: 480,
                },
                payload: vec![],
                arrival_ns: 0,
            });
        }
        rt.send(RuntimeMsg::SetEnabled(true));
        rt.send(RuntimeMsg::Roles(roles(1, 1, 1)));
        release.send(()).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !rt.shared().is_coordinator.load(Ordering::Relaxed) {
            assert!(
                std::time::Instant::now() < end,
                "roles/enable queued behind a full packet queue were lost"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        rt.shutdown();
    }

    fn pong(t1: u64, seq: u32, from: u64, late_ns: u64) -> RuntimeMsg {
        RuntimeMsg::Pong {
            sample: ClockSample {
                t1,
                t2: t1 + MS,
                t3: t1 + MS + 100_000,
                t4: t1 + 2 * MS + late_ns,
            },
            from: PeerId(from),
            ping_seq: seq,
        }
    }

    /// Steps a non-coordinator Dsp so it sends `n` clock pings; returns their (seq, t1).
    fn send_pings(d: &mut Dsp, t0: u64, n: u64) -> Vec<(u32, u64)> {
        (0..n)
            .map(|i| {
                let t = t0 + i * PING_FAST_NS;
                d.step(t);
                *d.outstanding_pings.back().expect("ping sent")
            })
            .collect()
    }

    #[test]
    fn pongs_are_accepted_once_and_only_for_outstanding_pings() {
        let (mut d, _ev) = dsp(2, Box::new(NullAudio));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2)));
        let pings = send_pings(&mut d, t0, 4);
        assert_eq!(pings.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 1, 2, 3]);
        let (s0, t1) = pings[0];
        // Wrong t1, unknown sequence, or not from the coordinator: ignored.
        d.handle(pong(t1 + 1, s0, 1, 0));
        d.handle(pong(t1, 99, 1, 0));
        d.handle(pong(t1, s0, 3, 0));
        assert!(
            d.clock.min_rtt_ns().is_none(),
            "no sample may be accepted yet"
        );
        for &(s, t) in &pings[..3] {
            d.handle(pong(t, s, 1, 0));
        }
        assert!(d.clock.min_rtt_ns().is_some());
        // Replays of consumed pings don't count as samples (4 are needed to sync).
        for &(s, t) in &pings[..3] {
            d.handle(pong(t, s, 1, 0));
        }
        assert!(!d.clock.is_synced(), "a replayed pong was accepted");
        let (s3, t3) = pings[3];
        d.handle(pong(t3, s3, 1, 0));
        assert!(d.clock.is_synced());
    }

    #[test]
    fn expired_pongs_are_ignored_and_outstanding_pings_are_bounded() {
        let (mut d, _ev) = dsp(2, Box::new(NullAudio));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2)));
        let pings = send_pings(&mut d, t0, 4);
        for &(s, t) in &pings[..3] {
            d.handle(pong(t, s, 1, 0));
        }
        let (s3, t3) = pings[3];
        d.handle(pong(t3, s3, 1, PING_TTL_NS));
        assert!(
            !d.clock.is_synced(),
            "a pong older than the TTL was accepted"
        );
        // Unanswered pings: at most MAX_OUTSTANDING_PINGS, none older than the TTL.
        let (mut d, _ev) = dsp(2, Box::new(NullAudio));
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2)));
        let mut t = t0;
        for _ in 0..40 {
            t += 5 * MS; // faster than the TTL can expire them
            d.next_ping_ns = 0;
            d.step(t);
        }
        assert_eq!(d.outstanding_pings.len(), MAX_OUTSTANDING_PINGS);
        assert_eq!(
            d.outstanding_pings.front().unwrap().0,
            40 - MAX_OUTSTANDING_PINGS as u32
        );
        d.next_ping_ns = 0;
        d.step(t + 2 * PING_TTL_NS);
        assert_eq!(d.outstanding_pings.len(), 1, "expired pings are pruned");
        // A role change (new coordinator/epoch) forgets them all.
        d.handle(RuntimeMsg::Roles(roles(3, 3, 2)));
        assert!(d.outstanding_pings.is_empty());
    }

    struct PanickingVdev;
    impl AudioBackend for PanickingVdev {
        fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_capture(&mut self) {}
        fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
            Err(DeviceError::NotFound)
        }
        fn stop_playback(&mut self) {}
        fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
            panic!("injected DSP panic");
        }
    }

    #[test]
    fn dsp_thread_death_is_reported() {
        let (rt, ev) = spawn_with(1, Box::new(PanickingVdev), no_aec());
        let msg = ev
            .iter()
            .find_map(|e| match e {
                RuntimeEvent::Error(m) => Some(m),
                _ => None,
            })
            .expect("an error event");
        assert!(msg.contains("audio stopped working"), "{msg}");
        rt.shutdown(); // joins the dead thread without hanging
    }

    /// Physical devices driven by the test: counts opens and exposes the producer ends.
    #[derive(Clone, Default)]
    struct FakeDevices {
        captures: Arc<std::sync::atomic::AtomicUsize>,
        playbacks: Arc<std::sync::atomic::AtomicUsize>,
        mic: Arc<Mutex<Option<rtrb::Producer<crate::audio::device_io::CaptureBlock>>>>,
        reports: Arc<Mutex<Option<rtrb::Producer<crate::audio::device_io::PlaybackReport>>>>,
        /// Keeps the consumer end of the output ring alive.
        output: Arc<Mutex<Option<rtrb::Consumer<f32>>>>,
    }
    impl FakeDevices {
        /// Queues one 10 ms capture block stamped `capture_ns`.
        fn mic_block(&self, index: u64, capture_ns: u64) {
            use crate::audio::device_io::{CaptureBlock, BLOCK_FRAMES};
            let b = CaptureBlock {
                first_frame: index,
                capture_ns,
                len: 480,
                samples: [0.0; BLOCK_FRAMES],
            };
            let _ = self.mic.lock().as_mut().expect("mic open").push(b);
        }
        fn report(&self) {
            let r = crate::audio::device_io::PlaybackReport::default();
            let _ = self.reports.lock().as_mut().expect("speaker open").push(r);
        }
        fn opens(&self) -> (usize, usize) {
            (
                self.captures.load(Ordering::SeqCst),
                self.playbacks.load(Ordering::SeqCst),
            )
        }
    }
    impl AudioBackend for FakeDevices {
        fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            let (p, c) = rtrb::RingBuffer::new(256);
            *self.mic.lock() = Some(p);
            Ok(CaptureHandle {
                blocks: c,
                sample_rate: 48_000,
                device_name: "Fake Mic".into(),
            })
        }
        fn stop_capture(&mut self) {}
        fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
            self.playbacks.fetch_add(1, Ordering::SeqCst);
            let (sp, sc) = rtrb::RingBuffer::new(48_000);
            let (rp, rc) = rtrb::RingBuffer::new(256);
            *self.reports.lock() = Some(rp);
            *self.output.lock() = Some(sc);
            Ok(PlaybackHandle {
                samples: sp,
                reports: rc,
                sample_rate: 48_000,
                device_name: "Fake Speaker".into(),
            })
        }
        fn stop_playback(&mut self) {}
        fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError> {
            Err(VirtualDeviceError::NotFound(libc::ENOENT))
        }
    }

    #[test]
    fn slow_starting_mic_gets_a_grace_period_and_restarts_back_off() {
        let fake = FakeDevices::default();
        let (mut d, ev) = dsp(2, Box::new(fake.clone()));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2))); // member, mic on, not the speaker
        assert_eq!(fake.opens().0, 1);
        let at = |ms: u64| t0 + ms * MS;
        for ms in (100..=4_800).step_by(100) {
            d.step(at(ms));
        }
        assert_eq!(
            fake.opens().0,
            1,
            "restarted within the 5 s first-delivery grace"
        );
        // First delivery at 4.9 s; from then on 1 s of silence is a stall.
        fake.mic_block(0, at(4_890));
        d.step(at(4_900));
        d.step(at(5_800));
        assert_eq!(fake.opens().0, 1);
        d.step(at(6_000));
        let (errs, _) = split_events(&ev);
        assert!(
            errs.iter().any(|m| m.contains("stopped delivering")),
            "{errs:?}"
        );
        // Reopened after a 1 s backoff…
        d.step(at(6_900));
        assert_eq!(fake.opens().0, 1, "reopened before the backoff elapsed");
        d.step(at(7_100));
        assert_eq!(fake.opens().0, 2);
        // …and never delivers: 5 s grace again, then the backoff doubles to 2 s.
        d.step(at(12_000));
        assert_eq!(fake.opens().0, 2);
        d.step(at(12_200)); // stalled: restart, reopen due at 14.2 s
        d.step(at(14_000));
        assert_eq!(fake.opens().0, 2, "backoff should have doubled to 2 s");
        d.step(at(14_300));
        assert_eq!(fake.opens().0, 3);
        // Audio flows again: announced as a notice, not an error.
        fake.mic_block(0, at(14_390));
        d.step(at(14_400));
        let (errs, notes) = split_events(&ev);
        assert!(
            notes.iter().any(|m| m == "Microphone recovered"),
            "{notes:?}"
        );
        assert!(!errs.iter().any(|m| m.contains("recovered")), "{errs:?}");
    }

    #[test]
    fn dsp_stall_with_data_in_the_rings_does_not_restart_devices() {
        let fake = FakeDevices::default();
        let (mut d, ev) = dsp(2, Box::new(fake.clone()));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 2, 2))); // member, mic on, room speaker
        assert_eq!(fake.opens(), (1, 1));
        let mut idx = 0;
        for i in 1..=10u64 {
            let t = t0 + i * 10 * MS;
            fake.mic_block(idx, t - 10 * MS);
            idx += 480;
            fake.report();
            d.step(t);
        }
        // Both devices keep delivering, but the DSP thread doesn't run for 3 s.
        fake.mic_block(idx, t0 + 3_000 * MS);
        fake.report();
        d.step(t0 + 3_100 * MS);
        assert_eq!(fake.opens(), (1, 1), "a DSP stall is not a device stall");
        let (errs, _) = split_events(&ev);
        assert!(!errs.iter().any(|m| m.contains("stopped")), "{errs:?}");
    }

    #[test]
    fn mic_latency_over_budget_is_reported_once() {
        let fake = FakeDevices::default();
        let (mut d, ev) = dsp(2, Box::new(fake.clone()));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(2, 1, 2))); // coordinator: its budget applies
        let budget = d.settings.coordinator.mic_latency_ns;
        let over = |ev: &crossbeam_channel::Receiver<RuntimeEvent>| {
            split_events(ev)
                .0
                .iter()
                .filter(|m| m.as_str() == MIC_OVER_BUDGET_MSG)
                .count()
        };
        // A 1 s backlog drained at once, then live audio: not reported.
        let mut idx = 0;
        for k in 0..100u64 {
            fake.mic_block(idx, t0 + k * 10 * MS);
            idx += 480;
        }
        let mut t = t0 + 1_020 * MS;
        let mut run = |d: &mut Dsp, latency: u64, steps: usize| {
            for _ in 0..steps {
                fake.mic_block(idx, t - latency);
                idx += 480;
                d.step(t);
                t += 10 * MS;
            }
        };
        run(&mut d, 20 * MS, 150);
        assert_eq!(over(&ev), 0);
        // Input latency just under the budget: the newest complete frame is then ~one frame
        // older than the budget, and must still be used.
        run(&mut d, budget - 5 * MS, 200);
        assert_eq!(
            over(&ev),
            0,
            "a frame is only complete FRAME_NS after its start"
        );
        // Input latency alone now exceeds the budget: every frame is dropped. Reported once.
        run(&mut d, budget + 60 * MS, 300);
        assert_eq!(over(&ev), 1);
    }

    #[test]
    fn members_judge_their_uplink_by_a_generous_fixed_budget_and_never_report_it() {
        let fake = FakeDevices::default();
        let (mut d, ev) = dsp(2, Box::new(fake.clone()));
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2))); // member
        let local_budget = d.settings.coordinator.mic_latency_ns;
        let (mut idx, mut t) = (0, t0 + 20 * MS);
        let mut run = |d: &mut Dsp, latency: u64, steps: usize| {
            for _ in 0..steps {
                fake.mic_block(idx, t - latency);
                idx += 480;
                d.step(t);
                t += 10 * MS;
            }
        };
        let over_since = |d: &Dsp| d.capture.running.as_ref().unwrap().over_budget_since;
        // Over our own (coordinator) setting but well within the uplink budget: still fresh.
        run(&mut d, local_budget + 60 * MS, 150);
        assert_eq!(over_since(&d), None);
        // Beyond even the uplink budget: dropped, but a member has no budget advice to give.
        run(&mut d, MEMBER_UPLINK_BUDGET_NS + 60 * MS, 300);
        assert!(over_since(&d).is_some());
        let (errs, _) = split_events(&ev);
        assert!(!errs.iter().any(|m| m == MIC_OVER_BUDGET_MSG), "{errs:?}");
    }

    #[test]
    fn dead_speaker_is_detected_without_a_speaker_pipeline() {
        let fake = FakeDevices::default();
        let (mut d, ev) = dsp(2, Box::new(fake.clone()));
        d.speaker = None; // e.g. the decoder could not be created
        let t0 = now_ns();
        d.handle(RuntimeMsg::SetEnabled(true));
        d.handle(RuntimeMsg::Roles(roles(1, 2, 2))); // member, room speaker
        assert_eq!(fake.opens().1, 1);
        for i in 1..=10u64 {
            fake.report();
            d.step(t0 + i * 10 * MS);
        }
        // The device stops calling back: its leftover reports must not count as activity.
        d.step(t0 + 1_500 * MS);
        let (errs, _) = split_events(&ev);
        assert!(
            errs.iter().any(|m| m.contains("Speaker stopped playing")),
            "{errs:?}"
        );
    }

    #[test]
    fn local_report_is_cleared_when_the_coordinator_or_epoch_changes() {
        let (mut d, _ev) = dsp(2, Box::new(NullAudio));
        let report = || {
            Some(PeerReport {
                peer: PeerId(2),
                rtt_ms: 5.0,
                jitter_ms: 0.0,
                loss_pct: 0.0,
                clock_offset_ms: 0.0,
                drift_ppm: 0.0,
                transport: "lo".into(),
            })
        };
        d.handle(RuntimeMsg::Roles(roles(1, 1, 2)));
        *d.shared.local_report.lock() = report();
        d.handle(RuntimeMsg::Roles(LocalRoles {
            mic_enabled: false,
            ..roles(1, 1, 2)
        }));
        assert!(
            d.shared.local_report.lock().is_some(),
            "same coordinator and epoch: kept"
        );
        d.handle(RuntimeMsg::Roles(roles(3, 3, 2)));
        assert!(d.shared.local_report.lock().is_none(), "new coordinator");
        *d.shared.local_report.lock() = report();
        d.handle(RuntimeMsg::Roles(LocalRoles {
            epoch: Epoch(2),
            ..roles(3, 3, 2)
        }));
        assert!(d.shared.local_report.lock().is_none(), "new epoch");
    }

    #[test]
    fn packet_drain_is_capped_per_wake_but_control_is_drained_fully() {
        let (mut d, _ev) = dsp(2, Box::new(NullAudio));
        let (ctl_tx, ctl_rx) = unbounded();
        let (pkt_tx, pkt_rx) = bounded(PACKET_QUEUE);
        for i in 0..1_000u32 {
            pkt_tx
                .send(RuntimeMsg::Packet {
                    header: RtHeader {
                        kind: PacketKind::Mic,
                        epoch: Epoch(1),
                        stream: StreamId::MIC,
                        sender: PeerId(1),
                        sequence: i,
                        sample_index: 0,
                        timestamp_ns: 0,
                        frame_count: 480,
                    },
                    payload: vec![],
                    arrival_ns: 0,
                })
                .unwrap();
        }
        ctl_tx.send(RuntimeMsg::Mute(true)).unwrap();
        ctl_tx.send(RuntimeMsg::SetEnabled(true)).unwrap();
        assert!(d.drain(&ctl_rx, &pkt_rx));
        assert!(d.muted && d.enabled, "control is drained fully");
        assert_eq!(pkt_rx.len(), 1_000 - MAX_PACKETS_PER_WAKE);
        assert!(d.drain(&ctl_rx, &pkt_rx));
        assert_eq!(pkt_rx.len(), 1_000 - 2 * MAX_PACKETS_PER_WAKE);
        ctl_tx.send(RuntimeMsg::Shutdown).unwrap();
        assert!(!d.drain(&ctl_rx, &pkt_rx));
    }
}
