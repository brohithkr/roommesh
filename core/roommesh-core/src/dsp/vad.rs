//! Lightweight VAD: SNR against a tracked noise floor + spectral flatness + speech-band ratio,
//! smoothed into a probability with hysteresis and hangover. Tuned for 48 kHz 10 ms frames.
use crate::dsp::level::measure;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::sync::Arc;

const FFT_LEN: usize = 512;
/// Frames quieter than this are digital silence (a processing warm-up, a muted source), not room
/// noise: they never seed or move the noise floor.
const DIGITAL_SILENCE_DB: f32 = -90.0;
/// The noise floor is never below this percentile of the last `FLOOR_WINDOW` frames' levels, so
/// a dip shorter than about 100 ms can't drag it down and leave steady noise looking like speech.
const FLOOR_WINDOW: usize = 200;
const FLOOR_PERCENTILE: f32 = 0.05;
const BIN_HZ: f32 = 48_000.0 / FFT_LEN as f32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VadResult {
    pub speech_prob: f32,
    pub is_speech: bool,
    pub level_db: f32,
    pub noise_floor_db: f32,
    pub snr_db: f32,
}

pub struct Vad {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    /// Reusable scratch buffer for per-bin power, sized to `spectrum` and refilled each call.
    power: Vec<f32>,
    /// Levels of the last `FLOOR_WINDOW` non-silent frames, and a scratch copy for the percentile.
    recent_levels: VecDeque<f32>,
    level_scratch: Vec<f32>,
    noise_floor_db: f32,
    prob: f32,
    hangover: u32,
    frames: u64,
    speech: bool,
}

impl Default for Vad {
    fn default() -> Self {
        Self::new()
    }
}

