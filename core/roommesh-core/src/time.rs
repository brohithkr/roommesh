//! Host clock (same domain as CoreAudio `mHostTime`) and sliding least-squares line fit.
use std::collections::VecDeque;

#[cfg(target_os = "macos")]
fn timebase() -> (u128, u128) {
    use std::sync::OnceLock;
    static TB: OnceLock<(u128, u128)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        (info.numer as u128, info.denom as u128)
    })
}

/// Converts `mach_absolute_time` ticks (CoreAudio host time) to nanoseconds.
#[cfg(target_os = "macos")]
pub fn host_ticks_to_ns(ticks: u64) -> u64 {
    let (n, d) = timebase();
    (ticks as u128 * n / d) as u64
}

#[cfg(target_os = "macos")]
pub fn ns_to_host_ticks(ns: u64) -> u64 {
    let (n, d) = timebase();
    (ns as u128 * d / n) as u64
}

/// Monotonic host time in nanoseconds.
#[cfg(target_os = "macos")]
pub fn now_ns() -> u64 {
    host_ticks_to_ns(unsafe { mach2::mach_time::mach_absolute_time() })
}

#[cfg(not(target_os = "macos"))]
pub fn now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Least-squares fit `y = y0 + slope * (x - x0)` over the last `capacity` points.
/// With fewer than 4 points the slope is `nominal_slope` through the newest point.
/// The slope is clamped to `nominal * (1 ± max_dev)`.
#[derive(Clone, Debug)]
pub struct LinearFit {
    points: VecDeque<(f64, f64)>,
    capacity: usize,
    nominal_slope: f64,
    max_dev: f64,
    x0: f64,
    y0: f64,
    slope: f64,
    valid: bool,
}

impl LinearFit {
    pub fn new(capacity: usize, nominal_slope: f64, max_dev: f64) -> Self {
        Self {
            points: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
            nominal_slope,
            max_dev,
            x0: 0.0,
            y0: 0.0,
            slope: nominal_slope,
            valid: false,
        }
    }
    pub fn clear(&mut self) {
        self.points.clear();
        self.valid = false;
    }
    pub fn len(&self) -> usize {
        self.points.len()
    }
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn push(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() { return; }
        if self.points.len() == self.capacity {
            self.points.pop_front();
        }
        self.points.push_back((x, y));
        self.refit();
    }

    fn refit(&mut self) {
        let n = self.points.len();
        let Some(&(xr, yr)) = self.points.back() else {
            self.valid = false;
            return;
        };
        if n < 4 {
            self.x0 = xr;
            self.y0 = yr;
            self.slope = self.nominal_slope;
            self.valid = true;
            return;
        }
        let nf = n as f64;
        let (mut sx, mut sy) = (0.0, 0.0);
        for &(x, y) in &self.points {
            sx += x - xr;
            sy += y - yr;
        }
        let (mx, my) = (sx / nf, sy / nf);
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for &(x, y) in &self.points {
            let dx = x - xr - mx;
            let dy = y - yr - my;
            sxx += dx * dx;
            sxy += dx * dy;
        }
        let raw = if sxx > 0.0 { sxy / sxx } else { self.nominal_slope };
        let a = self.nominal_slope * (1.0 - self.max_dev);
        let b = self.nominal_slope * (1.0 + self.max_dev);
        self.slope = raw.clamp(a.min(b), a.max(b));
        self.x0 = xr + mx;
        self.y0 = yr + my;
        self.valid = true;
    }

    pub fn eval(&self, x: f64) -> Option<f64> {
        self.valid.then_some(self.y0 + self.slope * (x - self.x0))
    }
    pub fn invert(&self, y: f64) -> Option<f64> {
        self.valid.then_some(self.x0 + (y - self.y0) / self.slope)
    }
    pub fn slope(&self) -> Option<f64> {
        self.valid.then_some(self.slope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn now_is_monotonic() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a);
    }
    #[test]
    fn fit_recovers_line() {
        let mut f = LinearFit::new(100, 2.0, 0.5);
        for i in 0..50 {
            let x = i as f64 * 10.0;
            f.push(x, 7.0 + 2.001 * x);
        }
        let y = f.eval(1000.0).unwrap();
        assert!((y - (7.0 + 2001.0)).abs() < 1e-6, "{y}");
        let x = f.invert(y).unwrap();
        assert!((x - 1000.0).abs() < 1e-6);
    }
    #[test]
    fn fit_clamps_slope_and_handles_few_points() {
        let mut f = LinearFit::new(10, 1.0, 0.001);
        f.push(0.0, 0.0);
        assert_eq!(f.slope(), Some(1.0)); // < 4 points: nominal slope
        for i in 1..10 {
            f.push(i as f64, i as f64 * 5.0);
        }
        assert!((f.slope().unwrap() - 1.001).abs() < 1e-12); // clamped
    }
    #[test]
    fn push_ignores_non_finite_samples() {
        let mut f = LinearFit::new(10, 1.0, 0.001);
        f.push(0.0, 0.0);
        f.push(f64::NAN, 5.0);
        f.push(1.0, f64::INFINITY);
        f.push(f64::NEG_INFINITY, f64::NAN);
        assert_eq!(f.len(), 1);
    }
}
