//! Public core facade used by the FFI layer: one control thread for room/control-plane state,
//! one DSP thread (AudioRuntime) for audio, and a lock-light realtime receive path.
//!
//! Threads:
//! - **control** (`roommesh-control`): owns the [`RoomEngine`] and the [`ControlChannel`]. Fed by
//!   transport events, user commands and runtime events; ticks the engine every 100 ms and
//!   executes its outputs (seal + send, rate-limited connects, events to the [`EventSink`],
//!   roles to the audio runtime). Also forwards `PeerReport`s, publishes connection-quality
//!   changes once a second and runs the handshake watchdog.
//! - **DSP** (`roommesh-dsp`, [`AudioRuntime`]): devices and audio pipelines.
//! - **caller** (whatever thread delivers transport events): realtime packets are opened here
//!   and either answered directly (clock pings, when this peer is the coordinator) or handed to
//!   the DSP thread — they never queue behind control-plane work.
//!
//! Clock pongs: the pong's realtime nonce is `(ClockPong, epoch, CLOCK, sequence)` under our
//! key for that peer, so its sequence must never repeat. A pong therefore takes its sequence
//! from a Core-lifetime counter ([`Core`]'s `pong_seq`), never from the ping it answers — a
//! duplicated or replayed ping would otherwise make us seal two different plaintexts under
//! one nonce. The ping's own sequence travels in the pong payload instead:
//! `[t1, t2, t3, ping_sequence]`, and the pinging DSP accepts a pong only for a ping it still
//! has outstanding with the same t1 (once). Pings themselves pass a per-sender replay window
//! (scoped to the session key and epoch) before they are answered.
use crate::engine::metrics::{merge_local_report, PeerMetrics};
use crate::engine::runtime::{
    AudioBackend, AudioRuntime, AudioSettings, RuntimeEvent, RuntimeMsg, RuntimeSender,
    RuntimeShared,
};
use crate::ids::{Epoch, PeerId, StreamId};
use crate::network::clock_sync::ClockSample;
use crate::network::control::{ControlChannel, ControlEvent, RtSessions};
use crate::network::realtime::{decode_packet, decode_times, encode_times, PacketKind, RtHeader};
use crate::network::secure::RtCipher;
use crate::network::transport::{LocalAdvertisement, PeerTransport, TransportEvent};
use crate::room::engine::{Command, Output, RoomConfig, RoomEngine, RoomError};
use crate::room::events::*;
use crate::room::protocol::ControlMessage;
use crate::room::state::{Capabilities, MemberInfo, PROTOCOL_VERSION};
use crate::time::now_ns;
use crossbeam_channel::{bounded, never, select, unbounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Engine tick period.
const TICK_MS: u64 = 100;
/// Connection-quality evaluation period.
const QUALITY_MS: u64 = 1_000;
/// A handshake outstanding this long gets its connection torn down (and later redialed).
const HANDSHAKE_TIMEOUT_MS: u64 = 5_000;
/// Minimum spacing between two `connect` calls to the same peer.
const CONNECT_INTERVAL_MS: u64 = 2_000;
/// How long `Core::command` waits for the control thread's answer.
const COMMAND_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(3)
};

/// Receives every [`RoomEvent`], on the control thread.
///
/// Contract: `on_event` must not block (hand the event to another queue/thread and return) and
/// must not call back into the [`Core`] synchronously. In particular a synchronous
/// [`Core::command`] from inside `on_event` is rejected at once with [`RoomError::Timeout`]:
/// the control thread that would run it is the one delivering the event. Query methods
/// (`room_snapshot`, `roles`, `metrics`, ...) only read caches and are fine.
pub trait EventSink: Send + Sync {
    fn on_event(&self, event: RoomEvent);
}

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub local_id: PeerId,
    pub name: String,
    pub capabilities: Capabilities,
    pub audio: AudioSettings,
    pub auto_elect: bool,
    pub fallback_speaker: bool,
}
impl CoreConfig {
    pub fn new(local_id: PeerId, name: String) -> Self {
        Self {
            local_id,
            name,
            capabilities: Capabilities::full(),
            audio: AudioSettings::default(),
            auto_elect: true,
            fallback_speaker: false,
        }
    }
}

enum CoreMsg {
    Transport(TransportEvent),
    /// A room command and where to answer it. The flag is claimed (set) by whichever side gets
    /// there first: the control thread, which then runs it, or a timed-out caller, which
    /// abandons it — so a command reported as timed out is never applied.
    Command(Command, Sender<Result<(), RoomError>>, Arc<AtomicBool>),
    Policies {
        auto_elect: bool,
        fallback_speaker: bool,
    },
    LocalInfo {
        name: String,
        capabilities: Capabilities,
    },
    Shutdown,
}

/// State published by the control thread for cheap reads from any thread.
#[derive(Default)]
struct Cache {
    snapshot: Mutex<Option<RoomSnapshot>>,
    nearby: Mutex<Vec<NearbyPeer>>,
    roles: Mutex<Option<LocalRoles>>,
    /// The room engine's view: this peer is the current coordinator (answers clock pings even
    /// while its own audio is disabled or its pipeline failed).
    is_coordinator: AtomicBool,
}

/// The RoomMesh core. The [`PeerTransport`] and [`EventSink`] given to [`Core::new`] must honour
/// their non-blocking, non-re-entrant contracts (see their docs). Dropping the Core stops the
/// control thread and joins the DSP thread.
pub struct Core {
    local: PeerId,
    name: Mutex<String>,
    tx: Sender<CoreMsg>,
    /// The control thread (re-entrant `command` calls from it fail fast).
    control_thread: std::thread::ThreadId,
    transport: Arc<dyn PeerTransport>,
    sessions: RtSessions,
    runtime: AudioRuntime,
    runtime_shared: Arc<RuntimeShared>,
    cache: Arc<Cache>,
    muted: AtomicBool,
    /// Sequence numbers of the clock pongs we seal (see module docs).
    pong_seq: AtomicU32,
    /// Per-sender replay window for clock pings.
    ping_replay: Mutex<HashMap<PeerId, ReplayWindow>>,
}

