//! Canonical audio format constants and the frame type passed between pipeline stages.
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 480; // 10 ms
pub const FRAME_NS: u64 = 10_000_000;
pub const NS_PER_SAMPLE: f64 = 1e9 / SAMPLE_RATE as f64;

/// One 10 ms mono frame. `timestamp_ns` is the time of the first sample in the
/// clock domain documented by whoever produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioFrame {
    pub sample_index: u64,
    pub timestamp_ns: u64,
    pub samples: Vec<f32>,
}

impl AudioFrame {
    pub fn silent(sample_index: u64, timestamp_ns: u64) -> Self {
        Self {
            sample_index,
            timestamp_ns,
            samples: vec![0.0; FRAME_SAMPLES],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_constants_consistent() {
        assert_eq!(
            FRAME_SAMPLES as u64 * 1_000_000_000 / SAMPLE_RATE as u64,
            FRAME_NS
        );
        assert_eq!(AudioFrame::silent(0, 0).samples.len(), FRAME_SAMPLES);
    }
}
