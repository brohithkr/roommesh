//! Capture side: device chunks (any rate, any callback size) → 10 ms 48 kHz frames on a uniform
//! host-time grid → Opus Mic packets stamped in coordinator time.

use crate::audio::codec::{CodecError, VoiceEncoder};
use crate::audio::frames::{AudioFrame, FRAME_NS, FRAME_SAMPLES, NS_PER_SAMPLE};
use crate::audio::timeline::TimelineReader;
use crate::ids::{Epoch, PeerId, StreamId};
use crate::network::realtime::{PacketKind, RtHeader};

const LOOKAHEAD_NS: u64 = 1_000_000;
/// Once the grid has caught up with live input, a consumer further behind than this skips ahead
/// (whole frames).
const MAX_BACKLOG_NS: u64 = 200_000_000;
/// After a reset the grid starts at the first chunk and drains what was buffered before the first
/// pop without the cap above (bounded by this and the 2 s timeline).
const MAX_FIRST_BACKLOG_NS: u64 = 1_500_000_000;
/// A chunk whose capture time is off from the previous chunk's end by more than this is a
/// discontinuity (device pause/restart); matches the timeline's own resync threshold.
const DISCONTINUITY_NS: u64 = 20_000_000;

pub struct FrameAssembler {
    timeline: TimelineReader,
    device_rate: u32,
    /// Capture time of the first chunk since construction/reset: where the frame grid starts.
    first_ts: Option<u64>,
    next_ts: Option<u64>,
    next_index: u64,
    /// Capture time just past the previous chunk (discontinuity detection).
    expected_ts: Option<u64>,
    /// Start of new data after a forward discontinuity: the grid skips to it.
    resume_at: Option<u64>,
    /// The start-up backlog has been drained (the backlog cap applies from then on).
    caught_up: bool,
}

impl FrameAssembler {
    pub fn new(device_rate: u32) -> Self {
        Self {
            timeline: TimelineReader::new(device_rate, 2.0),
            device_rate,
            first_ts: None,
            next_ts: None,
            next_index: 0,
            expected_ts: None,
            resume_at: None,
            caught_up: false,
        }
    }
    pub fn reset(&mut self) {
        self.timeline.reset();
        self.first_ts = None;
        self.next_ts = None;
        self.expected_ts = None;
        self.resume_at = None;
        self.caught_up = false;
    }
    /// `capture_ns`: host time the first sample hit the microphone (latency compensated).
    pub fn push(&mut self, first_index: u64, capture_ns: u64, samples: &[f32]) {
        if let Some(expected) = self.expected_ts {
            if capture_ns > expected + DISCONTINUITY_NS {
                // Paused: the grid resumes at the new data instead of emitting the silent gap.
                self.resume_at = Some(capture_ns);
            } else if capture_ns + DISCONTINUITY_NS < expected {
                // Went backwards (restart / clock step): start a fresh grid at the new data.
                self.first_ts = Some(capture_ns);
                self.next_ts = None;
                self.resume_at = None;
                self.caught_up = false;
            }
        }
        self.first_ts.get_or_insert(capture_ns);
        let dur = samples.len() as u64 * 1_000_000_000 / self.device_rate.max(1) as u64;
        self.expected_ts = Some(capture_ns + dur);
        self.timeline.push(first_index, capture_ns, samples);
    }
    /// Advances the grid by `frames` whole frames (keeps sample_index consistent with time).
    fn skip(&mut self, start: u64, frames: u64) -> u64 {
        self.next_index += frames * FRAME_SAMPLES as u64;
        start + frames * FRAME_NS
    }
    pub fn pop_frame(&mut self) -> Option<AudioFrame> {
        let until = self.timeline.available_until_ns()?;
        let latest = until.saturating_sub(FRAME_NS + LOOKAHEAD_NS);
        let start = match self.next_ts {
            Some(mut start) => {
                if let Some(r) = self.resume_at.take() {
                    if r > start {
                        start = self.skip(start, (r - start).div_ceil(FRAME_NS));
                    }
                }
                if self.caught_up && until.saturating_sub(start) > MAX_BACKLOG_NS && latest > start
                {
                    start = self.skip(start, (latest - start) / FRAME_NS);
                }
                start
            }
            None => {
                self.resume_at = None;
                self.first_ts
                    .filter(|&f| until.saturating_sub(f) <= MAX_FIRST_BACKLOG_NS)
                    .unwrap_or(latest)
            }
        };
        if until.saturating_sub(start) <= MAX_BACKLOG_NS {
            self.caught_up = true;
        }
        if start + FRAME_NS + LOOKAHEAD_NS > until {
            // Not enough data yet; remember a skipped-to grid position.
            if self.next_ts.is_some() {
                self.next_ts = Some(start);
            }
            return None;
        }
        let mut samples = vec![0.0; FRAME_SAMPLES];
        self.timeline.read(start, NS_PER_SAMPLE, &mut samples);
        let f = AudioFrame {
            sample_index: self.next_index,
            timestamp_ns: start,
            samples,
        };
        self.next_ts = Some(start + FRAME_NS);
        self.next_index += FRAME_SAMPLES as u64;
        Some(f)
    }
}

pub struct MicUplink {
    local: PeerId,
    enc: VoiceEncoder,
    seq: u32,
}

