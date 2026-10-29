//! The probe coordinator's walk/characterization half — the port of
//! `probe_coordinator.py` lines ~640-1689 against the room machinery
//! landed in [`crate::coordinator`].
//!
//! Error discipline, exactly the python's:
//! - a DARK notebook view ([`WalkError::Unavailable`]) parks one
//!   trial and retries at the next boundary — dying killed whole
//!   tenders (run 35224443449), guessing is the one sin the walk
//!   never commits;
//! - a LOUD failure (HTTP error status, malformed page) kills the
//!   invocation — the worker fails the trial and the next invocation
//!   retries, exactly like the python's uncaught exceptions;
//! - room ops stay forgiving (swallowed into warnings on their own
//!   side, the landed room contract).
//!
//! Called once per trial via [`ProbeCoordinator::decide_trial`];
//! returns what mode the systemtender operates in for that trial.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Map, Value};

use crate::coordinator::{ProbeCoordinator, MAX_HOLD_TRIALS};
use crate::coverage_walk::{round9, Bounds, CoverageWalk, ParamWalkStatus};
use crate::ledger::{Connect, DbError};
use crate::walk_policy::{HttpError, HttpTransport, WalkError, WalkPolicy};

/// Consult memo lifetime (seconds): char_complete and demand
/// publication poll per trial; the notebook deserves one GET per this
/// window, not one per trial. Boundaries always re-ask.
pub const AMBIENCE_DEBT_TTL: f64 = 60.0;
/// One consult's fence batch: walk this many window levels, then
/// re-consult — the window may have priced wider by then, and a
/// stale batch must never outlive its own fence.
pub const AMBIENCE_WALK_BATCH: usize = 8;

// ── walk-half types ──────────────────────────────────────────────────

/// The walk state machine — python's string states, as an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordState {
    Idle,
    ProbePush,
    ProbePause,
    Done,
    Cooldown,
    Hold,
}

impl CoordState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CoordState::Idle => "idle",
            CoordState::ProbePush => "probe_push",
            CoordState::ProbePause => "probe_pause",
            CoordState::Done => "done",
            CoordState::Cooldown => "cooldown",
            CoordState::Hold => "hold",
        }
    }
}

/// One probe the walk produced: the param, its index, the level, and
/// the push config (one param at the walked level, rest at neutral).
#[derive(Debug, Clone)]
pub struct Probe {
    pub param_name: String,
    pub param_idx: i64,
    pub level: f64,
    pub config: Map<String, Value>,
    pub ambience_anchor: Option<f64>,
    pub ambience_epoch: Option<i64>,
}

/// A pinned remeasure (the wish's precise remeasure; the ambience
/// re-walk rides it too): one param at the given levels, epoch-stamped.
#[derive(Debug, Clone)]
pub struct ProbeOverride {
    pub param_name: String,
    pub levels: Vec<f64>,
    pub idx: usize,
    pub ambience_anchor: Option<f64>,
    pub ambience_epoch: Option<i64>,
}

/// name → {lower, upper, is_int, idx} (the python `_param_bounds`).
#[derive(Debug, Clone, Copy)]
pub struct ParamBound {
    pub lower: f64,
    pub upper: f64,
    pub is_int: bool,
    pub idx: i64,
}

/// One settings-tree param with constraints (what the worker's
/// `_collect_upper_bounds` returns per entry).
#[derive(Debug, Clone)]
pub struct UpperBound {
    pub name: String,
    pub lower: f64,
    pub upper: f64,
    pub is_int: bool,
}

/// The coordinator's per-trial decision (the python's dict, as a
/// struct; `params: None` means the worker fails the trial).
#[derive(Debug, Clone)]
pub struct Decision {
    /// "hold" | "impulse"
    pub mode: &'static str,
    pub params: Option<Map<String, Value>>,
    /// "probe_push" | "probe_pause" | None
    pub impulse_phase: Option<&'static str>,
    pub probe_param: Option<String>,
    pub probe_param_idx: Option<i64>,
    pub probe_level: Option<f64>,
    /// receivers only — their HOLD trials carry the sender's phase
    pub lease_phase: Option<String>,
    pub detection_trial: bool,
    /// true when the probe banks as ambience at a resting spot
    pub ambience_probe: bool,
}

impl Decision {
    fn park(params: Option<Map<String, Value>>) -> Self {
        Decision {
            mode: "hold",
            params,
            impulse_phase: None,
            probe_param: None,
            probe_param_idx: None,
            probe_level: None,
            lease_phase: None,
            detection_trial: false,
            ambience_probe: false,
        }
    }
}

/// decide_trial's failure surface: a loud walk failure kills the
/// invocation (worker fails the trial); a DB failure at first-try
/// table setup propagates too (the python's untried `_ensure_tables`).
#[derive(Debug)]
pub enum CoordError {
    Walk(WalkError),
    Db(DbError),
}

impl From<WalkError> for CoordError {
    fn from(e: WalkError) -> Self {
        CoordError::Walk(e)
    }
}

impl From<DbError> for CoordError {
    fn from(e: DbError) -> Self {
        CoordError::Db(e)
    }
}

impl std::fmt::Display for CoordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoordError::Walk(e) => write!(f, "{e}"),
            CoordError::Db(e) => write!(f, "{e}"),
        }
    }
}

/// The worker's injected callbacks — the python constructor's
/// `collect_upper_bounds_fn` / `compute_neutral_params_fn`, plus the
/// rust replacements for the two injection points the python tests
/// hacked through config dicts (walk transport callable) and module
/// paths (nothing here): one injection point, explicit.
pub struct CoordHooks {
    /// the worker's settings-tree walker — REQUIRED, the walk's bounds
    pub collect_upper_bounds: Arc<dyn Fn(&Value) -> Vec<UpperBound> + Send + Sync>,
    /// the strain-format neutral-params callback (optional)
    pub compute_neutral_params: Option<Arc<dyn Fn() -> Option<Map<String, Value>> + Send + Sync>>,
    /// replaces the python tests' config-injected `walk_transport`
    pub walk_transport: Option<Arc<dyn HttpTransport>>,
    /// the coordinator's causal client (characterize + ambience-duty);
    /// defaults to [`ReqwestTransport`]
    pub http: Option<Arc<dyn HttpTransport>>,
}

impl CoordHooks {
    pub fn new(collect_upper_bounds: Arc<dyn Fn(&Value) -> Vec<UpperBound> + Send + Sync>) -> Self {
        CoordHooks {
            collect_upper_bounds,
            compute_neutral_params: None,
            walk_transport: None,
            http: None,
        }
    }
}

/// The active walk object: the notebook policy (production) or the
/// legacy ladder (explicit offline opt-in). The python duck-typed one
/// `_char_walk` slot over both.
pub enum CharWalk {
    Ladder(CoverageWalk),
    Notebook(WalkPolicy),
}

impl CharWalk {
    pub fn next_probe(
        &mut self,
        skip: &HashSet<String>,
    ) -> Result<Option<(String, f64)>, WalkError> {
        match self {
            CharWalk::Ladder(w) => Ok(w.next_probe(skip)),
            CharWalk::Notebook(p) => p.next_probe(skip),
        }
    }

    /// python parity: for the notebook this ADVANCES the round-robin
    /// (`can_probe = next_probe(...) is not None`).
    pub fn can_probe(&mut self, skip: &HashSet<String>) -> Result<bool, WalkError> {
        match self {
            CharWalk::Ladder(w) => Ok(w.can_probe(skip)),
            CharWalk::Notebook(p) => p.can_probe(skip),
        }
    }

    pub fn refine(&mut self) -> Result<(), WalkError> {
        match self {
            CharWalk::Ladder(w) => {
                w.refine();
                Ok(())
            }
            CharWalk::Notebook(p) => p.refine(None),
        }
    }

