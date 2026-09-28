//! Mixes the selected microphones: per-mic equal-power fades (crossfade on switches) and
//! 1/sqrt(n) normalization when several talkers are admitted, followed by a soft clipper.
use crate::dsp::arbitration::Selection;
use crate::dsp::crossfade::{FadeGain, LinearRamp};
use crate::ids::PeerId;
use std::collections::BTreeMap;

pub struct Mixer { fade_samples: usize, gains: BTreeMap<PeerId, FadeGain>, norm: LinearRamp }

pub fn soft_clip(x: f32) -> f32 {
    const T: f32 = 0.9;
    let a = x.abs();
    if a <= T {
        x
    } else {
        // Rational (x / (1 + x)) knee rather than tanh: tanh((a - T) / (1 - T)) rounds to
        // exactly 1.0f32 for any a beyond ~T + 4*(1-T), which would make this saturate to
        // exactly unity instead of staying strictly below it.
        let z = (a - T) / (1.0 - T);
        x.signum() * (T + (1.0 - T) * (z / (1.0 + z)))
    }
}

impl Mixer {
    pub fn new(fade_ms: f32) -> Self {
        let fade_samples = (48.0 * fade_ms) as usize;
        Self { fade_samples, gains: BTreeMap::new(), norm: LinearRamp::new(1.0, fade_samples) }
    }
    pub fn set_selection(&mut self, sel: &Selection) {
        for (peer, g) in self.gains.iter_mut() { g.set_on(sel.contains(*peer)); }
        for p in [sel.primary, sel.secondary].into_iter().flatten() {
            self.gains.entry(p).or_insert_with(|| FadeGain::new(self.fade_samples, false)).set_on(true);
        }
        self.norm.set_target(1.0 / (sel.count().max(1) as f32).sqrt());
    }
    pub fn mix(&mut self, inputs: &BTreeMap<PeerId, Vec<f32>>, out: &mut [f32]) {
        out.fill(0.0);
        for (i, o) in out.iter_mut().enumerate() {
            let n = self.norm.next();
            let mut acc = 0.0;
            for (peer, g) in self.gains.iter_mut() {
                let gv = g.next();
                if gv > 0.0 {
                    if let Some(x) = inputs.get(peer).and_then(|v| v.get(i)) { acc += gv * n * x; }
                }
            }
            *o = soft_clip(acc);
        }
        self.gains.retain(|_, g| !g.is_silent());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::arbitration::Selection;
    use crate::ids::PeerId;
    use std::collections::BTreeMap;
    const B: PeerId = PeerId(2);
    const C: PeerId = PeerId(3);
    struct Rng(u64);
    impl Rng { fn uni(&mut self) -> f32 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; (self.0 >> 40) as f32 / (1u64 << 24) as f32 - 0.5 } }
    fn inputs(rng: &mut Rng) -> BTreeMap<PeerId, Vec<f32>> {
        let mut m = BTreeMap::new();
        m.insert(B, (0..480).map(|_| rng.uni() * 0.2).collect());
        m.insert(C, (0..480).map(|_| rng.uni() * 0.2).collect());
        m
    }
    #[test]
    fn silent_without_selection() {
        let mut mx = Mixer::new(30.0);
        let mut out = vec![1.0; 480];
        mx.mix(&inputs(&mut Rng(1)), &mut out);
        assert!(out.iter().all(|v| *v == 0.0));
    }
    #[test]
    fn crossfades_and_settles_on_new_mic() {
        let mut rng = Rng(9);
        let mut mx = Mixer::new(30.0);
        mx.set_selection(&Selection { primary: Some(B), secondary: None });
        let mut out = vec![0.0; 480];
        for _ in 0..10 { mx.mix(&inputs(&mut rng), &mut out); }
        mx.set_selection(&Selection { primary: Some(C), secondary: None });
        for _ in 0..3 {
            let inp = inputs(&mut rng);
            mx.mix(&inp, &mut out);
            let p_out: f32 = out.iter().map(|v| v * v).sum();
            let p_in: f32 = inp[&C].iter().map(|v| v * v).sum();
            assert!((p_out / p_in - 1.0).abs() < 0.35, "power ratio {}", p_out / p_in);
        }
        for _ in 0..3 { mx.mix(&inputs(&mut rng), &mut out); }
        let inp = inputs(&mut rng);
        mx.mix(&inp, &mut out);
        for i in 0..480 { assert!((out[i] - inp[&C][i]).abs() < 1e-5); }
    }
    #[test]
    fn two_talkers_are_normalized() {
        let mut mx = Mixer::new(30.0);
        mx.set_selection(&Selection { primary: Some(B), secondary: Some(C) });
        let mut ones = BTreeMap::new();
        ones.insert(B, vec![0.3f32; 480]);
        ones.insert(C, vec![0.3f32; 480]);
        let mut out = vec![0.0; 480];
        for _ in 0..10 { mx.mix(&ones, &mut out); }
        assert!((out[479] - 0.6 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
    }
    #[test]
    fn soft_clip_bounds_output() {
        assert!(soft_clip(5.0) < 1.0);
        assert_eq!(soft_clip(0.5), 0.5);
    }
}
