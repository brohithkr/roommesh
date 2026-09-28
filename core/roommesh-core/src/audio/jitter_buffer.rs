//! Per-stream reorder buffer with deadline-driven playout, loss detection and RFC 3550 jitter.
//! Sequence numbers are extended from u32 to u64 to survive wraparound.
use crate::network::realtime::RtHeader;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct BufferedPacket { pub seq: u64, pub header: RtHeader, pub payload: Vec<u8>, pub arrival_ns: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushResult { Accepted, Duplicate, Late, Overflow }

#[derive(Debug)]
pub enum Pop {
    Packet(BufferedPacket),
    /// `seq` did not arrive before its deadline. Conceal it (Opus PLC, or FEC from `peek(seq+1)`).
    Missing { seq: u64, timestamp_ns: u64, sample_index: u64 },
    NotReady,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JitterStats {
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub duplicates: u64,
    /// RFC 3550 interarrival jitter estimate.
    pub jitter_ns: f64,
    pub depth_packets: usize,
    /// EMA of time packets spent buffered before use.
    pub mean_wait_ns: f64,
}
impl JitterStats {
    pub fn loss_ratio(&self) -> f64 {
        let total = self.received + self.lost;
        if total == 0 { 0.0 } else { self.lost as f64 / total as f64 }
    }
}

pub const MAX_CONSECUTIVE_MISSING: u32 = 5;

pub struct JitterBuffer {
    packets: BTreeMap<u64, BufferedPacket>,
    next_seq: Option<u64>,
    highest: Option<u64>,
    last_popped: Option<(u64, u64, u64)>, // (seq, timestamp, sample_index)
    consecutive_missing: u32,
    capacity: usize,
    frame_samples: u64,
    frame_ns: u64,
    last_transit: Option<f64>,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(capacity: usize, frame_samples: u64, frame_ns: u64) -> Self {
        Self { packets: BTreeMap::new(), next_seq: None, highest: None, last_popped: None,
               consecutive_missing: 0, capacity, frame_samples, frame_ns, last_transit: None,
               stats: JitterStats::default() }
    }

    pub fn reset(&mut self) {
        self.packets.clear();
        self.next_seq = None;
        self.highest = None;
        self.last_popped = None;
        self.consecutive_missing = 0;
        self.last_transit = None;
    }

    fn extend(&self, seq: u32) -> u64 {
        const WRAP: i128 = 1 << 32;
        let Some(h) = self.highest else { return seq as u64 + (1u64 << 32) };
        let h = h as i128;
        let mut best = (h & !(WRAP - 1)) | seq as i128;
        for c in [best - WRAP, best + WRAP] {
            if c >= 0 && (c - h).abs() < (best - h).abs() { best = c; }
        }
        best as u64
    }

    pub fn push(&mut self, header: RtHeader, payload: Vec<u8>, arrival_ns: u64) -> PushResult {
        let seq = self.extend(header.sequence);
        if let Some(next) = self.next_seq {
            if seq < next { self.stats.late += 1; return PushResult::Late; }
        }
        if self.packets.contains_key(&seq) { self.stats.duplicates += 1; return PushResult::Duplicate; }
        let transit = arrival_ns as f64 - header.timestamp_ns as f64;
        if let Some(prev) = self.last_transit {
            let d = (transit - prev).abs();
            self.stats.jitter_ns += (d - self.stats.jitter_ns) / 16.0;
        }
        self.last_transit = Some(transit);
        self.highest = Some(self.highest.map_or(seq, |h| h.max(seq)));
        if self.next_seq.is_none() { self.next_seq = Some(seq); }
        self.packets.insert(seq, BufferedPacket { seq, header, payload, arrival_ns });
        self.stats.received += 1;
        let mut result = PushResult::Accepted;
        while self.packets.len() > self.capacity {
            let first = *self.packets.keys().next().unwrap();
            self.packets.remove(&first);
            self.stats.lost += 1;
            self.next_seq = self.packets.keys().next().copied();
            result = PushResult::Overflow;
        }
        self.stats.depth_packets = self.packets.len();
        result
    }

    /// Returns the next in-order packet whose timestamp is `<= deadline_ns`, or declares it
    /// missing once a successor proves it should have arrived.
    pub fn pop_due(&mut self, deadline_ns: u64, now_ns: u64) -> Pop {
        let Some(next) = self.next_seq else { return Pop::NotReady };
        if let Some(p) = self.packets.get(&next) {
            if p.header.timestamp_ns > deadline_ns { return Pop::NotReady; }
            let p = self.packets.remove(&next).unwrap();
            self.next_seq = Some(next + 1);
            self.consecutive_missing = 0;
            self.last_popped = Some((next, p.header.timestamp_ns, p.header.sample_index));
            let wait = now_ns.saturating_sub(p.arrival_ns) as f64;
            self.stats.mean_wait_ns += (wait - self.stats.mean_wait_ns) / 32.0;
            self.stats.depth_packets = self.packets.len();
            return Pop::Packet(p);
        }
        // Without a buffered successor we have no direct evidence `next` was actually sent and
        // skipped; only synthesize a loss from `last_popped` extrapolation when the caller
        // explicitly asks for an unbounded/forced drain (deadline_ns == u64::MAX, e.g. a stall
        // watchdog trying to push the resync state machine along). A normal, finitely-deadlined
        // playout call just waits (NotReady) — the sender may simply have gone silent.
        let inferred = if let Some((&s, p)) = self.packets.iter().next() {
            let d = s - next;
            Some((p.header.timestamp_ns.saturating_sub(d * self.frame_ns),
                  p.header.sample_index.saturating_sub(d * self.frame_samples)))
        } else if deadline_ns == u64::MAX {
            if let Some((ls, lts, lsi)) = self.last_popped {
                if self.consecutive_missing >= MAX_CONSECUTIVE_MISSING {
                    self.reset(); // sender stopped; resync on the next packet
                    return Pop::NotReady;
                }
                let d = next - ls;
                Some((lts + d * self.frame_ns, lsi + d * self.frame_samples))
            } else {
                None
            }
        } else {
            None
        };
        let Some((ts, si)) = inferred else { return Pop::NotReady };
        if ts > deadline_ns { return Pop::NotReady; }
        self.next_seq = Some(next + 1);
        self.last_popped = Some((next, ts, si));
        self.consecutive_missing += 1;
        self.stats.lost += 1;
        Pop::Missing { seq: next, timestamp_ns: ts, sample_index: si }
    }

    pub fn peek(&self, seq: u64) -> Option<&BufferedPacket> { self.packets.get(&seq) }
    pub fn stats(&self) -> JitterStats { self.stats }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frames::FRAME_NS;
    use crate::ids::*;
    use crate::network::realtime::{PacketKind, RtHeader};

    fn h(seq: u32, i: u64) -> RtHeader {
        RtHeader { kind: PacketKind::Mic, epoch: Epoch(1), stream: StreamId::MIC, sender: PeerId(1),
                   sequence: seq, sample_index: i * 480, timestamp_ns: i * FRAME_NS, frame_count: 480 }
    }
    fn jb() -> JitterBuffer { JitterBuffer::new(64, 480, FRAME_NS) }
    fn seq_of(p: Pop) -> u32 { match p { Pop::Packet(p) => p.header.sequence, other => panic!("{other:?}") } }

    #[test]
    fn in_order_and_deadline() {
        let mut b = jb();
        for i in 0..3 { assert_eq!(b.push(h(i as u32, i), vec![], 0), PushResult::Accepted); }
        assert!(matches!(b.pop_due(0, 0), Pop::Packet(_)));
        assert!(matches!(b.pop_due(FRAME_NS - 1, 0), Pop::NotReady));
        assert_eq!(seq_of(b.pop_due(FRAME_NS, 0)), 1);
        assert_eq!(seq_of(b.pop_due(10 * FRAME_NS, 0)), 2);
        assert!(matches!(b.pop_due(10 * FRAME_NS, 0), Pop::NotReady));
    }
    #[test]
    fn reorders() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        b.push(h(2, 2), vec![], 0);
        b.push(h(1, 1), vec![], 0);
        let order: Vec<u32> = (0..3).map(|_| seq_of(b.pop_due(u64::MAX, 0))).collect();
        assert_eq!(order, vec![0, 1, 2]);
    }
    #[test]
    fn detects_loss_with_inferred_timing() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        b.push(h(2, 2), vec![], 0);
        seq_of(b.pop_due(u64::MAX, 0));
        match b.pop_due(2 * FRAME_NS, 0) {
            Pop::Missing { timestamp_ns, sample_index, .. } => { assert_eq!(timestamp_ns, FRAME_NS); assert_eq!(sample_index, 480); }
            other => panic!("{other:?}"),
        }
        assert_eq!(seq_of(b.pop_due(u64::MAX, 0)), 2);
        assert_eq!(b.stats().lost, 1);
    }
    #[test]
    fn duplicate_and_late() {
        let mut b = jb();
        assert_eq!(b.push(h(0, 0), vec![], 0), PushResult::Accepted);
        assert_eq!(b.push(h(0, 0), vec![], 0), PushResult::Duplicate);
        seq_of(b.pop_due(u64::MAX, 0));
        assert_eq!(b.push(h(0, 0), vec![], 0), PushResult::Late);
    }
    #[test]
    fn sequence_wraparound() {
        let mut b = jb();
        let seqs = [u32::MAX - 1, u32::MAX, 0, 1];
        for (i, s) in seqs.iter().enumerate() { b.push(h(*s, i as u64), vec![], 0); }
        let got: Vec<u32> = (0..4).map(|_| seq_of(b.pop_due(u64::MAX, 0))).collect();
        assert_eq!(got, seqs.to_vec());
    }
    #[test]
    fn resyncs_after_stream_stops() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        seq_of(b.pop_due(u64::MAX, 0));
        for _ in 0..MAX_CONSECUTIVE_MISSING { assert!(matches!(b.pop_due(u64::MAX, 0), Pop::Missing { .. })); }
        assert!(matches!(b.pop_due(u64::MAX, 0), Pop::NotReady));
        b.push(h(100, 100), vec![], 0);
        assert_eq!(seq_of(b.pop_due(u64::MAX, 0)), 100);
    }
    #[test]
    fn jitter_statistic_grows_with_variable_delay() {
        let mut b = jb();
        for i in 0..50u64 {
            let delay = if i % 2 == 0 { 1_000_000 } else { 9_000_000 };
            b.push(h(i as u32, i), vec![], i * FRAME_NS + delay);
        }
        assert!(b.stats().jitter_ns > 3_000_000.0);
    }
}