    pub fn status(&self) -> std::collections::BTreeMap<String, ParamWalkStatus> {
        match self {
            CharWalk::Ladder(w) => w.status(),
            CharWalk::Notebook(p) => p.status(),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn num(level: f64) -> Value {
    serde_json::Number::from_f64(level)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// one param at `level`, rest at neutral — list-valued params get the
/// level at every element, exactly like the python.
fn probe_config(neutral: &Map<String, Value>, param_name: &str, level: f64) -> Map<String, Value> {
    let mut config = neutral.clone();
    let replaced = match config.get(param_name) {
        Some(Value::Array(a)) => Value::Array(vec![num(level); a.len()]),
        _ => num(level),
    };
    config.insert(param_name.to_string(), replaced);
    config
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// python-style display of a JSON scalar for log lines (str/float of
/// whatever causal sent).
fn display(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_string(),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else {
                n.as_f64().map(|f| f.to_string()).unwrap_or_default()
            }
        }
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

impl<C: Connect> ProbeCoordinator<C> {
    // ─── Params ──────────────────────────────────────────────────────

    pub(crate) fn get_neutral_params(&self) -> Option<Map<String, Value>> {
        if let Some(cached) = self.neutral_params.borrow().as_ref() {
            return Some(cached.clone());
        }
        let det_cfg = self.det_cfg();
        if let Some(hold) = det_cfg.get("hold_params").and_then(|v| v.as_object()) {
            *self.hold_params_from_config.borrow_mut() = true;
            log::info!("Using config-specified hold params");
            *self.neutral_params.borrow_mut() = Some(hold.clone());
            return Some(hold.clone());
        }
        if let Some(compute) = self.hooks.compute_neutral_params.as_ref() {
            if let Some(params) = compute() {
                log::info!("Computed neutral hold params via callback");
                *self.neutral_params.borrow_mut() = Some(params.clone());
                return Some(params);
            }
        }
        let settings = self
            .config
            .get("settings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let upper_bounds = (self.hooks.collect_upper_bounds)(&settings);
        if upper_bounds.is_empty() {
            return None;
        }
        let mut params = Map::new();
        for ub in &upper_bounds {
            let midpoint = (ub.lower + ub.upper) / 2.0;
            let v = if ub.is_int {
                Value::from(midpoint as i64)
            } else {
                num(midpoint)
            };
            params.insert(ub.name.clone(), v);
        }
        if !params.is_empty() {
            log::info!("Computed neutral hold params from midpoints (fallback)");
            *self.neutral_params.borrow_mut() = Some(params.clone());
        }
        Some(params)
    }

    // ─── Adaptive Timeout ────────────────────────────────────────────

    /// Called by the worker after each trial. Tracks system rhythm.
    pub fn record_trial_duration(&self, duration_secs: f64) {
        let mut h = self.trial_duration_history.borrow_mut();
        h.push(duration_secs);
        if h.len() > 10 {
            h.remove(0);
        }
    }

    /// Deskewed timeout for causal calls.
    ///
    /// Budget = 2 × median trial duration ("waste at most 2 trials").
    /// Tightened = 10 × median observed causal RTT (if we know it's
    /// fast). Effective = min(budget, tightened), floored at 2s.
    pub(crate) fn causal_timeout(&self) -> f64 {
        let floor = 2.0f64;
        let mut budget = floor;
        {
            let trials = self.trial_duration_history.borrow();
            if !trials.is_empty() {
                budget = (floor).max(median(&trials) * 2.0);
            }
        }
        let rtts = self.causal_rtt_history.borrow();
        if !rtts.is_empty() {
            let tightened = floor.max(median(&rtts) * 10.0);
            return budget.min(tightened);
        }
        // No RTT history yet — use budget as-is (generous on first call)
        budget
    }

    // ─── Causal Client ───────────────────────────────────────────────

    /// Call causal's real-time endpoint after a probe round completes.
    /// Causal computes shift (CFAR-aware), updates its ResponseCurve,
    /// returns {shift, delta, converged}. The coordinator is the dumb
    /// instrument — causal owns all curves. Falls back to None on
    /// timeout or error: the walk continues to the next probe blind.
    fn query_causal_probe_result(&self, probe: &Probe) -> Option<Value> {
        let (Some(push_start), Some(pause_end)) = (
            *self.round_push_start.borrow(),
            *self.round_pause_end.borrow(),
        ) else {
            return None;
        };
        let mut payload = json!({
            "group_id": self.group_id,
            "sender_id": self.systemtender_id,
            "probe_param": probe.param_name,
            "probe_level": num(probe.level),
            // python datetime.now().isoformat(): naive, microseconds
            "push_start": push_start.naive_utc().format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
            "pause_end": pause_end.naive_utc().format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
            "convergence_threshold": num(self.convergence_threshold),
        });
        if let Some(b) = self.param_bounds.borrow().get(&probe.param_name) {
            payload["param_range"] = num(b.upper - b.lower);
        }
        if let Some(anchor) = probe.ambience_anchor {
            // The ambience bank: causal keys the resting spot from its
            // own standing-params read, pinned at this anchor — the
            // walk's own declaration of where it lives.
            payload["ambience_anchor"] = num(anchor);
            if let Some(epoch) = probe.ambience_epoch {
                payload["ambience_epoch"] = Value::from(epoch);
            }
        }
        let timeout = self.causal_timeout();
        let url = format!("{}/characterize", self.causal_url);
        let t0 = std::time::Instant::now();
        match self.http.call(
            "POST",
            &url,
            Some(&payload),
            Duration::from_secs_f64(timeout),
        ) {
            Ok(result) => {
                let rtt = t0.elapsed().as_secs_f64();
                {
                    let mut h = self.causal_rtt_history.borrow_mut();
                    h.push(rtt);
                    if h.len() > 10 {
                        h.remove(0);
                    }
                }
                log::debug!(
                    "CAUSAL probe_result: param={} level={} rtt={}ms shift={} delta={} converged={}",
                    probe.param_name,
                    probe.level,
                    rtt * 1000.0,
                    display(result.get("shift")),
                    display(result.get("delta")),
                    display(result.get("converged"))
                );
                Some(result)
            }
            Err(e) => {
                let rtt = t0.elapsed().as_secs_f64();
                log::info!(
                    "CAUSAL probe_result FAILED after {rtt:.1}s (timeout was {timeout:.1}s): {e:?} — continuing blind"
                );
                None
            }
        }
    }

    // ─── Receiver Observation Sharing ────────────────────────────────

    /// Called by the worker after each HOLD trial completes: writes
    /// the receiver's objective values to the shared table so the
    /// sender can compute coupling shift after each probe round.
    pub fn record_receiver_observation(
        &self,
        trial_num: i64,
        objective_values: &Map<String, Value>,
        lease_phase: Option<&str>,
    ) {
        let obj = Value::Object(objective_values.clone());
        // trial_num is an INT column (psycopg2 sent python ints); a
        // trial number past 2^31 fails the insert loudly, never silently
        let trial_num: i32 = trial_num as i32;
        if let Err(e) = self.exec(|txn| {
            txn.execute(
                "INSERT INTO receiver_observations \
                 (group_id, receiver_id, trial_num, objective_values, lease_phase) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                    &trial_num,
                    &obj,
                    &lease_phase,
                ],
            )
            .map(|_| ())
            .map_err(DbError::from)
        }) {
            log::info!("Failed to record receiver observation: {e}");
        }
    }

    // ─── Probe result processing ─────────────────────────────────────

    /// Called when a probe round (push + pause) completes. Calls
    /// causal to compute shift/convergence, updates the retirement
    /// set. None when causal is unreachable (the walk continues
    /// blind; the trial is told as FAIL to the char study).
    pub(crate) fn process_probe_result(&self, probe: &Probe) -> Option<Value> {
        let result = self.query_causal_probe_result(probe)?;
        let converged = result
            .get("converged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let param_name = probe.param_name.as_str();

        // The un-converge listener: a probe that disagreed with the
        // curve beyond bars sets causal's drift flag — the param's
        // retirement is void, it re-enters the walk. Converged ground
        // is never re-probed by the coverage walk, so this flag is the
        // only thing that ever revisits it.
        if result
            .get("drift")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            if self.converged_params.borrow().contains(param_name) {
                log::info!(
                    "UN-CONVERGE: {param_name} — probe disagreed with the curve beyond bars (drift flag); back into the walk"
                );
            }
            self.converged_params.borrow_mut().remove(param_name);
        }

        // Two-key retirement: stability AND shape. Converged alone can
        // retire with a fat unresolved bracket (run 4's threshold edge
        // stopped at (50,62] by luck). A param retires only when
        // converged AND every unresolved gap is priced out — its
        // remaining ignorance (jump × width / range) is below the
        // local measurement bar, so one more probe cannot see anything.
        let gaps = result
            .get("gaps")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // Per-gap bar: each bracket is priced against its OWN endpoint
        // fuzz (bars_sum), not the primary listener's — a quiet
        // listener's gap must not look cheap because a loud one is
        // noisy. The union now includes the sender's self-curve.
        let blocking = gaps
            .iter()
            .filter(|g| {
                g.get("unresolved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    && g.get("ignorance")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(f64::INFINITY)
                        > g.get("bars_sum").and_then(|v| v.as_f64()).unwrap_or(0.0)
            })
            .count();
        if converged && blocking == 0 {
            if !self.converged_params.borrow().contains(param_name) {
                let unresolved = gaps
                    .iter()
                    .filter(|g| {
                        g.get("unresolved")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .count();
                log::info!("CONVERGED: {param_name} (unresolved gaps priced out: {unresolved})");
            }
            self.converged_params
                .borrow_mut()
                .insert(param_name.to_string());
        } else if converged {
            log::info!(
                "CONVERGED but {blocking} gap(s) still priced above the bar — {param_name} stays in rotation (walk bisects the gaps)"
            );
        }

        Some(result)
    }

    // ─── Characterization Study ─────────────────────────────────────

    /// Build the deterministic coverage walk over all params.
    pub(crate) fn init_characterization(&self) -> Result<(), CoordError> {
        if self.get_neutral_params().is_none() {
            return Ok(());
        }
        let settings = self
            .config
            .get("settings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let upper_bounds = (self.hooks.collect_upper_bounds)(&settings);
        if upper_bounds.is_empty() {
            return Ok(());
        }
        let mut param_names = Vec::new();
        let mut param_bounds = std::collections::BTreeMap::new();
        let mut walk_bounds = Bounds::new();
        for (param_idx, ub) in upper_bounds.iter().enumerate() {
            let lower = ub.lower;
            let upper = ub.upper;
            let is_int = ub.is_int;
            param_bounds.insert(
                ub.name.clone(),
                ParamBound {
                    lower,
                    upper,
                    is_int,
                    idx: param_idx as i64,
                },
            );
            param_names.push(ub.name.clone());
            walk_bounds.insert(ub.name.clone(), (lower, upper, is_int));
        }
        *self.param_names.borrow_mut() = param_names.clone();
        *self.param_bounds.borrow_mut() = param_bounds;

        // Notebook walk (causal IS the notebook: stateless policy over
        // the banked curves, compass steering, self-curve in the
        // verdict). There is NO blind fallback: an unreachable
        // notebook raises — a silent rigid ladder would freeze thin
        // maps and hide the cause. The invocation dies loudly and the
        // next one retries; resume is exact, so a loud failure costs
        // one invocation, not the data. 'ladder' exists only as an
        // explicit legacy opt-in (offline tests).
        let det_cfg = self.det_cfg();
        let transport_configured = det_cfg
            .get("walk_transport")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let ladder = det_cfg
            .get("walk_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("notebook")
            == "ladder";
        if transport_configured {
            let transport = self.hooks.walk_transport.clone().ok_or_else(|| {
                WalkError::Loud(
                    "walk_transport configured but no transport injected (hooks.walk_transport)"
                        .into(),
                )
            })?;
            *self.char_walk.borrow_mut() = Some(CharWalk::Notebook(WalkPolicy::new(
                &self.causal_url,
                &self.group_id,
                &self.systemtender_id,
                self.refinement_depth,
                &walk_bounds,
                transport,
            )));
            log::info!("CHAR INIT: notebook walk (injected transport)");
        } else if ladder {
            *self.char_walk.borrow_mut() = Some(CharWalk::Ladder(CoverageWalk::new(&walk_bounds)));
            log::info!("CHAR INIT: legacy ladder (explicitly configured)");
        } else {
            let policy = WalkPolicy::new(
                &self.causal_url,
                &self.group_id,
                &self.systemtender_id,
                self.refinement_depth,
                &walk_bounds,
                self.http.clone(),
            );
            if !param_names.is_empty() {
                policy.view(&param_names[0])?; // notebook must answer
            }
            *self.char_walk.borrow_mut() = Some(CharWalk::Notebook(policy));
            log::info!("CHAR INIT: notebook walk (causal reachable)");
        }

        let floors = self
            .char_walk
            .borrow()
            .as_ref()
            .map(|w| {
                let st = w.status();
                param_names
                    .iter()
                    .filter_map(|n| st.get(n).map(|s| format!("{n}={:.1}", s.step)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        log::info!(
            "CHAR INIT: {} params, floors: {}, threshold={}",
            param_names.len(),
            floors,
            self.convergence_threshold
        );
        Ok(())
    }

    /// Pin the next push blocks to one param at the given levels —
    /// the wish's precise remeasure, and the instrument the ambience
    /// re-walk rides: when `ambience_anchor` is present the probe
    /// banks as ambience entries at that resting spot, epoch-stamped.
    /// The lease, push/pause windows and characterize calls stay this
    /// coordinator's own machinery; only the WHERE of the probe is
    /// pinned. Cleared when the levels are exhausted.
    pub fn set_probe_override(
        &self,
        param_name: &str,
        levels: &[f64],
        ambience_anchor: Option<f64>,
        ambience_epoch: Option<i64>,
    ) {
        *self.probe_override.borrow_mut() = Some(ProbeOverride {
            param_name: param_name.to_string(),
            levels: levels.to_vec(),
            idx: 0,
            ambience_anchor,
            ambience_epoch,
        });
    }

    pub(crate) fn override_next_probe(&self) -> Option<Probe> {
        // stage 1 — consume from the override (idx check, unknown-param
        // guard, idx advance), python statement order
        let (param_name, level, bounds, anchor, epoch) = {
            let mut ov_cell = self.probe_override.borrow_mut();
            let Some(ov) = ov_cell.as_mut() else {
                return None;
            };
            if ov.idx >= ov.levels.len() {
                drop(ov_cell);
                *self.probe_override.borrow_mut() = None;
                return None;
            }
            let level = ov.levels[ov.idx];
            let param_name = ov.param_name.clone();
            let bounds = self.param_bounds.borrow().get(&param_name).copied();
            let Some(bounds) = bounds else {
                // causal named a dial this strain does not carry (stale
                // plan, renamed param): drop the override, the walk
                // resumes — never crash the loop on a stale hint.
                log::debug!("Probe override param unknown to this strain: {param_name} — clearing");
                drop(ov_cell);
                *self.probe_override.borrow_mut() = None;
                return None;
            };
            ov.idx += 1;
            (
                param_name,
                level,
                bounds,
                ov.ambience_anchor,
                ov.ambience_epoch,
            )
        };
        // stage 2 — build the probe at neutral base
        let neutral = self.get_neutral_params()?;
        let config = probe_config(&neutral, &param_name, level);
        let (anchor, epoch) = if anchor.is_some() {
            (anchor, Some(epoch.unwrap_or(0)))
        } else {
            (None, None)
        };
        Some(Probe {
            param_name,
            param_idx: bounds.idx,
            level,
            config,
            ambience_anchor: anchor,
            ambience_epoch: epoch,
        })
    }

    /// Advance the walk: next param + level to push. None when the
    /// walk is finished (all params converged, or best-effort after
    /// refinement depth is exhausted).
    pub(crate) fn ask_next_probe(&self) -> Result<Option<Probe>, WalkError> {
        let converged = self.converged_params.borrow().clone();
        let next = {
            let mut walk = self.char_walk.borrow_mut();
            match walk.as_mut() {
                None => return Ok(None),
                Some(w) => w.next_probe(&converged)?,
            }
        };
        let (param_name, level) = match next {
            None => return Ok(None),
            Some(nl) => nl,
        };
        let bounds = *self
            .param_bounds
            .borrow()
            .get(&param_name)
            .expect("walk returned a param outside its bounds");
        let neutral = self
            .get_neutral_params()
            .expect("init guarantees neutral params once the walk exists");
        let config = probe_config(&neutral, &param_name, level);
        Ok(Some(Probe {
            param_name,
            param_idx: bounds.idx,
            level,
            config,
            ambience_anchor: None,
            ambience_epoch: None,
        }))
    }

    // ── Slice duty (rung 5: standing duty's ambience entries) ─────────

    /// The map's named ambience debt for THIS tender.
    ///
    /// Returns the debt list (possibly empty), or None when the
    /// consult went dark — None is never cached and never walked on:
    /// the park-and-retry discipline (a dark view is not an answer,
    /// and guessing is the one sin the walk never commits).
    pub(crate) fn ambience_debt(&self, force: bool) -> Option<Vec<Value>> {
        let now = std::time::Instant::now();
        if !force {
            if let Some((at, cache)) = self.ambience_debt_cache.borrow().as_ref() {
                if now.duration_since(*at).as_secs_f64() < AMBIENCE_DEBT_TTL {
                    return Some(cache.clone());
                }
            }
        }
        let url = format!(
            "{}/ambience-duty/{}?group={}",
            self.causal_url, self.systemtender_id, self.group_id
        );
        match self.http.call(
            "GET",
            &url,
            None,
            Duration::from_secs_f64(self.causal_timeout()),
        ) {
            Ok(body) => {
                let debt = body
                    .get("debt")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                *self.ambience_debt_cache.borrow_mut() = Some((now, debt.clone()));
                Some(debt)
            }
            Err(HttpError::Status(code)) if (400..500).contains(&(code as i64)) => {
                // The notebook ANSWERED: this causal predates local
                // ground (rolling upgrade) — no debt can exist. Parking
                // a tender on an answered page would be the dark-view
                // sin inverted.
                log::info!("AMBIENCE DUTY: notebook has no duty page ({code}) — no debt");
                *self.ambience_debt_cache.borrow_mut() = Some((now, Vec::new()));
                Some(Vec::new())
            }
            Err(e) => {
                log::info!("AMBIENCE DUTY: consult dark ({e:?}) — parking the question");
                None
            }
        }
    }

    /// The bounded half of ambience debt: entries that do not exist
    /// yet. This alone extends walk-pending and holds the quiet-bench
    /// lever — the tender stays while an ambience entry is missing,
    /// and leaves once it exists. Window aging deliberately does NOT
    /// gate here (the aging-vs-lifetime edge is the rung-5 ruling).
    pub(crate) fn ambience_unmeasured_pending(&self) -> Result<bool, WalkError> {
        let Some(debt) = self.ambience_debt(false) else {
            return Err(WalkError::Unavailable("ambience-duty consult dark".into()));
        };
        Ok(debt.iter().any(|d| {
            d.get("reason").and_then(|v| v.as_str()) == Some("unmeasured_operating_point")
        }))
    }

    /// The DONE-boundary consult: the map names its ambience debt,
    /// standing duty walks it before DONE may stand.
    ///
    /// Placement stays here — the dumb stateless legs: midpoint of
    /// the fattest unresolved in-window gap, unbanked levels only,
    /// fenced to the param's own bounds, one batch per consult. The
    /// walk itself is the ordinary override ride: same push/pause,
    /// same settle-and-read, one dial moving in a window around its
    /// held value. Ok(None) when the map names no debt this boundary
    /// (or the named dials are unknown to this strain).
    pub(crate) fn ambience_duty_probe(&self) -> Result<Option<Probe>, WalkError> {
        let Some(debt) = self.ambience_debt(true) else {
            return Err(WalkError::Unavailable("ambience-duty consult dark".into()));
        };
        let entry = debt.iter().find(|e| {
            e.get("param")
                .and_then(|v| v.as_str())
                .map(|p| self.param_bounds.borrow().contains_key(p))
                .unwrap_or(false)
        });
        let entry = match entry {
            None => return Ok(None),
            Some(e) => e,
        };
        let param = entry
            .get("param")
            .and_then(|v| v.as_str())
            .expect("matched entries carry a string param")
            .to_string();
        let bounds = *self.param_bounds.borrow().get(&param).expect("matched");
        let (lo_bound, hi_bound) = (bounds.lower, bounds.upper);

        let measured: HashSet<u64> = entry
            .get("levels")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_f64())
                    .map(|l| round9(l).to_bits())
                    .collect()
            })
            .unwrap_or_default();
        let mut gaps: Vec<(f64, f64, f64)> = Vec::new(); // (ignorance, from, to)
        for g in entry
            .get("gaps")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            if !g
                .get("unresolved")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            if g.get("ignorance")
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::INFINITY)
                <= g.get("bars_sum").and_then(|v| v.as_f64()).unwrap_or(0.0)
            {
                continue;
            }
            let from = g
                .get("from_level")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| WalkError::Loud("debt gap missing from_level".into()))?;
            let to = g
                .get("to_level")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| WalkError::Loud("debt gap missing to_level".into()))?;
            gaps.push((
                g.get("ignorance").and_then(|v| v.as_f64()).unwrap_or(0.0),
                from,
                to,
            ));
        }
        // sort: fattest ignorance first, then widest bracket (python
        // key defaults ignorance to 0.0 here — stable sort)
        gaps.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    (b.2 - b.1)
                        .partial_cmp(&(a.2 - a.1))
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        let mut levels: Vec<f64> = Vec::new();
        for (_, from, to) in gaps {
            let mid = (from + to) / 2.0;
            let mid = mid.clamp(lo_bound, hi_bound);
            if measured.contains(&round9(mid).to_bits()) {
                continue;
            }
            if levels.iter().any(|l| (mid - l).abs() < 1e-9) {
                continue;
            }
            levels.push(mid);
            if levels.len() >= AMBIENCE_WALK_BATCH {
                break;
            }
        }
        if levels.is_empty() {
            // The window is walked to priced-out: no debt this
            // boundary answers for. The map will re-name it if it
            // ages, or widen it if the edges price for it.
            return Ok(None);
        }
        let anchor = entry
            .get("anchor")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| WalkError::Loud("debt entry missing anchor".into()))?;
        // python: `entry.get('epoch') or 0` — missing and 0 both mean 0
        let epoch = entry.get("epoch").and_then(|v| v.as_i64()).unwrap_or(0);
        self.set_probe_override(&param, &levels, Some(anchor), Some(epoch));
        log::info!(
            "AMBIENCE DUTY: param={param} anchor={anchor} window={} reason={} levels={} epoch={epoch}",
            display(entry.get("window")),
            display(entry.get("reason")),
            levels.len(),
        );
        Ok(self.override_next_probe())
    }

    // ─── Char study bookkeeping ─────────────────────────────────────

    /// Record a completed probe: log the information score, check
    /// coverage exhaustion. Convergence is NOT decided here — causal's
    /// converged flag drives that (in `process_probe_result`).
    pub(crate) fn tell_char_study(
        &self,
        param_name: &str,
        result: Option<&Value>,
    ) -> Result<(), CoordError> {
        let Some(result) = result else {
            log::info!("CHAR TELL: {param_name} FAIL (causal unavailable)");
            return Ok(());
        };
        let z = result.get("z").and_then(|v| v.as_f64());
        let delta = result.get("delta").and_then(|v| v.as_f64());
        if let Some(z) = z {
            log::info!(
                "CHAR TELL: {param_name} z={z:.3} (delta={} drift={} bar={})",
                display(result.get("delta")),
                display(result.get("drift")),
                display(result.get("shift_bar"))
            );
        } else if let Some(delta) = delta {
            // Older causal: delta with the INFINITY catch
            // (f64::MAX/2 ≈ 8.99e+307 → 1.0).
            let mut value = delta;
            if value > 1e10 {
                log::info!(
                    "CHAR TELL: catching INFINITY delta={value:.2e} → replacing with 1.0 (first point, no prior)"
                );
                value = 1.0;
            }
            log::info!("CHAR TELL: {param_name} delta={value:.6}");
        } else {
            log::info!("CHAR TELL: {param_name} FAIL (no z, no delta)");
            return Ok(());
        }

        // Per-receiver detail when causal provides the receivers map
        // (multi-receiver curves). Top-level fields are already the
        // all-receiver aggregates — retirement consults those; these
        // lines are the per-listener paper trail.
        if let Some(receivers) = result.get("receivers").and_then(|v| v.as_object()) {
            if !receivers.is_empty() {
                let primary = result.get("primary_receiver").and_then(|v| v.as_str());
                // python iterates sorted(receivers.items())
                let mut order: Vec<&String> = receivers.keys().collect();
                order.sort();
                for recv in order {
                    let rr = &receivers[recv];
                    let entry = |k: &str| rr.get(k);
                    let shift = match entry("shift") {
                        None => 0.0,
                        Some(v) => match v.as_f64() {
                            Some(f) => f,
                            None => {
                                log::info!("CHAR TELL: {recv} (unparseable entry)");
                                continue;
                            }
                        },
                    };
                    let zv = match entry("z") {
                        None => 0.0,
                        Some(v) => match v.as_f64() {
                            Some(f) => f,
                            None => {
                                log::info!("CHAR TELL: {recv} (unparseable entry)");
                                continue;
                            }
                        },
                    };
                    let marker = if Some(recv.as_str()) == primary {
                        "*"
                    } else {
                        " "
                    };
                    log::info!(
                        "CHAR TELL:{marker} {recv} ch={} shift={shift:+.4} z={zv:.3} conv={} gaps={}",
                        display(entry("primary_channel")),
                        display(entry("converged")),
                        display(entry("unresolved_gaps"))
                    );
                }
            }
        }
        // Coverage check: can the walk still advance?
        let converged = self.converged_params.borrow().clone();
        let param_names = self.param_names.borrow().clone();
        if converged.len() < param_names.len() {
            let st = self.char_walk.borrow().as_ref().map(|w| w.status());
            let progress = st
                .as_ref()
                .map(|st| {
                    param_names
                        .iter()
                        .filter_map(|n| {
                            st.get(n).map(|s| {
                                format!("{n}={}/{}", s.levels_measured, display_opt(s.levels_total))
                            })
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            log::info!(
                "CHAR PROGRESS: {progress} levels, {}/{} params converged",
                converged.len(),
                param_names.len()
            );
            let can = {
                let mut walk = self.char_walk.borrow_mut();
                match walk.as_mut() {
                    None => false,
                    Some(w) => w.can_probe(&converged)?,
                }
            };
            if !can {
                log::info!("CHAR EXHAUSTED: walk complete at current floors");
                self.refine_study()?;
            }
        }
        Ok(())
    }

    /// True when the characterization walk has nothing left to probe.
    /// Every param either converged or the refinement depth is spent
    /// (best effort accepted). Quiet-bench lever: the worker polls
    /// this and lets itself out when the walks are done — finished
    /// walkers leave, the room goes still.
    pub fn char_complete(&self) -> Result<bool, CoordError> {
        // A pending probe override is un-walked work — the quiet-bench
        // lever must not fire under a wish's remeasure (miss-as-gavel:
        // the miss re-opens the walk; found live 2026-10-22).
        if self.probe_override.borrow().is_some() {
            return Ok(false);
        }
        let converged = self.converged_params.borrow().clone();
        let walk_done = {
            let mut walk = self.char_walk.borrow_mut();
            match walk.as_mut() {
                None => return Ok(false),
                Some(w) => match w.can_probe(&converged) {
                    Ok(can) => !can,
                    // A slow notebook must not kill the tender (run
                    // 35224443449: one >2s view read failed the whole
                    // worker job). Not complete — the loop lives and
                    // asks again next trial.
                    Err(WalkError::Unavailable(e)) => {
                        log::info!("CHAR COMPLETE: view unavailable ({e}) — retrying next trial");
                        return Ok(false);
                    }
                    Err(loud) => return Err(loud.into()),
                },
            }
        };
        if !walk_done {
            return Ok(false);
        }
        // Local ground outstanding: the tender stays (rung 5's bounded
        // debt — the quiet-bench lever waits for patches).
        match self.ambience_unmeasured_pending() {
            Ok(pending) => Ok(!pending),
            Err(WalkError::Unavailable(e)) => {
                log::info!("CHAR COMPLETE: view unavailable ({e}) — retrying next trial");
                Ok(false)
            }
            Err(loud) => Err(loud.into()),
        }
    }

    /// Halve the walk's resolution floors. Already-measured levels
    /// stay measured (the visited set persists); the next pass
    /// measures the midpoints the finer floor exposes. Curves persist
    /// in causal's CurveRegistry throughout.
    pub(crate) fn refine_study(&self) -> Result<(), CoordError> {
        if *self.refinement_level.borrow() >= self.refinement_depth {
            log::info!(
                "REFINEMENT: exhausted (depth={}) — accepting best effort",
                self.refinement_depth
            );
            let mut converged = self.converged_params.borrow_mut();
            converged.extend(self.param_names.borrow().iter().cloned());
            return Ok(());
        }
        // python builds old_floors as status()[n]['step'] per param —
        // a notebook status that skipped a dark param would KeyError
        // there (latent python crash); this port keeps the walk alive
        // and logs the floors it knows. Deviation noted, direction
        // preserved (refine already happened either way).
        let param_names = self.param_names.borrow().clone();
        let old_floors: Vec<(String, f64)> = {
            let walk = self.char_walk.borrow();
            match walk.as_ref() {
                None => Vec::new(),
                Some(w) => {
                    let st = w.status();
                    param_names
                        .iter()
                        .filter_map(|n| st.get(n).map(|s| (n.clone(), s.step)))
                        .collect()
                }
            }
        };
        {
            let mut walk = self.char_walk.borrow_mut();
            if let Some(w) = walk.as_mut() {
                w.refine()?;
            }
        }
        *self.refinement_level.borrow_mut() += 1;
        let new_floors: Vec<String> = {
            let walk = self.char_walk.borrow();
            match walk.as_ref() {
                None => Vec::new(),
                Some(w) => {
                    let st = w.status();
                    old_floors
                        .iter()
                        .filter_map(|(n, old)| {
                            st.get(n).map(|s| format!("{n}={old:.2}→{:.2}", s.step))
                        })
                        .collect()
                }
            }
        };
        log::debug!(
            "REFINEMENT: pass {} floors: {}",
            self.refinement_level.borrow(),
            new_floors.join(", ")
        );
        Ok(())
    }

    // ─── Main State Machine ──────────────────────────────────────────

    pub fn decide_trial(&self, trial_number: i64) -> Result<Decision, CoordError> {
        if !*self.initialized.borrow() {
            self.ensure_tables()?;
            self.cleanup_stale_state();
            *self.initialized.borrow_mut() = true;
        }
        let state = *self.state.borrow();
        log::debug!(
            "COORD trial={trial_number} state={} systemtender={}",
            state.as_str(),
            self.systemtender_id.chars().take(8).collect::<String>()
        );

        // Heartbeat for sender states
        if matches!(state, CoordState::ProbePush | CoordState::ProbePause) && !self.heartbeat() {
            log::info!("Lost sender lease — returning to IDLE");
            *self.state.borrow_mut() = CoordState::Idle;
            return Ok(self.idle_result());
        }

        match state {
            CoordState::Idle => self.handle_idle(trial_number),
            CoordState::ProbePush => self.handle_probe_push(trial_number),
            CoordState::ProbePause => self.handle_probe_pause(trial_number),
            CoordState::Done => self.handle_done(trial_number),
            CoordState::Cooldown => self.handle_cooldown(trial_number),
            CoordState::Hold => self.handle_hold(trial_number),
            // the enum makes unknown states unreachable; the python's
            // "Unknown state — resetting to IDLE" arm dies with it
        }
    }

    // ── IDLE ─────────────────────────────────────────────────────────

    fn handle_idle(&self, trial_number: i64) -> Result<Decision, CoordError> {
        *self.idle_count.borrow_mut() += 1;

        // Listening from trial 1: an active sender outranks everything.
        // A systemtender that exists can hold — there is no warmup
        // before receiving.
        if self.has_active_sender() {
            *self.state.borrow_mut() = CoordState::Hold;
            *self.hold_count.borrow_mut() = 0;
            return self.handle_hold(trial_number);
        }
        if self.count_active_systemtenders() < 2 {
            return Ok(self.idle_result());
        }

        // Initialize characterization walk on first entry
        if self.char_walk.borrow().is_none() {
            self.init_characterization()?;
            if self.char_walk.borrow().is_none() {
                return Ok(self.idle_result());
            }
        }

        // A finished walk never re-acquires: acquire → ask → None →
        // DONE would loop, starving the group's remaining walkers. It
        // must still republish demand=False: its last publish (inside
        // the walk) left walk_pending TRUE and nothing else refreshes
        // the flag — a stale TRUE then denies every active walker
        // forever (seed-47 deadlock, run 33720423947).
        // EXCEPT: a pending probe override is un-walked work — the
        // wish's remeasure re-opens the walk (miss-as-gavel; found
        // live 2026-10-22: the aimed hint starved behind a finished
        // walk — the card named the level, no window ever opened).
        if !self.walk_pending()? && self.probe_override.borrow().is_none() {
            self.publish_demand(false);
            return Ok(self.park_result("walk complete"));
        }
        if !self.walk_pending()? {
            log::info!("WALK RE-OPENED: probe override pending — the wish's remeasure");
        }

        // Try to become sender
        if self.try_acquire_lease(CoordState::ProbePush.as_str()) {
            log::info!("Acquired lease — starting characterization");
            *self.state.borrow_mut() = CoordState::ProbePush;
            *self.push_count.borrow_mut() = 0;
            *self.neutral_params.borrow_mut() = None; // force recompute
            return self.handle_probe_push(trial_number);
        }

        // Someone else is sender — become receiver
        if self.has_active_sender() {
            *self.state.borrow_mut() = CoordState::Hold;
            *self.hold_count.borrow_mut() = 0;
            return self.handle_hold(trial_number);
        }

        Ok(self.idle_result())
    }

    /// Parked trial: hold neutral, no lease phase. Same shape as the
    /// sender's pause parks — never carries lease_phase, so the
    /// worker's receiver-write gate keeps these trials out of
    /// receiver_observations.
    fn park_result(&self, why: &str) -> Decision {
        let params = self.get_neutral_params();
        match params {
            None => {
                log::info!("PARK ({why}): no neutral params — trial will FAIL");
                Decision::park(None)
            }
            Some(p) => Decision::park(Some(p)),
        }
    }

    fn idle_result(&self) -> Decision {
        self.park_result("idle")
    }

    /// Non-advancing check: can the walk still produce a probe for
    /// unconverged params at the current refinement floor — or does
    /// the map still name unmeasured ambience entries (rung 5's
    /// bounded debt: patches that do not exist yet)?
    pub(crate) fn walk_pending(&self) -> Result<bool, WalkError> {
        let converged = self.converged_params.borrow().clone();
        let can = {
            let mut walk = self.char_walk.borrow_mut();
            match walk.as_mut() {
                None => return Ok(false),
                Some(w) => match w.can_probe(&converged) {
                    Ok(can) => can,
                    // Dark view: stay engaged (True). Parking on a
                    // dark view would falsely end the walk; the
                    // notebook is authoritative the moment it answers
                    // again.
                    Err(WalkError::Unavailable(e)) => {
                        log::info!("WALK PENDING: view unavailable ({e}) — staying engaged");
                        return Ok(true);
                    }
                    Err(loud) => return Err(loud),
                },
            }
        };
        if can {
            return Ok(true);
        }
        match self.ambience_unmeasured_pending() {
            Ok(p) => Ok(p),
            Err(WalkError::Unavailable(e)) => {
                log::info!("WALK PENDING: view unavailable ({e}) — staying engaged");
                Ok(true)
            }
            Err(loud) => Err(loud),
        }
    }

    // ── PROBE_PUSH (sender) ──────────────────────────────────────────

    fn handle_probe_push(&self, trial_number: i64) -> Result<Decision, CoordError> {
        // First trial of a new push block: ask the walk for next level
        if *self.push_count.borrow() == 0 {
            // Precise remeasure: a wish override pins the WHERE of the
            // probe (the lease, push/pause windows and characterize
            // calls stay this coordinator's own machinery). Cleared
            // when its levels are exhausted — the walk then resumes.
            let mut probe = None;
            if self.probe_override.borrow().is_some() {
                probe = self.override_next_probe();
            }
            if probe.is_none() {
                match self.ask_next_probe() {
                    Ok(Some(p)) => probe = Some(p),
                    Ok(None) => {}
                    // Dark view: park ONE trial, ask again at the next
                    // boundary. Dying here killed whole tenders (run
                    // 35224443449); advancing without the notebook is
                    // guessing — both are wrong, waiting is right.
                    Err(WalkError::Unavailable(e)) => {
                        log::debug!(
                            "PROBE ASK: view unavailable ({e}) — skipping this trial boundary"
                        );
                        return Ok(self.idle_result());
                    }
                    Err(loud) => return Err(loud.into()),
                }
            }
            if probe.is_none() {
                // Standing duty's ambience entries: before DONE may
                // stand, the map's named slice debt gets its walk —
                // the same push/pause machinery, one dial fenced to a
                // window around its held value.
                match self.ambience_duty_probe() {
                    Ok(Some(p)) => probe = Some(p),
                    Ok(None) => {}
                    Err(WalkError::Unavailable(e)) => {
                        log::info!(
                            "AMBIENCE DUTY: consult dark ({e}) — skipping this trial boundary"
                        );
                        return Ok(self.idle_result());
                    }
                    Err(loud) => return Err(loud.into()),
                }
            }
            let probe = match probe {
                None => {
                    *self.state.borrow_mut() = CoordState::Done;
                    return self.handle_done(trial_number);
                }
                Some(p) => p,
            };
            *self.current_probe.borrow_mut() = Some(probe.clone());

            self.set_lease_phase(
                CoordState::ProbePush.as_str(),
                Some(self.push_block_size),
                None,
            );
            *self.round_push_start.borrow_mut() = Some(Utc::now());
            log::debug!(
                "PROBE_PUSH: param={} level={}",
                probe.param_name,
                probe.level
            );
        } else {
            self.decrement_budget(CoordState::ProbePush.as_str(), true);
        }

        *self.push_count.borrow_mut() += 1;
        if *self.push_count.borrow() >= self.push_block_size {
            *self.state.borrow_mut() = CoordState::ProbePause;
            *self.pause_count.borrow_mut() = 0;
        }

        let probe = self
            .current_probe
            .borrow()
            .clone()
            .expect("push handler always has a current probe");
        Ok(Decision {
            mode: "impulse",
            params: Some(probe.config),
            impulse_phase: Some("probe_push"),
            probe_param: Some(probe.param_name),
            probe_param_idx: Some(probe.param_idx),
            probe_level: Some(probe.level),
            lease_phase: None,
            detection_trial: true,
            // The fenced walk at the resting spot: the ledger's kind
            // stamp and causal's ambience bank both key on this.
            ambience_probe: probe.ambience_anchor.is_some(),
        })
    }

    // ── PROBE_PAUSE (sender) ─────────────────────────────────────────

    fn handle_probe_pause(&self, trial_number: i64) -> Result<Decision, CoordError> {
        let probe = self
            .current_probe
            .borrow()
            .clone()
            .expect("pause handler follows a push that set the probe");

        if *self.pause_count.borrow() == 0 {
            self.set_lease_phase(
                CoordState::ProbePause.as_str(),
                None,
                Some(self.pause_block_size),
            );
        } else {
            self.decrement_budget(CoordState::ProbePause.as_str(), false);
        }

        let params = self.get_neutral_params();
        if params.is_none() {
            *self.state.borrow_mut() = CoordState::Done;
            return self.handle_done(trial_number);
        }

        *self.pause_count.borrow_mut() += 1;
        if *self.pause_count.borrow() >= self.pause_block_size {
            *self.round_pause_end.borrow_mut() = Some(Utc::now());
            log::debug!(
                "PROBE_PAUSE: done for param={} level={}",
                probe.param_name,
                probe.level
            );

            // Close the loop: causal computes shift/z/delta, tell char study.
            let result = self.process_probe_result(&probe);
            self.tell_char_study(&probe.param_name, result.as_ref())?;

            *self.push_count.borrow_mut() = 0;
            *self.pause_count.borrow_mut() = 0;

            // Quantum yield: the lease is a shared microphone. A full
            // slice with a pending peer -> step aside at the completed
            // cycle boundary; the notebook resumes this walk exactly
            // on the next acquire. Solo walkers never yield (zero churn).
            *self.stretch_cycles.borrow_mut() += 1;
            if *self.stretch_cycles.borrow() >= self.quantum_cycles && self.live_peer_count() > 0 {
                *self.stretch_cycles.borrow_mut() = 0;
                self.release_lease();
                *self.state.borrow_mut() = CoordState::Cooldown;
                *self.cooldown_count.borrow_mut() = 0;
                log::info!("QUANTUM: {} cycles served — yielding lease (peer walk-pending); walk resumes from notebook",
                    self.quantum_cycles
                );
                return Ok(self.idle_result());
            }

            *self.state.borrow_mut() = CoordState::ProbePush;
        }

        Ok(Decision {
            mode: "hold",
            params,
            impulse_phase: Some("probe_pause"),
            probe_param: Some(probe.param_name),
            probe_param_idx: Some(probe.param_idx),
            probe_level: Some(probe.level),
            lease_phase: None,
            detection_trial: true,
            ambience_probe: false,
        })
    }

    // ── DONE (sender) ────────────────────────────────────────────────

    fn handle_done(&self, _trial_number: i64) -> Result<Decision, CoordError> {
        self.release_lease();
        *self.state.borrow_mut() = CoordState::Cooldown;
        *self.cooldown_count.borrow_mut() = 0;

        let n_converged = self.converged_params.borrow().len();
        let n_total = self.param_names.borrow().len();
        log::info!("DONE: released lease. Converged {n_converged}/{n_total} params.");
        Ok(self.idle_result())
    }

    // ── COOLDOWN ─────────────────────────────────────────────────────

    fn handle_cooldown(&self, trial_number: i64) -> Result<Decision, CoordError> {
        *self.cooldown_count.borrow_mut() += 1;
        if *self.cooldown_count.borrow() >= self.cooldown_trials {
            let non_converged: Vec<String> = {
                let converged = self.converged_params.borrow();
                self.param_names
                    .borrow()
                    .iter()
                    .filter(|p| !converged.contains(*p))
                    .cloned()
                    .collect()
            };
            if !non_converged.is_empty() && self.walk_pending()? {
                log::info!("COOLDOWN: done — re-acquiring for more probes");
                if self.try_acquire_lease(CoordState::ProbePush.as_str()) {
                    log::info!(
                        "Acquired lease — re-entry via cooldown (walk resumes from the notebook)"
                    );
                    *self.state.borrow_mut() = CoordState::ProbePush;
                    *self.push_count.borrow_mut() = 0;
                    return self.handle_probe_push(trial_number);
                }
            }
            log::info!("COOLDOWN: done — back to IDLE");
            *self.state.borrow_mut() = CoordState::Idle;
        }
        Ok(self.idle_result())
    }

    // ── HOLD (receiver) ──────────────────────────────────────────────

    fn handle_hold(&self, _trial_number: i64) -> Result<Decision, CoordError> {
        // A parked member is still a member: keep the census row alive
        // or the group's own count starves and the rota freezes.
        self.refresh_presence();
        if !self.has_active_sender() {
            log::info!("HOLD: sender finished — back to IDLE");
            *self.state.borrow_mut() = CoordState::Idle;
            return Ok(self.idle_result());
        }

        *self.hold_count.borrow_mut() += 1;
        if *self.hold_count.borrow() > MAX_HOLD_TRIALS {
            log::info!("HOLD: hit MAX_HOLD_TRIALS ({MAX_HOLD_TRIALS}) — returning to IDLE");
            *self.state.borrow_mut() = CoordState::Idle;
            return Ok(self.idle_result());
        }

        let params = self.get_neutral_params();
        if params.is_none() {
            *self.state.borrow_mut() = CoordState::Idle;
            return Ok(self.idle_result());
        }

        let phase = self.get_lease_phase();
        let log_it = *self.hold_count.borrow() % 5 == 1
            || phase != self.last_observed_phase.borrow().clone();
        if log_it {
            log::info!(
                "HOLD: receiver trial {}, sender phase={}",
                self.hold_count.borrow(),
                phase.clone().unwrap_or_else(|| "None".to_string())
            );
            *self.last_observed_phase.borrow_mut() = phase.clone();
        }

        Ok(Decision {
            mode: "hold",
            params,
            impulse_phase: None,
            probe_param: None,
            probe_param_idx: None,
            probe_level: None,
            lease_phase: phase,
            detection_trial: true,
            ambience_probe: false,
        })
    }

    // ─── Observability ───────────────────────────────────────────────

    /// Structured characterization status for trial attrs / dashboards.
    pub fn get_char_status(&self) -> Value {
        let param_names = self.param_names.borrow().clone();
        let converged = self.converged_params.borrow().clone();
        let mut status = Map::new();
        let mut n_measured = 0usize;
        let mut total: i64 = 0;
        let walk_st = self.char_walk.borrow().as_ref().map(|w| w.status());
        for name in &param_names {
            let st = walk_st.as_ref().and_then(|st| st.get(name));
            let levels_measured = st.map(|s| s.levels_measured).unwrap_or(0);
            status.insert(
                name.clone(),
                json!({
                    "converged": converged.contains(name),
                    "step": st.map(|s| num(s.step)),
                    "levels_total": st.and_then(|s| s.levels_total).map(Value::from),
                    "levels_measured": levels_measured,
                }),
            );
            n_measured += levels_measured;
            if let Some(t) = st.and_then(|s| s.levels_total) {
                total += t;
            }
        }
        json!({
            "state": self.state.borrow().as_str(),
            "refinement_level": *self.refinement_level.borrow(),
            "params": Value::Object(status),
            "levels_measured": n_measured,
            "levels_total": total,
            "converged_count": converged.len(),
            "params_total": param_names.len(),
        })
    }
}
fn display_opt(v: Option<i64>) -> String {
    v.map(|x| x.to_string())
        .unwrap_or_else(|| "None".to_string())
}

impl<C: Connect> ProbeCoordinator<C> {
    /// The detection config subtree (re-read per call, python parity).
    pub(crate) fn det_cfg(&self) -> Value {
        self.config
            .get("interference_detection")
            .or_else(|| self.config.get("detection"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()))
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;
    use crate::coordinator::ProbeCoordinator;
    use crate::ledger::{Connect, DbError};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // ── harness (the python suite's shapes) ──────────────────────────

    /// the python `_config()` — ladder walk, blocks of 3, cooldown 1
    fn config(params: &[(&str, f64, f64, bool)], over: Value) -> Value {
        let mut hold = Map::new();
        let mut settings = Map::new();
        for (name, lo, hi, _is_int) in params {
            hold.insert(name.to_string(), json!(((lo + hi) / 2.0) as f64));
            settings.insert(
                name.to_string(),
                json!({"constraints": [{"lower": lo, "upper": hi}]}),
            );
        }
        let mut det = json!({
            "group": "walktest",
            "hold_params": Value::Object(hold),
            "push_block_size": 3,
            "pause_block_size": 3,
            "cooldown_trials": 1,
            "convergence_threshold": 0.02,
            "refinement_depth": 3,
            "walk_policy": "ladder",
            "causal_url": "http://causal.test",
        });
        if let Some(o) = over.as_object() {
            for (k, v) in o {
                det[k] = v.clone();
            }
        }
        json!({
            "systemtender": {"type": "bench_generic", "uuid": "test-sender-1"},
            "settings": {"generic": Value::Object(settings)},
            "interference_detection": det,
        })
    }

    /// the python `_fake_upper_bounds` — reads settings.generic
    pub(crate) fn fake_upper_bounds(settings: &Value) -> Vec<UpperBound> {
        let gen = settings
            .get("generic")
            .and_then(|v| v.as_object())
            .unwrap_or(settings.as_object().unwrap());
        let mut out = Vec::new();
        for (name, cfg) in gen {
            let Some(constraints) = cfg.get("constraints").and_then(|v| v.as_array()) else {
                continue;
            };
            for c in constraints {
                let lower = c.get("lower").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let upper = c.get("upper").and_then(|v| v.as_f64()).unwrap_or(100.0);
                // python: isinstance(lower, int) and isinstance(upper, int)
                // — the JSON number TYPE, not the value's shape
                let is_int = c.get("lower").map(|v| v.is_i64()).unwrap_or(false)
                    && c.get("upper").map(|v| v.is_i64()).unwrap_or(false);
                out.push(UpperBound {
                    name: name.clone(),
                    lower,
                    upper,
                    is_int,
                });
            }
        }
        out
    }

    /// The causal stub — dispatches on URL path exactly like the real
    /// service split (characterize POSTs vs ambience-duty GETs).
    /// Responses are queued in CALL ORDER.
    #[derive(Clone)]
    struct CausalStub {
        characterize: Arc<Mutex<VecDeque<Result<Value, HttpError>>>>,
        ambience: Arc<Mutex<VecDeque<Result<Value, HttpError>>>>,
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl CausalStub {
        fn new(
            characterize: Vec<Result<Value, HttpError>>,
            ambience: Vec<Result<Value, HttpError>>,
        ) -> Self {
            CausalStub {
                characterize: Arc::new(Mutex::new(characterize.into())),
                ambience: Arc::new(Mutex::new(ambience.into())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn call_count(&self, fragment: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, u)| u.contains(fragment))
                .count()
        }
    }

    impl HttpTransport for CausalStub {
        fn call(
            &self,
            method: &str,
            url: &str,
            _payload: Option<&Value>,
            _timeout: Duration,
        ) -> Result<Value, HttpError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), url.to_string()));
            let queue = if url.contains("/characterize") {
                &self.characterize
            } else if url.contains("/ambience-duty/") {
                &self.ambience
            } else {
                return Ok(json!({}));
            };
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(HttpError::Dark("stub exhausted".into())))
        }
    }

    /// the offline coordinator: room ops degrade to warnings (the
    /// DSN points nowhere; connections fail lazily per verb) — the
    /// python `_noop_db` equivalent, honestly earned
    struct Offline;

    impl Connect for Offline {
        fn connect(&self) -> Result<postgres::Client, DbError> {
            Err(DbError::new("offline coordinator: no room in unit tests"))
        }
    }

    fn make_coord(cfg: &Value, stub: CausalStub) -> ProbeCoordinator<Offline> {
        let hooks = CoordHooks {
            collect_upper_bounds: Arc::new(fake_upper_bounds),
            compute_neutral_params: None,
            walk_transport: None,
            http: Some(Arc::new(stub)),
        };
        let coord = ProbeCoordinator::new("test-sender-1", cfg, Offline, hooks);
        // the python harness marks the room initialized and seeds a
        // debt-free map — identical contract here
        *coord.initialized.borrow_mut() = true;
        *coord.ambience_debt_cache.borrow_mut() = Some((std::time::Instant::now(), Vec::new()));
        coord
    }

    fn one_param() -> Value {
        config(&[("param_0", 0.0, 100.0, false)], json!({}))
    }

    fn converged_result(gaps: Value) -> Value {
        json!({
            "shift": 0.5, "delta": 0.25, "z": 2.5, "converged": true,
            "shift_bar": 0.1, "gaps": gaps,
        })
    }

    fn probe_for(name: &str, level: f64) -> Probe {
        Probe {
            param_name: name.to_string(),
            param_idx: 0,
            level,
            config: Map::new(),
            ambience_anchor: None,
            ambience_epoch: None,
        }
    }

    fn seed_round(coord: &ProbeCoordinator<Offline>) {
        *coord.round_push_start.borrow_mut() = Some(Utc::now());
        *coord.round_pause_end.borrow_mut() = Some(Utc::now());
    }

    // ── adaptive timeout (python test_timeout_*) ─────────────────────

    #[test]
    fn timeout_ladder_offline() {
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        // no history → floor
        assert_eq!(coord.causal_timeout(), 2.0);
        // scales with trial duration: median 45s → budget 90s
        for d in [40.0, 45.0, 50.0] {
            coord.record_trial_duration(d);
        }
        assert_eq!(coord.causal_timeout(), 90.0);
        // tightened by observed rtt: median 5ms → 0.05s → floored 2s
        *coord.causal_rtt_history.borrow_mut() = vec![0.005, 0.005];
        assert_eq!(coord.causal_timeout(), 2.0);
        // fast trials, slow causal: min(budget, tightened)
        let coord2 = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        *coord2.trial_duration_history.borrow_mut() = vec![1.0, 1.0];
        *coord2.causal_rtt_history.borrow_mut() = vec![0.5, 0.6];
        // budget = 2, tightened = 5.5 → 2
        assert_eq!(coord2.causal_timeout(), 2.0);
    }

    // ── neutral params (python _get_neutral_params) ──────────────────

    #[test]
    fn neutral_params_source_order() {
        // hold_params from config win
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        let p = coord.get_neutral_params().unwrap();
        assert_eq!(p["param_0"], json!(50.0));
        assert!(*coord.hold_params_from_config.borrow());

        // midpoint fallback when no hold_params and no callback
        let cfg = json!({
            "settings": {"generic": {"p": {"constraints": [{"lower": 0, "upper": 9}]}}},
            "interference_detection": {"group": "g", "walk_policy": "ladder"},
        });
        let hooks = CoordHooks::new(Arc::new(fake_upper_bounds));
        let coord = ProbeCoordinator::new("t", &cfg, Offline, hooks);
        *coord.initialized.borrow_mut() = true;
        let p = coord.get_neutral_params().unwrap();
        assert_eq!(p["p"], json!(4)); // int bounds → int midpoint

        // nothing collectable → None
        let cfg2 = json!({"settings": {}, "interference_detection": {}});
        let hooks2 = CoordHooks::new(Arc::new(fake_upper_bounds));
        let coord2 = ProbeCoordinator::new("t", &cfg2, Offline, hooks2);
        assert!(coord2.get_neutral_params().is_none());
    }

    #[test]
    fn probe_config_scalar_and_list() {
        let mut neutral = Map::new();
        neutral.insert("p".to_string(), json!(50.0));
        neutral.insert("arr".to_string(), json!([1.0, 2.0, 3.0]));
        let c = probe_config(&neutral, "p", 75.0);
        assert_eq!(c["p"], json!(75.0));
        let c = probe_config(&neutral, "arr", 9.0);
        assert_eq!(c["arr"], json!([9.0, 9.0, 9.0]));
    }

    // ── causal client + two-key retirement (python test_process_*) ───

    #[test]
    fn causal_query_success_and_blind_failure() {
        let stub = CausalStub::new(
            vec![Ok(json!({"shift": 0.1, "delta": 0.05, "converged": false}))],
            vec![],
        );
        let coord = make_coord(&one_param(), stub.clone());
        seed_round(&coord);
        let r = coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(r.is_some());
        assert_eq!(coord.causal_rtt_history.borrow().len(), 1);

        // dark causal → None, rtt NOT recorded
        let stub = CausalStub::new(vec![Err(HttpError::Dark("refused".into()))], vec![]);
        let coord = make_coord(&one_param(), stub);
        seed_round(&coord);
        assert!(coord
            .process_probe_result(&probe_for("param_0", 50.0))
            .is_none());
        assert!(coord.causal_rtt_history.borrow().is_empty());
    }

    #[test]
    fn two_key_retirement_matrix() {
        // converged + no gaps → retire
        let coord = make_coord(
            &one_param(),
            CausalStub::new(vec![Ok(converged_result(json!([])))], vec![]),
        );
        seed_round(&coord);
        coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(coord.converged_params.borrow().contains("param_0"));

        // converged but a gap priced ABOVE its bars → stays in rotation
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![Ok(converged_result(json!([
                    {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.1,
                     "from_level": 50.0, "to_level": 75.0}
                ])))],
                vec![],
            ),
        );
        seed_round(&coord);
        coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(!coord.converged_params.borrow().contains("param_0"));

        // unresolved but priced OUT (ignorance <= bars_sum) → retires
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![Ok(converged_result(json!([
                    {"unresolved": true, "ignorance": 0.05, "bars_sum": 0.1,
                     "from_level": 50.0, "to_level": 75.0}
                ])))],
                vec![],
            ),
        );
        seed_round(&coord);
        coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(coord.converged_params.borrow().contains("param_0"));

        // drift voids an existing retirement — but the two-key check
        // runs AFTER the discard (python statement order), so a result
        // that is simultaneously converged-clean re-retires; drift
        // bites when the result is not converged
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![
                    Ok(converged_result(json!([]))),
                    Ok(json!({"shift": 0.1, "converged": false, "drift": true, "gaps": []})),
                ],
                vec![],
            ),
        );
        seed_round(&coord);
        coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(coord.converged_params.borrow().contains("param_0"));
        coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(!coord.converged_params.borrow().contains("param_0"));

        // not converged → never retires, whatever the gaps say
        let coord = make_coord(
            &one_param(),
            CausalStub::new(vec![Ok(json!({"converged": false, "gaps": []}))], vec![]),
        );
        seed_round(&coord);
        coord.process_probe_result(&probe_for("param_0", 50.0));
        assert!(!coord.converged_params.borrow().contains("param_0"));
    }

    // ── ambience duty (python test_ambience_duty) ────────────────────

    #[test]
    fn ambience_debt_cache_ttl_and_answers() {
        let stub = CausalStub::new(
            vec![],
            vec![Ok(
                json!({"debt": [{"param": "param_0", "reason": "window_aging"}]}),
            )],
        );
        let coord = make_coord(&one_param(), stub.clone());
        // seeded cache (harness) holds until it expires or force
        let d = coord.ambience_debt(false).unwrap();
        assert!(d.is_empty()); // from the seeded cache, zero transport calls
        assert_eq!(stub.call_count("/ambience-duty/"), 0);
        // force re-consults
        let d = coord.ambience_debt(true).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(stub.call_count("/ambience-duty/"), 1);
        // and caches the answer
        let d = coord.ambience_debt(false).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(stub.call_count("/ambience-duty/"), 1);
    }

    #[test]
    fn ambience_4xx_is_an_answer_5xx_is_dark() {
        // 404: the notebook ANSWERED (predates local ground) → no debt
        let coord = make_coord(
            &one_param(),
            CausalStub::new(vec![], vec![Err(HttpError::Status(404))]),
        );
        *coord.ambience_debt_cache.borrow_mut() = None;
        assert!(coord.ambience_debt(true).unwrap().is_empty());
        // 500: dark → None, and the dark consult never writes the
        // cache (a dark view is not an answer) — forced re-asks stay dark
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Err(HttpError::Status(500)), Err(HttpError::Status(500))],
            ),
        );
        assert!(coord.ambience_debt(true).is_none());
        assert!(coord.ambience_debt(true).is_none());
        // unmeasured pending: only 'unmeasured_operating_point' counts
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Ok(
                    json!({"debt": [{"param": "param_0", "reason": "window_aging"}]}),
                )],
            ),
        );
        *coord.ambience_debt_cache.borrow_mut() = None;
        assert!(!coord.ambience_unmeasured_pending().unwrap());
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Ok(
                    json!({"debt": [{"param": "param_0", "reason": "unmeasured_operating_point"}]}),
                )],
            ),
        );
        *coord.ambience_debt_cache.borrow_mut() = None;
        assert!(coord.ambience_unmeasured_pending().unwrap());
    }

    #[test]
    fn ambience_duty_probe_walks_fattest_gaps_fenced() {
        // two gaps: ignorance 3 (0→50, mid 25) beats 1 (50→75, mid 62.5);
        // one measured mid skipped; batch capped at 8; fence clamps
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Ok(json!({"debt": [{
                    "param": "param_0",
                    "anchor": 50.0,
                    "reason": "unmeasured_operating_point",
                    "levels": [25.0, 62.5],
                    "gaps": [
                        {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.1,
                         "from_level": 50.0, "to_level": 75.0},
                        {"unresolved": true, "ignorance": 3.0, "bars_sum": 0.1,
                         "from_level": 0.0, "to_level": 50.0},
                        {"unresolved": true, "ignorance": 9.0, "bars_sum": 0.1,
                         "from_level": -40.0, "to_level": 0.0}
                    ],
                    "epoch": 7,
                }]}))],
            ),
        );
        // walk must exist for the override to resolve bounds
        coord.init_characterization().unwrap();
        let probe = coord.ambience_duty_probe().unwrap().unwrap();
        assert_eq!(probe.param_name, "param_0");
        assert_eq!(probe.ambience_anchor, Some(50.0));
        assert_eq!(probe.ambience_epoch, Some(7));
        // ignorance 9 gap first: mid (-40+0)/2 = -20 → fenced to 0.0
        assert_eq!(probe.level, 0.0);
        assert_eq!(probe.config["param_0"], json!(0.0));
        // the two measured mids (25, 62.5) never re-walk; the batch
        // holds exactly the one fenced survivor
        assert!(coord.override_next_probe().is_none()); // the batch is spent
                                                        // batch cap: many unmeasured gaps → at most AMBIENCE_WALK_BATCH
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Ok(json!({
                    "debt": [{
                        "param": "param_0", "anchor": 50.0, "epoch": 0,
                        "reason": "unmeasured_operating_point", "levels": [],
                        "gaps": [
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 0.0, "to_level": 10.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 10.0, "to_level": 20.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 20.0, "to_level": 30.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 30.0, "to_level": 40.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 40.0, "to_level": 50.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 50.0, "to_level": 60.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 60.0, "to_level": 70.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 70.0, "to_level": 80.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 80.0, "to_level": 90.0},
                            {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.0,
                             "from_level": 90.0, "to_level": 100.0}
                        ]
                    }]
                }))],
            ),
        );
        coord.init_characterization().unwrap();
        coord.ambience_duty_probe().unwrap().unwrap(); // pins the batch
        let mut n = 1; // the duty probe consumed one
        while coord.override_next_probe().is_some() {
            n += 1;
        }
        assert_eq!(n, AMBIENCE_WALK_BATCH);
    }

    #[test]
    fn ambience_duty_unknown_param_or_no_gaps_is_none() {
        // the named dial is not this strain's → no debt answer
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Ok(json!({"debt": [{"param": "not_a_dial", "anchor": 1.0,
                    "reason": "unmeasured_operating_point", "levels": [],
                    "gaps": []}]}))],
            ),
        );
        assert!(coord.ambience_duty_probe().unwrap().is_none());
        // all gaps priced out → nothing to walk
        let coord = make_coord(
            &one_param(),
            CausalStub::new(
                vec![],
                vec![Ok(json!({
                    "debt": [{
                        "param": "param_0", "anchor": 1.0,
                        "reason": "window_aging", "levels": [50.0],
                        "gaps": [{"unresolved": true, "ignorance": 0.05, "bars_sum": 0.1,
                                  "from_level": 50.0, "to_level": 100.0}]
                    }]
                }))],
            ),
        );
        assert!(coord.ambience_duty_probe().unwrap().is_none());
        // dark consult → Unavailable (the park-and-retry discipline)
        let coord = make_coord(
            &one_param(),
            CausalStub::new(vec![], vec![Err(HttpError::Dark("dark".into()))]),
        );
        assert!(matches!(
            coord.ambience_duty_probe(),
            Err(WalkError::Unavailable(_))
        ));
    }

    // ── override (python set_probe_override / _override_next_probe) ──

    #[test]
    fn override_lifecycle() {
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        coord.init_characterization().unwrap();
        assert!(coord.override_next_probe().is_none()); // no override yet
        coord.set_probe_override("param_0", &[10.0, 20.0], None, None);
        let a = coord.override_next_probe().unwrap();
        assert_eq!(a.level, 10.0);
        assert!(a.ambience_anchor.is_none());
        let b = coord.override_next_probe().unwrap();
        assert_eq!(b.level, 20.0);
        // exhausted → cleared, and None thereafter
        assert!(coord.override_next_probe().is_none());
        assert!(coord.probe_override.borrow().is_none());
        // unknown param → cleared with the warning, never a crash
        coord.set_probe_override("ghost", &[1.0], None, None);
        assert!(coord.override_next_probe().is_none());
        assert!(coord.probe_override.borrow().is_none());
    }

    // ── init + ask (python test_char_init / test_char_ask) ───────────

    #[test]
    fn ladder_init_builds_bounds_and_asks_in_walk_order() {
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        coord.init_characterization().unwrap();
        assert!(coord.char_walk.borrow().is_some());
        assert_eq!(coord.param_names.borrow().clone(), vec!["param_0"]);
        // the documented prefix: midpoint → edges
        let p = coord.ask_next_probe().unwrap().unwrap();
        assert_eq!((p.param_name.as_str(), p.level), ("param_0", 50.0));
        assert_eq!(p.param_idx, 0);
        assert_eq!(p.config["param_0"], json!(50.0));
        let p = coord.ask_next_probe().unwrap().unwrap();
        assert_eq!(p.level, 100.0);
        // after retiring the param, ask yields None
        coord.converged_params.borrow_mut().insert("param_0".into());
        assert!(coord.ask_next_probe().unwrap().is_none());
    }

    #[test]
    fn walk_pending_and_char_complete_ladder_semantics() {
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        // no walk yet
        assert!(!coord.walk_pending().unwrap());
        assert!(!coord.char_complete().unwrap());
        coord.init_characterization().unwrap();
        assert!(coord.walk_pending().unwrap());
        assert!(!coord.char_complete().unwrap());
        // exhaust the floor (5 levels), retire the param
        for _ in 0..5 {
            coord.ask_next_probe().unwrap().unwrap();
        }
        assert!(!coord.walk_pending().unwrap()); // ambience seeded empty
        assert!(coord.char_complete().unwrap());
        // unmeasured ambience debt holds the quiet-bench lever
        *coord.ambience_debt_cache.borrow_mut() = Some((
            std::time::Instant::now(),
            vec![json!({
                "param": "param_0", "reason": "unmeasured_operating_point"
            })],
        ));
        assert!(coord.walk_pending().unwrap());
        assert!(!coord.char_complete().unwrap());
    }

    #[test]
    fn probe_override_reopens_a_finished_walk() {
        // Miss-as-gavel: a pending aimed probe is un-walked work — the
        // quiet lever holds (found live 2026-10-22: the wish's remeasure
        // hint starved behind a finished walk; the card named the level,
        // no window ever opened).
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        coord.init_characterization().unwrap();
        // exhaust the floor and retire the param — a complete walk
        for _ in 0..5 {
            coord.ask_next_probe().unwrap().unwrap();
        }
        coord.converged_params.borrow_mut().insert("param_0".into());
        assert!(!coord.walk_pending().unwrap());
        assert!(coord.char_complete().unwrap());
        // the wish's aimed probe lands: the walk is open again
        coord.set_probe_override("param_0", &[50.0], None, None);
        assert!(!coord.char_complete().unwrap());
        // and the aimed level is served to the next push block
        let p = coord.override_next_probe().unwrap();
        assert_eq!((p.param_name.as_str(), p.level), ("param_0", 50.0));
    }

    // ── tell + refinement (python test_char_tell / test_refinement) ──

    #[test]
    fn tell_lines_and_exhaustion_refinement() {
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        coord.init_characterization().unwrap();
        // consume the floor
        for _ in 0..5 {
            coord.ask_next_probe().unwrap().unwrap();
        }
        // a converged-but-unretired tell triggers exhaustion → refine
        coord
            .tell_char_study("param_0", Some(&converged_result(json!([]))))
            .unwrap();
        assert_eq!(*coord.refinement_level.borrow(), 1);
        let st = coord.char_walk.borrow().as_ref().unwrap().status();
        assert_eq!(st["param_0"].step, 12.5); // 25.0 halved
                                              // depth cap accepts best effort: exhaust the refined floor,
                                              // then the next exhaustion finds the depth spent → everything
                                              // converges
        for _ in 0..4 {
            coord.ask_next_probe().unwrap().unwrap();
        }
        *coord.refinement_level.borrow_mut() = 3;
        coord
            .tell_char_study("param_0", Some(&converged_result(json!([]))))
            .unwrap();
        assert!(coord.converged_params.borrow().contains("param_0"));
        // FAIL tell (causal dark) parks quietly, no refinement
        let before = *coord.refinement_level.borrow();
        coord.tell_char_study("param_0", None).unwrap();
        assert_eq!(*coord.refinement_level.borrow(), before);
        // receivers map + INFINITY delta paths parse without panic
        let res = json!({
            "shift": 1.0, "delta": 1e308, "z": 3.0, "converged": true,
            "primary_receiver": "r2",
            "receivers": {
                "r1": {"primary_channel": 0, "shift": 0.5, "z": 1.0,
                        "converged": true, "unresolved_gaps": 0},
                "r2": {"primary_channel": 1, "shift": -0.25, "z": 2.0,
                        "converged": false, "unresolved_gaps": 2}
            }
        });
        coord.tell_char_study("param_0", Some(&res)).unwrap();
    }

    // ── observability ────────────────────────────────────────────────

    #[test]
    fn char_status_shape() {
        let coord = make_coord(&one_param(), CausalStub::new(vec![], vec![]));
        coord.init_characterization().unwrap();
        coord.ask_next_probe().unwrap();
        let st = coord.get_char_status();
        assert_eq!(st["state"], "idle");
        assert_eq!(st["params_total"], 1);
        assert_eq!(st["converged_count"], 0);
        assert_eq!(st["params"]["param_0"]["levels_measured"], 1);
        assert_eq!(st["params"]["param_0"]["step"], 25.0);
        assert_eq!(st["params"]["param_0"]["levels_total"], 5);
    }
}

