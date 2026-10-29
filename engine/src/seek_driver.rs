//! The seek driver — one question per trial, the rung's whole loop.
//!
//! Port of `engine/seek_driver.py`. The worker's trial loop asks this
//! driver what the NEXT trial does; the driver answers, the worker
//! effectuates and measures, then hands the reading back. Every dart
//! and every pass level is a real ledger trial through the ordinary
//! rails — stop flag, wish pulse, classification — and the spend count
//! is a count of real rows.
//!
//! The loop (design §XIV):
//! - DARTS: one joint random setting per trial until the declared
//!   candidates are spent. Joint settings are unattributable — they
//!   rank but cannot teach.
//! - PASSES: per dial, the injected policy names the next level; the
//!   trial pushes that level over the current baseline of every other
//!   dial. When the policy says the dial is done, its best measured
//!   level BECOMES the baseline (stepping) and the next dial walks
//!   from there. Rounds repeat until the budget seam or a round moves
//!   nothing (converged).
//! - SEAMS: the wallet is checked between trials only — a measurement
//!   is never aborted halfway. Budget exhaustion returns best-so-far.
//! - cover_now is the same walk with the search fields empty.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use serde_json::{Map, Value};

use crate::seek::{
    build_return, rank, roll_candidates, Finding, SeekPlan, Wallet, COVER_NOW, FIND_BEST, MAXIMIZE,
};
use crate::seek_policy::Policy;

pub const DARTS: &str = "darts";
pub const PASSES: &str = "passes";
pub const DONE: &str = "done";

pub const STOP_BUDGET: &str = "budget_exhausted";
pub const STOP_CONVERGED: &str = "converged";
pub const STOP_REGION: &str = "region_walked";
pub const STOP_STOPPED: &str = "stopped";

/// The contract violations the python raised as RuntimeError — the
/// driver's own discipline, kept loud.
#[derive(Debug, Clone, PartialEq)]
pub enum DriverError {
    ObserveOwed,
    ObserveWithoutStep,
    ResultBeforeCompletion,
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::ObserveOwed => write!(f, "observe() owed before the next step()"),
            DriverError::ObserveWithoutStep => write!(f, "observe() without an issued step"),
            DriverError::ResultBeforeCompletion => {
                write!(f, "result() before the seek completed")
            }
        }
    }
}

/// One issued commission. The python dict shape; `dart` was a key
/// present only on dart commissions (`'dart' not in third` in the
/// tests) — here it is a bool, false exactly when the python key was
/// absent.
#[derive(Debug, Clone)]
pub struct Commission {
    /// DARTS | PASSES
    pub phase: &'static str,
    /// 'push' (a dart) | 'commission' (a pass)
    pub mode: &'static str,
    /// the dart's joint setting (darts only)
    pub params: Option<Map<String, Value>>,
    /// true only on dart commissions
    pub dart: bool,
    /// the commissioned dial (passes only)
    pub probe_dial: Option<String>,
    /// the commissioned level (passes only)
    pub probe_level: Option<f64>,
    /// the stepped baseline the pass rides over (passes only)
    pub baseline: Option<Map<String, Value>>,
}

/// Per-trial state machine. step() issues a commission, observe()
/// closes it; the WORKER pays the wallet once per trial (every trial
/// the tender runs while the seek holds it is the seek's spend —
/// darts and protocol trials alike, one payer, no double count).
pub struct SeekDriver {
    plan: SeekPlan,
    policy: Policy,
    wallet: Wallet,
    direction: String,

    darts: VecDeque<Map<String, Value>>,
    dart_findings: Vec<Finding>,
    pass_findings: Vec<Finding>,

    /// dial -> stepped level
    baseline: BTreeMap<String, Value>,
    /// dial -> levels asked
    measured: BTreeMap<String, Vec<f64>>,
    /// dial -> (level, reading, bars)
    dial_readings: BTreeMap<String, Vec<(f64, f64, f64)>>,
    dials: Vec<String>,
    dial_idx: usize,
    /// the issued commission
    pending: Option<Commission>,
    phase: &'static str,
    rounds_done: i64,
    stop_reason: Option<String>,
    last_baseline: Option<BTreeMap<String, Value>>,
}

