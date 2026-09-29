//! Coordinator DSP: every enabled mic → timeline alignment → per-mic AEC → VAD → score →
//! arbitration with hysteresis → crossfading mixer → RoomMesh Microphone. Also schedules the
//! far-end (meeting) audio for the room speaker and keeps it as the AEC reference.
use crate::audio::codec::{CodecError, VoiceEncoder};
use crate::audio::frames::{AudioFrame, FRAME_NS, FRAME_SAMPLES, NS_PER_SAMPLE, SAMPLE_RATE};
use crate::audio::jitter_buffer::JitterStats;
use crate::audio::mixer::Mixer;
use crate::audio::timeline::TimelineReader;
use crate::dsp::aec::{AecStats, EchoCanceller, PassthroughAec, WebRtcAec};
use crate::dsp::arbitration::{Arbiter, ArbitrationConfig, MicObservation, Selection};
use crate::dsp::level::{measure, EnvelopeTracker};
use crate::dsp::scoring::{MicFeatures, MicScorer};
use crate::dsp::vad::Vad;
use crate::engine::stream::{StreamReceiver, CODEC_DELAY_NS};
use crate::ids::{Epoch, PeerId, StreamId};
use crate::network::realtime::{PacketKind, RtHeader};
use std::collections::BTreeMap;

/// Interpolation context past the end of a read window (the timeline's cubic reader needs two
/// samples beyond the last output sample), rounded up.
const PUMP_MARGIN_NS: u64 = 100_000;

#[derive(Clone, Debug, PartialEq)]
pub struct CoordinatorConfig {
    /// An output frame for time `t` uses mic audio captured at `t - mic_latency_ns` from every
    /// mic. Must cover a remote mic's worst normal path: 10 ms framing + network + jitter
    /// buffering + the 6.5 ms codec delay (default 70 ms).
    pub mic_latency_ns: u64,
    /// Far-end captured at `c` is scheduled to play on the room speaker at `c + playout_delay_ns`.
    pub playout_delay_ns: u64,
    /// The AEC reference is read `reference_lead_ns` after the mic capture time, keeping the
    /// canceller causal despite small scheduling errors.
    pub reference_lead_ns: u64,
    pub use_webrtc_aec: bool,
    pub noise_suppression: bool,
    pub farend_bitrate_bps: i32,
    pub arbitration: ArbitrationConfig,
}
impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            mic_latency_ns: 70_000_000,
            playout_delay_ns: 80_000_000,
            reference_lead_ns: 20_000_000,
            use_webrtc_aec: true,
            noise_suppression: true,
            farend_bitrate_bps: 64_000,
            arbitration: ArbitrationConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MicStatus {
    pub peer: PeerId,
    pub score: f32,
    pub speech_prob: f32,
    pub snr_db: f32,
    pub present: bool,
    pub aec: AecStats,
    pub jitter: Option<JitterStats>,
    pub buffer_ms: Option<f32>,
}

pub struct ProducedFrame {
    pub samples: Vec<f32>,
    pub selection: Selection,
    pub selection_changed: bool,
}

/// Far-end frame to transmit (or play locally when the coordinator is also the speaker).
pub struct PlaybackOut {
    pub header: RtHeader,
    pub payload: Vec<u8>,
    pub local: AudioFrame,
}

struct MicChannel {
    rx: Option<StreamReceiver>,
    local: Option<TimelineReader>,
    aec: Box<dyn EchoCanceller>,
    vad: Vad,
    scorer: MicScorer,
    env: EnvelopeTracker,
    /// Last analysed level, held in `env` during dropouts so envelope histories stay aligned.
    last_level_db: f32,
    /// Samples of the last produced frame this mic could not supply (0 = fully covered).
    last_missing: usize,
    status: MicStatus,
}

pub struct CoordinatorPipeline {
    cfg: CoordinatorConfig,
    local: PeerId,
    epoch: Epoch,
    mics: BTreeMap<PeerId, MicChannel>,
    /// Per-mic noise baselines (each member's own setting), applied to that mic's VAD.
    baselines: BTreeMap<PeerId, f32>,
    reference: TimelineReader,
    ref_env: EnvelopeTracker,
    farend_enc: VoiceEncoder,
    farend_seq: u32,
    farend_last: Option<(u64, u64)>, // (play_at, sample_index)
    arbiter: Arbiter,
    mixer: Mixer,
    last_sel: Selection,
}