#[cfg(test)]
mod room_walk_tests {
    //! DSN-gated: the walk state machine against a real room — the
    //! python suite's state-machine cases, upgraded from the noop DB
    //! to the real tables (CI runs these against the service postgres).
    use super::*;
    use crate::coordinator::db_tests::ROOM;
    use crate::coordinator::{register_interference_systemtender, ProbeCoordinator};
    use crate::ledger::DsnConnect;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn dsn() -> Option<String> {
        std::env::var("LEDGER_TEST_DSN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn raw(dsn: &str, sql: &str) {
        let mut client = postgres::Client::connect(dsn, postgres::NoTls).unwrap();
        client.execute(sql, &[]).unwrap();
    }

    fn park(dsn: &str, id: &str) {
        raw(
            dsn,
            &format!(
                "UPDATE interference_active_systemtenders \
                 SET params = '{{}}'::jsonb WHERE systemtender_id = '{id}'"
            ),
        );
    }

    #[derive(Clone)]
    struct Stub {
        characterize: Arc<Mutex<VecDeque<Result<Value, HttpError>>>>,
        ambience: Arc<Mutex<VecDeque<Result<Value, HttpError>>>>,
    }

    impl HttpTransport for Stub {
        fn call(
            &self,
            _method: &str,
            url: &str,
            _payload: Option<&Value>,
            _timeout: Duration,
        ) -> Result<Value, HttpError> {
            let q = if url.contains("/characterize") {
                &self.characterize
            } else {
                &self.ambience
            };
            q.lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(HttpError::Dark("stub exhausted".into())))
        }
    }