/// Sliding anti-replay window over one sender's clock-ping sequence numbers, scoped to one
/// realtime session key (its cipher instance, held so the identity can't be reused; a rekey
/// resets the window) and epoch.
#[derive(Default)]
struct ReplayWindow {
    scope: Option<(Arc<RtCipher>, Epoch)>,
    highest: u32,
    /// Bit `i` set: `highest - i` was seen.
    seen: u64,
}

impl ReplayWindow {
    const WIDTH: u32 = 64;
    /// Whether `seq` is new (and records it). Anything older than the window is rejected.
    fn accept(&mut self, key: &Arc<RtCipher>, epoch: Epoch, seq: u32) -> bool {
        let same = self
            .scope
            .as_ref()
            .is_some_and(|(k, e)| Arc::ptr_eq(k, key) && *e == epoch);
        if !same {
            *self = Self {
                scope: Some((key.clone(), epoch)),
                highest: seq,
                seen: 1,
            };
            return true;
        }
        if seq > self.highest {
            let shift = seq - self.highest;
            self.seen = if shift >= Self::WIDTH {
                0
            } else {
                self.seen << shift
            };
            self.seen |= 1;
            self.highest = seq;
            return true;
        }
        let back = self.highest - seq;
        if back >= Self::WIDTH || self.seen & (1 << back) != 0 {
            return false;
        }
        self.seen |= 1 << back;
        true
    }
}

impl Core {
    pub fn new(
        cfg: CoreConfig,
        transport: Arc<dyn PeerTransport>,
        sink: Arc<dyn EventSink>,
        backend: Box<dyn AudioBackend>,
    ) -> Self {
        let control = ControlChannel::new(cfg.local_id, cfg.name.clone());
        let sessions = control.realtime_sessions();
        let (rt_ev_tx, rt_ev_rx) = unbounded::<RuntimeEvent>();
        let runtime = AudioRuntime::spawn(
            cfg.local_id,
            backend,
            transport.clone(),
            sessions.clone(),
            rt_ev_tx,
            cfg.audio.clone(),
        );
        let runtime_shared = runtime.shared();
        let (tx, rx) = unbounded::<CoreMsg>();
        let cache = Arc::new(Cache::default());
        let ctl = ControlLoop {
            engine: RoomEngine::new(RoomConfig {
                auto_elect: cfg.auto_elect,
                fallback_speaker_to_coordinator: cfg.fallback_speaker,
                ..RoomConfig::new(MemberInfo {
                    id: cfg.local_id,
                    name: cfg.name.clone(),
                    mic_enabled: true,
                    capabilities: cfg.capabilities,
                })
            }),
            control,
            transport: transport.clone(),
            sink,
            cache: cache.clone(),
            runtime_tx: runtime.sender(),
            runtime_shared: runtime_shared.clone(),
            last_connect: HashMap::new(),
            last_tick_ms: 0,
            last_quality: HashMap::new(),
            last_quality_ms: 0,
            local: cfg.local_id,
        };
        let control_thread = std::thread::Builder::new()
            .name("roommesh-control".into())
            .spawn(move || {
                let sink = ctl.sink.clone();
                let run = std::panic::AssertUnwindSafe(move || ctl.run(rx, rt_ev_rx));
                if std::panic::catch_unwind(run).is_err() {
                    log::error!("roommesh-control panicked; room control has stopped");
                    sink.on_event(RoomEvent::Error {
                        message: "RoomMesh stopped working (internal error). Restart the app."
                            .into(),
                    });
                }
            })
            .expect("spawn control")
            .thread()
            .id();
        Self {
            local: cfg.local_id,
            name: Mutex::new(cfg.name),
            tx,
            control_thread,
            transport,
            sessions,
            runtime,
            runtime_shared,
            cache,
            muted: AtomicBool::new(false),
            pong_seq: AtomicU32::new(0),
            ping_replay: Mutex::new(HashMap::new()),
        }
    }

    pub fn local_peer(&self) -> PeerId {
        self.local
    }

    /// Starts advertising and browsing.
    pub fn start(&self) {
        // Never hold the name lock across the foreign call (it may call `set_local_info`).
        let name = self.name.lock().clone();
        self.transport.start(LocalAdvertisement {
            peer_id: self.local,
            name,
            protocol_version: PROTOCOL_VERSION,
        });
    }
    pub fn stop(&self) {
        self.transport.stop();
    }

    /// Entry point for every transport callback. Realtime packets are handled on the calling
    /// thread; everything else is queued for the control thread.
    pub fn handle_transport_event(&self, ev: TransportEvent) {
        match ev {
            TransportEvent::Realtime(p) => self.on_realtime(&p),
            other => {
                let _ = self.tx.send(CoreMsg::Transport(other));
            }
        }
    }

