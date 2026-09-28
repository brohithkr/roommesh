//! Binary realtime packet format (little endian). Layout, 44-byte header:
//! magic u16 | version u8 | kind u8 | epoch u32 | stream u32 | sender u64 | sequence u32 |
//! sample_index u64 | timestamp_ns u64 | frame_count u16 | payload_len u16 | payload…
use crate::ids::{Epoch, PeerId, StreamId};

pub const RT_MAGIC: u16 = 0x524D;
pub const RT_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 44;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketKind { Mic = 1, Playback = 2, ClockPing = 3, ClockPong = 4 }

impl PacketKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v { 1 => Self::Mic, 2 => Self::Playback, 3 => Self::ClockPing, 4 => Self::ClockPong, _ => return None })
    }
}

/// `timestamp_ns` is ALWAYS in the coordinator's clock domain for Mic/Playback packets
/// (Mic: capture time of first sample; Playback: scheduled play time of first sample).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtHeader {
    pub kind: PacketKind,
    pub epoch: Epoch,
    pub stream: StreamId,
    pub sender: PeerId,
    pub sequence: u32,
    pub sample_index: u64,
    pub timestamp_ns: u64,
    pub frame_count: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RtError {
    #[error("packet too short")] TooShort,
    #[error("bad magic")] BadMagic,
    #[error("unsupported version")] BadVersion,
    #[error("unknown kind")] BadKind,
    #[error("payload length mismatch")] LengthMismatch,
}

pub fn header_bytes(h: &RtHeader, payload_len: usize) -> [u8; HEADER_LEN] {
    debug_assert!(payload_len <= u16::MAX as usize, "payload_len {payload_len} does not fit in the u16 header field");
    let mut b = [0u8; HEADER_LEN];
    b[0..2].copy_from_slice(&RT_MAGIC.to_le_bytes());
    b[2] = RT_VERSION;
    b[3] = h.kind as u8;
    b[4..8].copy_from_slice(&h.epoch.0.to_le_bytes());
    b[8..12].copy_from_slice(&h.stream.0.to_le_bytes());
    b[12..20].copy_from_slice(&h.sender.0.to_le_bytes());
    b[20..24].copy_from_slice(&h.sequence.to_le_bytes());
    b[24..32].copy_from_slice(&h.sample_index.to_le_bytes());
    b[32..40].copy_from_slice(&h.timestamp_ns.to_le_bytes());
    b[40..42].copy_from_slice(&h.frame_count.to_le_bytes());
    b[42..44].copy_from_slice(&(payload_len as u16).to_le_bytes());
    b
}

pub fn encode_packet(h: &RtHeader, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    v.extend_from_slice(&header_bytes(h, payload.len()));
    v.extend_from_slice(payload);
    v
}

fn u16_at(b: &[u8], i: usize) -> u16 { u16::from_le_bytes([b[i], b[i + 1]]) }
fn u32_at(b: &[u8], i: usize) -> u32 { u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) }
fn u64_at(b: &[u8], i: usize) -> u64 { u64::from_le_bytes(b[i..i + 8].try_into().unwrap()) }

pub fn decode_packet(buf: &[u8]) -> Result<(RtHeader, &[u8]), RtError> {
    if buf.len() < HEADER_LEN { return Err(RtError::TooShort); }
    if u16_at(buf, 0) != RT_MAGIC { return Err(RtError::BadMagic); }
    if buf[2] != RT_VERSION { return Err(RtError::BadVersion); }
    let kind = PacketKind::from_u8(buf[3]).ok_or(RtError::BadKind)?;
    let len = u16_at(buf, 42) as usize;
    if buf.len() != HEADER_LEN + len { return Err(RtError::LengthMismatch); }
    let h = RtHeader {
        kind,
        epoch: Epoch(u32_at(buf, 4)),
        stream: StreamId(u32_at(buf, 8)),
        sender: PeerId(u64_at(buf, 12)),
        sequence: u32_at(buf, 20),
        sample_index: u64_at(buf, 24),
        timestamp_ns: u64_at(buf, 32),
        frame_count: u16_at(buf, 40),
    };
    Ok((h, &buf[HEADER_LEN..]))
}

/// Clock ping payload: t1. Pong payload: t1, t2, t3, ping_seq (all u64 LE; the times are host ns
/// of their clocks, ping_seq is the sequence number of the ping being answered).
pub fn encode_times(times: &[u64]) -> Vec<u8> { times.iter().flat_map(|t| t.to_le_bytes()).collect() }
pub fn decode_times(p: &[u8]) -> Vec<u64> {
    p.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hdr() -> RtHeader {
        RtHeader { kind: PacketKind::Mic, epoch: Epoch(42), stream: StreamId::MIC, sender: PeerId(0xabc),
                   sequence: 7, sample_index: 3360, timestamp_ns: 123_456_789, frame_count: 480 }
    }
    #[test]
    fn roundtrip() {
        let pkt = encode_packet(&hdr(), &[1, 2, 3]);
        assert_eq!(pkt.len(), HEADER_LEN + 3);
        let (h, p) = decode_packet(&pkt).unwrap();
        assert_eq!(h, hdr());
        assert_eq!(p, &[1, 2, 3]);
    }
    #[test]
    fn rejects_garbage() {
        assert_eq!(decode_packet(&[0u8; 4]).unwrap_err(), RtError::TooShort);
        let mut pkt = encode_packet(&hdr(), &[]);
        pkt[0] = 0;
        assert_eq!(decode_packet(&pkt).unwrap_err(), RtError::BadMagic);
        let mut pkt = encode_packet(&hdr(), &[9; 4]);
        pkt.truncate(HEADER_LEN + 2);
        assert_eq!(decode_packet(&pkt).unwrap_err(), RtError::LengthMismatch);
    }
    #[test]
    fn header_bytes_is_prefix() {
        let pkt = encode_packet(&hdr(), &[5; 10]);
        assert_eq!(&pkt[..HEADER_LEN], &header_bytes(&hdr(), 10)[..]);
    }
    #[test]
    fn rejects_bad_version_and_kind() {
        let mut pkt = encode_packet(&hdr(), &[]);
        pkt[2] = RT_VERSION + 1;
        assert_eq!(decode_packet(&pkt).unwrap_err(), RtError::BadVersion);
        let mut pkt = encode_packet(&hdr(), &[]);
        pkt[3] = 0xff;
        assert_eq!(decode_packet(&pkt).unwrap_err(), RtError::BadKind);
    }
}