    fn stub(chars: Vec<Value>) -> Stub {
        Stub {
            characterize: Arc::new(Mutex::new(chars.into_iter().map(Ok).collect())),
            ambience: Arc::new(Mutex::new(
                vec![Ok(json!({"debt": []}))]
                    .into_iter()
                    .collect::<VecDeque<_>>(),
            )),
        }
    }

    fn build(dsn_str: &str, cfg: &Value, http: Stub) -> ProbeCoordinator<DsnConnect> {
        let hooks = CoordHooks {
            collect_upper_bounds: Arc::new(super::walk_tests::fake_upper_bounds),
            compute_neutral_params: None,
            walk_transport: None,
            http: Some(Arc::new(http)),
        };
        ProbeCoordinator::new_dsn("tender-a", cfg, hooks, dsn_str).unwrap()
    }

    fn fresh_room(
        dsn: &str,
        cfg: &Value,
        http: Stub,
        register_peer: bool,
    ) -> ProbeCoordinator<DsnConnect> {
        let mut client = postgres::Client::connect(dsn, postgres::NoTls).unwrap();
        for t in [
            "sender_lease",
            "detection_readiness",
            "receiver_observations",
            "interference_active_systemtenders",
        ] {
            client
                .execute(&format!("DROP TABLE IF EXISTS {t}"), &[])
                .unwrap();
        }
        drop(client);
        let a = build(dsn, cfg, http);
        a.ensure_tables().unwrap();
        register_interference_systemtender(&DsnConnect::new(dsn).unwrap(), "tender-a", "walkroom")
            .unwrap();
        if register_peer {
            register_interference_systemtender(
                &DsnConnect::new(dsn).unwrap(),
                "tender-b",
                "walkroom",
            )
            .unwrap();
            park(dsn, "tender-b");
        }
        park(dsn, "tender-a");
        // the room is already ensured; decide_trial's first-try setup
        // would re-run harmlessly, but keep it one-motion like the worker
        a
    }

