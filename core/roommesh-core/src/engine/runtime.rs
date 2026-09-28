//! The audio runtime: owns devices and pipelines on one DSP thread and follows `LocalRoles`.
//!
//! One thread, woken every 2 ms or by a message. Per step: clock-sync pings (non-coordinators)
//! → drain capture → uplink or local mic → (coordinator) far-end in from the speaker ring,
//! schedule to the room speaker, produce mic frames on a host-time 10 ms grid into the mic ring
//! → (speaker) keep ~30 ms queued on the physical output → metrics every 250 ms.
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
};
use crate::audio::frames::{FRAME_NS, SAMPLE_RATE};
use crate::audio::shared_layout::SHM_NAME;
use crate::audio::virtual_device::{MicWriter, SharedRegion, SpeakerReader, VirtualDeviceError};
use crate::dsp::arbitration::Selection;
use crate::engine::coordinator::{CoordinatorConfig, CoordinatorPipeline};
use crate::engine::metrics::MetricsBook;
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
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(2);
const METRICS_INTERVAL_NS: u64 = 250_000_000;
const VDEV_RETRY_NS: u64 = 2_000_000_000;
/// Mic-grid lag beyond which the grid is re-anchored at "now" instead of catching up.
const MAX_GRID_LAG_NS: u64 = 200_000_000;
/// Target amount of audio queued on the physical room-speaker output.
const PLAYBACK_QUEUE_S: f64 = 0.03;

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

