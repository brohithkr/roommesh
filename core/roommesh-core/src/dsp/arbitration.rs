//! Active-microphone arbitration: confidence margin + confirmation period + minimum hold.
//! Optionally admits one simultaneous, *different* talker (low envelope correlation).
use crate::ids::PeerId;
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq)]
pub struct ArbitrationConfig {
    pub switch_margin: f32,
    pub confirm_ms: u64,
    pub min_hold_ms: u64,
    pub allow_multi: bool,
    pub multi_threshold: f32,
    pub multi_max_corr: f32,
    pub secondary_hangover_ms: u64,
    /// How long a primary mic may be missing from the observations (late packets, a brief
    /// dropout) before it is replaced. The selection is held unchanged meanwhile.
    pub primary_absence_grace_ms: u64,
}
impl Default for ArbitrationConfig {
    fn default() -> Self {
        Self { switch_margin: 0.15, confirm_ms: 150, min_hold_ms: 600, allow_multi: false,
               multi_threshold: 0.6, multi_max_corr: 0.5, secondary_hangover_ms: 300,
               primary_absence_grace_ms: 150 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MicObservation { pub peer: PeerId, pub score: f32, pub speaking: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Selection { pub primary: Option<PeerId>, pub secondary: Option<PeerId> }
impl Selection {
    pub fn contains(&self, p: PeerId) -> bool { self.primary == Some(p) || self.secondary == Some(p) }
    pub fn count(&self) -> usize { self.primary.is_some() as usize + self.secondary.is_some() as usize }
}

fn by_score(a: &&MicObservation, b: &&MicObservation) -> Ordering {
    a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal)
}

pub struct Arbiter {
    cfg: ArbitrationConfig,
    primary: Option<PeerId>,
    primary_since: u64,
    /// When the current primary was first missing from the observations.
    primary_absent_since: Option<u64>,
    candidate: Option<(PeerId, u64)>,
    secondary: Option<(PeerId, u64)>,
}

impl Arbiter {
    pub fn new(cfg: ArbitrationConfig) -> Self {
        Self { cfg, primary: None, primary_since: 0, primary_absent_since: None, candidate: None, secondary: None }
    }
    pub fn set_config(&mut self, cfg: ArbitrationConfig) { self.cfg = cfg; }
    pub fn selection(&self) -> Selection {
        Selection { primary: self.primary, secondary: self.secondary.map(|s| s.0) }
    }

    pub fn update(&mut self, now_ms: u64, obs: &[MicObservation], corr: impl Fn(PeerId, PeerId) -> f32) -> Selection {
        let find = |p: PeerId| obs.iter().find(|o| o.peer == p);
        if self.primary.is_some_and(|p| find(p).is_none()) {
            let since = *self.primary_absent_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) < self.cfg.primary_absence_grace_ms {
                // Brief absence: hold the selection rather than cut to another mic mid-word.
                if !obs.is_empty() { self.update_secondary(now_ms, obs, &corr); }
                return self.selection();
            }
            self.primary = None;
        }
        self.primary_absent_since = None;
        if obs.is_empty() {
            *self = Self::new(self.cfg.clone());
            return self.selection();
        }
        match self.primary {
            None => {
                let best = obs.iter().max_by(by_score).unwrap();
                self.primary = Some(best.peer);
                self.primary_since = now_ms;
                self.candidate = None;
            }
            Some(p) => {
                let cur = find(p).unwrap();
                let challenger = obs.iter().filter(|o| o.peer != p).max_by(by_score);
                match challenger {
                    Some(c) if c.speaking && c.score > cur.score + self.cfg.switch_margin => {
                        let since = match self.candidate {
                            Some((cp, s)) if cp == c.peer => s,
                            _ => { self.candidate = Some((c.peer, now_ms)); now_ms }
                        };
                        let confirmed = now_ms.saturating_sub(since) >= self.cfg.confirm_ms;
                        let held = now_ms.saturating_sub(self.primary_since) >= self.cfg.min_hold_ms;
                        if confirmed && held {
                            self.primary = Some(c.peer);
                            self.primary_since = now_ms;
                            self.candidate = None;
                            if self.secondary.is_some_and(|s| s.0 == c.peer) { self.secondary = None; }
                        }
                    }
                    // Keep a pending candidate through brief dips (pauses between syllables) as
                    // long as the current mic has not regained the lead.
                    Some(c) if self.candidate.is_some_and(|(cp, _)| cp == c.peer) && c.score >= cur.score => {}
                    _ => self.candidate = None,
                }
            }
        }
        self.update_secondary(now_ms, obs, &corr);
        self.selection()
    }

    fn update_secondary(&mut self, now_ms: u64, obs: &[MicObservation], corr: &impl Fn(PeerId, PeerId) -> f32) {
        let Some(primary) = self.primary else { self.secondary = None; return };
        if !self.cfg.allow_multi { self.secondary = None; return; }
        let primary_speaking = obs.iter().any(|o| o.peer == primary && o.speaking);
        let qualifying = obs.iter()
            .filter(|o| o.peer != primary && o.speaking && o.score >= self.cfg.multi_threshold
                && corr(primary, o.peer) < self.cfg.multi_max_corr)
            .max_by(by_score);
        match (qualifying, self.secondary) {
            (Some(q), _) if primary_speaking => self.secondary = Some((q.peer, now_ms)),
            (_, Some((s, last))) if s != primary && obs.iter().any(|o| o.peer == s)
                && now_ms.saturating_sub(last) < self.cfg.secondary_hangover_ms => {}
            _ => self.secondary = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PeerId;
    const A: PeerId = PeerId(1);
    const B: PeerId = PeerId(2);
    const C: PeerId = PeerId(3);
    const D: PeerId = PeerId(4);
    fn o(peer: PeerId, score: f32) -> MicObservation { MicObservation { peer, score, speaking: score > 0.3 } }
    fn no_corr(_: PeerId, _: PeerId) -> f32 { 0.0 }
    fn run(arb: &mut Arbiter, from: u64, to: u64, obs: &[MicObservation]) -> Selection {
        let mut s = Selection::default();
        let mut t = from;
        while t <= to { s = arb.update(t, obs, no_corr); t += 10; }
        s
    }

    #[test]
    fn picks_best_initially() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        let s = arb.update(0, &[o(A, 0.27), o(B, 0.93), o(C, 0.41), o(D, 0.22)], no_corr);
        assert_eq!(s.primary, Some(B));
    }
    #[test]
    fn small_margin_keeps_current() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        arb.update(0, &[o(B, 0.9), o(C, 0.1)], no_corr);
        let s = run(&mut arb, 10, 3000, &[o(B, 0.80), o(C, 0.84)]);
        assert_eq!(s.primary, Some(B));
    }
    #[test]
    fn sustained_better_candidate_switches_after_confirm_and_hold() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        run(&mut arb, 0, 1000, &[o(B, 0.9), o(C, 0.1)]);
        let obs = [o(B, 0.53), o(C, 0.92)];
        assert_eq!(run(&mut arb, 1010, 1150, &obs).primary, Some(B));
        assert_eq!(run(&mut arb, 1160, 1200, &obs).primary, Some(C));
    }
    #[test]
    fn brief_spike_ignored() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        run(&mut arb, 0, 1000, &[o(B, 0.9), o(C, 0.1)]);
        run(&mut arb, 1010, 1060, &[o(B, 0.5), o(C, 0.95)]);
        assert_eq!(run(&mut arb, 1070, 2000, &[o(B, 0.9), o(C, 0.1)]).primary, Some(B));
    }
    #[test]
    fn candidate_survives_pauses_between_syllables() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        run(&mut arb, 0, 1000, &[o(B, 0.9), o(C, 0.1)]);
        // C is clearly better in 80 ms bursts separated by 40 ms pauses where both score ~0
        let mut t = 1010;
        for _ in 0..3 {
            run(&mut arb, t, t + 70, &[o(B, 0.4), o(C, 0.9)]);
            t += 80;
            run(&mut arb, t, t + 30, &[MicObservation { peer: B, score: 0.02, speaking: true }, MicObservation { peer: C, score: 0.02, speaking: true }]);
            t += 40;
        }
        assert_eq!(arb.selection().primary, Some(C));
    }
    #[test]
    fn minimum_hold_time_respected() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        arb.update(0, &[o(B, 0.9)], no_corr);
        let obs = [o(B, 0.2), o(C, 0.95)];
        assert_eq!(run(&mut arb, 10, 590, &obs).primary, Some(B));
        assert_eq!(run(&mut arb, 600, 610, &obs).primary, Some(C));
    }
    #[test]
    fn vanished_primary_replaced_after_grace() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        arb.update(0, &[o(B, 0.9), o(C, 0.5)], no_corr);
        assert_eq!(arb.update(10, &[o(C, 0.5)], no_corr).primary, Some(B), "held during grace");
        assert_eq!(arb.update(150, &[o(C, 0.5)], no_corr).primary, Some(B));
        assert_eq!(arb.update(160, &[o(C, 0.5)], no_corr).primary, Some(C), "replaced after 150 ms");
        assert_eq!(arb.update(170, &[], no_corr).primary, Some(C), "empty obs also held");
        assert_eq!(arb.update(320, &[], no_corr), Selection::default());
    }
    #[test]
    fn primary_survives_brief_absence() {
        let mut arb = Arbiter::new(ArbitrationConfig::default());
        run(&mut arb, 0, 1000, &[o(B, 0.9), o(C, 0.5)]);
        // B's packets are late for 100 ms: C is the only observation but must not take over.
        assert_eq!(run(&mut arb, 1010, 1110, &[o(C, 0.9)]).primary, Some(B));
        assert_eq!(run(&mut arb, 1120, 1200, &[o(B, 0.9), o(C, 0.5)]).primary, Some(B));
        // The absence timer restarts after B came back.
        assert_eq!(run(&mut arb, 1210, 1350, &[o(C, 0.9)]).primary, Some(B));
    }
    #[test]
    fn simultaneous_talkers_only_when_enabled_and_uncorrelated() {
        let mut cfg = ArbitrationConfig::default();
        let obs = [o(B, 0.9), o(D, 0.8), o(A, 0.1)];
        let mut arb = Arbiter::new(cfg.clone());
        assert_eq!(arb.update(0, &obs, no_corr).secondary, None);
        cfg.allow_multi = true;
        let mut arb = Arbiter::new(cfg.clone());
        assert_eq!(arb.update(0, &obs, no_corr).secondary, Some(D));
        let mut arb = Arbiter::new(cfg);
        assert_eq!(arb.update(0, &obs, |_, _| 0.9).secondary, None, "same talker heard twice");
    }
    #[test]
    fn secondary_hangover() {
        let cfg = ArbitrationConfig { allow_multi: true, ..Default::default() };
        let mut arb = Arbiter::new(cfg);
        arb.update(0, &[o(B, 0.9), o(D, 0.8)], no_corr);
        assert_eq!(arb.update(100, &[o(B, 0.9), o(D, 0.1)], no_corr).secondary, Some(D));
        assert_eq!(arb.update(400, &[o(B, 0.9), o(D, 0.1)], no_corr).secondary, None);
    }
}
