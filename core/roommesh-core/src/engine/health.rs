//! Audio health counters: where audio is lost, counted by the DSP thread over a rolling 60 s
//! window and in total since audio started (or the counters were reset).
//!
//! - DSP wake lateness: how much later than asked the loop woke (worst, and p99 over wakes);
//! - speaker underruns: output callbacks that found the queue empty and played zeros;
//! - speaker gaps: meeting audio missing at its play time, inside otherwise-flowing audio;
//! - late meeting audio: far-end frames dropped because their play time had passed;
//! - far-end restarts: the far-end assembler reset on a discontinuity;
//! - room-mic silence: mic-grid slots written as silence after a stall;
//! - missing mic audio, per mic: samples a mic could not supply inside flowing audio.
//!
//! Everything here is plain bookkeeping on preallocated buckets, cheap enough for every wake.
use crate::engine::realtime::RtStatus;
use crate::ids::PeerId;

pub const WINDOW_SECS: u64 = 60;
const NS_PER_S: u64 = 1_000_000_000;

/// Counts samples missing inside flowing audio. A stream that hasn't started, or has been
/// silent for longer than `max_gap` samples (stopped), is not "missing"; a gap is counted when
/// audio resumes after it. Partially filled reads count their missing samples while flowing
/// (so a stream's final, partial read may add up to one read's worth).
#[derive(Clone, Debug)]
pub struct GapCounter {
    max_gap: u64,
    pending: u64,
    flowing: bool,
}

impl GapCounter {
    pub fn new(max_gap_samples: u64) -> Self {
        Self {
            max_gap: max_gap_samples,
            pending: 0,
            flowing: false,
        }
    }
    /// One read of `filled + missing` samples; returns the samples newly counted as missing.
    pub fn observe(&mut self, filled: usize, missing: usize) -> u64 {
        let (filled, missing) = (filled as u64, missing as u64);
        if filled == 0 {
            if self.flowing {
                self.pending += missing;
                if self.pending > self.max_gap {
                    self.flowing = false; // stopped, not a gap
                    self.pending = 0;
                }
            }
            return 0;
        }
        let counted = if self.flowing {
            std::mem::take(&mut self.pending) + missing
        } else {
            0 // starting: a partial first read is the stream's start
        };
        self.flowing = true;
        counted
    }
    pub fn reset(&mut self) {
        self.pending = 0;
        self.flowing = false;
    }
}

/// 0.1 ms bins below 10 ms, 1 ms bins to 100 ms, one bin beyond.
const FINE_BINS: usize = 100;
const COARSE_BINS: usize = 90;
const HIST_BINS: usize = FINE_BINS + COARSE_BINS + 1;
const FINE_NS: u64 = 100_000;
const COARSE_NS: u64 = 1_000_000;

fn bin_of(ns: u64) -> usize {
    let fine_end = FINE_BINS as u64 * FINE_NS;
    if ns < fine_end {
        (ns / FINE_NS) as usize
    } else {
        (FINE_BINS + ((ns - fine_end) / COARSE_NS) as usize).min(HIST_BINS - 1)
    }
}

/// Upper edge of bin `i` (u64::MAX for the overflow bin).
fn bin_upper_ns(i: usize) -> u64 {
    if i < FINE_BINS {
        (i as u64 + 1) * FINE_NS
    } else if i < HIST_BINS - 1 {
        FINE_BINS as u64 * FINE_NS + (i - FINE_BINS + 1) as u64 * COARSE_NS
    } else {
        u64::MAX
    }
}

#[derive(Clone, Debug)]
struct Hist {
    bins: [u32; HIST_BINS],
    n: u64,
    worst: u64,
}

impl Default for Hist {
    fn default() -> Self {
        Self {
            bins: [0; HIST_BINS],
            n: 0,
            worst: 0,
        }
    }
}

