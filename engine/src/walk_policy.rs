//! Stateless walk policy over causal's notebook page — the port of
//! `engine/walk_policy.py`.
//!
//! The walk is a pure function of the view causal reports — no RAM plan,
//! nothing to lose at invocation boundaries (the seed-48 amnesia defect).
//! Each step: fetch the notebook page for the param; honor the anchor
//! prefix; otherwise steer to the midpoint of the highest-priced unresolved
//! bracket across ALL curves of the param (listeners + self), each price
//! judged against its own bars (jump x width vs the gap's own endpoint
//! fuzz). Nothing priced above its bar -> the param is done at this floor.
//!
//! Deterministic tie-breaks for equal prices: wider bracket, then closer to
//! the range center, then higher level. All-equal prices degrade to a
//! geometric sweep — the safe fallback.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

use crate::coverage_walk::{round9, Bounds, ParamWalkStatus};

pub type BTreeStatus = std::collections::BTreeMap<String, ParamWalkStatus>;

/// The notebook page could not be read THIS call — transient class
/// only (timeout, connection refused/reset). A wrong page (HTTP error
/// status) stays loud: that is a config break, not a blip.
#[derive(Debug, Clone, PartialEq)]
pub enum WalkError {
    /// transient — the caller retries next trial (park, never die)
    Unavailable(String),
    /// HTTP error status or malformed page — the invocation dies
    /// loudly, exactly like the python's uncaught HTTPError/KeyError
    Loud(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Unavailable(m) => write!(f, "view unavailable: {m}"),
            WalkError::Loud(m) => write!(f, "walk error: {m}"),
        }
    }
}

/// How the notebook is reached. One mechanism behind both users (the
/// walk's fixed 2s view read and the coordinator's adaptive-timeout
/// calls): `call(method, url, payload, timeout)`.
///
/// Error classes mirror urllib's split the python leans on:
/// [`HttpError::Status`] = HTTPError (a wrong page — loud),
/// [`HttpError::Dark`] = timeouts/refused/reset (transient — dark).
#[derive(Debug, Clone, PartialEq)]
pub enum HttpError {
    Status(u16),
    Dark(String),
}

pub trait HttpTransport: Send + Sync {
    fn call(
        &self,
        method: &str,
        url: &str,
        payload: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, HttpError>;
}

/// The production transport: reqwest blocking, the sequential body's
/// own cadence. A 2 s guard on view reads is the caller's choice (the
/// python `_urllib_transport` receipt, same number); the coordinator's
/// causal calls pass their own adaptive timeout.
#[derive(Default)]
pub struct ReqwestTransport;

impl HttpTransport for ReqwestTransport {
    fn call(
        &self,
        method: &str,
        url: &str,
        payload: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, HttpError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| HttpError::Dark(e.to_string()))?;
        let mut req = match method {
            "GET" => client.get(url),
            "POST" => client.post(url),
            m => return Err(HttpError::Dark(format!("unsupported method {m}"))),
        };
        if let Some(p) = payload {
            req = req.json(p);
        }
        let resp = req.send().map_err(|e| {
            // reqwest folds timeouts and connection failures into one
            // error class — both are the transient/dark family here.
            HttpError::Dark(e.to_string())
        })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // urllib raises HTTPError for every non-2xx; the walk
            // treats them as loud (config break, not blip).
            return Err(HttpError::Status(status));
        }
        resp.json::<Value>()
            .map_err(|e| HttpError::Dark(format!("unparseable body: {e}")))
    }
}

/// Stateless walk policy over causal's notebook page.
pub struct WalkPolicy {
    url: String,
    group: String,
    systemtender: String,
    depth: i64,
    bounds: Bounds,
    transport: std::sync::Arc<dyn HttpTransport>,
    rr: usize,
}

impl WalkPolicy {
    pub fn new(
        causal_url: &str,
        group_id: &str,
        systemtender_id: &str,
        refinement_depth: i64,
        param_bounds: &Bounds,
        transport: std::sync::Arc<dyn HttpTransport>,
    ) -> Self {
        WalkPolicy {
            url: causal_url.trim_end_matches('/').to_string(),
            group: group_id.to_string(),
            systemtender: systemtender_id.to_string(),
            depth: refinement_depth,
            bounds: param_bounds.clone(),
            transport,
            rr: 0,
        }
    }