    fn ladder_cfg(over: Value) -> Value {
        let mut det = json!({
            "group": "walkroom",
            "hold_params": {"param_0": 50.0},
            "push_block_size": 3,
            "pause_block_size": 3,
            "cooldown_trials": 1,
            "refinement_depth": 3,
            "walk_policy": "ladder",
            "causal_url": "http://causal.test",
        });
        if let Some(o) = over.as_object() {
            for (k, v) in o {
                det[k] = v.clone();
            }
        }
        json!({
            "systemtender": {"type": "bench_generic", "uuid": "tender-a"},
            "settings": {"generic": {"param_0": {"constraints": [{"lower": 0.0, "upper": 100.0}]}}},
            "interference_detection": det,
        })
    }

    fn holder(dsn: &str) -> Option<String> {
        let mut client = postgres::Client::connect(dsn, postgres::NoTls).unwrap();
        let row = client
            .query_one("SELECT holder FROM sender_lease", &[])
            .unwrap();
        row.get::<_, Option<String>>(0)
    }

    /// the whole arc: IDLE → acquire → push/pause rounds → converged →
    /// DONE → COOLDOWN → IDLE → park "walk complete" (demand cleared)
    #[test]
    fn full_cycle_to_done_and_quiet_park() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = ROOM.lock().unwrap_or_else(|p| p.into_inner());
        // one characterize answer converges param_0; ambience stays empty
        let http = stub(vec![json!({
            "shift": 0.5, "delta": 0.25, "z": 2.5, "converged": true,
            "gaps": [],
        })]);
        let cfg = ladder_cfg(json!({"quantum_cycles": 99}));
        let a = fresh_room(&dsn, &cfg, http, true);