impl Hist {
    fn add(&mut self, ns: u64) {
        let b = &mut self.bins[bin_of(ns)];
        *b = b.saturating_add(1);
        self.n += 1;
        self.worst = self.worst.max(ns);
    }
    fn merge(&mut self, o: &Hist) {
        for (a, b) in self.bins.iter_mut().zip(o.bins.iter()) {
            *a = a.saturating_add(*b);
        }
        self.n += o.n;
        self.worst = self.worst.max(o.worst);
    }
    /// The `q` quantile (0..=1), as its bin's upper edge but never above the worst value.
    fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let want = ((self.n as f64 * q).ceil() as u64).clamp(1, self.n);
        let mut seen = 0u64;
        for (i, c) in self.bins.iter().enumerate() {
            seen += *c as u64;
            if seen >= want {
                return bin_upper_ns(i).min(self.worst);
            }
        }
        self.worst
    }
}

/// One event the DSP thread counts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HealthEvent {
    /// The loop woke `late_ns` after the time it asked for (0 when on time or early).
    Wake { late_ns: u64 },
    /// An output callback played `ns` of zeros: the queue had run dry.
    SpeakerUnderrun { ns: u64 },
    /// `ns` of meeting audio was missing at its play time on the room speaker.
    SpeakerMissing { ns: u64 },
    /// A far-end frame was dropped: its play time had passed.
    FarendLate,
    /// The far-end assembler restarted on a discontinuity.
    FarendReset,
    /// `slots` room-mic slots (10 ms each) were written as silence after a stall.
    MicSilence { slots: u64 },
    /// `ns` of `peer`'s mic audio was missing when the room mic needed it.
    MicMissing { peer: PeerId, ns: u64 },
}

/// The counts over some span (the last minute, or since audio started).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HealthCounts {
    pub wakes: u64,
    pub wake_worst_ns: u64,
    pub wake_p99_ns: u64,
    pub speaker_underruns: u64,
    pub speaker_underrun_ns: u64,
    pub speaker_missing_ns: u64,
    pub farend_late_frames: u64,
    pub farend_resets: u64,
    pub mic_silence_slots: u64,
    /// Per mic, in peer order; only mics that missed something.
    pub mic_missing_ns: Vec<(PeerId, u64)>,
}

/// What the Settings UI shows (see `get_audio_health`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioHealth {
    pub realtime: RtStatus,
    /// Times the system took real-time scheduling away from the audio thread.
    pub demotions: u32,
    /// Seconds the `last_minute` counts cover (less than 60 right after a start or reset).
    pub window_secs: u32,
    pub last_minute: HealthCounts,
    pub since_start: HealthCounts,
}

#[derive(Clone, Debug, Default)]
struct Bucket {
    /// The second (host time / 1 s) this bucket counts; `None` while unused.
    second: Option<u64>,
    wakes: Hist,
    speaker_underruns: u64,
    speaker_underrun_ns: u64,
    speaker_missing_ns: u64,
    farend_late_frames: u64,
    farend_resets: u64,
    mic_silence_slots: u64,
    mic_missing_ns: Vec<(PeerId, u64)>,
}

