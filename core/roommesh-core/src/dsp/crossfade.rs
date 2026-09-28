//! Equal-power fade gains (sin curve) and linear ramps used for mic transitions.
use std::f32::consts::FRAC_PI_2;

pub struct FadeGain {
    pos: f32,
    target: f32,
    step: f32,
}

impl FadeGain {
    pub fn new(fade_samples: usize, initially_on: bool) -> Self {
        let p = if initially_on { 1.0 } else { 0.0 };
        Self {
            pos: p,
            target: p,
            step: 1.0 / fade_samples.max(1) as f32,
        }
    }
    pub fn set_on(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }
    pub fn is_on(&self) -> bool {
        self.target > 0.5
    }
    pub fn is_silent(&self) -> bool {
        self.pos == 0.0 && self.target == 0.0
    }
    #[inline]
    #[allow(clippy::should_implement_trait)] // `next()` is this plan's public API name, not an Iterator.
    pub fn next(&mut self) -> f32 {
        if self.pos < self.target {
            self.pos = (self.pos + self.step).min(self.target);
        } else if self.pos > self.target {
            self.pos = (self.pos - self.step).max(self.target);
        }
        (self.pos * FRAC_PI_2).sin()
    }
}

pub struct LinearRamp {
    value: f32,
    target: f32,
    step: f32,
}

impl LinearRamp {
    pub fn new(initial: f32, ramp_samples: usize) -> Self {
        Self {
            value: initial,
            target: initial,
            step: 1.0 / ramp_samples.max(1) as f32,
        }
    }
    pub fn set_target(&mut self, t: f32) {
        self.target = t;
    }
    #[inline]
    #[allow(clippy::should_implement_trait)] // `next()` is this plan's public API name, not an Iterator.
    pub fn next(&mut self) -> f32 {
        if self.value < self.target {
            self.value = (self.value + self.step).min(self.target);
        } else if self.value > self.target {
            self.value = (self.value - self.step).max(self.target);
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fade_in_reaches_unity_and_is_equal_power_with_fade_out() {
        let mut a = FadeGain::new(100, true);
        let mut b = FadeGain::new(100, false);
        a.set_on(false);
        b.set_on(true);
        for _ in 0..100 {
            let (ga, gb) = (a.next(), b.next());
            assert!((ga * ga + gb * gb - 1.0).abs() < 0.03);
        }
        assert_eq!(a.next(), 0.0);
        assert!((b.next() - 1.0).abs() < 1e-6);
        assert!(a.is_silent());
    }
}