impl CoordinatorPipeline {
    pub fn new(local: PeerId, epoch: Epoch, cfg: CoordinatorConfig) -> Result<Self, CodecError> {
        Ok(Self {
            farend_enc: VoiceEncoder::new(cfg.farend_bitrate_bps)?,
            arbiter: Arbiter::new(cfg.arbitration.clone()),
            cfg,
            local,
            epoch,
            mics: BTreeMap::new(),
            baselines: BTreeMap::new(),
            reference: TimelineReader::new(SAMPLE_RATE, 3.0),
            ref_env: EnvelopeTracker::new(50),
            farend_seq: 0,
            farend_last: None,
            mixer: Mixer::new(30.0),
            last_sel: Selection::default(),
        })
    }
    pub fn config(&self) -> &CoordinatorConfig {
        &self.cfg
    }
    pub fn set_epoch(&mut self, e: Epoch) {
        self.epoch = e;
    }
    pub fn set_arbitration(&mut self, a: ArbitrationConfig) {
        self.arbiter.set_config(a.clone());
        self.cfg.arbitration = a;
    }

    fn make_aec(&self) -> Box<dyn EchoCanceller> {
        if self.cfg.use_webrtc_aec {
            match WebRtcAec::new(self.cfg.noise_suppression) {
                Ok(a) => return Box::new(a),
                Err(e) => log::error!("webrtc aec unavailable, echo will pass: {e}"),
            }
        }
        Box::new(PassthroughAec)
    }

    pub fn set_enabled_mics(&mut self, mics: &[PeerId]) {
        self.mics.retain(|p, _| mics.contains(p));
        for &p in mics {
            if self.mics.contains_key(&p) {
                continue;
            }
            let is_local = p == self.local;
            let mut vad = Vad::new();
            vad.set_baseline(self.baselines.get(&p).copied());
            let ch = MicChannel {
                rx: if is_local {
                    None
                } else {
                    StreamReceiver::new().ok()
                },
                local: is_local.then(|| TimelineReader::new(SAMPLE_RATE, 3.0)),
                aec: self.make_aec(),
                vad,
                scorer: MicScorer::new(),
                env: EnvelopeTracker::new(50),
                last_level_db: -90.0,
                last_missing: FRAME_SAMPLES,
                status: MicStatus {
                    peer: p,
                    score: 0.0,
                    speech_prob: 0.0,
                    snr_db: 0.0,
                    present: false,
                    aec: AecStats::default(),
                    jitter: None,
                    buffer_ms: None,
                },
            };
            self.mics.insert(p, ch);
        }
    }

    /// Sets each mic's noise baseline (a mic not listed has none: automatic floor). Applies to
    /// the enabled mics now and to mics enabled later.
    pub fn set_mic_baselines(&mut self, baselines: &[(PeerId, f32)]) {
        self.baselines = baselines.iter().copied().collect();
        for (p, ch) in self.mics.iter_mut() {
            ch.vad.set_baseline(self.baselines.get(p).copied());
        }
    }
    /// The noise baseline `peer`'s VAD is using (None: automatic, or the mic isn't enabled).
    pub fn mic_baseline(&self, peer: PeerId) -> Option<f32> {
        self.mics.get(&peer).and_then(|c| c.vad.baseline())
    }

    pub fn push_local_mic(&mut self, frame: &AudioFrame) {
        if let Some(tl) = self
            .mics
            .get_mut(&self.local)
            .and_then(|c| c.local.as_mut())
        {
            tl.push(frame.sample_index, frame.timestamp_ns, &frame.samples);
        }
    }

    /// Returns false when the packet is rejected (wrong epoch, mic not enabled, not a mic packet).
    pub fn push_remote_mic(&mut self, h: RtHeader, payload: Vec<u8>, arrival_ns: u64) -> bool {
        if h.kind != PacketKind::Mic || h.epoch != self.epoch {
            return false;
        }
        match self.mics.get_mut(&h.sender).and_then(|c| c.rx.as_mut()) {
            Some(rx) => {
                rx.push(h, payload, arrival_ns);
                true
            }
            None => false,
        }
    }