impl SeekDriver {
    pub fn new(plan: SeekPlan, mut policy: Policy) -> Self {
        let wallet = Wallet::new(plan.budget);
        let direction = plan
            .criterion
            .as_ref()
            .and_then(|c| c.get("direction"))
            .and_then(|v| v.as_str())
            .unwrap_or(MAXIMIZE)
            .to_string();
        let darts: VecDeque<Map<String, Value>> =
            roll_candidates(&plan, None).into_iter().collect();
        let dials: Vec<String> = plan.region.keys().cloned().collect();
        let phase = if darts.is_empty() { PASSES } else { DARTS };
        // the policy is owned, not called during construction
        let _ = &mut policy;
        SeekDriver {
            plan,
            policy,
            wallet,
            direction,
            darts,
            dart_findings: Vec::new(),
            pass_findings: Vec::new(),
            baseline: BTreeMap::new(),
            measured: BTreeMap::new(),
            dial_readings: BTreeMap::new(),
            dials,
            dial_idx: 0,
            pending: None,
            phase,
            rounds_done: 0,
            stop_reason: None,
            last_baseline: None,
        }
    }

    /// A step was issued and its observe is still owed — the worker
    /// must not ask for the next step until the commissioned probe has
    /// actually been measured.
    pub fn has_open_commission(&self) -> bool {
        self.pending.is_some()
    }

    pub fn wallet_exhausted(&self) -> bool {
        self.wallet.exhausted()
    }

    /// The worker pays here, once per trial — the single payer.
    pub fn wallet_pay(&mut self) {
        let _ = self.wallet.pay(1);
    }

    /// Guard (rung 7): read-only live view for the status payload —
    /// the wallet's numbers.
    pub fn wallet_status(&self) -> Value {
        serde_json::json!({
            "budget": self.wallet.budget,
            "spent": self.wallet.spent,
            "remaining": self.wallet.remaining(),
        })
    }

    /// The next commission, or None when the seek is complete. A DART
    /// is a direct joint push (the worker executes it itself —
    /// unattributable by design, never protocol). A PASS is a
    /// COMMISSION: the worker hands (dial, level) to the coordinator's
    /// probe override and lets the standing protocol execute it.
    pub fn step(&mut self) -> Result<Option<Commission>, DriverError> {
        if self.phase == DONE {
            return Ok(None);
        }
        if self.pending.is_some() {
            return Err(DriverError::ObserveOwed);
        }

        if self.phase == DARTS {
            if self.darts.is_empty() {
                self.phase = PASSES; // the seam after the dart phase
            } else if self.wallet.exhausted() {
                self.finish(STOP_BUDGET);
                return Ok(None);
            } else {
                let params = self.darts.pop_front().expect("checked non-empty");
                self.pending = Some(Commission {
                    phase: DARTS,
                    mode: "push",
                    params: Some(params),
                    dart: true,
                    probe_dial: None,
                    probe_level: None,
                    baseline: None,
                });
                return Ok(self.pending.clone());
            }
        }

        while self.phase == PASSES {
            if self.dial_idx >= self.dials.len() && self.end_of_round() {
                return Ok(None);
            }
            let dial = self.dials[self.dial_idx].clone();
            if self.plan.form == FIND_BEST && self.wallet.exhausted() {
                self.finish(STOP_BUDGET);
                return Ok(None);
            }
            let measured = self.measured.get(&dial).cloned().unwrap_or_default();
            let level = (self.policy)(&dial, &measured);
            let Some(level) = level else {
                // The dial prices out: step the baseline to its best
                // measured level and move to the next dial.
                self.step_baseline(&dial);
                self.dial_idx += 1;
                continue;
            };
            self.measured.entry(dial.clone()).or_default().push(level);
            self.pending = Some(Commission {
                phase: PASSES,
                mode: "commission",
                params: None,
                dart: false,
                probe_dial: Some(dial),
                probe_level: Some(level),
                baseline: Some(self.baseline.clone().into_iter().collect()),
            });
            return Ok(self.pending.clone());
        }
        Ok(None)
    }

