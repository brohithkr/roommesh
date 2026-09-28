//! Smooths the (frame counter, host time) pairs reported by a CoreAudio IO proc into a
//! presentation-time model. `latency_ns` is added for output (device + safety offset) or
//! should be passed negative for input (capture happened before the IO timestamp).
use crate::time::LinearFit;

pub struct DeviceClock {
    fit: LinearFit,
    origin: Option<(u64, u64)>,
    latency_ns: i64,
}

impl DeviceClock {
    pub fn new(sample_rate: f64, latency_ns: i64) -> Self {
        Self {
            fit: LinearFit::new(256, 1e9 / sample_rate, 2e-3),
            origin: None,
            latency_ns,
        }
    }
    pub fn reset(&mut self) {
        self.fit.clear();
        self.origin = None;
    }
    pub fn report(&mut self, frame_counter: u64, host_ns: u64) {
        // > 20 ms disagreement with the model = the stream paused/restarted: start over.
        if let Some(pred) = self.time_of(frame_counter) {
            let pred = pred as f64 - self.latency_ns as f64;
            if (pred - host_ns as f64).abs() > 20e6 {
                self.reset();
            }
        }
        let (of, ot) = *self.origin.get_or_insert((frame_counter, host_ns));
        self.fit
            .push(frame_counter as f64 - of as f64, host_ns as f64 - ot as f64);
    }
    pub fn time_of(&self, frame_counter: u64) -> Option<u64> {
        let (of, ot) = self.origin?;
        let t =
            self.fit.eval(frame_counter as f64 - of as f64)? + ot as f64 + self.latency_ns as f64;
        Some(t.max(0.0) as u64)
    }
    pub fn frame_at(&self, host_ns: u64) -> Option<f64> {
        let (of, ot) = self.origin?;
        Some(
            self.fit
                .invert(host_ns as f64 - self.latency_ns as f64 - ot as f64)?
                + of as f64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_frame_counter_to_time_with_latency() {
        let mut c = DeviceClock::new(48_000.0, 2_000_000);
        for i in 0..100u64 {
            let jitter = if i % 3 == 0 { 150_000 } else { 0 };
            c.report(
                i * 512,
                1_000_000_000 + i * 512 * 1_000_000_000 / 48_000 + jitter,
            );
        }
        let t = c.time_of(100 * 512).unwrap() as f64;
        let want = 1.0e9 + 100.0 * 512.0 * 1e9 / 48_000.0 + 2.0e6;
        assert!((t - want).abs() < 200_000.0, "{t} vs {want}");
        let f = c.frame_at(want as u64).unwrap();
        assert!((f - 51_200.0).abs() < 10.0);
        assert!(DeviceClock::new(48_000.0, 0).time_of(0).is_none());
    }
    #[test]
    fn resets_after_pause() {
        let mut c = DeviceClock::new(48_000.0, 0);
        for i in 0..50u64 {
            c.report(i * 480, i * 10_000_000);
        }
        // counter continues but 3 s of wall time passed with no frames
        for i in 50..60u64 {
            c.report(i * 480, 3_000_000_000 + i * 10_000_000);
        }
        let t = c.time_of(59 * 480).unwrap();
        assert!((t as i64 - (3_000_000_000i64 + 590_000_000)).abs() < 1_000_000);
    }
}
