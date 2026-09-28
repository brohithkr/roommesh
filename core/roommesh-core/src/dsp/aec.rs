//! Per-microphone acoustic echo cancellation. The coordinator runs one canceller per mic,
//! each fed the same far-end reference aligned to the mic's capture time (see engine::coordinator).
use webrtc_audio_processing::Processor;
use webrtc_audio_processing_config as apm;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AecStats {
    pub erle_db: Option<f32>,
    pub delay_ms: Option<u32>,
    pub converged: bool,
}

pub trait EchoCanceller: Send {
    /// `reference`: 10 ms of far-end audio played at the time `mic` was captured. `mic` is
    /// replaced by the echo-cancelled signal.
    fn process(&mut self, reference: &[f32], mic: &mut [f32]);
    fn stats(&self) -> AecStats;
}

pub struct PassthroughAec;
impl EchoCanceller for PassthroughAec {
    fn process(&mut self, _reference: &[f32], _mic: &mut [f32]) {}
    fn stats(&self) -> AecStats {
        AecStats::default()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("webrtc apm: {0}")]
pub struct AecError(String);

pub struct WebRtcAec {
    ap: Processor,
    render: Vec<f32>,
}

impl WebRtcAec {
    pub fn new(noise_suppression: bool) -> Result<Self, AecError> {
        let ap = Processor::new(48_000).map_err(|e| AecError(format!("{e:?}")))?;
        ap.set_config(apm::Config {
            echo_canceller: Some(apm::EchoCanceller::default()),
            high_pass_filter: Some(apm::HighPassFilter::default()),
            noise_suppression: noise_suppression.then(|| apm::NoiseSuppression {
                level: apm::NoiseSuppressionLevel::Moderate,
                ..Default::default()
            }),
            ..Default::default()
        });
        Ok(Self { ap, render: vec![0.0; 480] })
    }
}

impl EchoCanceller for WebRtcAec {
    fn process(&mut self, reference: &[f32], mic: &mut [f32]) {
        self.render.copy_from_slice(&reference[..480]);
        if let Err(e) = self.ap.process_render_frame([&mut self.render[..]]) {
            log::warn!("aec render: {e:?}");
        }
        if let Err(e) = self.ap.process_capture_frame([&mut mic[..480]]) {
            log::warn!("aec capture: {e:?}");
        }
    }
    fn stats(&self) -> AecStats {
        let s = self.ap.get_stats();
        let erle = s.echo_return_loss_enhancement.map(|v| v as f32);
        AecStats { erle_db: erle, delay_ms: s.delay_ms, converged: erle.is_some_and(|e| e > 6.0) }
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
    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|v| v * v).sum()
    }

    #[test]
    fn passthrough_leaves_mic_untouched() {
        let mut a = PassthroughAec;
        let mut mic = vec![0.25f32; 480];
        a.process(&[0.5; 480], &mut mic);
        assert!(mic.iter().all(|v| *v == 0.25));
    }

    #[test]
    fn webrtc_aec_removes_linear_echo() {
        let mut aec = WebRtcAec::new(false).unwrap();
        let mut rng = Rng(99);
        let delay = 240; // 5 ms acoustic path
        let total = 600; // 6 s
        let reference: Vec<f32> = (0..total * 480)
            .map(|i| {
                let env = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.0 * i as f32 / 48_000.0).sin();
                0.3 * env * rng.uni()
            })
            .collect();
        let (mut e_in, mut e_out) = (0.0f32, 0.0f32);
        for f in 0..total {
            let r = &reference[f * 480..(f + 1) * 480];
            let mut mic: Vec<f32> = (0..480)
                .map(|k| {
                    let i = f * 480 + k;
                    let echo = if i >= delay { 0.5 * reference[i - delay] } else { 0.0 };
                    echo + 0.0005 * rng.uni()
                })
                .collect();
            let before = energy(&mic);
            aec.process(r, &mut mic);
            if f >= 400 {
                e_in += before;
                e_out += energy(&mic);
            }
        }
        let erle = 10.0 * (e_in / e_out.max(1e-12)).log10();
        assert!(erle > 10.0, "echo reduction only {erle} dB");
        assert!(aec.stats().delay_ms.is_some() || aec.stats().erle_db.is_some());
    }
}
