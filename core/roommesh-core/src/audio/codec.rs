//! Opus wrapper for 10 ms mono 48 kHz frames with in-band FEC and packet-loss concealment.
use crate::audio::frames::{FRAME_SAMPLES, SAMPLE_RATE};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
    #[error("frame must be {FRAME_SAMPLES} samples, got {0}")]
    FrameSize(usize),
}

pub struct VoiceEncoder {
    enc: opus::Encoder,
    buf: Vec<u8>,
}

impl VoiceEncoder {
    pub fn new(bitrate_bps: i32) -> Result<Self, CodecError> {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate_bps))?;
        enc.set_inband_fec(true)?;
        enc.set_packet_loss_perc(10)?;
        Ok(Self { enc, buf: vec![0u8; 1500] })
    }
    pub fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>, CodecError> {
        if frame.len() != FRAME_SAMPLES {
            return Err(CodecError::FrameSize(frame.len()));
        }
        let n = self.enc.encode_float(frame, &mut self.buf)?;
        Ok(self.buf[..n].to_vec())
    }
}

pub struct VoiceDecoder {
    dec: opus::Decoder,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self { dec: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)? })
    }
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, CodecError> {
        Ok(self.dec.decode_float(packet, out, false)?)
    }
    /// Conceal one lost frame: FEC from the following packet when available, otherwise PLC.
    /// Always decodes exactly one `FRAME_SAMPLES`-sample frame, even into a larger buffer.
    pub fn conceal(&mut self, next_packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize, CodecError> {
        if out.len() < FRAME_SAMPLES {
            return Err(CodecError::FrameSize(out.len()));
        }
        let out = &mut out[..FRAME_SAMPLES];
        match next_packet {
            Some(p) => Ok(self.dec.decode_float(p, out, true)?),
            None => Ok(self.dec.decode_float(&[], out, false)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }
    #[test]
    fn roundtrip_preserves_level() {
        let mut enc = VoiceEncoder::new(48_000).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0f32; FRAME_SAMPLES];
        let mut last = 0.0;
        for f in 0..50 {
            let frame: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|k| 0.3 * (2.0 * std::f32::consts::PI * 440.0 * (f * 480 + k) as f32 / 48_000.0).sin())
                .collect();
            let pkt = enc.encode(&frame).unwrap();
            assert!(pkt.len() < 400);
            assert_eq!(dec.decode(&pkt, &mut out).unwrap(), FRAME_SAMPLES);
            last = rms(&out);
        }
        let want = 0.3 / 2f32.sqrt();
        assert!((20.0 * (last / want).log10()).abs() < 3.0, "rms {last}");
    }
    #[test]
    fn conceals_and_validates() {
        let mut enc = VoiceEncoder::new(32_000).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0f32; FRAME_SAMPLES];
        let pkt = enc.encode(&vec![0.1; FRAME_SAMPLES]).unwrap();
        dec.decode(&pkt, &mut out).unwrap();
        assert_eq!(dec.conceal(None, &mut out).unwrap(), FRAME_SAMPLES);
        assert_eq!(dec.conceal(Some(&pkt), &mut out).unwrap(), FRAME_SAMPLES);
        assert!(enc.encode(&[0.0; 100]).is_err());
    }
    #[test]
    fn conceal_writes_one_frame_into_a_larger_buffer() {
        let mut enc = VoiceEncoder::new(32_000).unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0f32; FRAME_SAMPLES];
        let pkt = enc.encode(&vec![0.1; FRAME_SAMPLES]).unwrap();
        dec.decode(&pkt, &mut out).unwrap();
        let mut big = vec![0.0f32; 5760];
        assert_eq!(dec.conceal(None, &mut big).unwrap(), FRAME_SAMPLES);
    }
    #[test]
    fn conceal_rejects_undersized_buffer() {
        let mut dec = VoiceDecoder::new().unwrap();
        let mut small = vec![0.0f32; 10];
        assert!(matches!(dec.conceal(None, &mut small), Err(CodecError::FrameSize(10))));
    }
}