    /// Hand the measured criterion reading for the open commission
    /// back: a dart ranked (joint params, unattributable), or a pass
    /// level measured (the commissioned dial at its level, over the
    /// stepped baseline).
    pub fn observe(&mut self, reading: f64, bars: f64) -> Result<(), DriverError> {
        let Some(pending) = self.pending.take() else {
            return Err(DriverError::ObserveWithoutStep);
        };
        if pending.phase == DARTS {
            self.dart_findings.push(Finding {
                params: pending.params.expect("dart carries params"),
                reading,
                bars,
            });
        } else {
            let dial = pending.probe_dial.clone().expect("pass carries a dial");
            let mut params: Map<String, Value> = pending.baseline.clone().unwrap_or_default();
            params.insert(
                dial.clone(),
                serde_json::Number::from_f64(pending.probe_level.expect("pass carries a level"))
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
            self.pass_findings.push(Finding {
                params,
                reading,
                bars,
            });
            self.dial_readings.entry(dial).or_default().push((
                pending.probe_level.expect("pass carries a level"),
                reading,
                bars,
            ));
        }
        Ok(())
    }

    /// The operator's stop between trials: come home with what's held.
    pub fn abort(&mut self) {
        self.finish(STOP_STOPPED);
    }

    pub fn abort_with_reason(&mut self, reason: &str) {
        self.finish(reason);
    }

    /// The declared compass's outcome name; None for cover_now.
    pub fn criterion_outcome(&self) -> Option<String> {
        if self.plan.form == COVER_NOW || self.plan.criterion.is_none() {
            return None;
        }
        self.plan
            .criterion
            .as_ref()
            .and_then(|c| c.get("outcome"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// The RETURN — the seek's only verdict kind (§XIV).
    pub fn result(&self) -> Result<Value, DriverError> {
        if self.phase != DONE {
            return Err(DriverError::ResultBeforeCompletion);
        }
        let best = if self.plan.form == FIND_BEST {
            let mut all = self.dart_findings.clone();
            all.extend(self.pass_findings.clone());
            rank(&all, &self.direction)
        } else {
            None
        };
        let stop = self
            .stop_reason
            .clone()
            .unwrap_or_else(|| STOP_CONVERGED.to_string());
        Ok(build_return(best.as_ref(), &self.wallet, &stop))
    }

    // ─── internals ──────────────────────────────────────────────────

    /// The dial's best measured level under the declared compass.
    fn best_level(&self, dial: &str) -> Option<f64> {
        let readings = self.dial_readings.get(dial)?;
        let pick = |better: f64| -> Option<(f64, f64)> {
            readings
                .iter()
                .fold(None, |acc: Option<(f64, f64)>, t| match acc {
                    None => Some((t.0, t.1)),
                    Some(b) if better == 1.0 && t.1 > b.1 => Some((t.0, t.1)),
                    Some(b) if better == -1.0 && t.1 < b.1 => Some((t.0, t.1)),
                    other => other,
                })
        };
        let best = if self.direction == MAXIMIZE {
            pick(1.0)
        } else {
            pick(-1.0)
        }?;
        Some(best.0)
    }

    fn step_baseline(&mut self, dial: &str) {
        if self.plan.form == COVER_NOW {
            return; // no compass declared — nothing to step by
        }
        if let Some(level) = self.best_level(dial) {
            self.baseline.insert(
                dial.to_string(),
                serde_json::Number::from_f64(level)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            );
        }
    }

    /// Round seam: pay nothing, decide whether another round runs.
    fn end_of_round(&mut self) -> bool {
        if self.plan.form == COVER_NOW {
            self.finish(STOP_REGION);
            return true;
        }
        if let Some(last) = &self.last_baseline {
            if &self.baseline == last {
                self.finish(STOP_CONVERGED); // a round moved nothing
                return true;
            }
        }
        if self.wallet.exhausted() {
            self.finish(STOP_BUDGET);
            return true;
        }
        self.last_baseline = Some(self.baseline.clone());
        self.rounds_done += 1;
        self.dial_idx = 0;
        self.measured = self.dials.iter().map(|d| (d.clone(), Vec::new())).collect();
        false
    }

    fn finish(&mut self, reason: &str) {
        self.stop_reason = Some(reason.to_string());
        self.phase = DONE;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seek::parse_seek_plan;
    use serde_json::json;

    fn find_plan(budget: i64, candidates: i64) -> SeekPlan {
        parse_seek_plan(&json!({
            "wish_id": "sw-t",
            "tender": "tender-uuid-t",
            "form": FIND_BEST,
            "region": {"a": [0.0, 10.0, false], "b": [0.0, 10.0, false]},
            "criterion": {"outcome": "room/obj", "direction": MAXIMIZE},
            "budget": budget,
            "candidates": candidates,
        }))
        .unwrap()
    }

    /// Fake walk machinery: named levels per dial, then None (done).
    fn queue_policy(queues: &[(&str, &[f64])]) -> Policy {
        let mut state: BTreeMap<String, VecDeque<f64>> = queues
            .iter()
            .map(|(d, q)| (d.to_string(), q.iter().copied().collect()))
            .collect();
        Box::new(move |dial: &str, _measured: &[f64]| {
            if let Some(q) = state.get_mut(dial) {
                if !q.is_empty() {
                    return q.pop_front();
                }
            }
            None
        })
    }

    // ── TestDarts ──

    #[test]
    fn darts_are_joint_pushes_one_per_trial() {
        let mut driver = SeekDriver::new(
            find_plan(10, 2),
            queue_policy(&[("a", &[5.0]), ("b", &[1.0])]),
        );
        let first = driver.step().unwrap().unwrap();
        assert_eq!(first.phase, DARTS);
        assert!(first.dart);
        let params = first.params.unwrap();
        assert!(params.contains_key("a") && params.contains_key("b"));
        driver.observe(0.4, 0.0).unwrap();
        let second = driver.step().unwrap().unwrap();
        assert!(second.dart);
        driver.observe(0.9, 0.0).unwrap();
        // dart phase over: the next step is a pass probe
        let third = driver.step().unwrap().unwrap();
        assert_eq!(third.phase, PASSES);
        assert!(!third.dart);
    }

    #[test]
    fn darts_rank_into_the_return_when_budget_ends() {
        let mut driver = SeekDriver::new(find_plan(2, 2), queue_policy(&[("a", &[]), ("b", &[])]));
        driver.step().unwrap();
        driver.wallet_pay();
        driver.observe(0.4, 0.0).unwrap();
        driver.step().unwrap();
        driver.wallet_pay();
        driver.observe(0.9, 0.0).unwrap();
        assert!(driver.step().unwrap().is_none()); // wallet empty at the seam
        let result = driver.result().unwrap();
        assert_eq!(result["verdict"], json!("RETURNED"));
        assert_eq!(result["stop_reason"], json!(STOP_BUDGET));
        assert_eq!(result["spend_count"], json!(2));
        assert_eq!(result["finding"]["reading"], json!(0.9));
    }

    // ── TestPasses ──

    #[test]
    fn priced_out_dial_steps_the_baseline() {
        let mut driver = SeekDriver::new(
            find_plan(50, 0),
            queue_policy(&[("a", &[5.0, 7.0]), ("b", &[1.0])]),
        );
        driver.step().unwrap();
        driver.observe(0.5, 0.0).unwrap(); // a=5.0
        driver.step().unwrap();
        driver.observe(0.8, 0.0).unwrap(); // a=7.0 <- best for a
                                           // a prices out INSIDE the next step: the same step commissions
                                           // b's first probe, with the stepped baseline riding along.
        let b_probe = driver.step().unwrap().unwrap();
        assert_eq!(b_probe.probe_dial.as_deref(), Some("b"));
        let baseline = b_probe.baseline.unwrap();
        assert_eq!(baseline["a"], json!(7.0)); // stepped
    }

    #[test]
    fn minimize_direction_picks_the_lowest() {
        let plan = parse_seek_plan(&json!({
            "wish_id": "sw-t", "tender": "tender-uuid-t", "form": FIND_BEST,
            "region": {"a": [0.0, 10.0, false], "b": [0.0, 10.0, false]},
            "criterion": {"outcome": "r", "direction": "min"},
            "budget": 100, "candidates": 0,
        }))
        .unwrap();
        let mut driver = SeekDriver::new(plan, queue_policy(&[("a", &[5.0, 7.0]), ("b", &[1.0])]));
        driver.step().unwrap();
        driver.observe(0.5, 0.0).unwrap(); // a=5.0
        driver.step().unwrap();
        driver.observe(0.2, 0.0).unwrap(); // a=7.0 lower
                                           // a prices out inside this step; it returns b's probe directly
        let b = driver.step().unwrap().unwrap();
        assert_eq!(b.probe_dial.as_deref(), Some("b"));
        driver.observe(0.4, 0.0).unwrap();
        // b done; round 2 asks nothing new (queues empty) -> converged
        assert!(driver.step().unwrap().is_none());
        let result = driver.result().unwrap();
        assert_eq!(result["finding"]["reading"], json!(0.2));
    }

    #[test]
    fn wallet_exhaustion_between_trials_only() {
        let mut driver = SeekDriver::new(
            find_plan(3, 0),
            queue_policy(&[("a", &[1.0, 2.0, 3.0]), ("b", &[9.0])]),
        );
        for reading in [0.1, 0.2, 0.3] {
            driver.step().unwrap();
            driver.wallet_pay();
            driver.observe(reading, 0.0).unwrap();
        }
        assert!(driver.step().unwrap().is_none()); // refused at the seam
        let result = driver.result().unwrap();
        assert_eq!(result["stop_reason"], json!(STOP_BUDGET));
        assert_eq!(result["spend_count"], json!(3));
    }

    // ── TestRounds ──

    #[test]
    fn round_that_moves_nothing_converges() {
        // Both dials offer one level each round; best never changes ->
        // round two's baseline equals round one's -> converged.
        let mut asked = BTreeMap::<String, usize>::new();
        let policy: Policy = Box::new(move |dial: &str, measured: &[f64]| {
            if measured.is_empty() {
                *asked.entry(dial.to_string()).or_insert(0) += 1;
                return Some(4.0);
            }
            None
        });
        let mut driver = SeekDriver::new(find_plan(100, 0), policy);
        for _ in 0..4 {
            driver.step().unwrap();
            driver.observe(0.5, 0.0).unwrap();
        }
        assert!(driver.step().unwrap().is_none());
        assert_eq!(
            driver.result().unwrap()["stop_reason"],
            json!(STOP_CONVERGED)
        );
    }

    // ── TestCoverNow ──

    #[test]
    fn cover_now_walks_once_and_returns_no_finding() {
        let plan = parse_seek_plan(&json!({
            "wish_id": "sw-c", "tender": "tender-uuid-c", "form": COVER_NOW,
            "region": {"a": [0.0, 4.0, false], "b": [0.0, 4.0, false]},
        }))
        .unwrap();
        let mut driver = SeekDriver::new(plan, queue_policy(&[("a", &[2.0]), ("b", &[3.0])]));
        let mut steps = 0;
        for reading in [0.5, 0.6] {
            let Some(_) = driver.step().unwrap() else {
                break;
            };
            driver.observe(reading, 0.0).unwrap();
            steps += 1;
        }
        while driver.step().unwrap().is_some() {
            // drain any zero-measurement remainder (python's `pass`
            // loop — in this scenario the round seam ends the walk)
        }
        assert_eq!(steps, 2);
        let result = driver.result().unwrap();
        assert_eq!(result["verdict"], json!("RETURNED"));
        assert_eq!(result["stop_reason"], json!(STOP_REGION));
        assert!(result["finding"].is_null());
        assert_eq!(result["spend_count"], json!(0)); // cover_now carries no wallet
    }

    #[test]
    fn cover_now_never_steps_a_baseline() {
        let plan = parse_seek_plan(&json!({
            "wish_id": "sw-c", "tender": "tender-uuid-c", "form": COVER_NOW,
            "region": {"a": [0.0, 4.0, false], "b": [0.0, 4.0, false]},
        }))
        .unwrap();
        let mut driver = SeekDriver::new(plan, queue_policy(&[("a", &[2.0]), ("b", &[3.0])]));
        driver.step().unwrap();
        driver.observe(0.9, 0.0).unwrap();
        let b = driver.step().unwrap().unwrap();
        let baseline = b.baseline.unwrap_or_default();
        assert!(!baseline.contains_key("a")); // no compass -> no stepping
    }

    // ── TestAbortAndContract ──

    #[test]
    fn abort_comes_home_returned() {
        let mut driver = SeekDriver::new(
            find_plan(100, 1),
            queue_policy(&[("a", &[1.0]), ("b", &[1.0])]),
        );
        driver.step().unwrap();
        driver.observe(0.7, 0.0).unwrap();
        driver.abort();
        assert!(driver.step().unwrap().is_none());
        let result = driver.result().unwrap();
        assert_eq!(result["verdict"], json!("RETURNED"));
        assert_eq!(result["stop_reason"], json!(STOP_STOPPED));
        assert_eq!(result["finding"]["reading"], json!(0.7));
    }

    #[test]
    fn step_demands_its_observe() {
        let mut driver =
            SeekDriver::new(find_plan(100, 2), queue_policy(&[("a", &[]), ("b", &[])]));
        driver.step().unwrap();
        assert_eq!(driver.step().unwrap_err(), DriverError::ObserveOwed);
    }

    #[test]
    fn observe_without_a_step_refused() {
        let mut driver =
            SeekDriver::new(find_plan(100, 0), queue_policy(&[("a", &[]), ("b", &[])]));
        assert_eq!(
            driver.observe(0.5, 0.0).unwrap_err(),
            DriverError::ObserveWithoutStep
        );
    }

    #[test]
    fn result_before_completion_refused() {
        let driver = SeekDriver::new(find_plan(100, 0), queue_policy(&[("a", &[]), ("b", &[])]));
        assert_eq!(
            driver.result().unwrap_err(),
            DriverError::ResultBeforeCompletion
        );
    }
}
