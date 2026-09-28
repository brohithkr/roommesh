//! Room-speaker playout: far-end frames scheduled in coordinator time are mapped to the local
//! clock and rendered at the physical output's presentation times (drift handled by the timeline).
use crate::audio::codec::CodecError;
use crate::audio::frames::{AudioFrame, SAMPLE_RATE};
use crate::audio::timeline::{ReadStatus, TimelineReader};
use crate::engine::stream::StreamReceiver;
use crate::ids::{Epoch, PeerId};
use crate::network::realtime::{PacketKind, RtHeader};

const REORDER_WINDOW_NS: u64 = 30_000_000;

pub struct SpeakerPipeline {
    rx: StreamReceiver,
    local: TimelineReader,
    epoch: Epoch,
    coordinator: PeerId,
}

impl SpeakerPipeline {
    pub fn new(epoch: Epoch, coordinator: PeerId) -> Result<Self, CodecError> {
        Ok(Self {
            rx: StreamReceiver::new()?,
            local: TimelineReader::new(SAMPLE_RATE, 3.0),
            epoch,
            coordinator,
        })
    }
    pub fn set_authority(&mut self, epoch: Epoch, coordinator: PeerId) {
        if (epoch, coordinator) != (self.epoch, self.coordinator) {
            self.epoch = epoch;
            self.coordinator = coordinator;
            self.local.reset();
            if let Ok(rx) = StreamReceiver::new() {
                self.rx = rx;
            }
        }
    }
    pub fn push_packet(&mut self, h: RtHeader, payload: Vec<u8>, arrival_ns: u64) -> bool {
        if h.kind != PacketKind::Playback || h.epoch != self.epoch || h.sender != self.coordinator {
            return false;
        }
        self.rx.push(h, payload, arrival_ns);
        true
    }
    /// Coordinator is also the speaker: frames already carry local play times.
    pub fn push_local(&mut self, frame: &AudioFrame) {
        self.local
            .push(frame.sample_index, frame.timestamp_ns, &frame.samples);
    }
    pub fn render(
        &mut self,
        start_local_ns: u64,
        out_ns_per_sample: f64,
        out: &mut [f32],
        now_ns: u64,
        to_local: impl Fn(u64) -> Option<u64>,
        to_coord: impl Fn(u64) -> Option<u64>,
    ) -> ReadStatus {
        let end_local = start_local_ns + (out.len() as f64 * out_ns_per_sample) as u64;
        if let Some(deadline) = to_coord(end_local + REORDER_WINDOW_NS) {
            self.rx.pump(deadline, now_ns, &to_local);
        }
        let remote = self
            .rx
            .timeline
            .read(start_local_ns, out_ns_per_sample, out);
        if remote.filled > 0 {
            return remote;
        }
        self.local.read(start_local_ns, out_ns_per_sample, out)
    }
    pub fn stats(&self) -> crate::audio::jitter_buffer::JitterStats {
        self.rx.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frames::*;
    use crate::engine::coordinator::{CoordinatorConfig, CoordinatorPipeline};
    use crate::ids::StreamId;
    const T0: u64 = 50_000_000_000;

    #[test]
    fn local_path_plays_exactly_at_schedule() {
        let mut sp = SpeakerPipeline::new(Epoch(1), PeerId(1)).unwrap();
        sp.push_local(&AudioFrame {
            sample_index: 0,
            timestamp_ns: T0,
            samples: (0..480).map(|k| k as f32 / 480.0).collect(),
        });
        sp.push_local(&AudioFrame {
            sample_index: 480,
            timestamp_ns: T0 + FRAME_NS,
            samples: vec![0.0; 480],
        });
        let mut out = vec![0.0f32; 100];
        sp.render(T0 + 1_000_000, NS_PER_SAMPLE, &mut out, T0, Some, Some);
        assert!((out[0] - 48.0 / 480.0).abs() < 1e-4, "{}", out[0]);
    }
    #[test]
    fn remote_path_maps_coordinator_time_to_local_clock() {
        let mut coord = CoordinatorPipeline::new(
            PeerId(1),
            Epoch(1),
            CoordinatorConfig {
                use_webrtc_aec: false,
                ..Default::default()
            },
        )
        .unwrap();
        let mut sp = SpeakerPipeline::new(Epoch(1), PeerId(1)).unwrap();
        let offset = 5_000_000u64; // local = coord - 5 ms
        let to_local = |c: u64| Some(c - offset);
        let to_coord = |l: u64| Some(l + offset);
        for i in 0..40u64 {
            let s: Vec<f32> = (0..480)
                .map(|k| {
                    0.3 * (2.0 * std::f32::consts::PI * 300.0 * (i * 480 + k) as f32 / 48_000.0)
                        .sin()
                })
                .collect();
            let pb = coord.push_farend(&AudioFrame {
                sample_index: i * 480,
                timestamp_ns: T0 + i * FRAME_NS,
                samples: s,
            });
            assert!(sp.push_packet(pb.header, pb.payload, T0 + i * FRAME_NS));
        }
        let play_local = T0 + coord.config().playout_delay_ns - offset + 200_000_000;
        let mut out = vec![0.0f32; 480];
        let st = sp.render(
            play_local,
            NS_PER_SAMPLE,
            &mut out,
            play_local - 50_000_000,
            to_local,
            to_coord,
        );
        assert_eq!(st.missing, 0);
        let rms = (out.iter().map(|v| v * v).sum::<f32>() / 480.0).sqrt();
        assert!((rms - 0.3 / 2f32.sqrt()).abs() < 0.08, "rms {rms}");
    }
    #[test]
    fn rejects_foreign_or_stale_packets() {
        let mut sp = SpeakerPipeline::new(Epoch(2), PeerId(1)).unwrap();
        let h = RtHeader {
            kind: PacketKind::Playback,
            epoch: Epoch(1),
            stream: StreamId::FAR_END,
            sender: PeerId(1),
            sequence: 0,
            sample_index: 0,
            timestamp_ns: 0,
            frame_count: 480,
        };
        assert!(!sp.push_packet(h, vec![], 0));
        assert!(!sp.push_packet(
            RtHeader {
                epoch: Epoch(2),
                sender: PeerId(9),
                ..h
            },
            vec![],
            0
        ));
    }
}