impl Bucket {
    fn clear(&mut self, second: Option<u64>) {
        let mut mics = std::mem::take(&mut self.mic_missing_ns);
        mics.clear();
        *self = Bucket {
            second,
            mic_missing_ns: mics,
            ..Default::default()
        };
    }
    fn add(&mut self, ev: HealthEvent) {
        match ev {
            HealthEvent::Wake { late_ns } => self.wakes.add(late_ns),
            HealthEvent::SpeakerUnderrun { ns } => {
                self.speaker_underruns += 1;
                self.speaker_underrun_ns += ns;
            }
            HealthEvent::SpeakerMissing { ns } => self.speaker_missing_ns += ns,
            HealthEvent::FarendLate => self.farend_late_frames += 1,
            HealthEvent::FarendReset => self.farend_resets += 1,
            HealthEvent::MicSilence { slots } => self.mic_silence_slots += slots,
            HealthEvent::MicMissing { peer, ns } => add_peer(&mut self.mic_missing_ns, peer, ns),
        }
    }
    fn merge_into(&self, acc: &mut Bucket) {
        acc.wakes.merge(&self.wakes);
        acc.speaker_underruns += self.speaker_underruns;
        acc.speaker_underrun_ns += self.speaker_underrun_ns;
        acc.speaker_missing_ns += self.speaker_missing_ns;
        acc.farend_late_frames += self.farend_late_frames;
        acc.farend_resets += self.farend_resets;
        acc.mic_silence_slots += self.mic_silence_slots;
        for &(p, ns) in &self.mic_missing_ns {
            add_peer(&mut acc.mic_missing_ns, p, ns);
        }
    }
    fn counts(&self) -> HealthCounts {
        let mut mics: Vec<(PeerId, u64)> = self
            .mic_missing_ns
            .iter()
            .copied()
            .filter(|(_, ns)| *ns > 0)
            .collect();
        mics.sort_by_key(|(p, _)| *p);
        HealthCounts {
            wakes: self.wakes.n,
            wake_worst_ns: self.wakes.worst,
            wake_p99_ns: self.wakes.quantile(0.99),
            speaker_underruns: self.speaker_underruns,
            speaker_underrun_ns: self.speaker_underrun_ns,
            speaker_missing_ns: self.speaker_missing_ns,
            farend_late_frames: self.farend_late_frames,
            farend_resets: self.farend_resets,
            mic_silence_slots: self.mic_silence_slots,
            mic_missing_ns: mics,
        }
    }
}

fn add_peer(v: &mut Vec<(PeerId, u64)>, peer: PeerId, ns: u64) {
    match v.iter_mut().find(|(p, _)| *p == peer) {
        Some((_, total)) => *total += ns,
        None => v.push((peer, ns)),
    }
}

/// The counters: one bucket per second of the window, plus the running total.
pub struct Health {
    window: Vec<Bucket>,
    total: Bucket,
    since_s: u64,
}

