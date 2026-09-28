//! NTP-style offset/drift estimation of the coordinator clock relative to the local host clock.
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSample {
    /// local send time
    pub t1: u64,
    /// coordinator receive time
    pub t2: u64,
    /// coordinator send time
    pub t3: u64,
    /// local receive time
    pub t4: u64,
}

#[derive(Clone, Copy, Debug)]
struct Measurement { local_mid: f64, offset: f64, rtt: f64 }

#[derive(Clone, Copy, Debug)]
struct Model { reference: f64, offset: f64, drift: f64 }

pub struct ClockEstimator { samples: VecDeque<Measurement>, capacity: usize, model: Option<Model> }

pub const MIN_SAMPLES: usize = 4;
const RTT_SLACK_NS: f64 = 2.0e6;
const MIN_SPAN_FOR_DRIFT_NS: f64 = 2.0e9;

impl Default for ClockEstimator { fn default() -> Self { Self::new() } }

impl ClockEstimator {
    pub fn new() -> Self { Self { samples: VecDeque::new(), capacity: 64, model: None } }
    pub fn reset(&mut self) { self.samples.clear(); self.model = None; }

    pub fn add_sample(&mut self, s: ClockSample) {
        // t4 < t1 (reply arrived before the request was sent) or t3 < t2 (coordinator replied
        // before it received the request) is causally impossible; such a sample is corrupt
        // (clock stepped backwards, malformed packet) and would poison the RTT/offset fit.
        if s.t4 < s.t1 || s.t3 < s.t2 { return; }
        // The round trip (t4 - t1) must be at least the coordinator's own processing time
        // (t3 - t2): the remaining time is network transit, which can't be negative. Without
        // this check a corrupt sample would silently clamp to rtt=0 below instead of being
        // rejected outright.
        if (s.t4 - s.t1) < (s.t3 - s.t2) { return; }
        let (t1, t2, t3, t4) = (s.t1 as f64, s.t2 as f64, s.t3 as f64, s.t4 as f64);
        let rtt = ((t4 - t1) - (t3 - t2)).max(0.0);
        let offset = ((t2 - t1) + (t3 - t4)) / 2.0;
        if self.samples.len() == self.capacity { self.samples.pop_front(); }
        self.samples.push_back(Measurement { local_mid: (t1 + t4) / 2.0, offset, rtt });
        self.recompute();
    }

    fn recompute(&mut self) {
        let min_rtt = self.samples.iter().map(|m| m.rtt).fold(f64::INFINITY, f64::min);
        let sel: Vec<Measurement> = self.samples.iter().copied().filter(|m| m.rtt <= min_rtt + RTT_SLACK_NS).collect();
        if sel.is_empty() { self.model = None; return; }
        let n = sel.len() as f64;
        let reference = sel.iter().map(|m| m.local_mid).sum::<f64>() / n;
        let mean_off = sel.iter().map(|m| m.offset).sum::<f64>() / n;
        let span = sel.iter().map(|m| m.local_mid).fold(f64::MIN, f64::max)
            - sel.iter().map(|m| m.local_mid).fold(f64::MAX, f64::min);
        let drift = if sel.len() >= 3 && span >= MIN_SPAN_FOR_DRIFT_NS {
            let (mut sxx, mut sxy) = (0.0, 0.0);
            for m in &sel {
                let dx = m.local_mid - reference;
                sxx += dx * dx;
                sxy += dx * (m.offset - mean_off);
            }
            if sxx > 0.0 { (sxy / sxx).clamp(-500e-6, 500e-6) } else { 0.0 }
        } else { 0.0 };
        self.model = Some(Model { reference, offset: mean_off, drift });
    }

    pub fn is_synced(&self) -> bool { self.samples.len() >= MIN_SAMPLES && self.model.is_some() }

    /// Maps a local timestamp into the coordinator's clock domain. Returns `None` until a model
    /// has been fit (see [`Self::is_synced`]); callers that need a value (rather than treating
    /// `None` as "not yet synced, hold off") should check `is_synced()` first.
    pub fn to_coord(&self, local_ns: u64) -> Option<u64> {
        let m = self.model?;
        let l = local_ns as f64;
        Some((l + m.offset + m.drift * (l - m.reference)).max(0.0) as u64)
    }
    /// Maps a coordinator timestamp into the local clock domain. See [`Self::to_coord`]; callers
    /// should check [`Self::is_synced`] before relying on the result.
    pub fn to_local(&self, coord_ns: u64) -> Option<u64> {
        let m = self.model?;
        let c = coord_ns as f64;
        Some(((c - m.offset + m.drift * m.reference) / (1.0 + m.drift)).max(0.0) as u64)
    }
    pub fn offset_ns(&self) -> Option<f64> { self.model.map(|m| m.offset) }
    pub fn drift_ppm(&self) -> Option<f64> { self.model.map(|m| m.drift * 1e6) }
    pub fn min_rtt_ns(&self) -> Option<f64> {
        self.samples.iter().map(|m| m.rtt).reduce(f64::min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Rng(u64);
    impl Rng { fn next(&mut self) -> f64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; (self.0 % 10_000) as f64 / 10_000.0 } }

    #[test]
    fn estimates_offset_and_drift() {
        // coordinator = local * (1 + 50ppm) + 3 ms
        let truth = |l: f64| l * (1.0 + 50e-6) + 3.0e6;
        let inv = |c: f64| (c - 3.0e6) / (1.0 + 50e-6);
        let mut est = ClockEstimator::new();
        let mut rng = Rng(0x1234_5678);
        let mut local = 1_000_000_000_000.0;
        for _ in 0..80 {
            let fwd = 1.0e6 + rng.next() * 4.0e6;
            let back = 1.0e6 + rng.next() * 4.0e6;
            let t1 = local;
            let t2 = truth(t1 + fwd);
            let t3 = t2 + 50_000.0;
            let t4 = inv(t3) + back;
            est.add_sample(ClockSample { t1: t1 as u64, t2: t2 as u64, t3: t3 as u64, t4: t4 as u64 });
            local += 250.0e6;
        }
        assert!(est.is_synced());
        let x = local as u64;
        let err = est.to_coord(x).unwrap() as f64 - truth(local);
        assert!(err.abs() < 500_000.0, "offset error {err} ns");
        let drift = est.drift_ppm().unwrap();
        assert!((drift - 50.0).abs() < 15.0, "drift {drift}");
        let back = est.to_local(est.to_coord(x).unwrap()).unwrap();
        assert!((back as i64 - x as i64).abs() < 10);
    }
    #[test]
    fn not_synced_until_enough_samples() {
        let mut est = ClockEstimator::new();
        assert!(est.to_coord(5).is_none());
        est.add_sample(ClockSample { t1: 0, t2: 10, t3: 11, t4: 20 });
        assert!(!est.is_synced());
    }
    #[test]
    fn ignores_causally_impossible_samples() {
        let mut est = ClockEstimator::new();
        est.add_sample(ClockSample { t1: 100, t2: 50, t3: 60, t4: 50 }); // t4 < t1
        est.add_sample(ClockSample { t1: 0, t2: 50, t3: 40, t4: 100 }); // t3 < t2
        // Round trip (t4 - t1 = 5) shorter than the coordinator's own processing time
        // (t3 - t2 = 20): the remaining network transit time would have to be negative.
        est.add_sample(ClockSample { t1: 0, t2: 100, t3: 120, t4: 5 });
        assert!(est.min_rtt_ns().is_none());
        assert!(!est.is_synced());
    }
}
