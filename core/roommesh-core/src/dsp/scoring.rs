//! Scores how well a (post-AEC) microphone captures the *current talker*, not how loud it is.
//! SNR is relative to each mic's own noise floor, so differing mic sensitivities cancel out;
//! `relative_snr_db` (this mic's SNR minus the best speaking mic's SNR in the same frame, ≤ 0)
//! keeps mics distinguishable when absolute SNR saturates.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct MicFeatures {
    pub speech_prob: f32,
    pub is_speech: bool,
    pub snr_db: f32,
    pub relative_snr_db: f32,
    pub level_db: f32,
    pub clip_ratio: f32,
    /// Envelope modulation depth 0..1 (direct-to-reverberant proxy).
    pub modulation: f32,
    /// 0..1: how much the cleaned mic still follows the far-end envelope (residual echo).
    pub echo_leak: f32,
}

pub struct MicScorer { history: f32 }

impl Default for MicScorer { fn default() -> Self { Self::new() } }

impl MicScorer {
    pub fn new() -> Self { Self { history: 0.5 } }
    pub fn history(&self) -> f32 { self.history }
    pub fn score(&mut self, f: &MicFeatures) -> f32 {
        let snr_term = ((f.snr_db - 3.0) / 27.0).clamp(0.0, 1.0);
        let rel_term = (1.0 + f.relative_snr_db / 15.0).clamp(0.0, 1.0);
        if f.is_speech { self.history += 0.01 * (snr_term - self.history); }
        let quality = 0.35 * snr_term + 0.3 * rel_term + 0.15 * f.modulation.clamp(0.0, 1.0) + 0.2 * self.history;
        let s = f.speech_prob.clamp(0.0, 1.0) * quality
            - 0.5 * f.clip_ratio.clamp(0.0, 1.0)
            - 0.3 * f.echo_leak.clamp(0.0, 1.0);
        s.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn f(prob: f32, snr: f32) -> MicFeatures {
        MicFeatures { speech_prob: prob, is_speech: prob > 0.5, snr_db: snr, relative_snr_db: 0.0, level_db: -25.0, clip_ratio: 0.0, modulation: 0.5, echo_leak: 0.0 }
    }
    #[test]
    fn closer_mic_scores_higher() {
        let (mut a, mut b) = (MicScorer::new(), MicScorer::new());
        let far = MicFeatures { relative_snr_db: -20.0, ..f(0.95, 10.0) };
        let (sa, sb) = (a.score(&f(0.95, 30.0)), b.score(&far));
        assert!(sa > sb + 0.2, "{sa} {sb}");
        assert!((0.0..=1.0).contains(&sa));
    }
    #[test]
    fn relative_snr_separates_saturated_mics() {
        // both far above the absolute SNR ceiling, 12 dB apart (typical 0.5 m vs 2 m)
        let near = MicScorer::new().score(&f(0.95, 45.0));
        let far = MicScorer::new().score(&MicFeatures { relative_snr_db: -12.0, ..f(0.95, 33.0) });
        assert!(near > far + 0.15, "{near} {far}");
    }
    #[test]
    fn silence_scores_near_zero_and_penalties_apply() {
        let mut s = MicScorer::new();
        assert!(s.score(&f(0.02, 30.0)) < 0.05);
        let base = MicScorer::new().score(&f(0.9, 25.0));
        let mut clip = f(0.9, 25.0); clip.clip_ratio = 0.3;
        let mut echo = f(0.9, 25.0); echo.echo_leak = 0.8;
        assert!(MicScorer::new().score(&clip) < base - 0.1);
        assert!(MicScorer::new().score(&echo) < base - 0.2);
    }
    #[test]
    fn history_tracks_quality_during_speech() {
        let mut s = MicScorer::new();
        for _ in 0..500 { s.score(&f(0.95, 30.0)); }
        assert!(s.history() > 0.9);
    }
}
