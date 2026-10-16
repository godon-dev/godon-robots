//! Deterministic coverage walk over per-parameter level spaces — the
//! port of `engine/coverage_walk.py` (the legacy "ladder", kept as the
//! explicit offline opt-in; the notebook walk is [`crate::walk_policy`]).
//!
//! Coverage contract: every eligible parameter's levels get measured —
//! worst case down to the resolution floor for floats, full enumeration
//! for ints. Retirement (converged params) only accelerates; it never
//! licenses skipping.
//!
//! Selection rule (one line): the next level for a parameter is the
//! candidate FARTHEST from everything already measured on that
//! parameter; ties go to the candidate closest to the range center,
//! then to the higher level. On a uniform grid this yields
//! midpoint -> edges -> quarters — the low-discrepancy prefix computed
//! directly instead of sampled.
//!
//! No sampler, no RNG, no seed: the walk is a pure function of
//! (bounds, floor, retirement, progress).

use std::collections::{BTreeMap, HashSet};

/// name -> (lower, upper, is_int)
pub type Bounds = BTreeMap<String, (f64, f64, bool)>;

/// Per-param walk state for logging/status endpoints
/// (the python `status()` dict, as a struct).
#[derive(Debug, Clone, PartialEq)]
pub struct ParamWalkStatus {
    pub step: f64,
    pub levels_total: Option<i64>,
    pub levels_measured: usize,
    pub levels: Vec<f64>,
}

/// Round-robin farthest-point walk over (param, level) cells.
pub struct CoverageWalk {
    bounds: Bounds,
    /// Resolution floor per float param (initial: range / 4).
    /// Int params enumerate and have no floor.
    floors: BTreeMap<String, f64>,
    measured: BTreeMap<String, Vec<f64>>,
    /// round-robin position
    rr: usize,
}

/// python `round(x, ndigits)` for the two precisions this file uses.
/// Banker's rounding is close enough never to diverge here: the values
/// rounded are grid midpoints, never .5-boundary straddlers at the
/// 6th/9th digit.
pub(crate) fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

pub(crate) fn round9(x: f64) -> f64 {
    (x * 1e9).round() / 1e9
}

impl CoverageWalk {
    pub fn new(bounds: &Bounds) -> Self {
        let mut floors = BTreeMap::new();
        for (name, (lo, hi, is_int)) in bounds {
            if !is_int && hi > lo {
                floors.insert(name.clone(), (hi - lo) / 4.0);
            }
        }
        let measured = bounds.keys().cloned().map(|n| (n, Vec::new())).collect();
        CoverageWalk {
            bounds: bounds.clone(),
            floors,
            measured,
            rr: 0,
        }
    }

    // ── candidates ───────────────────────────────────────────────────

    fn candidates(&self, name: &str) -> Vec<f64> {
        let (lo, hi, is_int) = self.bounds[name];
        let meas = &self.measured[name];
        if is_int {
            return ((lo.round() as i64)..=(hi.round() as i64))
                .map(|v| v as f64)
                .filter(|v| !meas.contains(v))
                .collect();
        }
        // Floats: bounds plus midpoints of all gaps (measured ∪ bounds).
        // Floats: bounds plus midpoints of all gaps (measured ∪ bounds).
        let mut anchors: Vec<f64> = meas.iter().cloned().chain([lo, hi]).collect();
        anchors.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut cands: Vec<f64> = vec![round6(lo), round6(hi)];
        for pair in anchors.windows(2) {
            cands.push(round6((pair[0] + pair[1]) / 2.0));
        }
        // python builds a SET of rounded candidates — dedupe by bits
        // (f64 has no Hash), order irrelevant (selection fully breaks
        // ties by rank key)
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        cands.retain(|c| seen.insert(c.to_bits()) && !meas.contains(c));
        cands
    }

    fn distance(&self, level: f64, meas: &[f64], span: f64) -> f64 {
        if meas.is_empty() {
            return span;
        }
        meas.iter().map(|m| (level - m).abs()).fold(f64::INFINITY, f64::min)
    }

    fn next_level(&self, name: &str) -> Option<f64> {
        let (lo, hi, is_int) = self.bounds[name];
        let meas = &self.measured[name];
        let mut cands = self.candidates(name);
        // Resolution floor: stop splitting when a new float level would
        // sit closer than the floor to an existing measurement. Ints
        // enumerate fully regardless of floor.
        if !is_int && !meas.is_empty() {
            let floor = self.floors.get(name).copied().unwrap_or(0.0);
            cands.retain(|c| self.distance(*c, meas, hi - lo) >= floor);
        }
        if cands.is_empty() {
            return None;
        }
        let center = (lo + hi) / 2.0;
        // rank: farthest from measured, closest to center, higher —
        // the tuple's last member (the level itself) makes the pick
        // order-independent.
        cands.into_iter().max_by(|a, b| {
            let ra = (
                self.distance(*a, meas, hi - lo),
                -(a - center).abs(),
                *a,
            );
            let rb = (
                self.distance(*b, meas, hi - lo),
                -(b - center).abs(),
                *b,
            );
            ra.partial_cmp(&rb).unwrap()
        })
    }

    // ── public API ───────────────────────────────────────────────────

