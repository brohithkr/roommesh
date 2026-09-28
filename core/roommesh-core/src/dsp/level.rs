//! Per-frame level metrics and a short envelope history (≈500 ms) used for the
//! direct-to-reverberant proxy (modulation depth), same-talker detection and echo-leak checks.
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameLevel {
    pub rms_db: f32,
    pub peak: f32,
    pub clip_ratio: f32,
}

pub fn measure(frame: &[f32]) -> FrameLevel {
    let n = frame.len().max(1) as f32;
    let (mut sq, mut peak, mut clipped) = (0.0f32, 0.0f32, 0usize);
    for &x in frame {
        sq += x * x;
        peak = peak.max(x.abs());
        if x.abs() >= 0.99 {
            clipped += 1;
        }
    }
    FrameLevel {
        rms_db: 10.0 * (sq / n + 1e-12).log10(),
        peak,
        clip_ratio: clipped as f32 / n,
    }
}

pub struct EnvelopeTracker {
    hist: VecDeque<f32>,
    cap: usize,
}

impl EnvelopeTracker {
    pub fn new(cap: usize) -> Self {
        Self {
            hist: VecDeque::with_capacity(cap),
            cap,
        }
    }
    pub fn push(&mut self, level_db: f32) {
        if self.hist.len() == self.cap {
            self.hist.pop_front();
        }
        self.hist.push_back(level_db.max(-90.0));
    }
    /// Standard deviation of the dB envelope mapped to 0..1 (12 dB → 1.0). Close, direct
    /// capture shows deeper syllabic modulation than distant/reverberant capture.
    pub fn modulation(&self) -> f32 {
        let n = self.hist.len();
        if n < 2 {
            return 0.0;
        }
        let mean = self.hist.iter().sum::<f32>() / n as f32;
        let var = self.hist.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n as f32;
        (var.sqrt() / 12.0).clamp(0.0, 1.0)
    }
    /// Pearson correlation of the most recent overlapping history.
    pub fn correlation(&self, other: &EnvelopeTracker) -> f32 {
        let n = self.hist.len().min(other.hist.len());
        if n < 5 {
            return 0.0;
        }
        // Iterate the two VecDeques' tails directly (no intermediate Vec allocations); `pairs()`
        // is a cheap closure so we can make the same zipped iterator twice, once for the means
        // and once for the (co)variances.
        let pairs = || {
            self.hist
                .iter()
                .rev()
                .take(n)
                .zip(other.hist.iter().rev().take(n))
        };
        let (mut sa, mut sb) = (0.0f32, 0.0f32);
        for (&x, &y) in pairs() {
            sa += x;
            sb += y;
        }
        let (ma, mb) = (sa / n as f32, sb / n as f32);
        let (mut sab, mut saa, mut sbb) = (0.0f32, 0.0f32, 0.0f32);
        for (&x, &y) in pairs() {
            let (da, db) = (x - ma, y - mb);
            sab += da * db;
            saa += da * da;
            sbb += db * db;
        }
        if saa <= 1e-9 || sbb <= 1e-9 {
            0.0
        } else {
            sab / (saa.sqrt() * sbb.sqrt())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measures_rms_peak_clip() {
        let f = vec![0.5f32; 480];
        let l = measure(&f);
        assert!((l.rms_db - (-6.0206)).abs() < 0.01);
        assert_eq!(l.peak, 0.5);
        assert_eq!(l.clip_ratio, 0.0);
        let mut c = vec![0.0f32; 480];
        c[..48].fill(1.0);
        assert!((measure(&c).clip_ratio - 0.1).abs() < 1e-6);
        assert!(measure(&[0.0; 480]).rms_db < -100.0);
    }
    #[test]
    fn modulation_and_correlation() {
        let mut flat = EnvelopeTracker::new(50);
        let mut modu = EnvelopeTracker::new(50);
        let mut other = EnvelopeTracker::new(50);
        for i in 0..50 {
            flat.push(-30.0);
            let v = if (i / 5) % 2 == 0 { -20.0 } else { -45.0 };
            modu.push(v);
            other.push(v - 3.0);
        }
        assert!(flat.modulation() < 0.05);
        assert!(modu.modulation() > 0.8);
        assert!(modu.correlation(&other) > 0.99);
        assert!(modu.correlation(&flat).abs() < 0.01);
    }
}
