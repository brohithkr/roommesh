//! Per-peer diagnostics shown in Settings → Advanced and summarised as connection quality.
use crate::audio::jitter_buffer::JitterStats;
use crate::engine::coordinator::MicStatus;
use crate::ids::PeerId;
use crate::room::events::ConnectionQuality;
use crate::room::protocol::PeerReport;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PeerMetrics {
    pub peer: PeerId,
    pub name: String,
    pub rtt_ms: Option<f32>,
    pub jitter_ms: Option<f32>,
    pub loss_pct: Option<f32>,
    pub clock_offset_ms: Option<f32>,
    pub drift_ppm: Option<f32>,
    pub buffer_ms: Option<f32>,
    pub mic_score: Option<f32>,
    pub speech_prob: Option<f32>,
    pub aec_erle_db: Option<f32>,
    pub aec_converged: bool,
    pub is_active: bool,
    pub transport: String,
    pub quality: ConnectionQuality,
}
// Kept here rather than derived on the types (room/events.rs, ids.rs) so the defaults stay next
// to the only code that relies on them (`..Default::default()` for PeerMetrics).
#[allow(clippy::derivable_impls)]
impl Default for ConnectionQuality {
    fn default() -> Self {
        ConnectionQuality::Good
    }
}
#[allow(clippy::derivable_impls)]
impl Default for PeerId {
    fn default() -> Self {
        PeerId(0)
    }
}

pub fn classify(
    loss_pct: Option<f32>,
    jitter_ms: Option<f32>,
    rtt_ms: Option<f32>,
) -> ConnectionQuality {
    let (l, j, r) = (
        loss_pct.unwrap_or(0.0),
        jitter_ms.unwrap_or(0.0),
        rtt_ms.unwrap_or(0.0),
    );
    if loss_pct.is_none() && jitter_ms.is_none() && rtt_ms.is_none() {
        return ConnectionQuality::Good;
    }
    if l < 1.0 && j < 10.0 && r < 30.0 {
        ConnectionQuality::Excellent
    } else if l < 5.0 && j < 30.0 && r < 80.0 {
        ConnectionQuality::Good
    } else {
        ConnectionQuality::Degraded
    }
}

/// Current (windowed) loss of a received stream in percent, for quality classification.
pub fn stream_loss_pct(s: &JitterStats) -> f32 {
    (s.recent_loss * 100.0) as f32
}

/// Interarrival jitter of a received stream in ms.
pub fn stream_jitter_ms(s: &JitterStats) -> f32 {
    (s.jitter_ns / 1e6) as f32
}

#[derive(Default)]
pub struct MetricsBook {
    peers: BTreeMap<PeerId, PeerMetrics>,
    /// Peers whose jitter/loss currently come from a mic stream the coordinator receives
    /// (more direct than the peer's own report, which then only fills in when this is absent).
    stream_fed: BTreeSet<PeerId>,
    /// Last reported (jitter_ms, loss_pct) per peer, restored when stream info is cleared.
    reported: BTreeMap<PeerId, (f32, f32)>,
}