        // t1: IDLE → sees 2 live members, inits the ladder, acquires,
        // first push
        let d = a.decide_trial(1).unwrap();
        assert_eq!(d.mode, "impulse");
        assert_eq!(d.impulse_phase, Some("probe_push"));
        assert!(d.detection_trial);
        assert!(!d.ambience_probe);
        assert_eq!(d.probe_param.as_deref(), Some("param_0"));
        assert_eq!(d.probe_level, Some(50.0));
        assert_eq!(holder(&dsn).as_deref(), Some("tender-a"));

        // t2, t3: push block completes → PAUSE
        assert_eq!(a.decide_trial(2).unwrap().impulse_phase, Some("probe_push"));
        let d = a.decide_trial(3).unwrap();
        assert_eq!(d.impulse_phase, Some("probe_push"));
        // t4-6: pause block, causal answers converged on the boundary
        for t in 4..=5 {
            let d = a.decide_trial(t).unwrap();
            assert_eq!(d.mode, "hold");
            assert_eq!(d.impulse_phase, Some("probe_pause"));
            assert_eq!(d.probe_level, Some(50.0));
            assert!(d.lease_phase.is_none(), "pause parks carry no lease_phase");
        }
        let _ = a.decide_trial(6).unwrap(); // boundary: process + tell
        assert!(a.converged_params.borrow().contains("param_0"));