impl MicUplink {
    pub fn new(local: PeerId, bitrate_bps: i32) -> Result<Self, CodecError> {
        Ok(Self {
            local,
            enc: VoiceEncoder::new(bitrate_bps)?,
            seq: 0,
        })
    }
    pub fn packetize(
        &mut self,
        frame: &AudioFrame,
        epoch: Epoch,
        coord_ts: u64,
    ) -> Result<(RtHeader, Vec<u8>), CodecError> {
        let payload = self.enc.encode(&frame.samples)?;
        let h = RtHeader {
            kind: PacketKind::Mic,
            epoch,
            stream: StreamId::MIC,
            sender: self.local,
            sequence: self.seq,
            sample_index: frame.sample_index,
            timestamp_ns: coord_ts,
            frame_count: FRAME_SAMPLES as u16,
        };
        self.seq = self.seq.wrapping_add(1);
        Ok((h, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frames::*;
    use crate::ids::*;
    #[test]
    fn assembler_emits_contiguous_48k_frames_from_44k1_device() {
        let mut fa = FrameAssembler::new(44_100);
        let t0 = 7_000_000_000u64;
        for c in 0..50u64 {
            let s: Vec<f32> = (0..441)
                .map(|k| {
                    (((c * 441 + k) as f64 / 44_100.0) * 2.0 * std::f64::consts::PI * 200.0).sin()
                        as f32
                })
                .collect();
            fa.push(c * 441, t0 + c * 10_000_000, &s);
        }
        let frames: Vec<AudioFrame> = std::iter::from_fn(|| fa.pop_frame()).collect();
        assert!(frames.len() >= 45, "{}", frames.len());
        for w in frames.windows(2) {
            assert_eq!(w[1].timestamp_ns - w[0].timestamp_ns, FRAME_NS);
            assert_eq!(w[1].sample_index - w[0].sample_index, FRAME_SAMPLES as u64);
        }
        let f = &frames[20];
        let t = (f.timestamp_ns - t0) as f64 / 1e9 + 100.0 / 48_000.0;
        assert!(
            (f.samples[100] - (t * 2.0 * std::f64::consts::PI * 200.0).sin() as f32).abs() < 2e-3
        );
    }
    #[test]
    fn pause_skips_the_gap_instead_of_bursting_silence() {
        for pause_ms in [150u64, 500] {
            let mut fa = FrameAssembler::new(48_000);
            let t0 = 3_000_000_000u64;
            let chunk = |c: u64| -> Vec<f32> {
                (0..480)
                    .map(|k| (((c * 480 + k) as f32) * 0.03).sin() * 0.5)
                    .collect()
            };
            let mut ts = t0;
            for c in 0..20u64 {
                fa.push(c * 480, ts, &chunk(c));
                while fa.pop_frame().is_some() {}
                ts += FRAME_NS;
            }
            // The device pauses; its sample counter continues where it left off.
            let resume = ts + pause_ms * 1_000_000;
            fa.push(20 * 480, resume, &chunk(20));
            let burst: Vec<AudioFrame> = std::iter::from_fn(|| fa.pop_frame()).collect();
            assert!(
                burst.len() <= 2,
                "{pause_ms} ms pause emitted {} frames",
                burst.len()
            );
            let mut frames = burst;
            for c in 21..30u64 {
                fa.push(c * 480, resume + (c - 20) * FRAME_NS, &chunk(c));
                frames.extend(std::iter::from_fn(|| fa.pop_frame()));
            }
            assert!(frames.len() >= 7, "{}", frames.len());
            for w in frames.windows(2) {
                assert_eq!(w[1].timestamp_ns - w[0].timestamp_ns, FRAME_NS);
                assert_eq!(w[1].sample_index - w[0].sample_index, FRAME_SAMPLES as u64);
            }
            for f in &frames {
                assert!(
                    f.timestamp_ns >= resume,
                    "frame from the gap at {}",
                    f.timestamp_ns
                );
                assert!(
                    f.samples.iter().map(|v| v * v).sum::<f32>() > 1.0,
                    "silent frame"
                );
            }
        }
    }
    #[test]
    fn slow_consumer_skips_whole_frames_after_backlog_cap() {
        let mut fa = FrameAssembler::new(48_000);
        let t0 = 3_000_000_000u64;
        fa.push(0, t0, &[0.1; 480]);
        fa.push(480, t0 + FRAME_NS, &[0.1; 480]);
        let first = fa.pop_frame().unwrap();
        for c in 2..60u64 {
            fa.push(c * 480, t0 + c * FRAME_NS, &[0.1; 480]);
        }
        let next = fa.pop_frame().unwrap();
        let skipped = (next.timestamp_ns - first.timestamp_ns) / FRAME_NS;
        assert!(skipped > 20, "{skipped}");
        assert_eq!(
            next.sample_index - first.sample_index,
            skipped * FRAME_SAMPLES as u64
        );
        assert!(std::iter::from_fn(|| fa.pop_frame()).count() <= 1);
    }
    #[test]
    fn uplink_packetizes_with_increasing_sequence() {
        let mut up = MicUplink::new(PeerId(5), 32_000).unwrap();
        let f = AudioFrame::silent(960, 123);
        let (h1, p1) = up.packetize(&f, Epoch(3), 999).unwrap();
        let (h2, _) = up.packetize(&f, Epoch(3), 1999).unwrap();
        assert_eq!((h1.sequence, h2.sequence), (0, 1));
        assert_eq!(
            (h1.sender, h1.epoch, h1.timestamp_ns, h1.sample_index),
            (PeerId(5), Epoch(3), 999, 960)
        );
        assert!(!p1.is_empty());
    }
}
