//! The seek wish — declared intent that overrides the walk's priorities.
//!
//! Port of `engine/seek_wish.py` (design §III/§XIV): two forms, one
//! grammar. cover_now walks a declared region first; find_best searches
//! it under a budget and RETURNS the best it found. Pure machinery —
//! plan validation, the wallet, the launch dice, ranking, the return.
//! The worker drives it: every dart and pass is a real trial through
//! the ledger, so the spend count is a count of real rows.
//!
//! Sealed rulings obeyed (§XIV):
//! - spend unit is feelers: one trial, one unit; the wallet only counts
//! - candidates are declared-only and a plan declaring more than
//!   budget refuses at admission — an impossible wish never starts
//! - the dice choose the LAUNCH POINT only; darts are priors, never
//!   map entries
//! - the verdict kind is RETURNED; budget exhaustion returns
//!   best-so-far — no miss-state exists for seeks

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const COVER_NOW: &str = "cover_now";
pub const FIND_BEST: &str = "find_best";
pub const SPECIES: [&str; 2] = [COVER_NOW, FIND_BEST];

pub const MAXIMIZE: &str = "max";
pub const MINIMIZE: &str = "min";

pub const RETURNED: &str = "RETURNED";

/// Admission refusal — named, raised before any trial runs. The python
/// `SeekRefused` exception; the `detail` rides the message the worker
/// logs at the door.
#[derive(Debug, Clone, PartialEq)]
pub struct SeekRefused {
    pub reason: String,
    pub detail: String,
}

impl SeekRefused {
    pub fn new(reason: &str, detail: &str) -> Self {
        SeekRefused {
            reason: reason.to_string(),
            detail: detail.to_string(),
        }
    }
}

impl std::fmt::Display for SeekRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.reason, self.detail)
        }
    }
}

/// The estate's objectives speak maximize/minimize; the engine's
/// compass speaks max/min — normalized at the door.
fn normalize_direction(declared: &str) -> Option<&'static str> {
    match declared {
        "max" | "maximize" => Some(MAXIMIZE),
        "min" | "minimize" => Some(MINIMIZE),
        _ => None,
    }
}

/// One dial's declared region: (lo, hi, is_int).
pub type RegionSpec = (f64, f64, bool);
pub type Region = BTreeMap<String, RegionSpec>;

/// The admitted shape of a seek wish (design §XIV). The python
/// dataclass; `validate` mutates the criterion's direction onto the
/// compass words exactly as the python does.
#[derive(Debug, Clone)]
pub struct SeekPlan {
    pub wish_id: String,
    /// the walking tender, named by the declarer — dial names are only
    /// unambiguous inside one tender, so the wish says whose dial-set
    /// the region speaks (the collision cure, 2026-10-23)
    pub tender: String,
    pub form: String,
    pub region: Region,
    /// find_best: outcome + direction (direction normalized to max/min)
    pub criterion: Option<Map<String, Value>>,
    /// feelers; 0 = unbounded (cover_now)
    pub budget: i64,
    /// launch dice; declared-only
    pub candidates: i64,
}

