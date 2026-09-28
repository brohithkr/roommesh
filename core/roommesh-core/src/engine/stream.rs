//! Receiving side of one network audio stream: jitter buffer → Opus (FEC/PLC) → timeline.

use crate::audio::codec::{CodecError, VoiceDecoder};
use crate::audio::frames::{FRAME_NS, FRAME_SAMPLES, SAMPLE_RATE};
use crate::audio::jitter_buffer::{JitterBuffer, JitterStats, Pop, PushResult};
use crate::audio::timeline::TimelineReader;
use crate::network::realtime::RtHeader;

pub struct StreamReceiver {
    jb: JitterBuffer,
    dec: VoiceDecoder,
    pub timeline: TimelineReader,
    scratch: Vec<f32>,
    last_arrival_ns: u64,
}

impl StreamReceiver {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            jb: JitterBuffer::new(64, FRAME_SAMPLES as u64, FRAME_NS),
            dec: VoiceDecoder::new()?,
            timeline: TimelineReader::new(SAMPLE_RATE, 3.0),
            scratch: vec![0.0; 5760],
            last_arrival_ns: 0,
        })
    }
    pub fn push(&mut self, h: RtHeader, payload: Vec<u8>, arrival_ns: u64) -> PushResult {
        self.last_arrival_ns = arrival_ns;
        self.jb.push(h, payload, arrival_ns)
    }
    pub fn last_arrival_ns(&self) -> u64 {
        self.last_arrival_ns
    }
    pub fn stats(&self) -> JitterStats {
        self.jb.stats()
    }

    /// Decodes every packet due by `deadline_ns` (packet-timestamp domain) into the timeline,
    /// converting timestamps with `map_ts` (identity on the coordinator; coord→local on a speaker).
    pub fn pump(&mut self, deadline_ns: u64, now_ns: u64, map_ts: impl Fn(u64) -> Option<u64>) {
        loop {
            match self.jb.pop_due(deadline_ns, now_ns) {
                Pop::NotReady => break,
                Pop::Packet(p) => {
                    let n = self.dec.decode(&p.payload, &mut self.scratch).unwrap_or(0);
                    if let Some(ts) = map_ts(p.header.timestamp_ns) {
                        self.timeline
                            .push(p.header.sample_index, ts, &self.scratch[..n]);
                    }
                }
                Pop::Missing {
                    seq,
                    timestamp_ns,
                    sample_index,
                } => {
                    let next = self.jb.peek(seq + 1).map(|p| p.payload.clone());
                    let n = self
                        .dec
                        .conceal(next.as_deref(), &mut self.scratch[..FRAME_SAMPLES])
                        .unwrap_or(0);
                    if let Some(ts) = map_ts(timestamp_ns) {
                        self.timeline.push(sample_index, ts, &self.scratch[..n]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::codec::VoiceEncoder;
    use crate::audio::frames::*;
    use crate::ids::*;
    use crate::network::realtime::*;

    fn packet(enc: &mut VoiceEncoder, i: u64, t0: u64) -> (RtHeader, Vec<u8>) {
        let s: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|k| 0.3 * ((i * 480 + k as u64) as f32 * 0.05).sin())
            .collect();
        let h = RtHeader {
            kind: PacketKind::Mic,
            epoch: Epoch(1),
            stream: StreamId::MIC,
            sender: PeerId(2),
            sequence: i as u32,
            sample_index: i * 480,
            timestamp_ns: t0 + i * FRAME_NS,
            frame_count: 480,
        };
        (h, enc.encode(&s).unwrap())
    }
    #[test]
    fn decodes_into_timeline_and_conceals_loss() {
        let mut enc = VoiceEncoder::new(48_000).unwrap();
        let mut rx = StreamReceiver::new().unwrap();
        let t0 = 1_000_000_000u64;
        for i in 0..30u64 {
            let (h, p) = packet(&mut enc, i, t0);
            if i == 10 {
                continue;
            } // lost
            rx.push(h, p, t0 + i * FRAME_NS + 3_000_000);
        }
        rx.pump(t0 + 25 * FRAME_NS, t0 + 25 * FRAME_NS, Some);
        let mut out = vec![0.0f32; 480];
        let st = rx
            .timeline
            .read(t0 + 10 * FRAME_NS, NS_PER_SAMPLE, &mut out);
        assert_eq!(st.missing, 0, "lost frame must be concealed, not a hole");
        assert!(
            out.iter().map(|v| v * v).sum::<f32>() > 1.0,
            "concealment should not be silent"
        );
        assert_eq!(rx.stats().lost, 1);
    }
}