impl MetricsBook {
    fn entry(&mut self, p: PeerId) -> &mut PeerMetrics {
        self.peers.entry(p).or_insert_with(|| PeerMetrics {
            peer: p,
            ..Default::default()
        })
    }
    pub fn set_name(&mut self, p: PeerId, name: String) {
        self.entry(p).name = name;
    }
    pub fn retain(&mut self, members: &[PeerId]) {
        self.peers.retain(|p, _| members.contains(p));
        self.stream_fed.retain(|p| members.contains(p));
        self.reported.retain(|p, _| members.contains(p));
    }
    pub fn apply_report(&mut self, r: &PeerReport) {
        self.reported.insert(r.peer, (r.jitter_ms, r.loss_pct));
        let stream_fed = self.stream_fed.contains(&r.peer);
        let e = self.entry(r.peer);
        e.rtt_ms = Some(r.rtt_ms);
        e.clock_offset_ms = Some(r.clock_offset_ms);
        e.drift_ppm = Some(r.drift_ppm);
        e.transport = r.transport.clone();
        if !stream_fed {
            e.jitter_ms = Some(r.jitter_ms);
            e.loss_pct = Some(r.loss_pct);
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn apply_mic(
        &mut self,
        p: PeerId,
        score: f32,
        speech_prob: f32,
        jitter_ms: Option<f32>,
        loss_pct: Option<f32>,
        buffer_ms: Option<f32>,
        erle_db: Option<f32>,
        converged: bool,
        active: bool,
    ) {
        let e = self.entry(p);
        e.mic_score = Some(score);
        e.speech_prob = Some(speech_prob);
        if jitter_ms.is_some() {
            e.jitter_ms = jitter_ms;
        }
        if loss_pct.is_some() {
            e.loss_pct = loss_pct;
        }
        e.buffer_ms = buffer_ms;
        e.aec_erle_db = erle_db;
        e.aec_converged = converged;
        e.is_active = active;
        if jitter_ms.is_some() || loss_pct.is_some() {
            self.stream_fed.insert(p);
        }
    }
    /// Convenience over [`apply_mic`](Self::apply_mic) for a coordinator `MicStatus`: loss is the
    /// windowed [`stream_loss_pct`], not the cumulative ratio.
    pub fn apply_mic_status(&mut self, s: &MicStatus, active: bool) {
        self.apply_mic(
            s.peer,
            s.score,
            s.speech_prob,
            s.jitter.as_ref().map(stream_jitter_ms),
            s.jitter.as_ref().map(stream_loss_pct),
            s.buffer_ms,
            s.aec.erle_db,
            s.aec.converged,
            active,
        );
    }
    /// Forgets mic-derived metrics for `p` (its mic left the pipeline, or we stopped being
    /// coordinator): score, speech probability, buffer, AEC and active flag are cleared, and
    /// jitter/loss fall back to the peer's last report (later reports refill them).
    pub fn clear_mic(&mut self, p: PeerId) {
        let Some(e) = self.peers.get_mut(&p) else {
            return;
        };
        e.mic_score = None;
        e.speech_prob = None;
        e.buffer_ms = None;
        e.aec_erle_db = None;
        e.aec_converged = false;
        e.is_active = false;
        if self.stream_fed.remove(&p) {
            let r = self.reported.get(&p);
            e.jitter_ms = r.map(|r| r.0);
            e.loss_pct = r.map(|r| r.1);
        }
    }
    /// [`clear_mic`](Self::clear_mic) for every peer.
    pub fn clear_all_mic(&mut self) {
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for p in peers {
            self.clear_mic(p);
        }
    }
    pub fn snapshot(&mut self) -> Vec<PeerMetrics> {
        for m in self.peers.values_mut() {
            m.quality = classify(m.loss_pct, m.jitter_ms, m.rtt_ms);
        }
        self.peers.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_quality() {
        assert_eq!(
            classify(Some(0.2), Some(3.0), Some(8.0)),
            ConnectionQuality::Excellent
        );
        assert_eq!(
            classify(Some(2.0), Some(15.0), Some(40.0)),
            ConnectionQuality::Good
        );
        assert_eq!(
            classify(Some(8.0), Some(15.0), Some(40.0)),
            ConnectionQuality::Degraded
        );
        assert_eq!(classify(None, None, None), ConnectionQuality::Good);
    }
    #[test]
    fn book_merges_reports_and_stream_stats() {
        let mut b = MetricsBook::default();
        b.set_name(PeerId(2), "Amaan".into());
        b.apply_report(&PeerReport {
            peer: PeerId(2),
            rtt_ms: 12.0,
            jitter_ms: 2.0,
            loss_pct: 0.5,
            clock_offset_ms: 1.5,
            drift_ppm: 3.0,
            transport: "awdl0".into(),
        });
        b.apply_mic(
            PeerId(2),
            0.8,
            0.9,
            Some(4.0),
            Some(0.5),
            Some(18.0),
            Some(15.0),
            true,
            true,
        );
        let m = b.snapshot();
        let p = m.iter().find(|p| p.peer == PeerId(2)).unwrap();
        assert_eq!(p.name, "Amaan");
        assert_eq!(p.transport, "awdl0");
        assert_eq!(p.mic_score, Some(0.8));
        assert!(p.is_active);
        assert_eq!(p.quality, ConnectionQuality::Excellent);
    }

    fn report(jitter_ms: f32, loss_pct: f32) -> PeerReport {
        PeerReport {
            peer: PeerId(2),
            rtt_ms: 12.0,
            jitter_ms,
            loss_pct,
            clock_offset_ms: 0.0,
            drift_ppm: 0.0,
            transport: "awdl0".into(),
        }
    }
    #[test]
    fn stream_stats_win_until_cleared_then_reports_refill() {
        let mut b = MetricsBook::default();
        b.apply_report(&report(2.0, 0.5));
        b.apply_mic(
            PeerId(2),
            0.8,
            0.9,
            Some(4.0),
            Some(1.5),
            Some(18.0),
            Some(15.0),
            true,
            true,
        );
        b.apply_report(&report(7.0, 0.1));
        let p = b.snapshot().remove(0);
        assert_eq!(
            (p.jitter_ms, p.loss_pct),
            (Some(4.0), Some(1.5)),
            "stream info wins"
        );
        b.clear_mic(PeerId(2));
        let p = b.snapshot().remove(0);
        assert_eq!(
            (p.jitter_ms, p.loss_pct),
            (Some(7.0), Some(0.1)),
            "last report restored"
        );
        assert_eq!(
            (p.mic_score, p.speech_prob, p.buffer_ms, p.aec_erle_db),
            (None, None, None, None)
        );
        assert!(!p.is_active && !p.aec_converged);
        b.apply_report(&report(9.0, 3.0));
        let p = b.snapshot().remove(0);
        assert_eq!(
            (p.jitter_ms, p.loss_pct),
            (Some(9.0), Some(3.0)),
            "reports refill"
        );
        assert_eq!(p.rtt_ms, Some(12.0));
    }
    #[test]
    fn clear_all_mic_clears_every_peer() {
        let mut b = MetricsBook::default();
        for p in [PeerId(2), PeerId(3)] {
            b.apply_mic(p, 0.5, 0.5, Some(1.0), Some(0.0), None, None, false, true);
        }
        b.clear_all_mic();
        assert!(b
            .snapshot()
            .iter()
            .all(|p| p.mic_score.is_none() && !p.is_active && p.jitter_ms.is_none()));
    }
    #[test]
    fn mic_status_uses_windowed_loss() {
        use crate::dsp::aec::AecStats;
        let mut b = MetricsBook::default();
        // A long-past outage keeps the cumulative ratio at 50 % but the stream is clean now.
        let jitter = JitterStats {
            received: 100,
            lost: 100,
            jitter_ns: 2e6,
            recent_loss: 0.002,
            ..Default::default()
        };
        let st = MicStatus {
            peer: PeerId(4),
            score: 0.7,
            speech_prob: 0.9,
            snr_db: 30.0,
            present: true,
            aec: AecStats {
                erle_db: Some(12.0),
                delay_ms: None,
                converged: true,
            },
            jitter: Some(jitter),
            buffer_ms: Some(20.0),
        };
        b.apply_mic_status(&st, true);
        let p = b.snapshot().remove(0);
        assert!((p.loss_pct.unwrap() - 0.2).abs() < 1e-4);
        assert_eq!(p.jitter_ms, Some(2.0));
        assert_eq!(p.quality, ConnectionQuality::Excellent);
        assert!(p.is_active && p.aec_converged);
    }
}