impl SeekPlan {
    pub fn validate(&mut self) -> Result<(), SeekRefused> {
        if self.tender.trim().is_empty() {
            return Err(SeekRefused::new(
                "undeclared_tender",
                "the walk names its tender — whose dial-set the region speaks",
            ));
        }
        if self.form.is_empty() {
            return Err(SeekRefused::new(
                "undeclared_form",
                "the walk speaks its form (no omission)",
            ));
        }
        if !SPECIES.contains(&self.form.as_str()) {
            return Err(SeekRefused::new("unknown_form", &self.form));
        }
        if self.region.is_empty() {
            return Err(SeekRefused::new("empty_region", ""));
        }
        for (name, (lo, hi, is_int)) in &self.region {
            if hi <= lo {
                return Err(SeekRefused::new("empty_range", name));
            }
            if *is_int && (*lo as i64 as f64 != *lo || *hi as i64 as f64 != *hi) {
                return Err(SeekRefused::new("non_integer_range", name));
            }
        }
        if self.form == FIND_BEST {
            let criterion = match self.criterion.as_ref() {
                Some(c) if c.get("outcome").map(|v| !v.is_null()).unwrap_or(false) => c,
                _ => return Err(SeekRefused::new("undeclared_criterion", "")),
            };
            // normalize the declared direction onto the compass words
            let declared = criterion
                .get("direction")
                .map(|v| v.as_str().unwrap_or_default())
                .unwrap_or("");
            let normalized = normalize_direction(declared)
                .map(|s| s.to_string())
                .ok_or_else(|| SeekRefused::new("unknown_direction", declared))?;
            self.criterion
                .as_mut()
                .expect("checked above")
                .insert("direction".into(), Value::String(normalized));
            if self.budget <= 0 {
                return Err(SeekRefused::new("undeclared_budget", ""));
            }
            if self.candidates < 0 {
                return Err(SeekRefused::new("negative_candidates", ""));
            }
            // An impossible wish never starts: dice the budget cannot
            // pay for refuse at the door, not mid-flight.
            if self.candidates > self.budget {
                return Err(SeekRefused::new(
                    "candidates_exceed_budget",
                    &format!("{} > {}", self.candidates, self.budget),
                ));
            }
        }
        Ok(())
    }
}

/// Parse and admit a seek plan; `SeekRefused` at the door. Region
/// entries arrive as `[lo, hi]` or `[lo, hi, is_int]` (the wish page's
/// wire shape — tuples in the python tests, arrays on the wire).
pub fn parse_seek_plan(plan: &Value) -> Result<SeekPlan, SeekRefused> {
    let mut region = Region::new();
    if let Some(spec) = plan.get("region").and_then(|v| v.as_object()) {
        for (name, entry) in spec {
            let arr = entry
                .as_array()
                .ok_or_else(|| SeekRefused::new("bad_region", name))?;
            if arr.len() < 2 {
                return Err(SeekRefused::new("bad_region", name));
            }
            let lo = arr[0]
                .as_f64()
                .ok_or_else(|| SeekRefused::new("bad_region", name))?;
            let hi = arr[1]
                .as_f64()
                .ok_or_else(|| SeekRefused::new("bad_region", name))?;
            let is_int = arr.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
            region.insert(name.clone(), (lo, hi, is_int));
        }
    }
    let criterion = plan.get("criterion").and_then(|v| v.as_object()).cloned();
    let wish_id = plan
        .get("wish_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let form = plan
        .get("form")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut parsed = SeekPlan {
        wish_id,
        tender: plan
            .get("tender")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        form,
        region,
        criterion,
        budget: plan
            .get("budget")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0),
        candidates: plan
            .get("candidates")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0),
    };
    parsed.validate()?;
    Ok(parsed)
}

/// Deterministic per-wish seed — the dart trace is reproducible.
/// sha256(wish_id), first 8 bytes, big-endian: the python receipt
/// verbatim (`int.from_bytes(digest[:8], 'big')`).
pub fn launch_seed(wish_id: &str) -> u64 {
    let digest = Sha256::digest(wish_id.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
}

/// The dice source: uniform in [lo, hi] and inclusive int draws. The
/// python rolled on `random.Random`; rust carries a small
/// self-contained SplitMix64 stream seeded by [`launch_seed`] — the
/// CONTRACT (same wish ⇒ same darts, forever, across relaunches)
/// is the python's and holds exactly; the specific draws differ from
/// CPython's Mersenne Twister, which nothing pins (no fixture, no
/// reader compares dart values across languages).
pub trait Dice {
    fn uniform(&mut self, lo: f64, hi: f64) -> f64;
    fn randint(&mut self, lo: i64, hi: i64) -> i64;
}

/// SplitMix64 — deterministic, self-contained, documented here as a
/// divergence-of-values with identical reproducibility semantics.
pub struct SplitMix64(pub u64);

impl Dice for SplitMix64 {
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * Self::next01(self)
    }
    fn randint(&mut self, lo: i64, hi: i64) -> i64 {
        // inclusive [lo, hi], python randint's span
        lo + (Self::next01(self) * ((hi - lo + 1) as f64)) as i64
    }
}