        // t7: back in PUSH, ask finds nothing → ambience empty → DONE
        // (the DONE handler parks and lands in COOLDOWN, python shape)
        let d = a.decide_trial(7).unwrap();
        assert_eq!(a.get_state(), CoordState::Cooldown);
        assert_eq!(d.mode, "hold"); // done parks at neutral
        assert!(!d.detection_trial);
        assert_eq!(d.params.as_ref().unwrap()["param_0"], json!(50.0));
        assert_eq!(holder(&dsn), None); // released

        // t8: cooldown spent (1 trial) → back to IDLE same trial
        let _ = a.decide_trial(8).unwrap();
        assert_eq!(a.get_state(), CoordState::Idle);

        // t9: IDLE — finished walk never re-acquires; demand=false
        let d = a.decide_trial(9).unwrap();
        assert_eq!(a.get_state(), CoordState::Idle);
        assert_eq!(d.mode, "hold");
        assert!(!d.detection_trial);
        let mut client = postgres::Client::connect(&dsn, postgres::NoTls).unwrap();
        let row = client
            .query_one(
                "SELECT walk_pending FROM interference_active_systemtenders \
                 WHERE systemtender_id = 'tender-a'",
                &[],
            )
            .unwrap();
        assert!(!row.get::<_, bool>(0), "walk complete must clear demand");
        assert_eq!(holder(&dsn), None);
    }

    /// quantum: the lease is a shared microphone — a full slice with a
    /// live peer yields at the cycle boundary; solo walkers never do
    #[test]
    fn quantum_yields_under_contention_only() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = ROOM.lock().unwrap_or_else(|p| p.into_inner());
        let http = stub(vec![json!({"shift": 0.5, "converged": false, "gaps": []})]);
        let cfg =
            ladder_cfg(json!({"quantum_cycles": 1, "push_block_size": 2, "pause_block_size": 2}));
        let a = fresh_room(&dsn, &cfg, http, true);
        let _ = a.decide_trial(1).unwrap(); // acquire + push 1
        let _ = a.decide_trial(2).unwrap(); // push 2 → PAUSE
        let _ = a.decide_trial(3).unwrap(); // pause 1
        let _ = a.decide_trial(4).unwrap(); // pause 2 → cycle done → YIELD
        assert_eq!(a.get_state(), CoordState::Cooldown);
        assert_eq!(holder(&dsn), None);

        // solo: same shape, no peer registered → never yields
        let http = stub(vec![json!({"shift": 0.5, "converged": false, "gaps": []})]);
        let cfg =
            ladder_cfg(json!({"quantum_cycles": 1, "push_block_size": 2, "pause_block_size": 2}));
        let a = fresh_room(&dsn, &cfg, http, false);
        let _ = a.decide_trial(1).unwrap();
        // wait: solo means census count 1 < 2 → parks in IDLE, no acquire
        assert_eq!(a.get_state(), CoordState::Idle);
        assert_eq!(holder(&dsn), None);
    }

    /// HOLD from the first trial: an active sender outranks everything,
    /// and the hold decision carries the sender's lease phase
    #[test]
    fn receiver_holds_from_first_trial_with_phase() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = ROOM.lock().unwrap_or_else(|p| p.into_inner());
        let http = stub(vec![]);
        let cfg = ladder_cfg(json!({}));
        let a = fresh_room(&dsn, &cfg, http, true);
        // tender-b takes the lease and holds it in probe_push
        raw(
            &dsn,
            "UPDATE sender_lease SET holder = 'tender-b', phase = 'probe_push', \
             last_heartbeat = NOW()",
        );
        let d = a.decide_trial(1).unwrap();
        assert_eq!(a.get_state(), CoordState::Hold);
        assert_eq!(d.mode, "hold");
        assert!(d.detection_trial);
        assert_eq!(d.lease_phase.as_deref(), Some("probe_push"));
        assert_eq!(d.params.as_ref().unwrap()["param_0"], json!(50.0));
        // sender gone → back to IDLE
        raw(&dsn, "UPDATE sender_lease SET holder = NULL, phase = NULL");
        let _ = a.decide_trial(2).unwrap();
        assert_eq!(a.get_state(), CoordState::Idle);
    }

    /// receiver observations land in the shared table for the sender
    #[test]
    fn receiver_observation_roundtrip() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = ROOM.lock().unwrap_or_else(|p| p.into_inner());
        let http = stub(vec![]);
        let cfg = ladder_cfg(json!({}));
        let a = fresh_room(&dsn, &cfg, http, false);
        let mut obs = Map::new();
        obs.insert("objective_0".to_string(), json!(0.75));
        a.record_receiver_observation(42, &obs, Some("probe_push"));
        let mut client = postgres::Client::connect(&dsn, postgres::NoTls).unwrap();
        let row = client
            .query_one(
                "SELECT receiver_id, trial_num, objective_values, lease_phase \
                 FROM receiver_observations ORDER BY id DESC LIMIT 1",
                &[],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "tender-a");
        assert_eq!(row.get::<_, i32>(1), 42);
        assert_eq!(
            row.get::<_, serde_json::Value>(2),
            json!({"objective_0": 0.75})
        );
        assert_eq!(
            row.get::<_, Option<String>>(3).as_deref(),
            Some("probe_push")
        );
    }

    /// MAX_HOLD_TRIALS still bounds a parked receiver (the python cap)
    #[test]
    fn hold_caps_at_max() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = ROOM.lock().unwrap_or_else(|p| p.into_inner());
        let http = stub(vec![]);
        let cfg = ladder_cfg(json!({}));
        let a = fresh_room(&dsn, &cfg, http, true);
        raw(
            &dsn,
            "UPDATE sender_lease SET holder = 'tender-b', phase = 'hold', \
             last_heartbeat = NOW()",
        );
        // the Idle→Hold entry resets the hold counter — seed AFTER the
        // first decide so the cap is one trial away
        let _ = a.decide_trial(1).unwrap();
        assert_eq!(a.get_state(), CoordState::Hold);
        *a.hold_count.borrow_mut() = MAX_HOLD_TRIALS;
        let _ = a.decide_trial(2).unwrap();
        assert_eq!(a.get_state(), CoordState::Idle);
    }
}