    /// Opens a realtime packet: clock pings are answered right here (when we coordinate),
    /// pongs and audio go to the DSP thread. Unknown senders / failed authentication: dropped.
    pub fn on_realtime(&self, packet: &[u8]) {
        let arrival = now_ns();
        let Ok((h, _)) = decode_packet(packet) else {
            return;
        };
        let Some(cipher) = self.sessions.read().get(&h.sender).cloned() else {
            return;
        };
        let Ok((h, payload)) = cipher.open(packet) else {
            return;
        };
        match h.kind {
            PacketKind::ClockPing => {
                if !self.cache.is_coordinator.load(Ordering::Relaxed) {
                    return;
                }
                // Replayed pings are not answered (no amplification, no stale samples).
                let fresh = self
                    .ping_replay
                    .lock()
                    .entry(h.sender)
                    .or_default()
                    .accept(&cipher, h.epoch, h.sequence);
                if !fresh {
                    return;
                }
                let Some(&t1) = decode_times(&payload).first() else {
                    return;
                };
                let t3 = now_ns();
                let rh = RtHeader {
                    kind: PacketKind::ClockPong,
                    epoch: h.epoch,
                    stream: StreamId::CLOCK,
                    sender: self.local,
                    sequence: self.pong_seq.fetch_add(1, Ordering::Relaxed),
                    sample_index: 0,
                    timestamp_ns: t3,
                    frame_count: 0,
                };
                let body = encode_times(&[t1, arrival, t3, h.sequence as u64]);
                self.transport
                    .send_realtime(h.sender, cipher.seal(&rh, &body));
            }
            PacketKind::ClockPong => {
                if let [t1, t2, t3, ping_seq] = decode_times(&payload)[..] {
                    let Ok(ping_seq) = u32::try_from(ping_seq) else {
                        return;
                    };
                    self.runtime.send(RuntimeMsg::Pong {
                        sample: ClockSample {
                            t1,
                            t2,
                            t3,
                            t4: arrival,
                        },
                        from: h.sender,
                        ping_seq,
                    });
                }
            }
            _ => self.runtime.send(RuntimeMsg::Packet {
                header: h,
                payload,
                arrival_ns: arrival,
            }),
        }
    }

    /// Runs a room command on the control thread and waits (up to 3 s) for its result.
    ///
    /// On `Ok`, the caches behind [`room_snapshot`](Self::room_snapshot), [`nearby`](Self::nearby)
    /// and [`roles`](Self::roles) already reflect the command (its events are delivered to the
    /// sink right after). [`RoomError::Timeout`] means the command was not applied: the control
    /// thread was busy for 3 s, or this was called from the control thread itself (i.e. from
    /// inside [`EventSink::on_event`]), which fails immediately.
    pub fn command(&self, c: Command) -> Result<(), RoomError> {
        if std::thread::current().id() == self.control_thread {
            log::error!("Core::command called re-entrantly from an event callback; rejected");
            return Err(RoomError::Timeout);
        }
        let (tx, rx) = bounded(1);
        let claim = Arc::new(AtomicBool::new(false));
        if self
            .tx
            .send(CoreMsg::Command(c, tx, claim.clone()))
            .is_err()
        {
            return Err(RoomError::NotInRoom); // control thread gone
        }
        match rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                if !claim.swap(true, Ordering::SeqCst) {
                    return Err(RoomError::Timeout); // abandoned: the control thread skips it
                }
                // The control thread took it just now; its answer is imminent.
                rx.recv_timeout(COMMAND_TIMEOUT).unwrap_or_else(|_| {
                    log::error!("room command started but its result never arrived");
                    Err(RoomError::Timeout)
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(RoomError::NotInRoom),
        }
    }

    pub fn room_snapshot(&self) -> Option<RoomSnapshot> {
        self.cache.snapshot.lock().clone()
    }
    pub fn nearby(&self) -> Vec<NearbyPeer> {
        self.cache.nearby.lock().clone()
    }
    pub fn roles(&self) -> LocalRoles {
        self.cache
            .roles
            .lock()
            .clone()
            .unwrap_or_else(LocalRoles::none)
    }
    /// Per-peer metrics. On the coordinator these come from its pipelines and the members'
    /// `PeerReport`s; elsewhere the entry for the coordinator is filled from our own link to it.
    pub fn metrics(&self) -> Vec<PeerMetrics> {
        merged_metrics(&self.runtime_shared, &self.roles())
    }
    pub fn virtual_device_ok(&self) -> bool {
        self.runtime_shared
            .virtual_device_ok
            .load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        self.runtime.send(RuntimeMsg::Mute(muted));
    }
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn start_audio(&self) {
        self.runtime.send(RuntimeMsg::SetEnabled(true));
    }
    pub fn stop_audio(&self) {
        self.runtime.send(RuntimeMsg::SetEnabled(false));
    }
    pub fn update_audio_settings(&self, s: AudioSettings) {
        self.runtime.send(RuntimeMsg::Settings(s));
    }
    pub fn set_policies(&self, auto_elect: bool, fallback_speaker: bool) {
        let _ = self.tx.send(CoreMsg::Policies {
            auto_elect,
            fallback_speaker,
        });
    }
    pub fn set_local_info(&self, name: String, capabilities: Capabilities) {
        *self.name.lock() = name.clone();
        let _ = self.tx.send(CoreMsg::LocalInfo { name, capabilities });
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.tx.send(CoreMsg::Shutdown);
    }
}

struct ControlLoop {
    engine: RoomEngine,
    control: ControlChannel,
    transport: Arc<dyn PeerTransport>,
    sink: Arc<dyn EventSink>,
    cache: Arc<Cache>,
    runtime_tx: RuntimeSender,
    runtime_shared: Arc<RuntimeShared>,
    last_connect: HashMap<PeerId, u64>,
    last_tick_ms: u64,
    last_quality: HashMap<PeerId, ConnectionQuality>,
    last_quality_ms: u64,
    local: PeerId,
}

fn now_ms() -> u64 {
    now_ns() / 1_000_000
}

/// The metrics book merged with our own link report (see [`merge_local_report`]): the single
/// source for both `Core::metrics()` and connection-quality events.
fn merged_metrics(shared: &RuntimeShared, roles: &LocalRoles) -> Vec<PeerMetrics> {
    let mut v = shared.metrics.lock().snapshot();
    let report = shared.local_report.lock().clone();
    merge_local_report(&mut v, roles, report.as_ref());
    v
}

impl ControlLoop {
    fn run(mut self, rx: Receiver<CoreMsg>, mut rt_rx: Receiver<RuntimeEvent>) {
        loop {
            select! {
                recv(rx) -> m => match m {
                    Ok(CoreMsg::Shutdown) | Err(_) => return,
                    Ok(m) => self.handle(m),
                },
                recv(rt_rx) -> e => match e {
                    Ok(e) => self.runtime_event(e),
                    // DSP thread gone: stop polling a disconnected channel (it'd spin).
                    Err(_) => rt_rx = never(),
                },
                default(Duration::from_millis(TICK_MS / 2)) => {},
            }
            let now = now_ms();
            if now.saturating_sub(self.last_tick_ms) >= TICK_MS {
                self.last_tick_ms = now;
                self.engine.tick(now);
                // Handshake watchdog: a connection whose handshake never completes is dropped;
                // the engine redials it (rate-limited) while it still has traffic for the peer.
                for p in self.control.stalled(now, HANDSHAKE_TIMEOUT_MS) {
                    log::info!("handshake with {p} stalled; reconnecting");
                    self.transport.disconnect(p);
                }
            }
            if now.saturating_sub(self.last_quality_ms) >= QUALITY_MS {
                self.last_quality_ms = now;
                self.quality();
            }
            self.flush(now);
        }
    }

