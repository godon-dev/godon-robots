//! The seek's level policy — the walk family's placement at the seam.
//!
//! Port of `engine/seek_policy.py`: farthest-point placement over the
//! seek's region, the same rule coverage_walk applies (midpoint first,
//! then the farthest candidate, ties to center). Stateless at this
//! seam by design — the DRIVER owns the measured list, so a fresh
//! round re-asks honestly at the new baseline instead of inheriting a
//! stale placement state.
//!
//! A dial is done when no candidate clears the resolution floor
//! (floats) or the enumeration is exhausted (ints). The
//! notebook-steered policy — gap-price steering, priced retirement —
//! is the named follow-on; when it lands, this file is replaced at
//! this one seam and the driver never knows.

use std::collections::BTreeMap;

use crate::seek::RegionSpec;

/// policy(dial, measured) -> next level or None (dial done). Levels
/// are f64 — the python policy returns floats for both int and float
/// dials (int enumeration yields `float(v)`), and the driver stores
/// what it is handed.
pub type Policy = Box<dyn FnMut(&str, &[f64]) -> Option<f64> + Send>;

pub fn region_policy(region: BTreeMap<String, RegionSpec>) -> Policy {
    Box::new(move |dial: &str, measured: &[f64]| {
        let (lo, hi, is_int) = match region.get(dial) {
            Some(spec) => *spec,
            None => return None,
        };
        let meas = measured.to_vec();
        if is_int {
            let cands: Vec<f64> = (lo.round() as i64..=hi.round() as i64)
                .map(|v| v as f64)
                .filter(|v| !meas.contains(v))
                .collect();
            if cands.is_empty() {
                return None;
            }
            let center = (lo + hi) / 2.0;
            // python `max(cands, key=lambda c: (-abs(c - center), c))`:
            // nearest to center, ties to the larger candidate
            let mut best = cands[0];
            let mut best_key = (0.0f64, 0.0f64);
            let mut first = true;
            for c in &cands {
                let key = (-(c - center).abs(), *c);
                if first || key > best_key {
                    best_key = key;
                    best = *c;
                    first = false;
                }
            }
            return Some(best);
        }

        // float dial: anchors = measured + endpoints, candidates are
        // the endpoints and every anchor midpoint (rounded to 6, the
        // python `round(x, 6)` receipts)
        let mut anchors: Vec<f64> = meas.clone();
        anchors.push(lo);
        anchors.push(hi);
        anchors.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut cand_set: Vec<f64> = vec![round6(lo), round6(hi)];
        for pair in anchors.windows(2) {
            cand_set.push(round6((pair[0] + pair[1]) / 2.0));
        }
        cand_set.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        cand_set.dedup();
        let cands: Vec<f64> = cand_set.into_iter().filter(|c| !meas.contains(c)).collect();

        let dist = |c: f64| -> f64 {
            if meas.is_empty() {
                hi - lo
            } else {
                meas.iter()
                    .map(|m| (c - m).abs())
                    .fold(f64::INFINITY, f64::min)
            }
        };

        // The floor bites only once something is measured (the first
        // probe is the midpoint regardless) — coverage_walk's rule.
        let cands = if !meas.is_empty() {
            let floor = (hi - lo) / 4.0;
            cands.into_iter().filter(|c| dist(*c) >= floor).collect()
        } else {
            cands
        };
        if cands.is_empty() {
            return None;
        }
        let center = (lo + hi) / 2.0;
        // max by (dist(c), -|c-center|, c)
        let mut best = cands[0];
        let mut best_key = (0.0f64, 0.0f64, 0.0f64);
        let mut first = true;
        for c in &cands {
            let key = (dist(*c), -(c - center).abs(), *c);
            if first || key > best_key {
                best_key = key;
                best = *c;
                first = false;
            }
        }
        Some(best)
    })
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(specs: &[(&str, RegionSpec)]) -> BTreeMap<String, RegionSpec> {
        specs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    // ── TestFloatPlacement ──

    #[test]
    fn first_probe_is_the_midpoint() {
        let mut policy = region_policy(region(&[("a", (0.0, 100.0, false))]));
        assert_eq!(policy("a", &[]), Some(50.0));
    }

    #[test]
    fn second_probe_is_an_edge_farthest_from_mid() {
        let mut policy = region_policy(region(&[("a", (0.0, 100.0, false))]));
        let second = policy("a", &[50.0]).unwrap();
        assert!(second == 0.0 || second == 100.0);
    }

    #[test]
    fn floor_ends_the_dial() {
        // range 100 -> floor 25; the walk runs to quarter resolution
        // (>= floor passes), then every remaining candidate sits
        // closer than 25 to a measurement and the dial is done.
        let mut policy = region_policy(region(&[("a", (0.0, 100.0, false))]));
        let mut measured: Vec<f64> = Vec::new();
        while let Some(level) = policy("a", &measured) {
            measured.push(level);
        }
        let mut sorted = measured.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(sorted, vec![0.0, 25.0, 50.0, 75.0, 100.0]);
    }

    #[test]
    fn placement_is_stateless_across_rounds() {
        let mut policy = region_policy(region(&[("a", (0.0, 100.0, false))]));
        assert_eq!(policy("a", &[]), Some(50.0));
        assert_eq!(policy("a", &[]), Some(50.0)); // no hidden memory
    }

    // ── TestIntPlacement ──

    #[test]
    fn ints_enumerate_center_out() {
        let mut policy = region_policy(region(&[("n", (1.0, 5.0, true))]));
        assert_eq!(policy("n", &[]), Some(3.0)); // center
        let mut measured: Vec<f64> = Vec::new();
        while let Some(level) = policy("n", &measured) {
            measured.push(level);
        }
        let mut sorted = measured.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(sorted, vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn ints_ignore_the_float_floor() {
        let mut policy = region_policy(region(&[("n", (1.0, 9.0, true))]));
        let mut measured: Vec<f64> = Vec::new();
        while let Some(level) = policy("n", &measured) {
            measured.push(level);
        }
        assert_eq!(measured.len(), 9); // full enumeration, always
    }

    // ── wave 3: the survivor map's region_policy arithmetic ──
    // oracles hand-derived in the comments (the estate's oracle habit)

    #[test]
    fn int_center_is_half_the_sum_not_the_product() {
        // bounds 0..10, six already taken: center (0+10)/2 = 5 is free and
        // nearest; a product center (0*10)/2 = 0 crowns 0 instead
        let mut policy = region_policy(region(&[("n", (0.0, 10.0, true))]));
        assert_eq!(policy("n", &[6.0]), Some(5.0));
    }

    #[test]
    fn int_first_candidate_always_competes() {
        // bounds 0..9, nothing measured: center 4.5, nearest ints 4 and 5,
        // tie to the larger → 5. if the first candidate could never take
        // its seat (&& for ||), the crown stays with cands[0] = 0
        let mut policy = region_policy(region(&[("n", (0.0, 9.0, true))]));
        assert_eq!(policy("n", &[]), Some(5.0));
    }

    #[test]
    fn the_floor_spans_the_range_not_the_sum() {
        // bounds 20..40 with 30 measured: floor (40-20)/4 = 5 keeps
        // 20/25/35/40 alive (distances 10/5/5/10) and the far tie crowns
        // 40; a sum floor (40+20)/4 = 15 starves every candidate and
        // ends the dial one probe early
        let mut policy = region_policy(region(&[("a", (20.0, 40.0, false))]));
        assert_eq!(policy("a", &[30.0]), Some(40.0));
    }
}