impl Vad {
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_LEN);
        let input = fft.make_input_vec();
        let spectrum = fft.make_output_vec();
        let power = vec![0.0f32; spectrum.len()];
        let window = (0..480)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / 479.0).cos())
            .collect();
        Self {
            fft,
            window,
            input,
            spectrum,
            power,
            recent_levels: VecDeque::with_capacity(FLOOR_WINDOW),
            level_scratch: Vec::with_capacity(FLOOR_WINDOW),
            noise_floor_db: -60.0,
            prob: 0.0,
            hangover: 0,
            frames: 0,
            speech: false,
        }
    }

    /// `FLOOR_PERCENTILE` of the recent frame levels: the level room noise alone reaches.
    fn recent_level_percentile(&mut self) -> f32 {
        self.level_scratch.clear();
        self.level_scratch
            .extend(self.recent_levels.iter().copied());
        let k = ((self.level_scratch.len() - 1) as f32 * FLOOR_PERCENTILE) as usize;
        let (_, v, _) = self
            .level_scratch
            .select_nth_unstable_by(k, |a, b| a.total_cmp(b));
        *v
    }

    pub fn process(&mut self, frame: &[f32]) -> VadResult {
        let level_db = measure(frame).rms_db;
        if !level_db.is_finite() {
            // Garbage/non-finite input: decay speech_prob/hangover exactly as a genuine
            // deep-silence frame would (see the `level_db < -75.0` floor below), so a broken or
            // glitching source doesn't get stuck reporting "speaking" forever. Never touch the
            // noise floor estimate, though -- it has nothing sane to learn from this frame.
            self.prob *= 0.6;
            if !self.prob.is_finite() {
                self.prob = 0.0;
            }
            if self.prob > 0.6 {
                self.speech = true;
                self.hangover = 15;
            } else if self.prob < 0.4 {
                if self.hangover > 0 {
                    self.hangover -= 1;
                } else {
                    self.speech = false;
                }
            }
            return VadResult {
                speech_prob: self.prob,
                is_speech: self.speech,
                level_db: -120.0,
                noise_floor_db: self.noise_floor_db,
                snr_db: 0.0,
            };
        }
        for (i, x) in self.input.iter_mut().enumerate() {
            *x = if i < frame.len() && i < 480 {
                frame[i] * self.window[i]
            } else {
                0.0
            };
        }
        let _ = self.fft.process(&mut self.input, &mut self.spectrum);
        let lo = (300.0 / BIN_HZ) as usize;
        let voice_hi = (3400.0 / BIN_HZ) as usize;
        let flat_hi = (4000.0 / BIN_HZ) as usize;
        for (p, c) in self.power.iter_mut().zip(self.spectrum.iter()) {
            *p = c.norm_sqr() + 1e-12;
        }
        let power = &self.power;
        let band = &power[lo..=flat_hi];
        let geo = (band.iter().map(|p| p.ln()).sum::<f32>() / band.len() as f32).exp();
        let arith = band.iter().sum::<f32>() / band.len() as f32;
        let flatness = (geo / arith).clamp(0.0, 1.0);
        let total: f32 = power[1..].iter().sum();
        let band_ratio = power[lo..=voice_hi].iter().sum::<f32>() / total;

        if level_db >= DIGITAL_SILENCE_DB {
            if self.recent_levels.is_empty() {
                self.noise_floor_db = level_db;
            } else if level_db < self.noise_floor_db {
                self.noise_floor_db += 0.2 * (level_db - self.noise_floor_db);
            } else {
                let rise: f32 = if self.speech { 0.005 } else { 0.05 };
                self.noise_floor_db += rise.min(level_db - self.noise_floor_db);
            }
            if self.recent_levels.len() == FLOOR_WINDOW {
                self.recent_levels.pop_front();
            }
            self.recent_levels.push_back(level_db);
            self.noise_floor_db = self.noise_floor_db.max(self.recent_level_percentile());
        }
        self.noise_floor_db = self.noise_floor_db.max(-100.0);
        let snr_db = level_db - self.noise_floor_db;

        let mut logit = 0.4 * (snr_db - 8.0) + 6.0 * (0.35 - flatness) + 3.0 * (band_ratio - 0.5);
        if level_db < -75.0 {
            logit = -10.0;
        }
        let inst = 1.0 / (1.0 + (-logit).exp());
        self.prob = 0.6 * self.prob + 0.4 * inst;
        if !self.prob.is_finite() {
            self.prob = 0.0;
        }
        if self.prob > 0.6 {
            self.speech = true;
            self.hangover = 15;
        } else if self.prob < 0.4 {
            if self.hangover > 0 {
                self.hangover -= 1;
            } else {
                self.speech = false;
            }
        }
        self.frames += 1;
        VadResult {
            speech_prob: self.prob,
            is_speech: self.speech,
            level_db,
            noise_floor_db: self.noise_floor_db,
            snr_db,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Rng(u64);
    impl Rng {
        fn uni(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
    }
    fn amp(db: f32) -> f32 {
        10f32.powf(db / 20.0) * 3f32.sqrt()
    }
    fn noise(rng: &mut Rng, db: f32) -> Vec<f32> {
        (0..480).map(|_| rng.uni() * amp(db)).collect()
    }
    fn voiced(frame: usize, rng: &mut Rng, noise_db: f32) -> Vec<f32> {
        (0..480)
            .map(|k| {
                let t = (frame * 480 + k) as f32 / 48_000.0;
                let env = 0.55 + 0.45 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                let v: f32 = (1..=10)
                    .map(|h| (2.0 * std::f32::consts::PI * 140.0 * h as f32 * t).sin() / h as f32)
                    .sum();
                0.08 * env * v + rng.uni() * amp(noise_db)
            })
            .collect()
    }
    #[test]
    fn silence_is_not_speech() {
        let mut v = Vad::new();
        for _ in 0..100 {
            let r = v.process(&[0.0; 480]);
            assert!(r.speech_prob.is_finite());
            assert!(!r.is_speech);
        }
    }
    #[test]
    fn stationary_noise_is_not_speech() {
        let mut v = Vad::new();
        let mut rng = Rng(42);
        let res: Vec<VadResult> = (0..200)
            .map(|_| v.process(&noise(&mut rng, -40.0)))
            .collect();
        assert_eq!(res[100..].iter().filter(|r| r.is_speech).count(), 0);
    }
    #[test]
    fn voiced_signal_detected_then_released() {
        let mut v = Vad::new();
        let mut rng = Rng(7);
        for _ in 0..100 {
            v.process(&noise(&mut rng, -50.0));
        }
        let speech: Vec<VadResult> = (0..100)
            .map(|f| v.process(&voiced(f, &mut rng, -50.0)))
            .collect();
        let frac = speech[10..].iter().filter(|r| r.is_speech).count() as f32 / 90.0;
        assert!(frac > 0.8, "speech fraction {frac}");
        assert!(speech[50].snr_db > 15.0);
        let after: Vec<VadResult> = (0..100)
            .map(|_| v.process(&noise(&mut rng, -50.0)))
            .collect();
        assert!(after[70..].iter().all(|r| !r.is_speech));
    }
    #[test]
    fn near_silent_first_frame_does_not_seed_the_noise_floor() {
        // WebRTC's echo canceller emits a near-silent first frame (about -98 dB). Seeding the
        // floor from it made steady room noise look like 30 dB SNR speech for about a minute.
        let mut v = Vad::new();
        let mut rng = Rng(5);
        v.process(&noise(&mut rng, -98.0));
        let res: Vec<VadResult> = (0..300)
            .map(|_| v.process(&noise(&mut rng, -50.0)))
            .collect();
        let speech = res[50..].iter().filter(|r| r.is_speech).count();
        assert_eq!(
            speech, 0,
            "steady noise reported as speech in {speech} frames"
        );
    }
    #[test]
    fn brief_quiet_dip_does_not_make_noise_look_like_speech() {
        let mut v = Vad::new();
        let mut rng = Rng(9);
        for _ in 0..200 {
            v.process(&noise(&mut rng, -50.0));
        }
        for _ in 0..5 {
            v.process(&noise(&mut rng, -85.0)); // 50 ms dip (processing glitch, AEC mute)
        }
        let res: Vec<VadResult> = (0..300)
            .map(|_| v.process(&noise(&mut rng, -50.0)))
            .collect();
        let speech = res.iter().filter(|r| r.is_speech).count();
        assert_eq!(
            speech, 0,
            "steady noise reported as speech in {speech} frames"
        );
    }
    #[test]
    fn non_finite_frame_is_handled_without_corrupting_state() {
        let mut v = Vad::new();
        let mut rng = Rng(3);
        for _ in 0..20 {
            v.process(&noise(&mut rng, -40.0));
        }
        let prev = v.process(&noise(&mut rng, -40.0));
        let nan_frame = vec![f32::NAN; 480];
        let r = v.process(&nan_frame);
        assert!(r.speech_prob.is_finite());
        // A non-finite frame decays speech_prob toward silence (like a genuine quiet frame)
        // rather than leaving it frozen; it must never increase it.
        assert!(r.speech_prob <= prev.speech_prob);
        assert_eq!(r.level_db, -120.0);
        assert_eq!(r.snr_db, 0.0);
        assert_eq!(
            r.noise_floor_db, prev.noise_floor_db,
            "must never move the noise floor"
        );
        // Subsequent normal frames still behave sanely: state was not corrupted.
        let after = v.process(&noise(&mut rng, -40.0));
        assert!(after.speech_prob.is_finite());
        assert!(!after.is_speech);
    }
    #[test]
    fn non_finite_frames_decay_speech_state_without_touching_noise_floor() {
        let mut v = Vad::new();
        let mut rng = Rng(7);
        for _ in 0..100 {
            v.process(&noise(&mut rng, -50.0));
        }
        let last_speech = (0..100)
            .map(|f| v.process(&voiced(f, &mut rng, -50.0)))
            .last()
            .unwrap();
        assert!(
            last_speech.is_speech,
            "should be speaking before the glitch"
        );
        let noise_floor = last_speech.noise_floor_db;
        let mut r = last_speech;
        for _ in 0..30 {
            r = v.process(&[f32::NAN; 480]);
            assert!(r.speech_prob.is_finite());
            assert_eq!(
                r.noise_floor_db, noise_floor,
                "non-finite frames must never move the noise floor"
            );
        }
        assert!(
            !r.is_speech,
            "a broken source must not be reported as speaking forever"
        );
    }
}
