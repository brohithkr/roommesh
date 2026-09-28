//! 4-point cubic Hermite interpolation for fractional-position reads (drift correction and
//! sample-rate conversion). Adequate for speech at ≤ ±0.1% rate deviation and 44.1↔48 kHz.
use std::collections::VecDeque;

#[inline]
pub fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

/// Value of `src` at fractional index `pos`; `None` without one sample of context each side.
#[inline]
pub fn sample_at(src: &VecDeque<f32>, pos: f64) -> Option<f32> {
    if pos.is_nan() || pos < 1.0 { return None; }
    let i = pos.floor() as usize;
    if i + 2 >= src.len() { return None; }
    let t = (pos - i as f64) as f32;
    Some(hermite(src[i - 1], src[i], src[i + 1], src[i + 2], t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    #[test]
    fn exact_at_integer_positions_and_smooth_between() {
        let src: VecDeque<f32> = (0..100).map(|i| (i as f32 * 0.05).sin()).collect();
        assert_eq!(sample_at(&src, 10.0).unwrap(), src[10]);
        let v = sample_at(&src, 10.5).unwrap();
        assert!((v - (10.5f32 * 0.05).sin()).abs() < 1e-4);
        assert!(sample_at(&src, 0.5).is_none());
        assert!(sample_at(&src, 98.0).is_none());
    }
}
