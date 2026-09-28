//! A sample stream addressable by absolute time. `push` adds contiguous chunks tagged with the
//! stream sample index and the timestamp of their first sample; a sliding linear fit maps
//! index↔time (absorbing jitter and drift). `read` renders the samples for times
//! `start + k * out_ns_per_sample` by interpolation. Reads must be (roughly) monotonic:
//! history before the last read start is discarded.
use crate::audio::resampler::sample_at;
use crate::time::LinearFit;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadStatus {
    pub filled: usize,
    pub missing: usize,
}

pub struct TimelineReader {
    buf: VecDeque<f32>,
    base_index: u64,
    next_index: Option<u64>,
    fit: LinearFit,
    origin: Option<(u64, u64)>,
    max_samples: usize,
}

impl TimelineReader {
    pub fn new(sample_rate: u32, max_seconds: f64) -> Self {
        Self {
            buf: VecDeque::new(),
            base_index: 0,
            next_index: None,
            fit: LinearFit::new(400, 1e9 / sample_rate as f64, 1e-3),
            origin: None,
            max_samples: (sample_rate as f64 * max_seconds) as usize,
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.next_index = None;
        self.fit.clear();
        self.origin = None;
    }

    pub fn push(&mut self, first_index: u64, timestamp_ns: u64, samples: &[f32]) {
        // A timestamp that disagrees with the fitted line by > 20 ms means the source paused or
        // restarted: start a fresh segment instead of corrupting the fit.
        if let Some((oi, ot)) = self.origin {
            if let Some(pred) = self.fit.eval(first_index as f64 - oi as f64) {
                if (pred + ot as f64 - timestamp_ns as f64).abs() > 20e6 {
                    self.reset();
                }
            }
        }
        let mut idx = first_index;
        let mut data = samples;
        match self.next_index {
            None => {
                self.buf.clear();
                self.base_index = idx;
            }
            Some(expected) if idx > expected => {
                let gap = (idx - expected) as usize;
                if gap > self.max_samples {
                    self.reset();
                    self.base_index = idx;
                } else {
                    self.buf.extend(std::iter::repeat_n(0.0, gap));
                }
            }
            Some(expected) if idx < expected => {
                // A genuine stream restart (index reset alongside a time jump) is already caught
                // above by the timestamp-discontinuity check, which resets `next_index` to `None`
                // before we ever reach this arm. What's left here is a plain overlap: trim the
                // already-seen prefix and splice the rest onto the existing segment.
                let overlap = (expected - idx) as usize;
                if overlap >= data.len() {
                    return;
                }
                data = &data[overlap..];
                idx = expected;
            }
            _ => {}
        }
        let (oi, ot) = *self.origin.get_or_insert((first_index, timestamp_ns));
        self.fit.push(
            first_index as f64 - oi as f64,
            timestamp_ns as f64 - ot as f64,
        );
        self.buf.extend(data.iter().copied());
        self.next_index = Some(idx + data.len() as u64);
        if self.buf.len() > self.max_samples {
            let drop = self.buf.len() - self.max_samples;
            self.buf.drain(..drop);
            self.base_index += drop as u64;
        }
    }

    /// Fractional stream index at time `t_ns`.
    pub fn index_at(&self, t_ns: u64) -> Option<f64> {
        let (oi, ot) = self.origin?;
        Some(self.fit.invert(t_ns as f64 - ot as f64)? + oi as f64)
    }

    /// Timestamp of the newest sample that can be interpolated.
    pub fn available_until_ns(&self) -> Option<u64> {
        let (oi, ot) = self.origin?;
        if self.buf.len() < 4 {
            return None;
        }
        let last = self.base_index + self.buf.len() as u64 - 3;
        Some((self.fit.eval(last as f64 - oi as f64)? + ot as f64) as u64)
    }

    pub fn read(&mut self, start_ns: u64, out_ns_per_sample: f64, out: &mut [f32]) -> ReadStatus {
        let mut st = ReadStatus::default();
        let (Some(i0), Some(slope)) = (self.index_at(start_ns), self.fit.slope()) else {
            out.fill(0.0);
            st.missing = out.len();
            return st;
        };
        let step = out_ns_per_sample / slope;
        let base = self.base_index as f64;
        for (k, o) in out.iter_mut().enumerate() {
            match sample_at(&self.buf, i0 + k as f64 * step - base) {
                Some(v) => {
                    *o = v;
                    st.filled += 1;
                }
                None => {
                    *o = 0.0;
                    st.missing += 1;
                }
            }
        }
        let drop = ((i0 - base).floor() as i64 - 2).clamp(0, self.buf.len() as i64) as usize;
        self.buf.drain(..drop);
        self.base_index += drop as u64;
        st
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    const T0: f64 = 5.0e12;

    fn feed(tl: &mut TimelineReader, fs_true: f64, chunks: usize, chunk: usize, f: f64) {
        for c in 0..chunks {
            let n0 = (c * chunk) as u64;
            let t0 = T0 + n0 as f64 * 1e9 / fs_true;
            let s: Vec<f32> = (0..chunk)
                .map(|k| {
                    let t = (n0 as f64 + k as f64) / fs_true;
                    (2.0 * PI * f * t).sin() as f32
                })
                .collect();
            tl.push(n0, t0 as u64, &s);
        }
    }

    #[test]
    fn reads_by_time_despite_drift() {
        let fs_true = 48_000.0 * (1.0 + 200e-6);
        let mut tl = TimelineReader::new(48_000, 5.0);
        feed(&mut tl, fs_true, 300, 480, 440.0); // 3 s
        let start = T0 + 2.0e9;
        let mut out = vec![0.0f32; 480];
        let st = tl.read(start as u64, 1e9 / 48_000.0, &mut out);
        assert_eq!(st.missing, 0);
        for (k, v) in out.iter().enumerate() {
            let t = (start - T0) / 1e9 + k as f64 / 48_000.0;
            let want = (2.0 * PI * 440.0 * t).sin() as f32;
            assert!((v - want).abs() < 2e-3, "k={k} got {v} want {want}");
        }
    }
    #[test]
    fn converts_44k1_to_48k() {
        let mut tl = TimelineReader::new(44_100, 5.0);
        feed(&mut tl, 44_100.0, 100, 441, 300.0);
        let start = T0 + 0.5e9;
        let mut out = vec![0.0f32; 480];
        assert_eq!(tl.read(start as u64, 1e9 / 48_000.0, &mut out).missing, 0);
        let t = (start - T0) / 1e9 + 100.0 / 48_000.0;
        assert!((out[100] - (2.0 * PI * 300.0 * t).sin() as f32).abs() < 2e-3);
    }
    #[test]
    fn reports_missing_outside_data_and_zero_fills_gaps() {
        let mut tl = TimelineReader::new(48_000, 5.0);
        let mut out = vec![1.0f32; 480];
        assert_eq!(tl.read(0, 1e9 / 48_000.0, &mut out).missing, 480);
        tl.push(0, T0 as u64, &vec![0.5; 480]);
        tl.push(960, (T0 + 20e6) as u64, &vec![0.5; 480]); // gap 480..960
        let st = tl.read((T0 + 12e6) as u64, 1e9 / 48_000.0, &mut out[..96]);
        assert_eq!(st.missing, 0);
        assert!(
            out[..96].iter().all(|v| v.abs() < 1e-6),
            "gap must be zero-filled"
        );
        let mut late = vec![0.0f32; 480];
        assert!(
            tl.read((T0 + 1e9) as u64, 1e9 / 48_000.0, &mut late)
                .missing
                > 0
        );
    }
    #[test]
    fn restarts_segment_on_time_discontinuity() {
        let mut tl = TimelineReader::new(48_000, 5.0);
        for c in 0..10u64 {
            tl.push(c * 480, (T0 + c as f64 * 10e6) as u64, &vec![0.1; 480]);
        }
        // same index sequence continues but 2 s later (source paused)
        for c in 10..20u64 {
            tl.push(
                c * 480,
                (T0 + 2e9 + c as f64 * 10e6) as u64,
                &vec![0.7; 480],
            );
        }
        let mut out = vec![0.0f32; 48];
        let st = tl.read((T0 + 2e9 + 150e6) as u64, 1e9 / 48_000.0, &mut out);
        assert_eq!(st.missing, 0);
        assert!(out.iter().all(|v| (v - 0.7).abs() < 1e-5));
    }
    #[test]
    fn ignores_duplicate_pushes() {
        let mut tl = TimelineReader::new(48_000, 5.0);
        tl.push(0, T0 as u64, &vec![0.25; 480]);
        tl.push(0, T0 as u64, &vec![0.9; 480]);
        let mut out = vec![0.0f32; 10];
        tl.read((T0 + 1e6) as u64, 1e9 / 48_000.0, &mut out);
        assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-6));
    }
}
