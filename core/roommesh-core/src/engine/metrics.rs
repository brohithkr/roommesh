//! Per-peer diagnostics shown in Settings → Advanced and summarised as connection quality.
use crate::ids::PeerId;
use crate::room::events::ConnectionQuality;
use crate::room::protocol::PeerReport;
use std::collections::BTreeMap;

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

#[derive(Default)]
pub struct MetricsBook {
    peers: BTreeMap<PeerId, PeerMetrics>,
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
    }
    pub fn apply_report(&mut self, r: &PeerReport) {
        let e = self.entry(r.peer);
        e.rtt_ms = Some(r.rtt_ms);
        e.clock_offset_ms = Some(r.clock_offset_ms);
        e.drift_ppm = Some(r.drift_ppm);
        e.transport = r.transport.clone();
        if e.jitter_ms.is_none() {
            e.jitter_ms = Some(r.jitter_ms);
        }
        if e.loss_pct.is_none() {
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
}