    pub fn push_farend(&mut self, frame: &AudioFrame) -> PlaybackOut {
        let play_at = frame.timestamp_ns + self.cfg.playout_delay_ns;
        let index = match self.farend_last {
            Some((last_t, last_i)) => {
                let expected = last_t + FRAME_NS;
                if play_at.abs_diff(expected) < 2_000_000 {
                    last_i + FRAME_SAMPLES as u64
                } else {
                    last_i
                        + ((play_at.saturating_sub(last_t)) as f64 / NS_PER_SAMPLE).round() as u64
                }
            }
            None => 0,
        };
        self.farend_last = Some((play_at, index));
        self.reference.push(index, play_at, &frame.samples);
        let payload = self.farend_enc.encode(&frame.samples).unwrap_or_default();
        let header = RtHeader {
            kind: PacketKind::Playback,
            epoch: self.epoch,
            stream: StreamId::FAR_END,
            sender: self.local,
            sequence: self.farend_seq,
            sample_index: index,
            timestamp_ns: play_at,
            frame_count: FRAME_SAMPLES as u16,
        };
        self.farend_seq = self.farend_seq.wrapping_add(1);
        PlaybackOut {
            header,
            payload,
            local: AudioFrame {
                sample_index: index,
                timestamp_ns: play_at,
                samples: frame.samples.clone(),
            },
        }
    }

    pub fn produce(&mut self, out_ns: u64, now_ns: u64) -> ProducedFrame {
        let t_mic = out_ns.saturating_sub(self.cfg.mic_latency_ns);
        let mut reference = vec![0.0f32; FRAME_SAMPLES];
        self.reference.read(
            t_mic + self.cfg.reference_lead_ns,
            NS_PER_SAMPLE,
            &mut reference,
        );
        let ref_db = measure(&reference).rms_db;
        self.ref_env.push(ref_db);
        let farend_active = ref_db > -55.0;

        // Pass 1: align, cancel echo, analyse every mic.
        let mut cleaned: BTreeMap<PeerId, Vec<f32>> = BTreeMap::new();
        let mut feats: Vec<(PeerId, MicFeatures, bool)> = Vec::with_capacity(self.mics.len());
        for (peer, ch) in self.mics.iter_mut() {
            let mut mic = vec![0.0f32; FRAME_SAMPLES];
            let st = match (ch.rx.as_mut(), ch.local.as_mut()) {
                (Some(rx), _) => {
                    // The receiver shifts decoded audio back by the codec delay, so the packet
                    // stamped `ts` ends at `ts + FRAME_NS - CODEC_DELAY_NS`: to cover the read
                    // window up to `t_mic + FRAME_NS` (plus interpolation context) at any phase,
                    // decode every packet stamped up to that far past it.
                    rx.pump(
                        t_mic + FRAME_NS + CODEC_DELAY_NS + PUMP_MARGIN_NS,
                        now_ns,
                        Some,
                    );
                    ch.status.jitter = Some(rx.stats());
                    ch.status.buffer_ms = rx
                        .timeline
                        .available_until_ns()
                        .map(|u| (u as f64 - (t_mic + FRAME_NS) as f64) as f32 / 1e6);
                    rx.timeline.read(t_mic, NS_PER_SAMPLE, &mut mic)
                }
                (None, Some(tl)) => tl.read(t_mic, NS_PER_SAMPLE, &mut mic),
                _ => continue,
            };
            ch.last_missing = st.missing;
            let present = st.missing < FRAME_SAMPLES / 2;
            let clip = measure(&mic).clip_ratio;
            // The AEC always runs so its render/capture streams stay in step.
            ch.aec.process(&reference, &mut mic);
            ch.status.present = present;
            ch.status.aec = ch.aec.stats();
            if !present {
                // One entry per frame for every mic keeps envelope histories time-aligned
                // (same-talker / echo-leak correlations compare them index by index).
                ch.env.push(ch.last_level_db);
                // Dropout (stream not started, stalled or lost): zero-filled audio must not reach
                // the VAD/envelope, or the noise floor collapses to digital silence and every
                // later frame looks like high-SNR speech.
                ch.status.speech_prob = 0.0;
                ch.status.snr_db = 0.0;
                feats.push((*peer, MicFeatures::default(), false));
                cleaned.insert(*peer, mic);
                continue;
            }
            let v = ch.vad.process(&mic);
            ch.env.push(v.level_db);
            ch.last_level_db = v.level_db;
            let echo_leak = if farend_active && v.snr_db < 20.0 {
                ch.env.correlation(&self.ref_env).max(0.0)
            } else {
                0.0
            };
            let f = MicFeatures {
                speech_prob: v.speech_prob,
                is_speech: v.is_speech,
                snr_db: v.snr_db,
                relative_snr_db: 0.0,
                level_db: v.level_db,
                clip_ratio: clip,
                modulation: ch.env.modulation(),
                echo_leak,
            };
            ch.status.speech_prob = v.speech_prob;
            ch.status.snr_db = v.snr_db;
            feats.push((*peer, f, present));
            cleaned.insert(*peer, mic);
        }
        // Pass 2: score relative to the best speaking mic of this frame.
        let best_snr = feats
            .iter()
            .filter(|(_, f, p)| *p && f.is_speech)
            .map(|(_, f, _)| f.snr_db)
            .fold(f32::MIN, f32::max);
        let mut obs = Vec::with_capacity(feats.len());
        for (peer, mut f, present) in feats {
            if best_snr > f32::MIN {
                f.relative_snr_db = (f.snr_db - best_snr).min(0.0);
            }
            let ch = self.mics.get_mut(&peer).expect("mic channel");
            let score = if present { ch.scorer.score(&f) } else { 0.0 };
            ch.status.score = score;
            if present {
                obs.push(MicObservation {
                    peer,
                    score,
                    speaking: f.is_speech,
                });
            }
        }
        let mics = &self.mics;
        // Output time, not wall time: frames produced in a burst still advance the arbiter's clock.
        let sel = self.arbiter.update(out_ns / 1_000_000, &obs, |a, b| {
            match (mics.get(&a), mics.get(&b)) {
                (Some(x), Some(y)) => x.env.correlation(&y.env),
                _ => 0.0,
            }
        });
        self.mixer.set_selection(&sel);
        let mut samples = vec![0.0f32; FRAME_SAMPLES];
        self.mixer.mix(&cleaned, &mut samples);
        let selection_changed = sel != self.last_sel;
        self.last_sel = sel;
        ProducedFrame {
            samples,
            selection: sel,
            selection_changed,
        }
    }

