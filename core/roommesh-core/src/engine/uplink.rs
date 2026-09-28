//! Capture side: device chunks (any rate, any callback size) → 10 ms 48 kHz frames on a uniform
//! host-time grid → Opus Mic packets stamped in coordinator time.

use crate::audio::codec::{CodecError, VoiceEncoder};
use crate::audio::frames::{AudioFrame, FRAME_NS, FRAME_SAMPLES, NS_PER_SAMPLE};
use crate::audio::timeline::TimelineReader;
use crate::ids::{Epoch, PeerId, StreamId};
use crate::network::realtime::{PacketKind, RtHeader};

const LOOKAHEAD_NS: u64 = 1_000_000;
/// If the consumer falls further behind than this, skip ahead (must stay below the 2 s timeline).
const MAX_BACKLOG_NS: u64 = 1_000_000_000;

pub struct FrameAssembler {
    timeline: TimelineReader,
    /// Capture time of the first chunk since construction/reset: where the frame grid starts.
    first_ts: Option<u64>,
    next_ts: Option<u64>,
    next_index: u64,
}

impl FrameAssembler {
    pub fn new(device_rate: u32) -> Self {
        Self {
            timeline: TimelineReader::new(device_rate, 2.0),
            first_ts: None,
            next_ts: None,
            next_index: 0,
        }
    }
    pub fn reset(&mut self) {
        self.timeline.reset();
        self.first_ts = None;
        self.next_ts = None;
    }
    /// `capture_ns`: host time the first sample hit the microphone (latency compensated).
    pub fn push(&mut self, first_index: u64, capture_ns: u64, samples: &[f32]) {
        self.first_ts.get_or_insert(capture_ns);
        self.timeline.push(first_index, capture_ns, samples);
    }
    pub fn pop_frame(&mut self) -> Option<AudioFrame> {
        let until = self.timeline.available_until_ns()?;
        let mut start = self
            .next_ts
            .or(self.first_ts)
            .unwrap_or_else(|| until.saturating_sub(FRAME_NS + LOOKAHEAD_NS));
        if until.saturating_sub(start) > MAX_BACKLOG_NS {
            start = until - FRAME_NS - LOOKAHEAD_NS;
        }
        if start + FRAME_NS + LOOKAHEAD_NS > until {
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