    /// Refinement depth (carried for status callers; the floor itself
    /// lives in causal's notebook, not here).
    pub fn depth(&self) -> i64 {
        self.depth
    }

    // ── notebook IO ──────────────────────────────────────────────────

    pub fn view(&self, param: &str) -> Result<Value, WalkError> {
        let url = format!(
            "{}/walk-view/{}?param={}&group={}",
            self.url, self.systemtender, param, self.group
        );
        match self
            .transport
            .call("GET", &url, None, Duration::from_secs(2))
        {
            Ok(v) => Ok(v),
            Err(HttpError::Status(code)) => {
                // wrong page — loud (config break, not blip)
                Err(WalkError::Loud(format!("view http {code}")))
            }
            Err(HttpError::Dark(e)) => Err(WalkError::Unavailable(format!("view io: {e}"))),
        }
    }

    // ── the policy ───────────────────────────────────────────────────

    /// Midpoint of the fattest unresolved bracket across ALL the
    /// param's curves (listeners + self); None when everything left
    /// is priced out (its own bars say nothing hides there).
    fn fattest_bracket_midpoint(view: &Value, center: f64) -> Result<Option<f64>, WalkError> {
        let mut best_key: Option<(f64, f64, f64, f64)> = None;
        let mut best_mid: Option<f64> = None;
        for curve in curves_of(view) {
            for g in gaps_of(curve) {
                if !g
                    .get("unresolved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    continue;
                }
                let ignorance = g
                    .get("ignorance")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(f64::INFINITY);
                let bars_sum = g.get("bars_sum").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if ignorance <= bars_sum {
                    continue; // priced out
                }
                // required keys — python KeyErrors (dies loudly) if a
                // gap lacks its levels; same contract here
                let from = g
                    .get("from_level")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| WalkError::Loud("gap missing from_level".into()))?;
                let to = g
                    .get("to_level")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| WalkError::Loud("gap missing to_level".into()))?;
                let mid = (from + to) / 2.0;
                let width = to - from;
                let key = (ignorance, width, -(mid - center).abs(), mid);
                let better = match &best_key {
                    None => true,
                    Some(bk) => {
                        key.partial_cmp(bk).unwrap_or(std::cmp::Ordering::Equal)
                            == std::cmp::Ordering::Greater
                    }
                };
                if better {
                    best_key = Some(key);
                    best_mid = Some(mid);
                }
            }
        }
        Ok(best_mid)
    }

    fn next_level(&self, name: &str) -> Result<Option<f64>, WalkError> {
        let (lo, hi, is_int) = self.bounds[name];
        // The notebook is authoritative: if it is unreachable the walk
        // FAILS LOUDLY here (the invocation dies, the next one retries)
        // — it never guesses, and never silently freezes at the anchors.
        let view = self.view(name)?;
        let mut measured: HashSet<u64> = HashSet::new();
        for curve in curves_of(&view) {
            for lv in levels_of(curve) {
                let lv = lv
                    .as_f64()
                    .ok_or_else(|| WalkError::Loud("non-numeric level in view".into()))?;
                measured.insert(round9(lv).to_bits());
            }
        }
        let unbanked = |level: f64| !measured.contains(&round9(level).to_bits());

        // Prefix contract: anchors for pricing, derived from the notebook
        // (not from RAM) — a restarted walker never re-walks them.
        let center = (lo + hi) / 2.0;
        for anchor in [(lo + hi) / 2.0, hi, lo] {
            if unbanked(anchor) {
                return Ok(Some(anchor));
            }
        }

        // Compass: midpoint of the fattest unresolved bracket anywhere on
        // this param's curves (listeners + self).
        let mid = Self::fattest_bracket_midpoint(&view, center)?;
        Ok(mid.map(|m| if is_int { m.round() } else { m }))
    }

    /// Next (param, level), or None when every eligible param is done.
    /// `skip` holds retired param names (the two-key verdict's output).
    pub fn next_probe(
        &mut self,
        skip: &HashSet<String>,
    ) -> Result<Option<(String, f64)>, WalkError> {
        let eligible: Vec<&String> = self.bounds.keys().filter(|n| !skip.contains(*n)).collect();
        if eligible.is_empty() {
            return Ok(None);
        }
        for _ in 0..eligible.len() {
            let name = eligible[self.rr % eligible.len()];
            self.rr += 1;
            if let Some(level) = self.next_level(name)? {
                return Ok(Some((name.clone(), level)));
            }
        }
        Ok(None)
    }

    /// NOTE (python parity): this is next_probe in disguise — it
    /// ADVANCES the round-robin exactly like the python
    /// `can_probe = self.next_probe(...) is not None`.
    pub fn can_probe(&mut self, skip: &HashSet<String>) -> Result<bool, WalkError> {
        Ok(self.next_probe(skip)?.is_some())
    }

    /// Descend a floor: one param, or every bounded param when None.
    /// Transport failures propagate raw (the python refine has no
    /// catch — a dark refine kills the invocation; the worker's
    /// per-trial error handling contains it).
    pub fn refine(&self, param: Option<&str>) -> Result<(), WalkError> {
        let params: Vec<String> = match param {
            Some(p) => vec![p.to_string()],
            None => self.bounds.keys().cloned().collect(),
        };
        for p in params {
            let url = format!("{}/walk-view/refine", self.url);
            let payload = serde_json::json!({
                "group_id": self.group,
                "sender_id": self.systemtender,
                "probe_param": p,
            });
            // raw flight — the python refine has no catch, so every
            // transport failure class kills the invocation here
            if let Err(e) =
                self.transport
                    .call("POST", &url, Some(&payload), Duration::from_secs(2))
            {
                return Err(WalkError::Loud(format!("refine transport: {e:?}")));
            }
        }
        Ok(())
    }

    /// Per-param walk state for the CHAR PROGRESS trail. A param whose
    /// view fails (any error) is skipped — the python catches bare
    /// `Exception` per param and continues.
    pub fn status(&self) -> BTreeStatus {
        let mut out = BTreeStatus::new();
        for (name, (lo, hi, is_int)) in &self.bounds {
            let view = match self.view(name) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut levelset: Vec<f64> = Vec::new();
            for curve in curves_of(&view) {
                for lv in levels_of(curve) {
                    if let Some(lv) = lv.as_f64() {
                        levelset.push(round9(lv));
                    }
                }
            }
            levelset.sort_by(|a, b| a.partial_cmp(b).unwrap());
            levelset.dedup_by(|a, b| a.to_bits() == b.to_bits());
            let level = view
                .get("refinement_level")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let (step, total) = if *is_int {
                (1.0, hi.round() as i64 - lo.round() as i64 + 1)
            } else {
                let step = (hi - lo) / 2f64.powi((level + 2) as i32);
                let total = ((hi - lo) / step).round() as i64 + 1;
                (step, total)
            };
            out.insert(
                name.clone(),
                ParamWalkStatus {
                    step,
                    levels_total: Some(total),
                    levels_measured: levelset.len(),
                    levels: levelset,
                },
            );
        }
        out
    }
}