    /// Next (param, level) to measure, or None when the walk is done.
    /// `skip` holds converged param names (retirement).
    pub fn next_probe(&mut self, skip: &HashSet<String>) -> Option<(String, f64)> {
        let eligible: Vec<&String> = self
            .bounds
            .keys()
            .filter(|n| !skip.contains(*n))
            .collect();
        if eligible.is_empty() {
            return None;
        }
        for _ in 0..eligible.len() {
            let name = eligible[self.rr % eligible.len()];
            self.rr += 1;
            if let Some(level) = self.next_level(name) {
                self.measured.get_mut(name).unwrap().push(level);
                return Some((name.clone(), level));
            }
        }
        None
    }

    /// Pure check: would next_probe return a cell? (no mutation)
    pub fn can_probe(&self, skip: &HashSet<String>) -> bool {
        self.bounds
            .keys()
            .filter(|n| !skip.contains(*n))
            .any(|n| self.next_level(n).is_some())
    }

    /// Halve resolution floors — next pass measures the new midpoints.
    pub fn refine(&mut self) {
        for floor in self.floors.values_mut() {
            *floor /= 2.0;
        }
    }

    /// Per-param walk state for logging/status endpoints.
    pub fn status(&self) -> BTreeMap<String, ParamWalkStatus> {
        let mut out = BTreeMap::new();
        for (name, (lo, hi, is_int)) in &self.bounds {
            let floor = self.floors.get(name);
            let total = if *is_int {
                Some(hi.round() as i64 - lo.round() as i64 + 1)
            } else {
                floor.map(|f| ((hi - lo) / f).round() as i64 + 1)
            };
            out.insert(
                name.clone(),
                ParamWalkStatus {
                    step: if *is_int { 1.0 } else { floor.copied().unwrap_or(0.0) },
                    levels_total: total,
                    levels_measured: self.measured[name].len(),
                    levels: self.measured[name].clone(),
                },
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds_0_100() -> Bounds {
        [("p0".to_string(), (0.0, 100.0, false))]
            .into_iter()
            .collect()
    }

    /// python test_order_mid_edges_quarters: the coarse pass drains
    /// to exactly [50, 100, 0, 75, 25] — farthest-point spread-first.
    #[test]
    fn midpoint_edges_quarters() {
        let mut w = CoverageWalk::new(&bounds_0_100());
        let mut seq = Vec::new();
        while let Some((_, lv)) = w.next_probe(&HashSet::new()) {
            seq.push(lv);
        }
        assert_eq!(seq, vec![50.0, 100.0, 0.0, 75.0, 25.0]);
    }

    #[test]
    fn floor_stops_the_ladder() {
        // initial floor = 100/4 = 25 → 5 levels (50, 100, 0, 75, 25)
        // then nothing new can sit >= 25 from everything measured
        let mut w = CoverageWalk::new(&bounds_0_100());
        for _ in 0..5 {
            assert!(w.next_probe(&HashSet::new()).is_some());
        }
        assert!(w.next_probe(&HashSet::new()).is_none());
        assert!(!w.can_probe(&HashSet::new()));
        // python test_refine_measures_only_new_midpoints: after refine
        // the fine pass drains to exactly the four new midpoints
        w.refine();
        let mut fine = Vec::new();
        while let Some((_, lv)) = w.next_probe(&HashSet::new()) {
            fine.push(lv);
        }
        fine.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(fine, vec![12.5, 37.5, 62.5, 87.5]);
    }

    #[test]
    fn ints_enumerate_fully() {
        let bounds: Bounds = [("n".to_string(), (0.0, 3.0, true))].into_iter().collect();
        let mut w = CoverageWalk::new(&bounds);
        let mut got: Vec<f64> = Vec::new();
        while let Some((_, lv)) = w.next_probe(&HashSet::new()) {
            got.push(lv);
        }
        let mut expect = vec![0.0, 1.0, 2.0, 3.0];
        expect.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(got.len(), 4);
        let mut sorted = got.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(sorted, expect);
        // int params have no floor: refine changes nothing for them
        w.refine();
        assert!(!w.can_probe(&HashSet::new()));
    }

    #[test]
    fn retirement_skips_and_rotation_rounds() {
        let mut bounds = Bounds::new();
        bounds.insert("a".to_string(), (0.0, 100.0, false));
        bounds.insert("b".to_string(), (0.0, 100.0, false));
        let mut w = CoverageWalk::new(&bounds);
        // rotation: first ask rounds across both params
        let first = w.next_probe(&HashSet::new()).unwrap().0;
        let second = w.next_probe(&HashSet::new()).unwrap().0;
        assert_ne!(first, second);
        // retire everything → done
        let skip: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        assert!(w.next_probe(&skip).is_none());
        assert!(!w.can_probe(&skip));
    }

    #[test]
    fn status_reports_floors_and_counts() {
        let mut w = CoverageWalk::new(&bounds_0_100());
        w.next_probe(&HashSet::new());
        let st = w.status();
        let p0 = &st["p0"];
        assert_eq!(p0.step, 25.0);
        assert_eq!(p0.levels_total, Some(5)); // (100-0)/25 + 1
        assert_eq!(p0.levels_measured, 1);
        assert_eq!(p0.levels, vec![50.0]);
        w.refine();
        assert_eq!(w.status()["p0"].step, 12.5);
        assert_eq!(w.status()["p0"].levels_total, Some(9));
    }

    /// can_probe is the pure check: it must not consume a level.
    #[test]
    fn can_probe_does_not_mutate() {
        let mut w = CoverageWalk::new(&bounds_0_100());
        for _ in 0..5 {
            w.next_probe(&HashSet::new());
        }
        assert!(!w.can_probe(&HashSet::new()));
        assert_eq!(w.status()["p0"].levels_measured, 5);
    }
}
