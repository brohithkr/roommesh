//! Per-stream reorder buffer with deadline-driven playout, loss detection and RFC 3550 jitter.
//! Sequence numbers are extended from u32 to u64 to survive wraparound.
use crate::network::realtime::RtHeader;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct BufferedPacket {
    pub seq: u64,
    pub header: RtHeader,
    pub payload: Vec<u8>,
    pub arrival_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushResult {
    Accepted,
    Duplicate,
    Late,
    Overflow,
}

#[derive(Debug)]
pub enum Pop {
    Packet(BufferedPacket),
    /// `seq` did not arrive before its deadline. Conceal it (Opus PLC, or FEC from `peek(seq+1)`).
    Missing {
        seq: u64,
        timestamp_ns: u64,
        sample_index: u64,
    },
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
    /// Windowed loss ratio 0..1: EWMA (alpha = 1/100, ~1 s of frames) over frames reaching
    /// playout (0) or lost (1: concealed, skipped, evicted or discarded). Use this for
    /// current connection quality; `loss_ratio()` is cumulative since the stream started.
    pub recent_loss: f64,
}
impl JitterStats {
    /// Cumulative loss ratio since the stream started (see `recent_loss` for a windowed one).
    pub fn loss_ratio(&self) -> f64 {
        let total = self.received + self.lost;
        if total == 0 {
            0.0
        } else {
            self.lost as f64 / total as f64
        }
    }
}

/// The longest gap (in frames) that is concealed by synthesizing `Pop::Missing` entries. A due
/// gap longer than this is skipped (counted as `lost`) instead of concealed, since concealing a
/// long outage produces worse audio than a clean resync.
pub const MAX_CONSECUTIVE_MISSING: u32 = 5;

const RECENT_LOSS_ALPHA: f64 = 1.0 / 100.0;

pub struct JitterBuffer {
    packets: BTreeMap<u64, BufferedPacket>,
    next_seq: Option<u64>,
    highest: Option<u64>,
    /// Timestamp of the last frame handed out (packet or concealment).
    last_ts: Option<u64>,
    /// Largest timestamp of any accepted packet (sender-restart detection).
    max_ts: Option<u64>,
    capacity: usize,
    frame_samples: u64,
    frame_ns: u64,
    last_transit: Option<f64>,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(capacity: usize, frame_samples: u64, frame_ns: u64) -> Self {
        Self {
            packets: BTreeMap::new(),
            next_seq: None,
            highest: None,
            last_ts: None,
            max_ts: None,
            capacity,
            frame_samples,
            frame_ns,
            last_transit: None,
            stats: JitterStats::default(),
        }
    }

    pub fn reset(&mut self) {
        self.packets.clear();
        self.next_seq = None;
        self.highest = None;
        self.last_ts = None;
        self.max_ts = None;
        self.stats.depth_packets = 0;
        self.last_transit = None;
    }

    fn extend(&self, seq: u32) -> u64 {
        const WRAP: i128 = 1 << 32;
        let Some(h) = self.highest else {
            return seq as u64 + (1u64 << 32);
        };
        let h = h as i128;
        let mut best = (h & !(WRAP - 1)) | seq as i128;
        for c in [best - WRAP, best + WRAP] {
            if c >= 0 && (c - h).abs() < (best - h).abs() {
                best = c;
            }
        }
        best as u64
    }

    pub fn push(&mut self, header: RtHeader, payload: Vec<u8>, arrival_ns: u64) -> PushResult {
        let seq = self.extend(header.sequence);
        if let Some(next) = self.next_seq {
            if seq < next {
                // Behind the playout point yet clearly newer than anything ever received: the
                // sender restarted its sequence counter (app restart, new uplink). Resync instead
                // of rejecting. (Compared with the newest *received* timestamp, not the last
                // played one: after a skip, a straggler from the skipped range is merely late.)
                let restart_slack = MAX_CONSECUTIVE_MISSING as u64 * self.frame_ns;
                if self
                    .max_ts
                    .is_some_and(|t| header.timestamp_ns > t.saturating_add(restart_slack))
                {
                    let discarded = self.packets.len() as u64;
                    self.stats.received -= discarded;
                    self.stats.lost += discarded;
                    self.note_frames(discarded, true);
                    self.reset();
                    return self.push(header, payload, arrival_ns);
                }
                self.stats.late += 1;
                return PushResult::Late;
            }
        }
        if self.packets.contains_key(&seq) {
            self.stats.duplicates += 1;
            return PushResult::Duplicate;
        }
        let transit = arrival_ns as f64 - header.timestamp_ns as f64;
        if let Some(prev) = self.last_transit {
            let d = (transit - prev).abs();
            self.stats.jitter_ns += (d - self.stats.jitter_ns) / 16.0;
        }
        self.last_transit = Some(transit);
        self.highest = Some(self.highest.map_or(seq, |h| h.max(seq)));
        self.max_ts = Some(
            self.max_ts
                .map_or(header.timestamp_ns, |t| t.max(header.timestamp_ns)),
        );
        if self.next_seq.is_none() {
            self.next_seq = Some(seq);
        }
        self.packets.insert(
            seq,
            BufferedPacket {
                seq,
                header,
                payload,
                arrival_ns,
            },
        );
        self.stats.received += 1;
        let mut result = PushResult::Accepted;
        while self.packets.len() > self.capacity {
            let first = *self.packets.keys().next().unwrap();
            self.packets.remove(&first);
            // The evicted packet did arrive but never reaches playout: count it only under
            // `lost` (not also under `received`), so `loss_ratio` reflects packets that failed
            // to reach playout rather than double-counting the same packet in both buckets.
            self.stats.received -= 1;
            // Playout jumps from `next_seq` to the new oldest packet: the evicted packet and
            // any never-received sequence numbers in between are all lost.
            let old_next = self.next_seq.unwrap_or(first).min(first);
            let new_next = self.packets.keys().next().copied();
            let jumped = new_next.map_or(1, |n| n - old_next);
            self.stats.lost += jumped;
            self.note_frames(jumped, true);
            self.next_seq = new_next;
            result = PushResult::Overflow;
        }
        self.stats.depth_packets = self.packets.len();
        result
    }

    /// Returns the next in-order packet whose timestamp is `<= deadline_ns`, or declares it
    /// missing once a buffered successor proves it was sent. A due gap longer than
    /// `MAX_CONSECUTIVE_MISSING` frames is skipped (counted as lost) instead of concealed.
    pub fn pop_due(&mut self, deadline_ns: u64, now_ns: u64) -> Pop {
        let Some(next) = self.next_seq else {
            return Pop::NotReady;
        };
        // Only a buffered successor proves `next` was sent; with none, wait.
        let Some((&first, succ)) = self.packets.iter().next() else {
            return Pop::NotReady;
        };
        let gap = first - next;
        let due_ts = succ.header.timestamp_ns.saturating_sub(gap * self.frame_ns);
        // Not due yet: a reordered packet may still fill the gap.
        if due_ts > deadline_ns {
            return Pop::NotReady;
        }
        if gap > MAX_CONSECUTIVE_MISSING as u64 {
            // Outage too long to conceal usefully: resync on the successor.
            self.stats.lost += gap;
            self.note_frames(gap, true);
            self.next_seq = Some(first);
            return self.pop_due(deadline_ns, now_ns);
        }
        if gap == 0 {
            let p = self.packets.remove(&first).unwrap();
            self.next_seq = Some(first + 1);
            self.last_ts = Some(p.header.timestamp_ns);
            let wait = now_ns.saturating_sub(p.arrival_ns) as f64;
            self.stats.mean_wait_ns += (wait - self.stats.mean_wait_ns) / 32.0;
            self.stats.depth_packets = self.packets.len();
            self.note_frames(1, false);
            return Pop::Packet(p);
        }
        let sample_index = succ
            .header
            .sample_index
            .saturating_sub(gap * self.frame_samples);
        self.next_seq = Some(next + 1);
        self.last_ts = Some(due_ts);
        self.stats.lost += 1;
        self.note_frames(1, true);
        Pop::Missing {
            seq: next,
            timestamp_ns: due_ts,
            sample_index,
        }
    }

    /// Feeds `n` frames that were all lost (or all delivered) into the `recent_loss` EWMA.
    fn note_frames(&mut self, n: u64, lost: bool) {
        if n == 0 {
            return;
        }
        let keep = (1.0 - RECENT_LOSS_ALPHA).powf(n as f64);
        let target = if lost { 1.0 } else { 0.0 };
        self.stats.recent_loss = self.stats.recent_loss * keep + target * (1.0 - keep);
    }

    pub fn peek(&self, seq: u64) -> Option<&BufferedPacket> {
        self.packets.get(&seq)
    }
    pub fn stats(&self) -> JitterStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frames::FRAME_NS;
    use crate::ids::*;
    use crate::network::realtime::{PacketKind, RtHeader};

    fn h(seq: u32, i: u64) -> RtHeader {
        RtHeader {
            kind: PacketKind::Mic,
            epoch: Epoch(1),
            stream: StreamId::MIC,
            sender: PeerId(1),
            sequence: seq,
            sample_index: i * 480,
            timestamp_ns: i * FRAME_NS,
            frame_count: 480,
        }
    }
    fn jb() -> JitterBuffer {
        JitterBuffer::new(64, 480, FRAME_NS)
    }
    fn seq_of(p: Pop) -> u32 {
        match p {
            Pop::Packet(p) => p.header.sequence,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn in_order_and_deadline() {
        let mut b = jb();
        for i in 0..3 {
            assert_eq!(b.push(h(i as u32, i), vec![], 0), PushResult::Accepted);
        }
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
            Pop::Missing {
                timestamp_ns,
                sample_index,
                ..
            } => {
                assert_eq!(timestamp_ns, FRAME_NS);
                assert_eq!(sample_index, 480);
            }
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
        for (i, s) in seqs.iter().enumerate() {
            b.push(h(*s, i as u64), vec![], 0);
        }
        let got: Vec<u32> = (0..4).map(|_| seq_of(b.pop_due(u64::MAX, 0))).collect();
        assert_eq!(got, seqs.to_vec());
    }
    #[test]
    fn resyncs_after_stream_stops() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        seq_of(b.pop_due(u64::MAX, 0));
        assert!(matches!(b.pop_due(u64::MAX, 0), Pop::NotReady));
        b.push(h(100, 100), vec![], 0);
        assert_eq!(seq_of(b.pop_due(u64::MAX, 0)), 100);
    }
    #[test]
    fn short_gap_concealed_long_gap_skipped() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        seq_of(b.pop_due(0, 0));
        b.push(
            h(
                1 + MAX_CONSECUTIVE_MISSING,
                1 + MAX_CONSECUTIVE_MISSING as u64,
            ),
            vec![],
            0,
        );
        for _ in 0..MAX_CONSECUTIVE_MISSING {
            assert!(matches!(b.pop_due(u64::MAX, 0), Pop::Missing { .. }));
        }
        seq_of(b.pop_due(u64::MAX, 0));
        b.push(h(1000, 1000), vec![], 0);
        assert!(
            matches!(b.pop_due(999 * FRAME_NS, 0), Pop::NotReady),
            "skip must not pop early"
        );
        assert_eq!(seq_of(b.pop_due(1000 * FRAME_NS, 0)), 1000);
        assert_eq!(b.stats().lost, MAX_CONSECUTIVE_MISSING as u64 + 1000 - 7);
    }
    #[test]
    fn long_reorder_is_not_skipped_before_due() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        seq_of(b.pop_due(0, 0));
        b.push(h(20, 20), vec![], 0); // 1..19 still in flight
        assert!(matches!(b.pop_due(FRAME_NS / 2, 0), Pop::NotReady));
        b.push(h(1, 1), vec![], 0);
        assert_eq!(seq_of(b.pop_due(FRAME_NS, 0)), 1);
        assert_eq!(b.stats().lost, 0);
    }
    #[test]
    fn resyncs_when_sender_restarts_sequence() {
        let mut b = jb();
        for i in 0..50u64 {
            b.push(h(i as u32, i), vec![], 0);
            seq_of(b.pop_due(u64::MAX, 0));
        }
        assert_eq!(b.push(h(10, 10), vec![], 0), PushResult::Late);
        assert_eq!(b.push(h(0, 200), vec![], 0), PushResult::Accepted);
        assert_eq!(seq_of(b.pop_due(u64::MAX, 0)), 0);
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
    #[test]
    fn overflow_drop_not_double_counted() {
        let mut b = JitterBuffer::new(2, 480, FRAME_NS);
        // Push 3 packets into a capacity-2 buffer without popping: the oldest is evicted.
        for i in [5u64, 6, 7] {
            b.push(h(i as u32, i), vec![], 0);
        }
        let s = b.stats();
        // One packet was evicted: it should count once, under `lost`, not also under `received`.
        assert_eq!(s.received, 2);
        assert_eq!(s.lost, 1);
    }

    #[test]
    fn straggler_after_skip_is_late_not_a_restart() {
        let mut b = jb();
        b.push(h(0, 0), vec![], 0);
        seq_of(b.pop_due(0, 0));
        b.push(h(20, 20), vec![], 0);
        b.push(h(21, 21), vec![], 0);
        assert!(
            matches!(b.pop_due(15 * FRAME_NS, 0), Pop::NotReady),
            "gap skipped, 20 not due"
        );
        assert_eq!(b.stats().lost, 19);
        assert_eq!(b.push(h(12, 12), vec![], 0), PushResult::Late);
        assert_eq!(b.stats().depth_packets, 2);
        assert_eq!(seq_of(b.pop_due(20 * FRAME_NS, 0)), 20);
        assert_eq!(seq_of(b.pop_due(21 * FRAME_NS, 0)), 21);
    }
    #[test]
    fn restart_discards_buffered_packets_as_lost() {
        let mut b = jb();
        for i in 0..10u64 {
            b.push(h(i as u32, i), vec![], 0);
        }
        for _ in 0..5 {
            seq_of(b.pop_due(u64::MAX, 0));
        } // 5..9 still buffered
        assert_eq!(b.push(h(0, 500), vec![], 0), PushResult::Accepted);
        let s = b.stats();
        assert_eq!((s.received, s.lost, s.depth_packets), (6, 5, 1));
        assert_eq!(seq_of(b.pop_due(u64::MAX, 0)), 0);
    }
    #[test]
    fn overflow_counts_jumped_sequence_gap_as_lost() {
        let mut b = JitterBuffer::new(2, 480, FRAME_NS);
        b.push(h(0, 0), vec![], 0);
        b.push(h(5, 5), vec![], 0); // 1..4 not (yet) received
        b.push(h(6, 6), vec![], 0); // evicts 0; playout jumps 0 -> 5
        let s = b.stats();
        assert_eq!((s.received, s.lost), (2, 5));
        assert_eq!(seq_of(b.pop_due(u64::MAX, 0)), 5);
    }
    #[test]
    fn recent_loss_is_windowed_and_cumulative_is_kept() {
        let mut b = jb();
        let mut seen_high = 0.0f64;
        // 500 frames with every 5th lost (20 %), then 500 clean frames; played out in real time.
        for i in 0..1000u64 {
            if i >= 500 || i % 5 != 1 {
                b.push(h(i as u32, i), vec![], 0);
            }
            while !matches!(b.pop_due(i * FRAME_NS, 0), Pop::NotReady) {}
            if i == 499 {
                seen_high = b.stats().recent_loss;
            }
        }
        let s = b.stats();
        assert!(
            (0.12..0.28).contains(&seen_high),
            "recent loss while lossy {seen_high}"
        );
        assert!(
            s.recent_loss < 0.01,
            "recent loss after recovery {}",
            s.recent_loss
        );
        assert!(
            (s.loss_ratio() - 0.1).abs() < 0.01,
            "cumulative {}",
            s.loss_ratio()
        );
    }
}