fn curves_of(view: &Value) -> &[Value] {
    view.get("curves")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

fn gaps_of(curve: &Value) -> &[Value] {
    curve
        .get("gaps")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

fn levels_of(curve: &Value) -> &[Value] {
    curve
        .get("levels")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Recording stub transport — the python tests' injected callable.
    /// Responses are listed in CALL ORDER (front of the queue first).
    #[derive(Clone)]
    struct StubTransport {
        queue: std::sync::Arc<Mutex<Vec<Result<Value, HttpError>>>>,
        calls: std::sync::Arc<Mutex<Vec<(String, String)>>>,
    }

    impl StubTransport {
        fn new(responses: Vec<Result<Value, HttpError>>) -> Self {
            let mut q = responses;
            q.reverse(); // pop() from the end = call order
            StubTransport {
                queue: std::sync::Arc::new(Mutex::new(q)),
                calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl HttpTransport for StubTransport {
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
            self.queue
                .lock()
                .unwrap()
                .pop()
                .expect("stub transport exhausted")
        }
    }

    fn bounds() -> Bounds {
        [("p0".to_string(), (0.0, 100.0, false))]
            .into_iter()
            .collect()
    }

    fn policy(t: StubTransport) -> WalkPolicy {
        WalkPolicy::new(
            "http://causal.test/",
            "g",
            "t-1",
            3,
            &bounds(),
            std::sync::Arc::new(t),
        )
    }

    fn view_with_levels(levels: &[f64]) -> Result<Value, HttpError> {
        Ok(json!({
            "curves": [{"levels": levels}],
            "refinement_level": 0
        }))
    }

    #[test]
    fn anchor_prefix_mid_hi_lo() {
        // empty notebook: anchors come back in prefix order
        let t = StubTransport::new(vec![
            view_with_levels(&[]),
            view_with_levels(&[50.0]),
            view_with_levels(&[50.0, 100.0]),
        ]);
        let mut p = policy(t);
        let skip = HashSet::new();
        assert_eq!(p.next_probe(&skip).unwrap().unwrap().1, 50.0);
        assert_eq!(p.next_probe(&skip).unwrap().unwrap().1, 100.0);
        assert_eq!(p.next_probe(&skip).unwrap().unwrap().1, 0.0);
    }

    #[test]
    fn anchors_derived_from_notebook_not_ram() {
        // the notebook says 50 is banked → the first anchor is skipped;
        // a restarted walker never re-walks it
        let t = StubTransport::new(vec![view_with_levels(&[50.0])]);
        let mut p = policy(t);
        let skip = HashSet::new();
        assert_eq!(p.next_probe(&skip).unwrap().unwrap().1, 100.0);
    }

    #[test]
    fn compass_steers_to_fattest_bracket_midpoint() {
        let view = json!({
            "curves": [{
                "levels": [50.0, 100.0, 0.0],
                "gaps": [
                    {"unresolved": true, "ignorance": 1.0, "bars_sum": 0.1,
                     "from_level": 50.0, "to_level": 75.0},
                    {"unresolved": true, "ignorance": 3.0, "bars_sum": 0.1,
                     "from_level": 0.0, "to_level": 50.0}
                ]
            }]
        });
        let t = StubTransport::new(vec![Ok(view)]);
        let mut p = policy(t);
        let skip = HashSet::new();
        // higher-priced bracket wins (ignorance 3 > 1) → midpoint 25
        assert_eq!(p.next_probe(&skip).unwrap().unwrap().1, 25.0);
    }

    #[test]
    fn priced_out_gaps_do_not_steer() {
        // every gap priced under its own bars → the param is done
        let view = json!({
            "curves": [{
                "levels": [50.0, 100.0, 0.0],
                "gaps": [
                    {"unresolved": true, "ignorance": 0.05, "bars_sum": 0.1,
                     "from_level": 50.0, "to_level": 100.0}
                ]
            }]
        });
        let t = StubTransport::new(vec![Ok(view.clone()), Ok(view)]);
        let mut p = policy(t);
        let skip = HashSet::new();
        assert!(p.next_probe(&skip).unwrap().is_none());
        assert!(!p.can_probe(&skip).unwrap());
    }

    #[test]
    fn dark_view_is_unavailable_status_is_loud() {
        let t = StubTransport::new(vec![Err(HttpError::Dark("refused".into()))]);
        let p = policy(t);
        assert!(matches!(p.view("p0"), Err(WalkError::Unavailable(_))));
        let t = StubTransport::new(vec![Err(HttpError::Status(404))]);
        let p = policy(t);
        assert!(matches!(p.view("p0"), Err(WalkError::Loud(_))));
    }

    #[test]
    fn refine_posts_per_param() {
        let t = StubTransport::new(vec![Ok(json!({})), Ok(json!({}))]);
        let mut b = Bounds::new();
        b.insert("a".to_string(), (0.0, 10.0, false));
        b.insert("b".to_string(), (0.0, 10.0, false));
        let p = WalkPolicy::new("http://c", "g", "s", 3, &b, std::sync::Arc::new(t.clone()));
        p.refine(None).unwrap();
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1.ends_with("/walk-view/refine"));
    }

    #[test]
    fn status_skips_dark_params_and_derives_floors() {
        let view = json!({
            "curves": [{"levels": [50.0, 12.5]}],
            "refinement_level": 0
        });
        let t = StubTransport::new(vec![Ok(view)]);
        let p = policy(t);
        let st = p.status();
        assert_eq!(st["p0"].levels_measured, 2);
        assert_eq!(st["p0"].levels, vec![12.5, 50.0]);
        // level 0 → step = (100-0)/2^(0+2) = 25
        assert_eq!(st["p0"].step, 25.0);
        assert_eq!(st["p0"].levels_total, Some(5));
    }

    fn single_param_bounds(lo: f64, hi: f64, is_int: bool) -> Bounds {
        [("p0".to_string(), (lo, hi, is_int))].into_iter().collect()
    }

    fn policy_over(t: StubTransport, bounds: &Bounds) -> WalkPolicy {
        WalkPolicy::new(
            "http://causal.test/",
            "g",
            "t-1",
            3,
            bounds,
            std::sync::Arc::new(t),
        )
    }

    #[test]
    fn walk_error_renders_its_two_words() {
        assert_eq!(
            WalkError::Unavailable("x".into()).to_string(),
            "view unavailable: x"
        );
        assert_eq!(WalkError::Loud("y".into()).to_string(), "walk error: y");
    }

    #[test]
    fn depth_carries_the_refinement_depth() {
        // 3 is none of 0, 1, -1 — the carried depth must survive
        let p = policy(StubTransport::new(vec![]));
        assert_eq!(p.depth(), 3);
    }

    #[test]
    fn equal_ignorance_the_wider_bracket_wins() {
        // key order: ignorance first, then WIDTH as a difference
        // to-minus-from. Three equal-priced brackets with widths
        // 2 / 8 / 1 — only the difference crowns the 1..9 one
        let view = json!({
            "curves": [{
                "levels": [],
                "gaps": [
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 1000.0, "to_level": 1002.0},
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 1.0, "to_level": 9.0},
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 0.1, "to_level": 1.1}
                ]
            }]
        });
        // a product instead crowns the 1000..1002 one, a quotient the
        // 0.1..1.1 one
        let mid = WalkPolicy::fattest_bracket_midpoint(&view, 0.0)
            .unwrap()
            .unwrap();
        assert_eq!(mid, 5.0);
    }

    #[test]
    fn equal_price_the_closer_midpoint_wins() {
        // third key: NEGATED distance to center, so closer wins. With
        // center -100 the mid-14 bracket is closer (114 < 134); a
        // dropped or plus-replaced minus in (mid - center) measures
        // 86 vs 66 and crowns the mid-34 one
        let view = json!({
            "curves": [{
                "levels": [],
                "gaps": [
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 10.0, "to_level": 18.0},
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 30.0, "to_level": 38.0}
                ]
            }]
        });
        let mid = WalkPolicy::fattest_bracket_midpoint(&view, -100.0)
            .unwrap()
            .unwrap();
        assert_eq!(mid, 14.0);
    }

    #[test]
    fn the_distance_is_a_difference_not_a_ratio() {
        // |m - c| must SUBTRACT: with center -60, mid -50 is closer
        // (10 vs 70). A quotient |m / c| inverts the order
        // (0.83 vs 0.17) and crowns mid 10 instead
        let view = json!({
            "curves": [{
                "levels": [],
                "gaps": [
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": -54.0, "to_level": -46.0},
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 6.0, "to_level": 14.0}
                ]
            }]
        });
        let mid = WalkPolicy::fattest_bracket_midpoint(&view, -60.0)
            .unwrap()
            .unwrap();
        assert_eq!(mid, -50.0);
    }

    #[test]
    fn compass_center_is_half_the_range_sum() {
        // center = (lo + hi) / 2: bounds 3..100 put it at 51.5, closer
        // to the mid-100 bracket (48.5 vs 51.5). A modulo or a
        // difference in the center arithmetic lands at 1.0 or -48.5
        // and crowns the mid-0 bracket instead
        let view = json!({
            "curves": [{
                "levels": [51.5, 100.0, 3.0],
                "gaps": [
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 96.0, "to_level": 104.0},
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": -4.0, "to_level": 4.0}
                ]
            }]
        });
        let t = StubTransport::new(vec![Ok(view)]);
        let p = policy_over(t, &single_param_bounds(3.0, 100.0, false));
        // anchors 51.5 / 100 / 3 are all banked → the compass decides
        assert_eq!(p.next_level("p0").unwrap(), Some(100.0));
    }

    #[test]
    fn compass_center_is_not_the_geometric_mean() {
        // same bounds, brackets at mid 250 and mid 25: the true center
        // 51.5 keeps the compass on 25 (26.5 vs 198.5), while a
        // product center — (lo*hi)/2 = 150 or (lo+hi)*2 = 206 —
        // flips to the far bracket
        let view = json!({
            "curves": [{
                "levels": [51.5, 100.0, 3.0],
                "gaps": [
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 246.0, "to_level": 254.0},
                    {"unresolved": true, "ignorance": 2.0, "bars_sum": 0.1,
                     "from_level": 21.0, "to_level": 29.0}
                ]
            }]
        });
        let t = StubTransport::new(vec![Ok(view)]);
        let p = policy_over(t, &single_param_bounds(3.0, 100.0, false));
        assert_eq!(p.next_level("p0").unwrap(), Some(25.0));
    }

    #[test]
    fn round_robin_advances_past_done_params() {
        // a is done (anchors banked, gaps priced out), b still has its
        // first anchor. The cursor must ADVANCE past a — a
        // multiplication in its place pins the walk on a forever
        let done = json!({
            "curves": [{
                "levels": [50.0, 100.0, 0.0],
                "gaps": [
                    {"unresolved": true, "ignorance": 0.05, "bars_sum": 0.1,
                     "from_level": 50.0, "to_level": 100.0}
                ]
            }]
        });
        let t = StubTransport::new(vec![Ok(done), view_with_levels(&[])]);
        let mut b = Bounds::new();
        b.insert("a".to_string(), (0.0, 100.0, false));
        b.insert("b".to_string(), (0.0, 100.0, false));
        let mut p = policy_over(t, &b);
        let skip = HashSet::new();
        assert_eq!(p.next_probe(&skip).unwrap(), Some(("b".to_string(), 50.0)));
    }

    #[test]
    fn can_probe_is_true_when_work_remains() {
        // an empty notebook leaves the first anchor unbanked — work
        // remains, so the verdict is true, never a hardcoded false
        let t = StubTransport::new(vec![view_with_levels(&[])]);
        let mut p = policy(t);
        let skip = HashSet::new();
        assert!(p.can_probe(&skip).unwrap());
    }

    #[test]
    fn int_total_spans_the_rounded_range_inclusive() {
        // integer dial 2..10: step 1, total = 10 - 2 + 1 = 9 points
        let t = StubTransport::new(vec![view_with_levels(&[2.0, 10.0])]);
        let p = policy_over(t, &single_param_bounds(2.0, 10.0, true));
        let st = p.status();
        assert_eq!(st["p0"].step, 1.0);
        assert_eq!(st["p0"].levels_total, Some(9));
    }

    #[test]
    fn float_step_and_total_from_a_nonzero_floor() {
        // float dial 10..20 at floor 0: step = (20-10)/2^2 = 2.5,
        // total = (20-10)/2.5 + 1 = 5 — a zero floor hides the
        // difference between hi-lo and hi+lo, this one cannot
        let t = StubTransport::new(vec![view_with_levels(&[10.0, 20.0])]);
        let p = policy_over(t, &single_param_bounds(10.0, 20.0, false));
        let st = p.status();
        assert_eq!(st["p0"].step, 2.5);
        assert_eq!(st["p0"].levels_total, Some(5));
    }
}