    pub fn statuses(&self) -> Vec<MicStatus> {
        self.mics.values().map(|c| c.status.clone()).collect()
    }
    pub fn aec_converged(&self) -> bool {
        self.mics
            .values()
            .filter(|c| c.status.present)
            .all(|c| c.status.aec.converged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frames::*;
    use crate::engine::uplink::MicUplink;

    const T0: u64 = 10_000_000_000;
    fn hash(i: u64) -> f32 {
        let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
    /// Syllable-like bursts: 150 ms voiced, 100 ms silent (so noise floors can be measured).
    fn talker(n: u64) -> f32 {
        let t = n as f32 / 48_000.0;
        let phase = (t * 4.0).fract();
        let env = if phase < 0.6 {
            (phase / 0.6 * std::f32::consts::PI).sin()
        } else {
            0.0
        };
        env * (1..=10)
            .map(|h| (2.0 * std::f32::consts::PI * 140.0 * h as f32 * t).sin() / h as f32)
            .sum::<f32>()
            * 0.2
    }
    fn far(n: u64) -> f32 {
        let env = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.0 * n as f32 / 48_000.0).sin();
        0.2 * env * hash(n ^ 0xABCD)
    }
    fn idx(t: u64) -> u64 {
        (t - T0) * 48 / 1_000_000
    }

    struct Sim {
        pipe: CoordinatorPipeline,
        up2: MicUplink,
        up3: MicUplink,
    }
    impl Sim {
        fn new(webrtc: bool) -> Self {
            let cfg = CoordinatorConfig {
                use_webrtc_aec: webrtc,
                ..Default::default()
            };
            let mut pipe = CoordinatorPipeline::new(PeerId(1), Epoch(1), cfg).unwrap();
            pipe.set_enabled_mics(&[PeerId(1), PeerId(2), PeerId(3)]);
            Sim {
                pipe,
                up2: MicUplink::new(PeerId(2), 64_000).unwrap(),
                up3: MicUplink::new(PeerId(3), 64_000).unwrap(),
            }
        }
        /// One 10 ms step at time t. gains: talker gain per mic 1..3; echo: far-end echo gain.
        fn step(&mut self, t: u64, gains: [f32; 3], echo: f32, farend_on: bool) -> ProducedFrame {
            if farend_on {
                let play0 = idx(t + self.pipe.config().playout_delay_ns);
                let s: Vec<f32> = (0..480).map(|k| far(play0 + k)).collect();
                self.pipe.push_farend(&AudioFrame {
                    sample_index: idx(t),
                    timestamp_ns: t,
                    samples: s,
                });
            }
            let n0 = idx(t);
            let mic = |g: f32, salt: u64| -> Vec<f32> {
                (0..480u64)
                    .map(|k| {
                        let n = n0 + k;
                        let e = if farend_on && n >= 144 {
                            echo * far(n - 144)
                        } else {
                            0.0
                        }; // 3 ms acoustic path
                        g * talker(n) + e + 0.001 * hash(n ^ salt)
                    })
                    .collect()
            };
            let f1 = AudioFrame {
                sample_index: n0,
                timestamp_ns: t,
                samples: mic(gains[0], 1),
            };
            self.pipe.push_local_mic(&f1);
            for (up, g, salt) in [
                (&mut self.up2, gains[1], 2u64),
                (&mut self.up3, gains[2], 3u64),
            ] {
                let f = AudioFrame {
                    sample_index: n0,
                    timestamp_ns: t,
                    samples: mic(g, salt),
                };
                let (h, p) = up.packetize(&f, Epoch(1), t).unwrap();
                self.pipe.push_remote_mic(h, p, t + 5_000_000);
            }
            self.pipe.produce(t, t)
        }
    }

    #[test]
    fn selects_the_mic_nearest_the_talker_and_follows_them() {
        let mut sim = Sim::new(false);
        let mut t = T0;
        let sel_at = |sim: &mut Sim, t: &mut u64, until_s: f64, gains: [f32; 3]| {
            let mut last = Selection::default();
            while (*t - T0) as f64 / 1e9 < until_s {
                last = sim.step(*t, gains, 0.0, false).selection;
                *t += FRAME_NS;
            }
            last
        };
        assert_eq!(
            sel_at(&mut sim, &mut t, 2.0, [0.05, 0.5, 0.08]).primary,
            Some(PeerId(2))
        );
        // The talker moves to mic 3 at 2.0 s (mic time): record when the output switches.
        let moved = t;
        let mut switched_at = None;
        let mut last = Selection::default();
        while (t - T0) as f64 / 1e9 < 4.0 {
            last = sim.step(t, [0.05, 0.08, 0.5], 0.0, false).selection;
            if switched_at.is_none() && last.primary == Some(PeerId(3)) {
                switched_at = Some(t);
            }
            t += FRAME_NS;
        }
        assert_eq!(last.primary, Some(PeerId(3)));
        let latency_ms = (switched_at.expect("switched to mic 3") - moved) / 1_000_000;
        assert!(latency_ms <= 600, "switch to mic 3 took {latency_ms} ms");
        // Talker bursts are 150 ms voiced / 100 ms silent and output lags the mics by
        // mic_latency (70 ms), so a single frame can land in a gap: check one full period.
        let mut energy = 0.0f32;
        for _ in 0..25 {
            let out = sim.step(t, [0.05, 0.08, 0.5], 0.0, false);
            energy += out.samples.iter().map(|v| v * v).sum::<f32>();
            t += FRAME_NS;
        }
        assert!(energy > 0.01, "mixed output energy {energy}");
    }
    #[test]
    fn a_mics_noise_baseline_reaches_its_vad() {
        let mut sim = Sim::new(false);
        let mut t = T0;
        let mut run = |sim: &mut Sim, secs: u64| {
            let mut last = Selection::default();
            for _ in 0..secs * 100 {
                last = sim.step(t, [0.05, 0.5, 0.08], 0.0, false).selection;
                t += FRAME_NS;
            }
            last
        };
        assert_eq!(run(&mut sim, 2).primary, Some(PeerId(2)));
        // Mac 2 sets a baseline above everything its mic hears: it is never speech any more,
        // so the next-best mic takes over.
        sim.pipe.set_mic_baselines(&[(PeerId(2), 0.0)]);
        assert_eq!(sim.pipe.mic_baseline(PeerId(2)), Some(0.0));
        assert_eq!(sim.pipe.mic_baseline(PeerId(3)), None);
        assert_eq!(run(&mut sim, 3).primary, Some(PeerId(3)));
        let st = sim.pipe.statuses();
        let mic2 = st.iter().find(|s| s.peer == PeerId(2)).unwrap();
        assert!(mic2.speech_prob < 0.1, "{mic2:?}");
        // Cleared: automatic again.
        sim.pipe.set_mic_baselines(&[]);
        assert_eq!(sim.pipe.mic_baseline(PeerId(2)), None);
        assert_eq!(run(&mut sim, 3).primary, Some(PeerId(2)));
    }
    #[test]
    fn baselines_apply_to_mics_enabled_later() {
        let mut pipe =
            CoordinatorPipeline::new(PeerId(1), Epoch(1), CoordinatorConfig::default()).unwrap();
        pipe.set_enabled_mics(&[PeerId(1)]);
        pipe.set_mic_baselines(&[(PeerId(1), -50.0), (PeerId(2), -42.0)]);
        assert_eq!(pipe.mic_baseline(PeerId(1)), Some(-50.0));
        assert_eq!(pipe.mic_baseline(PeerId(2)), None, "not enabled yet");
        pipe.set_enabled_mics(&[PeerId(1), PeerId(2)]);
        assert_eq!(pipe.mic_baseline(PeerId(2)), Some(-42.0));
    }
    /// A remote stream whose packet grid is offset from the output grid by any phase must fully
    /// cover every output frame: the receive timeline is shifted back by the codec delay, so the
    /// pump deadline has to reach that far past the read window.
    #[test]
    fn remote_mic_fully_covers_output_at_every_phase() {
        // 0..=10 ms in 1 ms steps, plus the edge where a packet's shifted audio ends exactly at
        // the read window's end.
        let phases_us = (0..=10u64)
            .map(|m| m * 1_000)
            .chain([6_450, 6_500, 6_520, 6_550]);
        for phase_us in phases_us {
            let mut pipe =
                CoordinatorPipeline::new(PeerId(1), Epoch(1), CoordinatorConfig::default())
                    .unwrap();
            pipe.set_enabled_mics(&[PeerId(2)]);
            let mut up = MicUplink::new(PeerId(2), 64_000).unwrap();
            let phase = phase_us * 1_000;
            for k in 0..150u64 {
                let t = T0 + k * FRAME_NS;
                let cap = t + phase;
                let f = AudioFrame {
                    sample_index: k * 480,
                    timestamp_ns: cap,
                    samples: (0..480u64).map(|i| talker(k * 480 + i)).collect(),
                };
                let (h, p) = up.packetize(&f, Epoch(1), cap).unwrap();
                pipe.push_remote_mic(h, p, cap + 3_000_000);
                pipe.produce(t, t);
                if k > 20 {
                    let missing = pipe.mics[&PeerId(2)].last_missing;
                    assert_eq!(
                        missing, 0,
                        "phase {phase_us} us, frame {k}: {missing} missing"
                    );
                }
            }
        }
    }
    #[test]
    fn rejects_wrong_epoch_and_disabled_mics() {
        let mut sim = Sim::new(false);
        sim.pipe.set_enabled_mics(&[PeerId(1), PeerId(2)]);
        let f = AudioFrame::silent(0, T0);
        let (h, p) = sim.up3.packetize(&f, Epoch(1), T0).unwrap();
        assert!(!sim.pipe.push_remote_mic(h, p, T0));
        let (h, p) = sim.up2.packetize(&f, Epoch(9), T0).unwrap();
        assert!(!sim.pipe.push_remote_mic(h, p, T0));
    }
    #[test]
    fn webrtc_aec_suppresses_speaker_echo_on_all_mics() {
        let mut sim = Sim::new(true);
        let mut t = T0;
        let (mut e_mix, mut e_raw) = (0.0f32, 0.0f32);
        while t - T0 < 6_000_000_000 {
            let out = sim.step(t, [0.0, 0.0, 0.0], 0.3, true);
            if t - T0 > 4_000_000_000 {
                e_mix += out.samples.iter().map(|v| v * v).sum::<f32>();
                let n0 = idx(t);
                e_raw += (0..480u64)
                    .map(|k| {
                        let v = 0.3 * far(n0 + k - 144);
                        v * v
                    })
                    .sum::<f32>();
            }
            t += FRAME_NS;
        }
        let reduction = 10.0 * (e_raw / e_mix.max(1e-12)).log10();
        assert!(reduction > 10.0, "echo reduction {reduction} dB");
    }
    #[test]
    fn playback_packets_are_scheduled_and_sequenced() {
        let mut sim = Sim::new(false);
        let f = AudioFrame {
            sample_index: 0,
            timestamp_ns: T0,
            samples: vec![0.1; 480],
        };
        let a = sim.pipe.push_farend(&f);
        let b = sim.pipe.push_farend(&AudioFrame {
            timestamp_ns: T0 + FRAME_NS,
            ..f.clone()
        });
        assert_eq!(a.header.kind, PacketKind::Playback);
        assert_eq!(
            a.header.timestamp_ns,
            T0 + sim.pipe.config().playout_delay_ns
        );
        assert_eq!(b.header.sequence, a.header.sequence + 1);
        assert_eq!(b.header.sample_index, a.header.sample_index + 480);
        assert_eq!(a.local.timestamp_ns, a.header.timestamp_ns);
    }

    #[test]
    fn silent_room_with_echo_cancellation_selects_no_talker() {
        // Every mic hears only steady room noise (about -45 dBFS, a typical laptop mic in a
        // quiet office). With the real processing chain (WebRTC AEC + noise suppression) no mic
        // may be judged to be speaking, so the active mic must not move.
        let cfg = CoordinatorConfig {
            use_webrtc_aec: true,
            ..Default::default()
        };
        let mut pipe = CoordinatorPipeline::new(PeerId(1), Epoch(1), cfg).unwrap();
        pipe.set_enabled_mics(&[PeerId(1), PeerId(2), PeerId(3)]);
        let mut up2 = MicUplink::new(PeerId(2), 64_000).unwrap();
        let mut up3 = MicUplink::new(PeerId(3), 64_000).unwrap();
        let room = |n0: u64, gain: f32, salt: u64| -> Vec<f32> {
            (0..480u64).map(|k| gain * hash((n0 + k) ^ salt)).collect()
        };
        let (mut speaking, mut changes, mut last) = (0usize, 0usize, None);
        let mut t = T0;
        while t - T0 < 20_000_000_000 {
            let n0 = idx(t);
            pipe.push_local_mic(&AudioFrame {
                sample_index: n0,
                timestamp_ns: t,
                samples: room(n0, 0.010, 1 << 40),
            });
            for (up, gain, salt) in [(&mut up2, 0.008, 2u64 << 40), (&mut up3, 0.013, 3u64 << 40)] {
                let f = AudioFrame {
                    sample_index: n0,
                    timestamp_ns: t,
                    samples: room(n0, gain, salt),
                };
                let (h, p) = up.packetize(&f, Epoch(1), t).unwrap();
                pipe.push_remote_mic(h, p, t + 5_000_000);
            }
            let out = pipe.produce(t, t);
            if t - T0 > 1_000_000_000 {
                speaking += pipe
                    .statuses()
                    .iter()
                    .filter(|s| s.speech_prob > 0.6)
                    .count();
                if last.is_some() && out.selection.primary != last {
                    changes += 1;
                }
            }
            last = out.selection.primary;
            t += FRAME_NS;
        }
        assert_eq!(
            speaking, 0,
            "room noise judged to be speech in {speaking} mic-frames"
        );
        assert_eq!(
            changes, 0,
            "active mic changed {changes} times in a silent room"
        );
    }
}