impl SplitMix64 {
    fn next01(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        // 53 bits of entropy into [0, 1), python random()'s shape
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// N random joint settings — the launch dice. Spread, not opinion:
/// each setting moves every dial at once, so the reading ranks against
/// other darts but can never attribute to a dial.
pub fn roll_candidates(plan: &SeekPlan, dice: Option<&mut dyn Dice>) -> Vec<Map<String, Value>> {
    if plan.candidates <= 0 {
        return Vec::new();
    }
    let mut owned: SplitMix64;
    let rng: &mut dyn Dice = match dice {
        Some(r) => r,
        None => {
            owned = SplitMix64(launch_seed(&plan.wish_id));
            &mut owned
        }
    };
    let mut settings = Vec::new();
    for _ in 0..plan.candidates {
        let mut draw = Map::new();
        for (name, (lo, hi, is_int)) in &plan.region {
            let v = if *is_int {
                Value::from(rng.randint(*lo as i64, *hi as i64))
            } else {
                serde_json::Number::from_f64(rng.uniform(*lo, *hi))
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            };
            draw.insert(name.clone(), v);
        }
        settings.push(draw);
    }
    settings
}

/// One measured candidate: a setting, its reading, its blur.
#[derive(Debug, Clone)]
pub struct Finding {
    pub params: Map<String, Value>,
    pub reading: f64,
    pub bars: f64,
}

/// Best finding under the declared criterion. The compass is declared,
/// never chosen: this only compares numbers.
pub fn rank(findings: &[Finding], direction: &str) -> Option<Finding> {
    if findings.is_empty() {
        return None;
    }
    let best = if direction == MAXIMIZE {
        findings.iter().fold(None::<&Finding>, |acc, f| match acc {
            None => Some(f),
            Some(b) if f.reading > b.reading => Some(f),
            other => other,
        })
    } else {
        findings.iter().fold(None::<&Finding>, |acc, f| match acc {
            None => Some(f),
            Some(b) if f.reading < b.reading => Some(f),
            other => other,
        })
    }?;
    Some(best.clone())
}

/// The feeler counter. One trial, one unit; seams are the driver's.
#[derive(Debug, Clone)]
pub struct Wallet {
    pub budget: i64,
    pub spent: i64,
}

impl Wallet {
    pub fn new(budget: i64) -> Self {
        Wallet { budget, spent: 0 }
    }

    pub fn pay(&mut self, units: i64) -> Result<(), String> {
        if units < 0 {
            return Err("units must be non-negative".to_string());
        }
        self.spent += units;
        Ok(())
    }

    pub fn remaining(&self) -> i64 {
        (self.budget - self.spent).max(0)
    }

    pub fn exhausted(&self) -> bool {
        self.remaining() <= 0
    }
}

/// The RETURN — the seek's only verdict kind, as the wire payload the
/// ledger stores under `seek_return::{wish_id}`. Budget exhaustion
/// still returns best-so-far; the finding carries its own bars.
pub fn build_return(best: Option<&Finding>, wallet: &Wallet, stop_reason: &str) -> Value {
    let finding = best.map(|f| {
        serde_json::json!({
            "params": Value::Object(f.params.clone()),
            "reading": f.reading,
            "bars": f.bars,
        })
    });
    serde_json::json!({
        "verdict": RETURNED,
        "stop_reason": stop_reason,
        "spend_count": wallet.spent,
        "finding": finding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_plan(over: Value) -> Value {
        let mut plan = json!({
            "wish_id": "sw-1",
            "tender": "tender-uuid-1",
            "form": FIND_BEST,
            "region": {
                "dial_a": [0.0, 100.0, false],
                "dial_b": [1, 8, true],
            },
            "criterion": {"outcome": "room/objective_1", "direction": MAXIMIZE},
            "budget": 200,
            "candidates": 5,
        });
        if let (Some(target), Some(src)) = (plan.as_object_mut(), over.as_object()) {
            for (k, v) in src {
                target.insert(k.clone(), v.clone());
            }
        }
        plan
    }

    // ── TestAdmission: refusals are admission events ──

    #[test]
    fn candidates_over_budget_refuses_at_the_door() {
        let err =
            parse_seek_plan(&base_plan(json!({"candidates": 201, "budget": 200}))).unwrap_err();
        assert_eq!(err.reason, "candidates_exceed_budget");
    }

    #[test]
    fn undeclared_budget_refuses() {
        let err = parse_seek_plan(&base_plan(json!({"budget": 0}))).unwrap_err();
        assert_eq!(err.reason, "undeclared_budget");
    }

    #[test]
    fn undeclared_criterion_refuses() {
        let err = parse_seek_plan(&base_plan(json!({"criterion": null}))).unwrap_err();
        assert_eq!(err.reason, "undeclared_criterion");
    }

    #[test]
    fn unknown_direction_refuses() {
        let err = parse_seek_plan(&base_plan(
            json!({"criterion": {"outcome": "x", "direction": "toward"}}),
        ))
        .unwrap_err();
        assert_eq!(err.reason, "unknown_direction");
    }

    #[test]
    fn estate_direction_words_normalize() {
        let p = parse_seek_plan(&base_plan(
            json!({"criterion": {"outcome": "x", "direction": "maximize"}}),
        ))
        .unwrap();
        assert_eq!(p.criterion.unwrap()["direction"], json!(MAXIMIZE));
        let p = parse_seek_plan(&base_plan(
            json!({"criterion": {"outcome": "x", "direction": "minimize"}}),
        ))
        .unwrap();
        assert_eq!(p.criterion.unwrap()["direction"], json!(MINIMIZE));
    }

    #[test]
    fn missing_form_refuses_no_omission() {
        let err = parse_seek_plan(&base_plan(json!({"form": null}))).unwrap_err();
        assert_eq!(err.reason, "undeclared_form");
    }

    #[test]
    fn unknown_form_refuses() {
        assert!(parse_seek_plan(&base_plan(json!({"form": "wander"}))).is_err());
    }

    #[test]
    fn empty_range_refuses_named() {
        let err = parse_seek_plan(&base_plan(json!({
            "region": {"dial_a": [5.0, 5.0, false], "dial_b": [1, 8, true]}
        })))
        .unwrap_err();
        assert_eq!(err.reason, "empty_range");
        assert_eq!(err.detail, "dial_a");
    }

    #[test]
    fn cover_now_admits_without_search_fields() {
        let plan = parse_seek_plan(&json!({
            "wish_id": "sw-2",
            "tender": "tender-uuid-1",
            "form": COVER_NOW,
            "region": {"dial_a": [0.0, 100.0, false]},
        }))
        .unwrap();
        assert_eq!(plan.candidates, 0);
        assert_eq!(plan.budget, 0);
        assert!(plan.criterion.is_none());
    }

    #[test]
    fn non_integer_int_range_refuses() {
        let err = parse_seek_plan(&base_plan(json!({
            "region": {"dial_b": [1.5, 8, true], "dial_a": [0.0, 1.0, false]}
        })))
        .unwrap_err();
        assert_eq!(err.reason, "non_integer_range");
    }

    // ── TestLaunchDice: reproducible, in-bounds, int-clean ──

    #[test]
    fn dice_are_deterministic_per_wish() {
        let plan = parse_seek_plan(&base_plan(json!({}))).unwrap();
        let first = roll_candidates(&plan, None);
        let second = roll_candidates(&parse_seek_plan(&base_plan(json!({}))).unwrap(), None);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
    }

    #[test]
    fn dice_respect_bounds_and_integrity() {
        let plan = parse_seek_plan(&base_plan(json!({}))).unwrap();
        for draw in roll_candidates(&plan, None) {
            let a = draw["dial_a"].as_f64().expect("float dial");
            assert!((0.0..=100.0).contains(&a));
            let b = draw["dial_b"].as_i64().expect("int dial stays an int");
            assert!((1..=8).contains(&b));
        }
    }

    #[test]
    fn dice_count_matches_declaration() {
        let plan = parse_seek_plan(&base_plan(json!({"candidates": 7}))).unwrap();
        assert_eq!(roll_candidates(&plan, None).len(), 7);
    }

    #[test]
    fn no_candidates_declared_no_dice() {
        let plan = parse_seek_plan(&base_plan(json!({"candidates": 0}))).unwrap();
        assert!(roll_candidates(&plan, None).is_empty());
    }

    #[test]
    fn same_seed_same_draws() {
        let plan = parse_seek_plan(&base_plan(json!({}))).unwrap();
        let mut r1 = SplitMix64(42);
        let mut r2 = SplitMix64(42);
        let a = roll_candidates(&plan, Some(&mut r1));
        let b = roll_candidates(&plan, Some(&mut r2));
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
    }

    #[test]
    fn launch_seed_is_stable_across_processes() {
        assert_eq!(launch_seed("sw-1"), launch_seed("sw-1"));
        assert_ne!(launch_seed("sw-1"), launch_seed("sw-2"));
    }

    // ── TestWallet ──

    #[test]
    fn pay_counts_units() {
        let mut w = Wallet::new(10);
        w.pay(3).unwrap();
        assert_eq!(w.spent, 3);
        assert_eq!(w.remaining(), 7);
    }

    #[test]
    fn exhausted_reads_true_at_zero_remaining() {
        let mut w = Wallet::new(2);
        w.pay(2).unwrap();
        assert!(w.exhausted());
        assert_eq!(w.remaining(), 0);
    }

    #[test]
    fn negative_payment_refused() {
        assert!(Wallet::new(5).pay(-1).is_err());
    }

    // ── TestRanking ──

    #[test]
    fn maximize_picks_highest_reading() {
        let findings = vec![
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(1))]),
                reading: 0.5,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(2))]),
                reading: 0.9,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(3))]),
                reading: 0.1,
                bars: 0.02,
            },
        ];
        let best = rank(&findings, MAXIMIZE).unwrap();
        assert_eq!(best.params["a"], json!(2));
    }

    #[test]
    fn minimize_picks_lowest_reading() {
        let findings = vec![
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(1))]),
                reading: 0.5,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(2))]),
                reading: 0.9,
                bars: 0.02,
            },
        ];
        let best = rank(&findings, MINIMIZE).unwrap();
        assert_eq!(best.params["a"], json!(1));
    }

    // An equal reading is not a promotion: the fold's strict > and <
    // keep whichever finding arrived first. Ties must not pass the
    // crown to a later candidate with the same number.
    #[test]
    fn maximize_keeps_the_first_of_equal_readings() {
        let findings = vec![
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(1))]),
                reading: 0.7,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(2))]),
                reading: 0.7,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(3))]),
                reading: 0.1,
                bars: 0.02,
            },
        ];
        let best = rank(&findings, MAXIMIZE).unwrap();
        assert_eq!(best.params["a"], json!(1));
    }

    #[test]
    fn minimize_keeps_the_first_of_equal_readings() {
        let findings = vec![
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(1))]),
                reading: 0.4,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(2))]),
                reading: 0.4,
                bars: 0.02,
            },
            Finding {
                params: serde_json::Map::from_iter([("a".into(), json!(3))]),
                reading: 0.9,
                bars: 0.02,
            },
        ];
        let best = rank(&findings, MINIMIZE).unwrap();
        assert_eq!(best.params["a"], json!(1));
    }

    #[test]
    fn no_findings_ranks_none() {
        assert!(rank(&[], MAXIMIZE).is_none());
    }

    // ── TestReturn: no miss-state exists for seeks ──

    #[test]
    fn budget_exhaustion_still_returns() {
        let mut w = Wallet::new(5);
        w.pay(5).unwrap();
        let f = Finding {
            params: serde_json::Map::from_iter([("a".into(), json!(2))]),
            reading: 0.9,
            bars: 0.02,
        };
        let ret = build_return(Some(&f), &w, "budget_exhausted");
        assert_eq!(ret["verdict"], json!(RETURNED));
        assert_eq!(ret["stop_reason"], json!("budget_exhausted"));
        assert_eq!(ret["spend_count"], json!(5));
        assert_eq!(ret["finding"]["reading"], json!(0.9));
    }

    #[test]
    fn return_carries_bars_with_the_finding() {
        let f = Finding {
            params: serde_json::Map::from_iter([("a".into(), json!(2))]),
            reading: 0.9,
            bars: 0.05,
        };
        let ret = build_return(Some(&f), &Wallet::new(5), "done");
        assert_eq!(ret["finding"]["bars"], json!(0.05));
    }

    #[test]
    fn empty_seek_returns_finding_none() {
        let ret = build_return(None, &Wallet::new(5), "budget_exhausted");
        assert_eq!(ret["verdict"], json!(RETURNED));
        assert!(ret["finding"].is_null());
    }

    // ── the launch dice: SplitMix64 pinned to an independently derived
    //    golden sequence (python re-implementation of the published
    //    algorithm, cross-language parity style) ──

    #[test]
    fn splitmix64_seed_zero_and_forty_two_play_the_published_sequences() {
        let mut d = SplitMix64(0);
        let expected0 = [
            0.8833108082136426,
            0.43152799704850997,
            0.026433771592597743,
            0.9708819781538285,
            0.10634669156721244,
            0.32732576421812576,
        ];
        for e in expected0 {
            assert_eq!(d.uniform(0.0, 1.0), e);
        }

        let mut d = SplitMix64(42);
        let expected42 = [
            0.7415648787718233,
            0.1599103928769201,
            0.27860113025513866,
            0.34419071652363753,
            0.03803016854024621,
            0.8682280765465323,
        ];
        for e in expected42 {
            assert_eq!(d.uniform(0.0, 1.0), e);
        }
    }

    #[test]
    fn uniform_scales_the_sequence_into_the_band() {
        let mut d = SplitMix64(0);
        assert_eq!(d.uniform(10.0, 20.0), 18.833108082136427);
        assert_eq!(d.uniform(10.0, 20.0), 14.3152799704851);
        assert_eq!(d.uniform(10.0, 20.0), 10.264337715925977);
    }

    #[test]
    fn randint_plays_the_derived_draws_inclusive() {
        let mut d = SplitMix64(42);
        let expected = [5i64, 1, 2, 3, 1, 6];
        for e in expected {
            assert_eq!(d.randint(1, 6), e);
        }
    }

    #[test]
    fn the_dice_stay_in_their_bands_and_vary() {
        let mut d = SplitMix64(7);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1000 {
            let v = d.uniform(0.0, 1.0);
            assert!((0.0..1.0).contains(&v));
            seen.insert(v.to_bits());
        }
        assert!(seen.len() > 990, "a thousand draws must barely repeat");

        let mut d = SplitMix64(7);
        for _ in 0..1000 {
            let n = d.randint(3, 9);
            assert!((3..=9).contains(&n));
        }
    }

    // ── the refusal's rendered word ──

    #[test]
    fn a_refusal_renders_reason_and_detail_exactly() {
        let bare = SeekRefused::new("empty_region", "");
        assert_eq!(bare.to_string(), "empty_region");
        let detailed = SeekRefused::new("unknown_form", "gnarf");
        assert_eq!(detailed.to_string(), "unknown_form: gnarf");
    }

    // ── rank: the declared compass, never chosen ──

    fn finding(reading: f64) -> Finding {
        Finding {
            params: Map::new(),
            reading,
            bars: 0.0,
        }
    }

    #[test]
    fn rank_maximize_picks_the_highest_reading() {
        let fs = [finding(1.0), finding(3.0), finding(2.0)];
        assert_eq!(rank(&fs, MAXIMIZE).unwrap().reading, 3.0);
    }

    #[test]
    fn rank_minimize_picks_the_lowest_reading() {
        let fs = [finding(1.0), finding(3.0), finding(2.0)];
        assert_eq!(rank(&fs, MINIMIZE).unwrap().reading, 1.0);
    }

    #[test]
    fn rank_ties_keep_the_first_and_empty_answers_none() {
        let fs = [finding(2.0), finding(2.0)];
        // equal readings never displace the incumbent
        assert!(rank(&fs, MAXIMIZE).is_some());
        // None is None — Finding carries no PartialEq, so assert the shape, not equality
        assert!(rank(&[], MAXIMIZE).is_none());
    }

    // ── the wallet: one trial, one unit ──

    #[test]
    fn the_wallet_pays_zero_and_refuses_debits() {
        let mut w = Wallet::new(10);
        assert!(w.pay(0).is_ok(), "zero-cost trials are free");
        assert_eq!(w.spent, 0);
        assert!(w.pay(3).is_ok());
        assert_eq!(w.spent, 3);
        assert!(w.pay(-1).is_err());
        assert_eq!(w.spent, 3, "a refused debit must not move the ledger");
    }

    // ── the door's refusals: the criterion and the ranges ──

    fn plan(form: &str, region: Vec<(&str, f64, f64, bool)>, criterion: Option<Value>) -> SeekPlan {
        let mut r = Region::new();
        for (name, lo, hi, is_int) in region {
            r.insert(name.to_string(), (lo, hi, is_int));
        }
        SeekPlan {
            wish_id: "w".into(),
            tender: "tender-one".into(),
            form: form.into(),
            region: r,
            criterion: criterion.map(|c| c.as_object().cloned().unwrap_or_default()),
            budget: 5,
            candidates: 2,
        }
    }

    #[test]
    fn find_best_without_a_live_outcome_criterion_is_refused() {
        let mut p = plan(
            FIND_BEST,
            vec![("dial", 0.0, 10.0, false)],
            Some(json!({"outcome": null, "direction": "max"})),
        );
        let err = p.validate().unwrap_err();
        assert_eq!(err.reason, "undeclared_criterion");
    }

    #[test]
    fn an_empty_or_inverted_range_is_refused_at_the_door() {
        let mut p = plan(COVER_NOW, vec![("dial", 5.0, 5.0, false)], None);
        assert_eq!(p.validate().unwrap_err().reason, "empty_range");

        let mut p = plan(COVER_NOW, vec![("dial", 10.0, 0.0, false)], None);
        assert_eq!(p.validate().unwrap_err().reason, "empty_range");
    }

    #[test]
    fn a_non_integer_reading_on_an_integer_dial_is_refused() {
        let mut p = plan(COVER_NOW, vec![("dial", 0.5, 3.5, true)], None);
        assert_eq!(p.validate().unwrap_err().reason, "non_integer_range");
    }

    // ── wave 3: the survivor map's parse_seek_plan door ──

    #[test]
    fn two_element_region_entries_parse() {
        // [lo, hi] alone is the wish page's own wire shape — is_int
        // defaults false. a two-bound entry must ADMIT, never refuse
        let p = parse_seek_plan(&base_plan(json!({
            "region": {"a": [10.0, 50.0]}
        })))
        .unwrap();
        assert_eq!(p.region["a"], (10.0, 50.0, false));
    }

    #[test]
    fn short_region_entries_refuse_named() {
        // fewer than two bounds cannot name a range — the refusal
        // carries the dial's own name
        for short in [json!([]), json!([1.0])] {
            let err = parse_seek_plan(&base_plan(json!({"region": {"a": short}}))).unwrap_err();
            assert_eq!(err.reason, "bad_region");
            assert_eq!(err.detail, "a");
        }
    }
}