impl Health {
    pub fn new(now_ns: u64) -> Self {
        Self {
            window: vec![Bucket::default(); WINDOW_SECS as usize],
            total: Bucket::default(),
            since_s: now_ns / NS_PER_S,
        }
    }
    /// Clears the window and the totals; counting starts again at `now_ns`.
    pub fn reset(&mut self, now_ns: u64) {
        for b in &mut self.window {
            b.clear(None);
        }
        self.total.clear(None);
        self.since_s = now_ns / NS_PER_S;
    }
    pub fn record(&mut self, now_ns: u64, ev: HealthEvent) {
        self.total.add(ev);
        let s = now_ns / NS_PER_S;
        let b = &mut self.window[(s % WINDOW_SECS) as usize];
        match b.second {
            Some(bs) if bs == s => {}
            Some(bs) if bs > s => return, // time went backwards: only the total counts it
            _ => b.clear(Some(s)),
        }
        b.add(ev);
    }
    /// The counts of the last [`WINDOW_SECS`] seconds up to `now_ns`.
    pub fn last_minute(&self, now_ns: u64) -> HealthCounts {
        let s = now_ns / NS_PER_S;
        let mut acc = Bucket::default();
        for b in &self.window {
            if b.second.is_some_and(|bs| bs <= s && s - bs < WINDOW_SECS) {
                b.merge_into(&mut acc);
            }
        }
        acc.counts()
    }
    pub fn since_start(&self) -> HealthCounts {
        self.total.counts()
    }
    /// Seconds the window covers at `now_ns`: since the start or reset, at most a minute.
    pub fn window_secs(&self, now_ns: u64) -> u32 {
        ((now_ns / NS_PER_S).saturating_sub(self.since_s) + 1).min(WINDOW_SECS) as u32
    }
    pub fn snapshot(&self, now_ns: u64, realtime: RtStatus, demotions: u32) -> AudioHealth {
        AudioHealth {
            realtime,
            demotions,
            window_secs: self.window_secs(now_ns),
            last_minute: self.last_minute(now_ns),
            since_start: self.since_start(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;
    const T0: u64 = 1_000 * S;

    #[test]
    fn gaps_inside_flowing_audio_are_counted_when_it_resumes() {
        let mut g = GapCounter::new(1_000);
        assert_eq!(g.observe(0, 480), 0, "not started");
        assert_eq!(g.observe(200, 280), 0, "a partial first read is the start");
        assert_eq!(g.observe(480, 0), 0);
        assert_eq!(g.observe(0, 480), 0, "pending until audio resumes");
        assert_eq!(g.observe(0, 100), 0);
        assert_eq!(g.observe(400, 80), 580 + 80);
        assert_eq!(g.observe(480, 0), 0);
        // A silence longer than max_gap is a stop, not a gap…
        for _ in 0..3 {
            assert_eq!(g.observe(0, 480), 0);
        }
        // …and the restart's partial read is a start again.
        assert_eq!(g.observe(100, 380), 0);
        assert_eq!(g.observe(480, 0), 0);
        g.reset();
        assert_eq!(g.observe(0, 480), 0);
        assert_eq!(g.observe(480, 0), 0, "reset: starting again");
    }

    #[test]
    fn lateness_bins_and_quantiles() {
        assert_eq!(bin_of(0), 0);
        assert_eq!(bin_of(99_999), 0);
        assert_eq!(bin_of(100_000), 1);
        assert_eq!(bin_of(9_999_999), 99);
        assert_eq!(bin_of(10 * MS), 100);
        assert_eq!(bin_of(99 * MS), 189);
        assert_eq!(bin_of(100 * MS), 190);
        assert_eq!(bin_of(u64::MAX), 190);
        assert_eq!(bin_upper_ns(0), 100_000);
        assert_eq!(bin_upper_ns(99), 10 * MS);
        assert_eq!(bin_upper_ns(100), 11 * MS);
        let mut h = Hist::default();
        assert_eq!(h.quantile(0.99), 0);
        for _ in 0..990 {
            h.add(50_000);
        }
        for _ in 0..10 {
            h.add(1_850_000);
        }
        assert_eq!(h.n, 1_000);
        assert_eq!(h.worst, 1_850_000);
        assert_eq!(h.quantile(0.99), 100_000, "990 of 1000 within 0.1 ms");
        assert_eq!(h.quantile(0.995), 1_850_000, "capped at the worst value");
        h.add(250 * MS);
        assert_eq!(h.quantile(1.0), 250 * MS);
    }

    #[test]
    fn the_window_keeps_the_last_minute_and_the_total_keeps_everything() {
        let mut h = Health::new(T0);
        h.record(T0, HealthEvent::FarendLate);
        h.record(T0 + 500 * MS, HealthEvent::FarendLate);
        h.record(T0 + 10 * S, HealthEvent::SpeakerUnderrun { ns: 5 * MS });
        h.record(T0 + 10 * S, HealthEvent::SpeakerUnderrun { ns: 2 * MS });
        h.record(T0 + 20 * S, HealthEvent::MicSilence { slots: 3 });
        h.record(T0 + 20 * S, HealthEvent::FarendReset);
        h.record(T0 + 30 * S, HealthEvent::SpeakerMissing { ns: 20 * MS });
        let m = h.last_minute(T0 + 59 * S);
        assert_eq!(m.farend_late_frames, 2);
        assert_eq!((m.speaker_underruns, m.speaker_underrun_ns), (2, 7 * MS));
        assert_eq!(m.mic_silence_slots, 3);
        assert_eq!(m.farend_resets, 1);
        assert_eq!(m.speaker_missing_ns, 20 * MS);
        assert_eq!(h.window_secs(T0 + 59 * S), 60);
        assert_eq!(h.window_secs(T0 + 9 * S), 10);
        // A minute after T0, T0's second has left the window.
        let m = h.last_minute(T0 + 60 * S);
        assert_eq!(m.farend_late_frames, 0);
        assert_eq!(m.speaker_underruns, 2);
        let m = h.last_minute(T0 + 75 * S);
        assert_eq!((m.speaker_underruns, m.mic_silence_slots), (0, 3));
        assert_eq!(h.last_minute(T0 + 200 * S), HealthCounts::default());
        let t = h.since_start();
        assert_eq!(t.farend_late_frames, 2);
        assert_eq!(t.speaker_underruns, 2);
        assert_eq!(t.speaker_missing_ns, 20 * MS);
        // A bucket reused a minute later starts from zero.
        h.record(T0 + 70 * S, HealthEvent::FarendLate);
        assert_eq!(h.last_minute(T0 + 70 * S).speaker_underruns, 0);
        assert_eq!(h.last_minute(T0 + 70 * S).farend_late_frames, 1);
        assert_eq!(h.since_start().farend_late_frames, 3);
    }

    #[test]
    fn wake_lateness_worst_and_p99_follow_the_window() {
        let mut h = Health::new(T0);
        for i in 0..1_000 {
            h.record(T0 + i * 2 * MS, HealthEvent::Wake { late_ns: 30_000 });
        }
        h.record(T0 + 2 * S, HealthEvent::Wake { late_ns: 1_800_000 });
        let m = h.last_minute(T0 + 3 * S);
        assert_eq!(m.wakes, 1_001);
        assert_eq!(m.wake_worst_ns, 1_800_000);
        assert_eq!(m.wake_p99_ns, 100_000);
        // 70 s later the late wake has left the window but stays in the total.
        h.record(T0 + 72 * S, HealthEvent::Wake { late_ns: 10_000 });
        let m = h.last_minute(T0 + 72 * S);
        assert_eq!((m.wakes, m.wake_worst_ns), (1, 10_000));
        assert_eq!(h.since_start().wake_worst_ns, 1_800_000);
        assert_eq!(h.since_start().wakes, 1_002);
    }

    #[test]
    fn mic_misses_are_kept_per_mic_in_peer_order() {
        let mut h = Health::new(T0);
        h.record(
            T0,
            HealthEvent::MicMissing {
                peer: PeerId(3),
                ns: 10 * MS,
            },
        );
        h.record(
            T0 + S,
            HealthEvent::MicMissing {
                peer: PeerId(2),
                ns: 5 * MS,
            },
        );
        h.record(
            T0 + 2 * S,
            HealthEvent::MicMissing {
                peer: PeerId(3),
                ns: MS,
            },
        );
        h.record(
            T0 + 2 * S,
            HealthEvent::MicMissing {
                peer: PeerId(4),
                ns: 0,
            },
        );
        let m = h.last_minute(T0 + 2 * S);
        assert_eq!(
            m.mic_missing_ns,
            vec![(PeerId(2), 5 * MS), (PeerId(3), 11 * MS)]
        );
        assert_eq!(h.since_start().mic_missing_ns, m.mic_missing_ns);
    }

    #[test]
    fn reset_clears_everything_and_restarts_the_window() {
        let mut h = Health::new(T0);
        h.record(T0, HealthEvent::FarendLate);
        h.record(T0, HealthEvent::Wake { late_ns: 3 * MS });
        h.reset(T0 + 5 * S);
        assert_eq!(h.last_minute(T0 + 5 * S), HealthCounts::default());
        assert_eq!(h.since_start(), HealthCounts::default());
        assert_eq!(h.window_secs(T0 + 5 * S), 1);
        let snap = h.snapshot(T0 + 6 * S, RtStatus::TimeConstraintOnly, 2);
        assert_eq!(snap.realtime, RtStatus::TimeConstraintOnly);
        assert_eq!(snap.demotions, 2);
        assert_eq!(snap.window_secs, 2);
    }

    #[test]
    fn events_from_the_past_count_only_in_the_total() {
        let mut h = Health::new(T0);
        h.record(T0 + 61 * S, HealthEvent::FarendLate); // bucket 1 now holds second 1061
        h.record(T0 + S, HealthEvent::FarendLate); // second 1001 maps to the same bucket
        assert_eq!(h.last_minute(T0 + 61 * S).farend_late_frames, 1);
        assert_eq!(h.since_start().farend_late_frames, 2);
    }
}
