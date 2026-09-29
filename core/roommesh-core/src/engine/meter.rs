//! The local mic meter behind Settings → Audio: this Mac's capture through the same processing
//! the coordinator applies to every mic (echo canceller + noise suppression, fed a silent
//! far-end reference, then the VAD with this Mac's noise baseline), so the user sees the level
//! the room actually judges.
use crate::audio::frames::FRAME_SAMPLES;
use crate::dsp::aec::EchoCanceller;
use crate::dsp::vad::Vad;
use crate::engine::coordinator::{make_echo_canceller, CoordinatorConfig};

/// How fast the peak marker falls back to the level (20 dB/s at 10 ms frames).
const PEAK_DECAY_DB_PER_FRAME: f32 = 0.2;

/// One frame's reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MicMeter {
    /// Processed level (dBFS).
    pub level_db: f32,
    /// The effective noise floor: the automatic floor, raised to the baseline when one is set.
    pub floor_db: f32,
    /// `max(0, level_db - floor_db)`: how far above the floor the room hears this mic.
    pub perceived_db: f32,
    pub is_speech: bool,
    /// Recent peak of `level_db`, decaying at 20 dB/s.
    pub peak_db: f32,
    /// The floor Automatic would use (the VAD's own tracked floor).
    pub auto_floor_db: f32,
}

pub struct MeterChain {
    aec: Box<dyn EchoCanceller>,
    vad: Vad,
    reference: [f32; FRAME_SAMPLES],
    scratch: [f32; FRAME_SAMPLES],
    peak_db: Option<f32>,
}

impl MeterChain {
    /// Processing as the coordinator's (`cfg.use_webrtc_aec`, `cfg.noise_suppression`).
    pub fn new(cfg: &CoordinatorConfig, baseline_db: Option<f32>) -> Self {
        let mut vad = Vad::new();
        vad.set_baseline(baseline_db);
        Self {
            aec: make_echo_canceller(cfg),
            vad,
            reference: [0.0; FRAME_SAMPLES],
            scratch: [0.0; FRAME_SAMPLES],
            peak_db: None,
        }
    }
    pub fn set_baseline(&mut self, baseline_db: Option<f32>) {
        self.vad.set_baseline(baseline_db);
    }
    /// Meters one 10 ms frame (`FRAME_SAMPLES` samples; shorter is zero-padded).
    pub fn process(&mut self, frame: &[f32]) -> MicMeter {
        let n = frame.len().min(FRAME_SAMPLES);
        self.scratch[..n].copy_from_slice(&frame[..n]);
        self.scratch[n..].fill(0.0);
        self.aec.process(&self.reference, &mut self.scratch);
        let v = self.vad.process(&self.scratch);
        let peak = match self.peak_db {
            Some(p) => v.level_db.max(p - PEAK_DECAY_DB_PER_FRAME),
            None => v.level_db,
        };
        self.peak_db = Some(peak);
        MicMeter {
            level_db: v.level_db,
            floor_db: v.noise_floor_db,
            perceived_db: (v.level_db - v.noise_floor_db).max(0.0),
            is_speech: v.is_speech,
            peak_db: peak,
            auto_floor_db: v.auto_floor_db,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passthrough() -> CoordinatorConfig {
        CoordinatorConfig {
            use_webrtc_aec: false,
            ..Default::default()
        }
    }
    /// A frame at `db` dBFS RMS (square wave).
    fn tone(db: f32) -> Vec<f32> {
        let a = 10f32.powf(db / 20.0);
        (0..FRAME_SAMPLES)
            .map(|i| if i % 2 == 0 { a } else { -a })
            .collect()
    }

    #[test]
    fn reports_level_floor_and_perceived_level() {
        let mut m = MeterChain::new(&passthrough(), None);
        let r = (0..100).map(|_| m.process(&tone(-50.0))).last().unwrap();
        assert!((r.level_db + 50.0).abs() < 0.1, "{r:?}");
        assert_eq!(r.floor_db, r.auto_floor_db);
        assert_eq!(r.perceived_db, (r.level_db - r.floor_db).max(0.0));
        m.set_baseline(Some(-40.0));
        let r = m.process(&tone(-50.0));
        assert_eq!(r.floor_db, -40.0);
        assert_eq!(r.perceived_db, 0.0, "below the baseline: nothing perceived");
        assert!(!r.is_speech);
        let r = m.process(&tone(-30.0));
        assert!((r.perceived_db - 10.0).abs() < 0.1, "{r:?}");
    }

    #[test]
    fn peak_holds_the_loudest_level_and_decays() {
        let mut m = MeterChain::new(&passthrough(), None);
        m.process(&tone(-60.0));
        let r = m.process(&tone(-20.0));
        assert!((r.peak_db + 20.0).abs() < 0.1);
        let r = (0..50).map(|_| m.process(&tone(-60.0))).last().unwrap();
        assert!(
            (r.peak_db - (-20.0 - 50.0 * PEAK_DECAY_DB_PER_FRAME)).abs() < 0.2,
            "{r:?}"
        );
        let r = (0..500).map(|_| m.process(&tone(-60.0))).last().unwrap();
        assert!(
            (r.peak_db - r.level_db).abs() < 0.1,
            "falls back to the level"
        );
    }

    #[test]
    fn runs_the_coordinators_processing() {
        // With WebRTC processing the meter reads the processed signal (the high-pass filter
        // removes most of a DC offset), not the raw capture.
        let cfg = CoordinatorConfig::default();
        let mut m = MeterChain::new(&cfg, None);
        let dc = vec![0.1f32; FRAME_SAMPLES]; // -20 dBFS raw
        let r = (0..200).map(|_| m.process(&dc)).last().unwrap();
        assert!(r.level_db < -40.0, "{r:?}");
    }
}