    fn handle(&mut self, m: CoreMsg) {
        let now = now_ms();
        match m {
            CoreMsg::Transport(ev) => self.transport_event(now, ev),
            CoreMsg::Command(c, reply, claim) => {
                if claim.swap(true, Ordering::SeqCst) {
                    return; // the caller timed out and gave up on it
                }
                let result = self.engine.command(now, c);
                // Caches first, so the caller sees them fresh on Ok; then the events.
                let events = self.execute(now);
                let _ = reply.send(result);
                self.deliver(events);
            }
            CoreMsg::Policies {
                auto_elect,
                fallback_speaker,
            } => {
                self.engine.set_auto_elect(auto_elect);
                self.engine.set_fallback_speaker(fallback_speaker);
            }
            CoreMsg::LocalInfo { name, capabilities } => {
                self.control.set_name(name.clone());
                self.engine.set_local_info(MemberInfo {
                    id: self.local,
                    name,
                    mic_enabled: true, // the engine keeps the room's value
                    capabilities,
                });
            }
            CoreMsg::Shutdown => {}
        }
    }

    fn transport_event(&mut self, now: u64, ev: TransportEvent) {
        match ev {
            TransportEvent::Discovered { peer, name } => self.engine.on_discovered(peer, name),
            TransportEvent::Lost(p) => self.engine.on_lost(p),
            TransportEvent::Connected(p) => {
                if let Some(hello) = self.control.on_connected(p, now) {
                    self.transport.send_control(p, hello);
                }
            }
            TransportEvent::Disconnected(p) => {
                if self.control.on_disconnected(p).is_some() {
                    self.engine.on_session_down(now, p);
                }
            }
            TransportEvent::Control { peer, frame } => match self.control.on_frame(peer, &frame) {
                Ok(out) => {
                    if let Some(r) = out.reply {
                        self.transport.send_control(peer, r);
                    }
                    match out.event {
                        Some(ControlEvent::SessionUp { peer, name, sas }) => {
                            self.engine.on_session_up(now, peer, name, sas);
                        }
                        Some(ControlEvent::Message {
                            msg: ControlMessage::PeerReport(r),
                            from,
                        }) => {
                            // Only the peer itself may report on its own link, only to the
                            // coordinator, and only while it is a member of our room.
                            let accept =
                                r.peer == from
                                    && self.cache.roles.lock().as_ref().is_some_and(|l| {
                                        l.is_coordinator && l.members.contains(&from)
                                    });
                            if accept {
                                self.runtime_shared.metrics.lock().apply_report(&r);
                            }
                        }
                        Some(ControlEvent::Message { from, msg }) => {
                            self.engine.on_message(now, from, msg)
                        }
                        // A legitimate in-band re-open (HELLO_REQUEST we answered, or a peer
                        // that's still connected but restarted): notify the engine only, the
                        // connection itself is fine (or already being re-handshaked over).
                        Some(ControlEvent::SessionDown(p)) => {
                            self.engine.on_session_down(now, p);
                        }
                        // Crypto desync with no re-open in progress: the connection itself is
                        // suspect, so tear it down too and let a fresh `on_connected` start a
                        // clean handshake.
                        Some(ControlEvent::SessionFailed(p)) => {
                            self.engine.on_session_down(now, p);
                            self.transport.disconnect(p);
                        }
                        None => {}
                    }
                }
                Err(e) => log::warn!("control frame from {peer}: {e}"),
            },
            TransportEvent::Realtime(_) => {} // handled on the caller's thread
        }
    }

    fn runtime_event(&mut self, e: RuntimeEvent) {
        let now = now_ms();
        match e {
            RuntimeEvent::Selection { primary, secondary } => {
                self.engine.on_local_selection(now, primary, secondary)
            }
            RuntimeEvent::AecStatus(converged) => self
                .sink
                .on_event(RoomEvent::AecStatusChanged { converged }),
            RuntimeEvent::Error(message) => self.sink.on_event(RoomEvent::Error { message }),
            RuntimeEvent::Report(r) => {
                let Some(c) = self.engine.manifest().map(|m| m.coordinator) else {
                    return;
                };
                if c != self.local {
                    if let Some(f) = self.control.seal(c, &ControlMessage::PeerReport(r)) {
                        self.transport.send_control(c, f);
                    }
                }
            }
        }
    }

    /// Emits `ConnectionQualityChanged` for every peer whose classification changed.
    fn quality(&mut self) {
        let roles = self
            .cache
            .roles
            .lock()
            .clone()
            .unwrap_or_else(LocalRoles::none);
        let metrics = merged_metrics(&self.runtime_shared, &roles);
        self.last_quality
            .retain(|p, _| metrics.iter().any(|m| m.peer == *p));
        for m in metrics.into_iter().filter(|m| m.peer != self.local) {
            if self.last_quality.get(&m.peer) != Some(&m.quality) {
                self.last_quality.insert(m.peer, m.quality);
                self.sink.on_event(RoomEvent::ConnectionQualityChanged {
                    peer: m.peer,
                    quality: m.quality,
                });
            }
        }
    }

    fn flush(&mut self, now: u64) {
        let events = self.execute(now);
        self.deliver(events);
    }