/// Device access, swappable for tests.
pub trait AudioBackend: Send {
    fn start_capture(&mut self, sel: &DeviceSelector) -> Result<CaptureHandle, DeviceError>;
    fn stop_capture(&mut self);
    fn start_playback(&mut self, sel: &DeviceSelector) -> Result<PlaybackHandle, DeviceError>;
    fn stop_playback(&mut self);
    fn open_virtual_device(&mut self) -> Result<Arc<SharedRegion>, VirtualDeviceError>;
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
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeEvent {
    Selection {
        primary: Option<PeerId>,
        secondary: Option<PeerId>,
    },
    AecStatus(bool),
    Report(PeerReport),
    Error(String),
}

/// State the DSP thread publishes for other threads. The DSP thread only ever holds these locks
/// briefly and uses `try_lock` for the metrics book, so readers can never stall audio.
#[derive(Default)]
pub struct RuntimeShared {
    pub is_coordinator: AtomicBool,
    pub metrics: Mutex<MetricsBook>,
    pub local_report: Mutex<Option<PeerReport>>,
    pub virtual_device_ok: AtomicBool,
}

pub struct AudioRuntime {
    tx: Sender<RuntimeMsg>,
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
        let (tx, rx) = bounded::<RuntimeMsg>(4096);
        let shared = Arc::new(RuntimeShared::default());
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
            .spawn(move || dsp.run(rx))
            .expect("spawn dsp");
        Self {
            tx,
            shared,
            join: Some(join),
        }
    }
    /// Never blocks: a message is dropped if the queue is full (4096 entries).
    pub fn send(&self, m: RuntimeMsg) {
        let _ = self.tx.try_send(m);
    }
    pub fn sender(&self) -> Sender<RuntimeMsg> {
        self.tx.clone()
    }
    pub fn shared(&self) -> Arc<RuntimeShared> {
        self.shared.clone()
    }
    pub fn shutdown(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        let _ = self.tx.send(RuntimeMsg::Shutdown);
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

struct CoordState {
    pipe: CoordinatorPipeline,
    vdev: Option<(MicWriter, SpeakerReader)>,
    farend: FrameAssembler,
    farend_clock: DeviceClock,
    next_out_ns: u64,
    last_vdev_attempt_ns: Option<u64>,
    vdev_error_reported: bool,
    selection: Selection,
}

struct Playback {
    h: PlaybackHandle,
    clock: PlaybackClock,
    pushed: u64,
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
    pings_sent: u32,
    next_ping_ns: u64,
    capture: Option<(CaptureHandle, FrameAssembler)>,
    uplink: Option<MicUplink>,
    playback: Option<Playback>,
    speaker: Option<SpeakerPipeline>,
    coord: Option<CoordState>,
    last_metrics_ns: u64,
    last_aec: bool,
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
        Self {
            local,
            backend,
            transport,
            sessions,
            events,
            shared,
            uplink: MicUplink::new(local, settings.mic_bitrate_bps).ok(),
            settings,
            enabled: false,
            muted: false,
            roles: LocalRoles::none(),
            clock: ClockEstimator::new(),
            ping_seq: 0,
            mic_seq: 0,
            farend_seq: 0,
            pings_sent: 0,
            next_ping_ns: 0,
            capture: None,
            playback: None,
            speaker: SpeakerPipeline::new(Epoch(0), local).ok(),
            coord: None,
            last_metrics_ns: 0,
            last_aec: false,
            play_buf: Vec::new(),
        }
    }

    fn error(&self, msg: String) {
        log::warn!("{msg}");
        let _ = self.events.try_send(RuntimeEvent::Error(msg));
    }

    fn run(mut self, rx: Receiver<RuntimeMsg>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(m) => {
                    if !self.handle(m) {
                        break;
                    }
                    let mut shutdown = false;
                    while let Ok(m) = rx.try_recv() {
                        if !self.handle(m) {
                            shutdown = true;
                            break;
                        }
                    }
                    if shutdown {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.step(now_ns());
        }
        self.stop_all();
    }

    /// Re-applies the current roles from scratch (after devices/pipelines were torn down).
    fn reapply_roles(&mut self) {
        let r = std::mem::replace(&mut self.roles, LocalRoles::none());
        self.apply_roles(r);
    }

    fn handle(&mut self, m: RuntimeMsg) -> bool {
        match m {
            RuntimeMsg::Roles(r) => self.apply_roles(r),
            RuntimeMsg::Settings(s) => self.apply_settings(s),
            RuntimeMsg::Mute(m) => self.muted = m,
            RuntimeMsg::SetEnabled(e) => {
                self.enabled = e;
                if !e {
                    self.stop_all();
                }
                self.reapply_roles();
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
            RuntimeMsg::Pong { sample, from } => {
                if self.roles.coordinator == Some(from) {
                    self.clock.add_sample(sample);
                }
            }
            RuntimeMsg::Shutdown => return false,
        }
        true
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
            self.uplink = MicUplink::new(self.local, s.mic_bitrate_bps).ok();
        }
        if let Some(cs) = self.coord.as_mut() {
            cs.pipe.set_arbitration(s.coordinator.arbitration.clone());
        }
        self.settings = s;
        if devices_changed {
            self.stop_all();
            self.reapply_roles();
        } else if pipeline_changed && self.coord.is_some() {
            self.coord = None;
            self.reapply_roles();
        }
    }

    fn stop_all(&mut self) {
        if self.capture.take().is_some() {
            self.backend.stop_capture();
        }
        if self.playback.take().is_some() {
            self.backend.stop_playback();
        }
        self.coord = None;
        self.shared.is_coordinator.store(false, Ordering::Relaxed);
    }

    fn apply_roles(&mut self, new: LocalRoles) {
        let old = std::mem::replace(&mut self.roles, new.clone());
        let active = self.enabled && new.room_id.is_some();
        if (old.coordinator, old.epoch) != (new.coordinator, new.epoch) {
            self.clock.reset();
            self.pings_sent = 0;
            self.next_ping_ns = 0;
            if let Some(sp) = self.speaker.as_mut() {
                sp.set_authority(new.epoch, new.coordinator.unwrap_or(self.local));
            }
        }
        // microphone capture
        let want_capture = active && new.mic_enabled;
        if want_capture && self.capture.is_none() {
            match self.backend.start_capture(&self.settings.input) {
                Ok(h) => {
                    let fa = FrameAssembler::new(h.sample_rate);
                    self.capture = Some((h, fa));
                }
                Err(e) => self.error(format!("Microphone unavailable: {e}")),
            }
        } else if !want_capture && self.capture.take().is_some() {
            self.backend.stop_capture();
        }
        // room speaker playback
        let want_play = active && new.is_speaker;
        if want_play && self.playback.is_none() {
            match self.backend.start_playback(&self.settings.output) {
                Ok(h) => {
                    let clock = PlaybackClock::new(h.sample_rate as f64);
                    self.playback = Some(Playback {
                        h,
                        clock,
                        pushed: 0,
                    });
                }
                Err(e) => self.error(format!("Speaker unavailable: {e}")),
            }
        } else if !want_play && self.playback.take().is_some() {
            self.backend.stop_playback();
        }
        // coordinator pipeline
        if active && new.is_coordinator {
            match self.coord.as_mut() {
                Some(cs) => {
                    cs.pipe.set_epoch(new.epoch);
                    cs.pipe.set_enabled_mics(&new.enabled_mics);
                }
                None => match CoordinatorPipeline::new(
                    self.local,
                    new.epoch,
                    self.settings.coordinator.clone(),
                ) {
                    Ok(mut pipe) => {
                        pipe.set_enabled_mics(&new.enabled_mics);
                        self.coord = Some(CoordState {
                            pipe,
                            vdev: None,
                            farend: FrameAssembler::new(SAMPLE_RATE),
                            farend_clock: DeviceClock::new(SAMPLE_RATE as f64, 0),
                            next_out_ns: 0,
                            last_vdev_attempt_ns: None,
                            vdev_error_reported: false,
                            selection: Selection::default(),
                        });
                    }
                    Err(e) => self.error(format!("Coordinator pipeline failed: {e}")),
                },
            }
        } else {
            self.coord = None; // stop writing: RoomMesh Microphone now serves silence
        }
        self.shared
            .is_coordinator
            .store(self.coord.is_some(), Ordering::Relaxed);
    }

    fn step(&mut self, now: u64) {
        if !self.enabled || self.roles.room_id.is_none() {
            return;
        }
        self.clock_sync(now);
        self.capture_step();
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
        send_rt(
            &self.transport,
            &self.sessions,
            c,
            &h,
            &encode_times(&[now]),
        );
        self.pings_sent = self.pings_sent.saturating_add(1);
        self.next_ping_ns = now
            + if self.pings_sent < 10 {
                50_000_000
            } else {
                250_000_000
            };
    }

    fn capture_step(&mut self) {
        let Some((h, fa)) = self.capture.as_mut() else {
            return;
        };
        while let Ok(b) = h.blocks.pop() {
            fa.push(b.first_frame, b.capture_ns, &b.samples[..b.len as usize]);
        }
        while let Some(frame) = fa.pop_frame() {
            if self.muted {
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
    }

    // Borrow note: `cs` borrows `self.coord`; everything else is reached through disjoint
    // fields — never call a `&mut self` method while `cs` is alive.
    fn coordinator_step(&mut self, now: u64) {
        let Some(cs) = self.coord.as_mut() else {
            return;
        };
        if cs.vdev.is_none()
            && cs
                .last_vdev_attempt_ns
                .is_none_or(|t| now.saturating_sub(t) > VDEV_RETRY_NS)
        {
            cs.last_vdev_attempt_ns = Some(now);
            match self.backend.open_virtual_device() {
                Ok(r) => {
                    cs.vdev = Some((MicWriter::new(r.clone()), SpeakerReader::new(r)));
                    self.shared.virtual_device_ok.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    self.shared
                        .virtual_device_ok
                        .store(false, Ordering::Relaxed);
                    if !cs.vdev_error_reported {
                        cs.vdev_error_reported = true;
                        let msg = format!("RoomMesh virtual devices unavailable: {e}");
                        log::warn!("{msg}");
                        let _ = self.events.try_send(RuntimeEvent::Error(msg));
                    }
                }
            }
        }
        // far-end: what the meeting app plays into "RoomMesh Speaker"
        if let Some((_, spk)) = cs.vdev.as_mut() {
            while let Some(chunk) = spk.read(4096) {
                cs.farend_clock.report(chunk.write_pos, chunk.write_host_ns);
                let ts = cs.farend_clock.time_of(chunk.first_pos).unwrap_or(now);
                cs.farend.push(chunk.first_pos, ts, &chunk.samples);
            }
        }
        while let Some(f) = cs.farend.pop_frame() {
            let mut pb = cs.pipe.push_farend(&f);
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
        // room microphone: one frame per 10 ms of host time, one frame ahead of "now"
        if cs.next_out_ns == 0 || now.saturating_sub(cs.next_out_ns) > MAX_GRID_LAG_NS {
            cs.next_out_ns = now;
        }
        while cs.next_out_ns <= now + FRAME_NS {
            let pf = cs.pipe.produce(cs.next_out_ns, now);
            if let Some((w, _)) = cs.vdev.as_ref() {
                w.write(&pf.samples, now);
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
        let (Some(pb), Some(sp)) = (self.playback.as_mut(), self.speaker.as_mut()) else {
            return;
        };
        while let Ok(r) = pb.h.reports.pop() {
            pb.clock.report(r);
        }
        let rate = pb.h.sample_rate as f64;
        let target = (rate * PLAYBACK_QUEUE_S) as u64;
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
                for s in cs.pipe.statuses() {
                    let active =
                        self.roles.members.contains(&s.peer) && cs.selection.contains(s.peer);
                    book.apply_mic(
                        s.peer,
                        s.score,
                        s.speech_prob,
                        s.jitter.map(|j| j.jitter_ns as f32 / 1e6),
                        s.jitter.map(|j| j.loss_ratio() as f32 * 100.0),
                        s.buffer_ms,
                        s.aec.erle_db,
                        s.aec.converged,
                        active,
                    );
                }
            }
            let conv = cs.pipe.aec_converged();
            if conv != self.last_aec {
                self.last_aec = conv;
                let _ = self.events.try_send(RuntimeEvent::AecStatus(conv));
            }
        }
        if !self.roles.is_coordinator {
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
                jitter_ms: jitter.map(|j| j.jitter_ns as f32 / 1e6).unwrap_or(0.0),
                loss_pct: jitter.map(|j| j.loss_ratio() as f32 * 100.0).unwrap_or(0.0),
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
    fn spawn_with(
        local: u64,
        backend: Box<dyn AudioBackend>,
        settings: AudioSettings,
    ) -> (AudioRuntime, crossbeam_channel::Receiver<RuntimeEvent>) {
        let net = LoopbackNetwork::new();
        let (tsink, _trx) = unbounded();
        let transport = net.transport(PeerId(local), tsink);
        let (ev_tx, ev_rx) = unbounded();
        let rt = AudioRuntime::spawn(
            PeerId(local),
            backend,
            transport,
            Default::default(),
            ev_tx,
            settings,
        );
        (rt, ev_rx)
    }
    fn spawn(local: u64) -> (AudioRuntime, crossbeam_channel::Receiver<RuntimeEvent>) {
        spawn_with(local, Box::new(NullAudio), AudioSettings::default())
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

    /// No physical devices, but a (test) driver region: the coordinator must still drive the
    /// RoomMesh Microphone on the host-time grid.
    struct VdevOnly(Arc<SharedRegion>);
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
            Ok(self.0.clone())
        }
    }

    #[test]
    fn coordinator_writes_mic_ring_on_host_time_grid_without_mics() {
        let region = Arc::new(SharedRegion::create_for_test().unwrap());
        let mut settings = AudioSettings::default();
        settings.coordinator.use_webrtc_aec = false;
        let (rt, _ev) = spawn_with(1, Box::new(VdevOnly(region.clone())), settings);
        rt.send(RuntimeMsg::SetEnabled(true));
        rt.send(RuntimeMsg::Roles(roles(1, 1, 1)));
        // Driver emulation: drain the mic ring; the read position then equals `write_pos`
        // (each drain covers far less than the ring size).
        let mut buf = vec![0.0f32; crate::audio::shared_layout::RING_FRAMES];
        let mut read = 0u64;
        let mut pos = || {
            read += region.test_driver_read_mic(read, &mut buf) as u64;
            read
        };
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(rt.shared().virtual_device_ok.load(Ordering::Relaxed));
        let (p0, t0) = (pos(), now_ns());
        assert!(p0 > 0, "coordinator should already be writing the mic ring");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (p1, t1) = (pos(), now_ns());
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
}