    fn deliver(&self, events: Vec<RoomEvent>) {
        for e in events {
            self.sink.on_event(e);
        }
    }

    /// Executes the engine's pending outputs: sends, connects, cache and runtime updates.
    /// Returns the events for the sink (delivered by the caller, after any command reply).
    fn execute(&mut self, now: u64) -> Vec<RoomEvent> {
        let mut events = Vec::new();
        for o in self.engine.take_outputs() {
            match o {
                Output::Send { to, msg } => {
                    if let Some(f) = self.control.seal(to, &msg) {
                        self.transport.send_control(to, f);
                    }
                }
                Output::Connect(p) => {
                    let due = self
                        .last_connect
                        .get(&p)
                        .is_none_or(|t| now.saturating_sub(*t) >= CONNECT_INTERVAL_MS);
                    if due {
                        self.last_connect.insert(p, now);
                        self.transport.connect(p);
                    }
                }
                Output::Event(e) => {
                    match &e {
                        RoomEvent::RoomChanged(s) => {
                            if let Some(s) = s {
                                let mut book = self.runtime_shared.metrics.lock();
                                for m in &s.members {
                                    if m.is_local {
                                        // Listed only while its own mic is analysed.
                                        book.note_name(m.id, m.name.clone());
                                    } else {
                                        book.set_name(m.id, m.name.clone());
                                    }
                                }
                            }
                            *self.cache.snapshot.lock() = s.clone();
                        }
                        RoomEvent::NearbyChanged(n) => *self.cache.nearby.lock() = n.clone(),
                        _ => {}
                    }
                    events.push(e);
                }
                Output::Roles(r) => {
                    {
                        let mut book = self.runtime_shared.metrics.lock();
                        book.retain(&r.members);
                        if r.is_coordinator {
                            for p in r.members.iter().filter(|p| !r.enabled_mics.contains(p)) {
                                book.clear_mic(*p);
                            }
                        } else {
                            // Reports and mic metrics exist only on the coordinator.
                            book.reset_to_names();
                        }
                    }
                    self.cache
                        .is_coordinator
                        .store(r.is_coordinator, Ordering::Relaxed);
                    *self.cache.roles.lock() = Some(r.clone());
                    self.runtime_tx.send(RuntimeMsg::Roles(r));
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::device_io::*;
    use crate::audio::shared_layout::RING_FRAMES;
    use crate::audio::virtual_device::SharedRegion;
    use crate::engine::runtime::{AudioBackend, NullAudio};
    use crate::network::loopback::LoopbackNetwork;
    use crossbeam_channel::unbounded;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Collector(Mutex<Vec<RoomEvent>>);
    impl EventSink for Collector {
        fn on_event(&self, e: RoomEvent) {
            self.0.lock().push(e);
        }
    }

    fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    struct Node {
        core: Arc<Core>,
        events: Arc<Collector>,
    }
    impl Drop for Node {
        /// Quiesce audio before the Core drops.
        fn drop(&mut self) {
            self.core.stop_audio();
            self.core.stop();
        }
    }

    /// A started Core on the loopback network. Its transport pump holds it weakly, so the Core
    /// drops (joining its threads) with the last strong reference.
    fn spawn_core(
        net: &Arc<LoopbackNetwork>,
        id: u64,
        backend: Box<dyn AudioBackend>,
        sink: Arc<dyn EventSink>,
    ) -> Arc<Core> {
        let (tx, rx) = unbounded();
        let transport = net.transport(PeerId(id), tx);
        let cfg = CoreConfig::new(PeerId(id), format!("Mac {id}"));
        let core = Arc::new(Core::new(cfg, transport, sink, backend));
        let weak = Arc::downgrade(&core);
        std::thread::spawn(move || {
            for ev in rx {
                let Some(c) = weak.upgrade() else { break };
                c.handle_transport_event(ev);
            }
        });
        core.start();
        core
    }

    fn node(net: &Arc<LoopbackNetwork>, id: u64, backend: Box<dyn AudioBackend>) -> Node {
        let events = Arc::new(Collector::default());
        let core = spawn_core(net, id, backend, events.clone());
        Node { core, events }
    }

    fn join(host: &Node, guest: &Node, guest_id: u64) {
        host.core
            .command(Command::Invite(PeerId(guest_id)))
            .unwrap();
        let mut room = None;
        wait("invite", 5, || {
            room = guest.events.0.lock().iter().rev().find_map(|e| match e {
                RoomEvent::InviteReceived { room_id, .. } => Some(*room_id),
                _ => None,
            });
            room.is_some()
        });
        guest
            .core
            .command(Command::RespondToInvite {
                room_id: room.unwrap(),
                accept: true,
            })
            .unwrap();
    }

    /// Sink that can block the control thread or call back into the Core from `on_event`.
    #[derive(Default)]
    struct HookSink {
        core: std::sync::OnceLock<std::sync::Weak<Core>>,
        /// Next RoomChanged blocks the control thread this long.
        block_ms: std::sync::atomic::AtomicU64,
        /// Next event runs a command re-entrantly; its result and duration land here.
        reenter: AtomicBool,
        reentrant: Mutex<Option<(Result<(), RoomError>, Duration)>>,
    }
    impl EventSink for HookSink {
        fn on_event(&self, e: RoomEvent) {
            if matches!(e, RoomEvent::RoomChanged(_)) {
                let ms = self.block_ms.swap(0, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(ms));
            }
            if self.reenter.swap(false, Ordering::SeqCst) {
                if let Some(c) = self.core.get().and_then(|w| w.upgrade()) {
                    let t = Instant::now();
                    let r = c.command(Command::Rename("re-entrant".into()));
                    *self.reentrant.lock() = Some((r, t.elapsed()));
                }
            }
        }
    }
    fn hooked(net: &Arc<LoopbackNetwork>, id: u64) -> (Arc<Core>, Arc<HookSink>) {
        let sink = Arc::new(HookSink::default());
        let core = spawn_core(net, id, Box::new(NullAudio), sink.clone());
        let _ = sink.core.set(Arc::downgrade(&core));
        (core, sink)
    }

    #[test]
    fn command_ok_means_caches_are_already_fresh() {
        let net = LoopbackNetwork::new();
        let (core, _sink) = hooked(&net, 1);
        for i in 0..20 {
            core.command(Command::CreateRoom {
                name: format!("R{i}"),
            })
            .unwrap();
            let snap = core.room_snapshot();
            assert_eq!(snap.map(|s| s.name), Some(format!("R{i}")), "round {i}");
            assert!(core.roles().is_coordinator, "round {i}");
            core.command(Command::Leave).unwrap();
            assert!(core.room_snapshot().is_none(), "round {i}");
        }
    }

    #[test]
    fn reentrant_command_from_the_sink_fails_fast() {
        let net = LoopbackNetwork::new();
        let (core, sink) = hooked(&net, 1);
        sink.reenter.store(true, Ordering::SeqCst);
        core.command(Command::CreateRoom { name: "R".into() })
            .unwrap();
        wait("the sink ran", 3, || sink.reentrant.lock().is_some());
        let (r, took) = sink.reentrant.lock().take().unwrap();
        assert_eq!(r, Err(RoomError::Timeout));
        assert!(took < Duration::from_millis(100), "took {took:?}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            core.room_snapshot().unwrap().name,
            "R",
            "must not be applied"
        );
    }

    #[test]
    fn timed_out_command_is_never_applied() {
        let net = LoopbackNetwork::new();
        let (core, sink) = hooked(&net, 1);
        core.command(Command::CreateRoom { name: "R".into() })
            .unwrap();
        // The next RoomChanged holds the control thread well past the command timeout.
        sink.block_ms
            .store(COMMAND_TIMEOUT.as_millis() as u64 * 3, Ordering::SeqCst);
        core.command(Command::Rename("First".into())).unwrap();
        let t = Instant::now();
        assert_eq!(
            core.command(Command::Rename("Second".into())),
            Err(RoomError::Timeout)
        );
        assert!(t.elapsed() >= COMMAND_TIMEOUT);
        wait("control thread free again", 5, || {
            core.command(Command::Rename("First".into())).is_ok()
        });
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(core.room_snapshot().unwrap().name, "First");
    }

    /// Panics on the first RoomChanged; records every Error event.
    #[derive(Default)]
    struct PanicSink {
        panicked: AtomicBool,
        errors: Mutex<Vec<String>>,
    }
    impl EventSink for PanicSink {
        fn on_event(&self, e: RoomEvent) {
            match e {
                RoomEvent::RoomChanged(_) if !self.panicked.swap(true, Ordering::SeqCst) => {
                    panic!("injected control-thread panic")
                }
                RoomEvent::Error { message } => self.errors.lock().push(message),
                _ => {}
            }
        }
    }

    #[test]
    fn control_thread_death_is_reported() {
        let net = LoopbackNetwork::new();
        let sink = Arc::new(PanicSink::default());
        let core = spawn_core(&net, 1, Box::new(NullAudio), sink.clone());
        core.command(Command::CreateRoom { name: "R".into() })
            .unwrap();
        wait("error event", 3, || !sink.errors.lock().is_empty());
        assert!(sink.errors.lock()[0].contains("stopped working"));
        assert!(core.command(Command::Leave).is_err());
    }

    #[test]
    fn three_cores_form_room_change_coordinator_and_sync_clocks() {
        let net = LoopbackNetwork::new();
        let (a, b, c) = (
            node(&net, 1, Box::new(NullAudio)),
            node(&net, 2, Box::new(NullAudio)),
            node(&net, 3, Box::new(NullAudio)),
        );
        a.core
            .command(Command::CreateRoom {
                name: "Conference Room".into(),
            })
            .unwrap();
        join(&a, &b, 2);
        join(&a, &c, 3);
        wait("3 members everywhere", 8, || {
            [&a, &b, &c]
                .iter()
                .all(|n| n.core.room_snapshot().is_some_and(|s| s.members.len() == 3))
        });
        c.core.command(Command::SetCoordinator(PeerId(2))).unwrap();
        wait("coordinator = 2", 8, || {
            [&a, &b, &c].iter().all(|n| {
                n.core
                    .room_snapshot()
                    .is_some_and(|s| s.coordinator == PeerId(2))
                    && n.core.roles().coordinator == Some(PeerId(2))
            })
        });
        for n in [&a, &b, &c] {
            n.core.start_audio();
        }
        // A real round trip was measured (rtt > 0 needs actual ping/pong samples; an unsynced
        // report carries 0) and reported to the coordinator.
        wait("clock reports reach coordinator", 8, || {
            b.core
                .metrics()
                .iter()
                .any(|m| m.peer == PeerId(1) && m.rtt_ms.is_some_and(|r| r > 0.0))
        });
        // ... and A sees its own link to the coordinator.
        wait("A's own link to the coordinator", 8, || {
            a.core
                .metrics()
                .iter()
                .any(|m| m.peer == PeerId(2) && m.rtt_ms.is_some_and(|r| r > 0.0))
        });
    }

    /// A raw peer (its own ControlChannel over the loopback) with a session to `core`.
    struct RawPeer {
        transport: Arc<crate::network::loopback::LoopbackTransport>,
        rx: Receiver<TransportEvent>,
        control: ControlChannel,
    }
    fn raw_peer(net: &Arc<LoopbackNetwork>, id: u64, to: &Node, to_id: u64) -> RawPeer {
        let (tx, rx) = unbounded();
        let transport = net.transport(PeerId(id), tx);
        let mut p = RawPeer {
            transport,
            rx,
            control: ControlChannel::new(PeerId(id), "raw".into()),
        };
        p.transport.connect(PeerId(to_id));
        let end = Instant::now() + Duration::from_secs(5);
        while !p.control.has_session(PeerId(to_id)) {
            assert!(Instant::now() < end, "raw handshake timed out");
            match p.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(TransportEvent::Connected(q)) => {
                    if let Some(f) = p.control.on_connected(q, 0) {
                        p.transport.send_control(q, f);
                    }
                }
                Ok(TransportEvent::Control { peer, frame }) => {
                    if let Some(r) = p.control.on_frame(peer, &frame).unwrap().reply {
                        p.transport.send_control(peer, r);
                    }
                }
                _ => {}
            }
        }
        wait("core sees the raw session", 5, || {
            to.core
                .nearby()
                .iter()
                .any(|n| n.id == PeerId(id) && n.connected)
        });
        p
    }

    #[test]
    fn clock_pings_are_answered_once_with_distinct_pong_sequences() {
        let net = LoopbackNetwork::new();
        let a = node(&net, 1, Box::new(NullAudio));
        let raw = raw_peer(&net, 2, &a, 1);
        // Coordinator by room state only: audio stays disabled, pings must still be answered.
        a.core
            .command(Command::CreateRoom { name: "R".into() })
            .unwrap();
        wait("A coordinates", 3, || a.core.roles().is_coordinator);
        let epoch = a.core.room_snapshot().unwrap().epoch;
        let cipher = raw.control.realtime_sessions().read()[&PeerId(1)].clone();
        let ping = |seq: u32| {
            cipher.seal(
                &RtHeader {
                    kind: PacketKind::ClockPing,
                    epoch,
                    stream: StreamId::CLOCK,
                    sender: PeerId(2),
                    sequence: seq,
                    sample_index: 0,
                    timestamp_ns: 123,
                    frame_count: 0,
                },
                &encode_times(&[123 + seq as u64]),
            )
        };
        // A duplicated (replayed) ping is answered once; the next ping is answered too.
        for p in [ping(7), ping(7), ping(8), ping(7)] {
            a.core.handle_transport_event(TransportEvent::Realtime(p));
        }
        let mut pongs = Vec::new();
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            if let Ok(TransportEvent::Realtime(p)) = raw.rx.recv_timeout(Duration::from_millis(50))
            {
                pongs.push(cipher.open(&p).expect("pong authenticates"));
            }
        }
        assert_eq!(
            pongs.len(),
            2,
            "each distinct ping is answered exactly once"
        );
        for ((h, payload), seq) in pongs.iter().zip([7u64, 8]) {
            assert_eq!(h.kind, PacketKind::ClockPong);
            assert_eq!(h.sender, PeerId(1));
            let t = decode_times(payload);
            assert_eq!(t.len(), 4);
            assert_eq!(
                (t[0], t[3]),
                (123 + seq, seq),
                "t1 and the ping's sequence are echoed"
            );
            assert!(t[2] >= t[1]);
        }
        assert_ne!(
            pongs[0].0.sequence, pongs[1].0.sequence,
            "pong nonces must never repeat"
        );
    }

    #[test]
    fn replay_window_accepts_each_sequence_once_and_resets_on_rekey_or_epoch() {
        use crate::network::secure::{decode_hello, Handshake};
        let cipher = || {
            let (a, b) = (
                Handshake::new(PeerId(1), "a".into()),
                Handshake::new(PeerId(2), "b".into()),
            );
            a.complete(&decode_hello(&b.hello()).unwrap())
                .unwrap()
                .realtime()
        };
        let (c1, c2) = (cipher(), cipher());
        let mut w = ReplayWindow::default();
        let e = Epoch(1);
        assert!(w.accept(&c1, e, 10));
        assert!(!w.accept(&c1, e, 10), "duplicate");
        assert!(w.accept(&c1, e, 12));
        assert!(w.accept(&c1, e, 11), "reordered within the window");
        assert!(!w.accept(&c1, e, 11));
        assert!(w.accept(&c1, e, 200));
        assert!(!w.accept(&c1, e, 12), "older than the window");
        assert!(w.accept(&c1, Epoch(2), 12), "new epoch resets");
        assert!(w.accept(&c2, Epoch(2), 0), "new key resets");
        assert!(!w.accept(&c2, Epoch(2), 0));
    }

    #[test]
    fn peer_reports_apply_only_on_the_coordinator_from_members() {
        use crate::room::protocol::PeerReport;
        let net = LoopbackNetwork::new();
        let a = node(&net, 1, Box::new(NullAudio));
        let mut raw = raw_peer(&net, 2, &a, 1);
        let send_report = |raw: &mut RawPeer| {
            let r = PeerReport {
                peer: PeerId(2),
                rtt_ms: 9.0,
                jitter_ms: 1.0,
                loss_pct: 0.0,
                clock_offset_ms: 0.0,
                drift_ppm: 0.0,
                transport: "lo".into(),
            };
            let f = raw
                .control
                .seal(PeerId(1), &ControlMessage::PeerReport(r))
                .expect("session");
            raw.transport.send_control(PeerId(1), f);
            std::thread::sleep(Duration::from_millis(200));
        };
        let has_report = || {
            a.core
                .metrics()
                .iter()
                .any(|m| m.peer == PeerId(2) && m.rtt_ms.is_some())
        };
        send_report(&mut raw);
        assert!(!has_report(), "not in a room: report must be ignored");
        a.core
            .command(Command::CreateRoom { name: "R".into() })
            .unwrap();
        wait("A coordinates", 3, || a.core.roles().is_coordinator);
        send_report(&mut raw);
        assert!(!has_report(), "reports from non-members must be ignored");
    }

    #[test]
    fn clock_ping_ignored_when_not_coordinator() {
        let net = LoopbackNetwork::new();
        let a = node(&net, 1, Box::new(NullAudio));
        let raw = raw_peer(&net, 2, &a, 1);
        let cipher = raw.control.realtime_sessions().read()[&PeerId(1)].clone();
        let h = RtHeader {
            kind: PacketKind::ClockPing,
            epoch: crate::ids::Epoch(1),
            stream: StreamId::CLOCK,
            sender: PeerId(2),
            sequence: 0,
            sample_index: 0,
            timestamp_ns: 0,
            frame_count: 0,
        };
        a.core.handle_transport_event(TransportEvent::Realtime(
            cipher.seal(&h, &encode_times(&[1])),
        ));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!raw
            .rx
            .try_iter()
            .any(|e| matches!(e, TransportEvent::Realtime(_))));
    }

    // ---- fake devices for the audio end-to-end test ----
    struct FakeAudio {
        region: Option<Arc<SharedRegion>>,
        tone: bool,
        played_energy: Arc<Mutex<f64>>,
        stop: Arc<AtomicBool>,
    }
    impl AudioBackend for FakeAudio {
        fn start_capture(&mut self, _: &DeviceSelector) -> Result<CaptureHandle, DeviceError> {
            let (mut prod, cons) = rtrb::RingBuffer::<CaptureBlock>::new(256);
            let (tone, stop) = (self.tone, self.stop.clone());
            std::thread::spawn(move || {
                let mut counter = 0u64;
                let start = crate::time::now_ns();
                while !stop.load(Ordering::Relaxed) {
                    let due = (crate::time::now_ns() - start) * 48 / 1_000_000;
                    while counter + 480 <= due {
                        let mut b = CaptureBlock {
                            first_frame: counter,
                            capture_ns: start + counter * 1_000_000 / 48,
                            len: 480,
                            samples: [0.0; BLOCK_FRAMES],
                        };
                        for k in 0..480 {
                            let t = (counter + k as u64) as f32 / 48_000.0;
                            let env = 0.55 + 0.45 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                            let v: f32 = (1..=8)
                                .map(|h| {
                                    (2.0 * std::f32::consts::PI * 150.0 * h as f32 * t).sin()
                                        / h as f32
                                })
                                .sum();
                            b.samples[k] = if tone { 0.1 * env * v } else { 0.0 };
                        }
                        let _ = prod.push(b);
                        counter += 480;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            Ok(CaptureHandle {
                blocks: cons,
                sample_rate: 48_000,
                device_name: "Fake Mic".into(),
            })
        }
        fn stop_capture(&mut self) {}
        fn start_playback(&mut self, _: &DeviceSelector) -> Result<PlaybackHandle, DeviceError> {
            let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(48_000);
            let (mut rp, rc) = rtrb::RingBuffer::<PlaybackReport>::new(512);
            let (energy, stop) = (self.played_energy.clone(), self.stop.clone());
            std::thread::spawn(move || {
                let (mut out, mut popped) = (0u64, 0u64);
                while !stop.load(Ordering::Relaxed) {
                    let _ = rp.push(PlaybackReport {
                        output_frames: out,
                        popped_frames: popped,
                        play_ns: crate::time::now_ns() + 10_000_000,
                    });
                    for _ in 0..480 {
                        if let Ok(v) = cons.pop() {
                            popped += 1;
                            *energy.lock() += (v * v) as f64;
                        }
                        out += 1;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            Ok(PlaybackHandle {
                samples: prod,
                reports: rc,
                sample_rate: 48_000,
                device_name: "Fake Speaker".into(),
            })
        }
        fn stop_playback(&mut self) {}
        fn open_virtual_device(
            &mut self,
        ) -> Result<Arc<SharedRegion>, crate::audio::virtual_device::VirtualDeviceError> {
            self.region
                .clone()
                .ok_or(crate::audio::virtual_device::VirtualDeviceError::NotFound(
                    2,
                ))
        }
    }

    #[test]
    fn audio_flows_mic_to_virtual_mic_and_farend_to_room_speaker() {
        let net = LoopbackNetwork::new();
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let b_energy = Arc::new(Mutex::new(0.0));
        let a = node(
            &net,
            1,
            Box::new(FakeAudio {
                region: Some(region.clone()),
                tone: false,
                played_energy: Arc::default(),
                stop: stop.clone(),
            }),
        );
        let b = node(
            &net,
            2,
            Box::new(FakeAudio {
                region: None,
                tone: true,
                played_energy: b_energy.clone(),
                stop: stop.clone(),
            }),
        );
        let mut cfg = AudioSettings::default();
        cfg.coordinator.use_webrtc_aec = false;
        for n in [&a, &b] {
            n.core.update_audio_settings(cfg.clone());
        }
        a.core
            .command(Command::CreateRoom { name: "R".into() })
            .unwrap();
        join(&a, &b, 2);
        wait("2 members", 5, || {
            b.core.room_snapshot().is_some_and(|s| s.members.len() == 2)
        });
        a.core
            .command(Command::SetSpeaker(Some(PeerId(2))))
            .unwrap();
        wait("B is the room speaker", 5, || b.core.roles().is_speaker);
        for n in [&a, &b] {
            n.core.start_audio();
        }

        // Emulate the driver for 4 s: feed far-end into the speaker ring, drain the mic ring.
        let mut pos = 0u64;
        let mut mic_energy = 0.0f64;
        let start = crate::time::now_ns();
        let mut buf = vec![0.0f32; RING_FRAMES];
        let mut far_written = 0u64;
        while crate::time::now_ns() - start < 4_000_000_000 {
            let now = crate::time::now_ns();
            let due = (now - start) * 48 / 1_000_000;
            while far_written + 480 <= due {
                let s: Vec<f32> = (0..480)
                    .map(|k| 0.2 * ((far_written + k) as f32 * 0.03).sin())
                    .collect();
                region.test_driver_write_speaker(&s, now);
                far_written += 480;
            }
            let n = region.test_driver_read_mic(pos, &mut buf);
            pos += n as u64;
            if now - start > 2_000_000_000 {
                mic_energy += buf[..n].iter().map(|v| (v * v) as f64).sum::<f64>();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        assert!(
            mic_energy > 1.0,
            "B's mic should reach A's RoomMesh Microphone (energy {mic_energy})"
        );
        assert!(
            *b_energy.lock() > 1.0,
            "far-end should play on B, the room speaker (energy {})",
            *b_energy.lock()
        );
    }
}
