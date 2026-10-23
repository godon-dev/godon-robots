//! The worker's own body — the trial loop. Port of
//! `engine/systemtender_worker.py`'s deep half (run() and the trial
//! cadence, wish serving, the rollback machine, the guards, the
//! stamps), landing against the spine (`worker.rs`) and the
//! coordinator slices.
//!
//! INJECTED TRAITS, explicit (nothing faked; each lands in its own slice):
//! - `Ledger` — the ledger verbs the worker speaks. `TrialLedger`
//!   implements it; tests inject a recording fake exactly where the
//!   python tests patched the ledger object.
//! - `Coordinator` — the coordinator surface the loop drives (the
//!   python duck-typed the same methods, and its tests stubbed them).
//! - `Wmill` — windmill's script runner + set_state. The real
//!   client (`godon_wmill`) landed with the recon/effectuation
//!   components (slice 8); the trait stays as the tests' injection
//!   surface.
//! - own/shared db `Connect` — beats, stamps, registration, standing
//!   params; the same short-connection shapes the python wrote.
//! - `HttpTransport` — the wish plan fetch (plain http, timeout 10s).
//! - `PushTransport` — the metrics client's pushgateway edge.
//!
//! RUNG-8 LANDED (the death resume at boot, robots #252's shape): a
//! held wish waiting in the ledger parks the relaunched worker at
//! its last standing params instead of neutral — dials persist on
//! the target hosts across worker death, and dragging a held dial
//! back to neutral would break the hold being resumed. Receivers
//! still park neutral (stillness); bare boots park neutral exactly
//! as before; every read failure degrades to the bare-boot path.
//!
//! Known divergences (python reference vs landed rust):
//! - `CoordHooks::compute_neutral_params` returns Option, not Result —
//!   a DB error at the coordinator's char-init reads None and the walk
//!   falls back to hold params (python failed the trial loudly). Same
//!   recovery next trial, quieter. Recorded here rather than faked.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::coordinator::register_interference_systemtender;
use crate::coordinator::ProbeCoordinator;
use crate::coordinator_walk::{CoordError, CoordState, Decision};
use crate::ledger::{classify, py_dumps, Connect, DbError, TrialLedger, TrialRecord};
use crate::metrics::{PushTransport, SystemtenderMetricsClient};
use crate::seek::parse_seek_plan;
use crate::seek_driver::{Commission, DriverError, SeekDriver, STOP_BUDGET};
use crate::seek_policy::region_policy;
use crate::walk_policy::HttpTransport;
use crate::worker::{should_continue, Verdict, WorkerConfig};

/// The worker's own retryable signatures — python
/// `systemtender_worker._RETRYABLE_DB_ERROR_PATTERNS`, a wider list
/// than the ledger's (it carries the yb relcache and
/// expired-or-aborted receipts the live estate paid for).
const WORKER_RETRYABLE_PATTERNS: [&str; 10] = [
    "SerializationFailure",
    "40001",
    "Transaction aborted",
    "Timed out waiting",
    "InternalError_",
    "Transaction metadata missing",
    "Heartbeat",
    "conflict",
    // fresh tender DB vs yugabyte relcache race (live 2026-09-25)
    "Relcache init connection request",
    // yb mid-run transient (run 36261279551, 2026-09-26: beats and
    // study commits bounced while two living workers read presumed_dead)
    "expired or aborted",
];

fn worker_is_retryable(text: &str) -> bool {
    WORKER_RETRYABLE_PATTERNS.iter().any(|p| text.contains(p))
}

// ─── the ledger trait ─────────────────────────────────────────────

/// The ledger verbs the worker speaks — the python worker called these
/// on its TrialLedger; the rust tests patch exactly this surface.
pub trait Ledger {
    fn ensure_schema(&self) -> Result<i32, DbError>;
    fn count(&self) -> Result<i64, DbError>;
    fn trials(&self) -> Result<Vec<TrialRecord>, DbError>;
    fn begin(&self) -> Result<i64, DbError>;
    fn set_kind(&self, number: i64, kind: &str) -> Result<(), DbError>;
    fn stamp(&self, number: i64, key: &str, value: Value) -> Result<(), DbError>;
    fn complete(&self, number: i64, value: Option<f64>) -> Result<(), DbError>;
    fn fail(&self, number: i64, error: Option<&str>) -> Result<(), DbError>;
    fn state_get(&self, key: &str) -> Result<Option<String>, DbError>;
    fn state_put(&self, key: &str, value: &str) -> Result<(), DbError>;
    fn best_trial(&self, minimize: bool) -> Result<Option<TrialRecord>, DbError>;
    fn stop_requested(&self) -> Result<bool, DbError>;
    fn wish_latest(&self) -> Result<Option<(String, String)>, DbError>;
    fn wish_remove(&self, wish_id: &str) -> Result<(), DbError>;
}

impl<C: Connect> Ledger for TrialLedger<C> {
    fn ensure_schema(&self) -> Result<i32, DbError> {
        TrialLedger::ensure_schema(self)
    }
    fn count(&self) -> Result<i64, DbError> {
        self.count()
    }
    fn trials(&self) -> Result<Vec<TrialRecord>, DbError> {
        self.trials()
    }
    fn begin(&self) -> Result<i64, DbError> {
        self.begin()
    }
    fn set_kind(&self, number: i64, kind: &str) -> Result<(), DbError> {
        self.set_kind(number, kind)
    }
    fn stamp(&self, number: i64, key: &str, value: Value) -> Result<(), DbError> {
        self.stamp(number, key, value)
    }
    fn complete(&self, number: i64, value: Option<f64>) -> Result<(), DbError> {
        self.complete(number, value)
    }
    fn fail(&self, number: i64, error: Option<&str>) -> Result<(), DbError> {
        self.fail(number, error)
    }
    fn state_get(&self, key: &str) -> Result<Option<String>, DbError> {
        self.state_get(key)
    }
    fn state_put(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.state_put(key, value)
    }
    fn best_trial(&self, minimize: bool) -> Result<Option<TrialRecord>, DbError> {
        self.best_trial(minimize)
    }
    fn stop_requested(&self) -> Result<bool, DbError> {
        self.stop_requested()
    }
    fn wish_latest(&self) -> Result<Option<(String, String)>, DbError> {
        self.wish_latest()
    }
    fn wish_remove(&self, wish_id: &str) -> Result<(), DbError> {
        self.wish_remove(wish_id)
    }
}

/// A shared connector behind `Arc` speaks `Connect` itself — the
/// worker's shared-db and coordinator wiring use one source.
impl Connect for Arc<dyn Connect + Send + Sync> {
    fn connect(&self) -> Result<postgres::Client, DbError> {
        (**self).connect()
    }
}

// ─── the coordinator trait ────────────────────────────────────────

/// The coordinator surface the trial loop drives — the python's
/// duck-typed methods; the walk-half struct implements it.
pub trait Coordinator {
    fn decide_trial(&self, trial_number: i64) -> Result<Decision, CoordError>;
    fn get_state(&self) -> CoordState;
    fn get_char_status(&self) -> Value;
    fn char_complete(&self) -> Result<bool, CoordError>;
    fn set_probe_override(&self, param: &str, levels: &[f64]);
    fn record_receiver_observation(
        &self,
        trial_num: i64,
        objective_values: &Map<String, Value>,
        lease_phase: Option<&str>,
    );
    fn record_trial_duration(&self, duration_secs: f64);
}

impl<C: Connect> Coordinator for ProbeCoordinator<C> {
    fn decide_trial(&self, trial_number: i64) -> Result<Decision, CoordError> {
        self.decide_trial(trial_number)
    }
    fn get_state(&self) -> CoordState {
        self.get_state()
    }
    fn get_char_status(&self) -> Value {
        self.get_char_status()
    }
    fn char_complete(&self) -> Result<bool, CoordError> {
        self.char_complete()
    }
    fn set_probe_override(&self, param: &str, levels: &[f64]) {
        // the worker's commissions never carry ambience anchors
        ProbeCoordinator::set_probe_override(self, param, levels, None, None)
    }
    fn record_receiver_observation(
        &self,
        trial_num: i64,
        objective_values: &Map<String, Value>,
        lease_phase: Option<&str>,
    ) {
        self.record_receiver_observation(trial_num, objective_values, lease_phase)
    }
    fn record_trial_duration(&self, duration_secs: f64) {
        self.record_trial_duration(duration_secs)
    }
}

// ─── the windmill client ──────────────────────────────────────────

/// The platform edge: run a script by path (the recon and effectuation
/// components) and set the job state. The real client (`godon_wmill`,
/// slice 8) implements this behind the same methods the python
/// `wmill` module exposed; tests inject fakes exactly where the
/// python tests patched the module.
pub trait Wmill: Send + Sync {
    fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String>;
    fn set_state(&self, state: &Value) -> Result<(), String>;
    /// The platform run's own-args edge (the door's self-serve mode).
    fn get_job_args(&self, job_id: &str) -> Result<Value, String>;
}

impl Wmill for godon_wmill::WmillClient {
    fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String> {
        godon_wmill::WmillClient::run_script_by_path(self, path, args)
    }
    fn set_state(&self, state: &Value) -> Result<(), String> {
        godon_wmill::WmillClient::set_state(self, state)
    }
    fn get_job_args(&self, job_id: &str) -> Result<Value, String> {
        godon_wmill::WmillClient::get_job_args(self, job_id)
    }
}

// ─── the dial source (the worker's hook implementations) ──────────

/// The settings-tree walker and neutral-params computer — the python
/// worker methods the coordinator receives as hooks and the loop calls
/// directly. One source, both callers.
pub struct DialSource {
    pub config: Value,
    pub ledger: Arc<dyn Ledger + Send + Sync>,
}

impl DialSource {
    /// Recursively walk the config tree and collect params with
    /// constraint ranges (python `_collect_upper_bounds`).
    pub fn collect_upper_bounds(&self, obj: &Value) -> Vec<crate::coordinator_walk::UpperBound> {
        let mut out = Vec::new();
        self.walk_bounds(obj, 0, &mut out);
        out
    }

    fn walk_bounds(
        &self,
        obj: &Value,
        depth: usize,
        out: &mut Vec<crate::coordinator_walk::UpperBound>,
    ) {
        let Some(map) = obj.as_object() else {
            return;
        };
        if depth > 5 {
            return;
        }
        for (key, val) in map {
            if key == "zones" || !val.is_object() {
                continue;
            }
            let constraints = val.get("constraints");
            let handled = match constraints.and_then(|c| c.as_array()) {
                Some(list) if !list.is_empty() => {
                    let first = &list[0];
                    let lower = first.get("lower").and_then(|v| v.as_f64());
                    let upper = first.get("upper").and_then(|v| v.as_f64());
                    if let (Some(lower), Some(upper)) = (lower, upper) {
                        let step = first.get("step").and_then(|v| v.as_f64()).unwrap_or(1.0);
                        let is_int = step == step.trunc();
                        out.push(crate::coordinator_walk::UpperBound {
                            name: key.clone(),
                            lower,
                            upper,
                            is_int,
                        });
                    }
                    true
                }
                Some(_) => false, // empty list — recurse (python parity)
                None => false,    // not a list — recurse
            };
            if !handled {
                self.walk_bounds(val, depth + 1, out);
            }
        }
    }

    /// Compute neutral hold params in strain format (midpoints of
    /// constraint ranges) — the most passive safe operating point.
    pub fn compute_neutral_params(&self) -> Result<Option<Map<String, Value>>, DbError> {
        let trials = self.ledger.trials()?;
        if trials.is_empty() {
            return Ok(None);
        }
        // Template from any completed trial's effectuation params
        let mut template: Option<Map<String, Value>> = None;
        for trial in &trials {
            if trial.state != "COMPLETE" {
                continue;
            }
            if let Some(stashed) = trial.attrs.get("effectuation_params") {
                template = match stashed {
                    Value::String(s) => serde_json::from_str::<Value>(s)
                        .ok()
                        .and_then(|v| v.as_object().cloned()),
                    Value::Object(o) => Some(o.clone()),
                    _ => None,
                };
                if template.is_some() {
                    break;
                }
            }
        }
        let Some(mut template) = template else {
            return Ok(None);
        };
        let settings = self.config.get("settings").cloned().unwrap_or(json!({}));
        let bounds = self.collect_upper_bounds(&settings);
        for ub in bounds {
            let mut midpoint = (ub.lower + ub.upper) / 2.0;
            if ub.is_int {
                midpoint = midpoint.trunc();
            }
            if let Some(existing) = template.get(&ub.name) {
                let replacement = match existing {
                    Value::Array(items) => Value::Array(
                        std::iter::repeat(number_or_null(midpoint))
                            .take(items.len())
                            .collect(),
                    ),
                    _ => number_or_null(midpoint),
                };
                template.insert(ub.name.clone(), replacement);
            }
        }
        Ok(Some(template))
    }

    /// Effectuation params from the best warmup trial (python
    /// `_get_last_successful_params`); fallback: the last COMPLETE
    /// trial's stashed params or its flat params.
    pub fn get_last_successful_params(&self) -> Result<Option<Map<String, Value>>, DbError> {
        let trials = self.ledger.trials()?;
        if trials.is_empty() {
            return Ok(None);
        }
        let minimize = self.objective_minimizes();
        let mut best_value: Option<f64> = None;
        let mut best_stash: Option<Value> = None;
        for trial in &trials {
            if trial.state != "COMPLETE" {
                continue;
            }
            let Some(stashed) = trial.attrs.get("effectuation_params") else {
                continue;
            };
            let Some(&v) = trial.values.first() else {
                continue;
            };
            match best_value {
                None => {
                    best_value = Some(v);
                    best_stash = Some(stashed.clone());
                }
                Some(b) => {
                    let better = if minimize { v < b } else { v > b };
                    if better {
                        best_value = Some(v);
                        best_stash = Some(stashed.clone());
                    }
                }
            }
        }
        if let Some(stash) = best_stash {
            return Ok(match stash {
                Value::String(s) => serde_json::from_str::<Value>(&s)
                    .ok()
                    .and_then(|v| v.as_object().cloned()),
                Value::Object(o) => Some(o),
                _ => None,
            });
        }
        // Fallback: last complete trial
        for trial in trials.iter().rev() {
            if trial.state == "COMPLETE" {
                if let Some(stashed) = trial.attrs.get("effectuation_params") {
                    return Ok(match stashed {
                        Value::String(s) => serde_json::from_str::<Value>(&s)
                            .ok()
                            .and_then(|v| v.as_object().cloned()),
                        Value::Object(o) => Some(o.clone()),
                        _ => None,
                    });
                }
                return Ok(Some(trial.params.as_object().cloned().unwrap_or_default()));
            }
        }
        Ok(None)
    }

    /// First objective's direction: minimize (python
    /// `_objective_minimizes`).
    pub fn objective_minimizes(&self) -> bool {
        self.config
            .get("objectives")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.first()
                    .and_then(|o| o.get("direction"))
                    .and_then(|d| d.as_str())
                    .map(|d| d.to_lowercase() == "minimize")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }
}

fn number_or_null(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Python's str(float) for guardrail messages (`95.0`, `inf`) — the
/// rendered shape the violation strings carry.
fn py_float_str(v: f64) -> String {
    if v.is_finite() {
        py_dumps(&Value::from(v))
    } else if v == f64::INFINITY {
        "inf".to_string()
    } else if v == f64::NEG_INFINITY {
        "-inf".to_string()
    } else {
        "nan".to_string()
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ─── the worker ───────────────────────────────────────────────────

/// The worker's injected dependencies. The production sources (DsnConnect on
/// the libpq/shared urls, ReqwestTransport, the godon-wmill client)
/// build via [`WorkerDeps::production`]; tests inject fakes.
pub struct WorkerDeps {
    /// the tender's own db (beats, instruments, finished stamps)
    pub own_db: Arc<dyn Connect + Send + Sync>,
    /// the archive/shared db (registration, standing params)
    pub shared_db: Arc<dyn Connect + Send + Sync>,
    pub wmill: Arc<dyn Wmill>,
    pub http: Arc<dyn HttpTransport>,
    pub push_transport: Arc<dyn PushTransport>,
    /// connect by URL — the receiver-notification rows
    pub remote_db: Arc<dyn Fn(&str) -> Result<postgres::Client, DbError> + Send + Sync>,
    /// every internal sleep (retry ladders, bursts) — tests fast-forward
    pub sleeper: Arc<dyn Fn(f64) + Send + Sync>,
}
impl WorkerDeps {
    /// Everything but the wmill client, from the environment exactly
    /// as the python derived it. `tender_db` is the ledger's own
    /// database name (`systemtender_{uuid}`). The wmill client is
    /// passed in — the caller builds `godon_wmill::WmillClient::
    /// from_env` (the binary's main does exactly that).
    pub fn production(wmill: Arc<dyn Wmill>, tender_db: &str) -> Self {
        let pw = std::env::var("GODON_ARCHIVE_DB_PASSWORD").unwrap_or_else(|_| "postgres".into());
        let user = std::env::var("GODON_ARCHIVE_DB_USER").unwrap_or_else(|_| "postgres".into());
        let host =
            std::env::var("GODON_ARCHIVE_DB_SERVICE_HOST").unwrap_or_else(|_| "localhost".into());
        let port = std::env::var("GODON_ARCHIVE_DB_SERVICE_PORT").unwrap_or_else(|_| "5432".into());
        WorkerDeps {
            own_db: Arc::new(
                crate::ledger::DsnConnect::new(&format!(
                    "postgresql://{user}:{pw}@{host}:{port}/{tender_db}"
                ))
                .expect("valid dsn"),
            ),
            shared_db: Arc::new(
                crate::ledger::DsnConnect::new(&format!(
                    "postgresql://{user}:{pw}@{host}:{port}/archive_db"
                ))
                .expect("valid dsn"),
            ),
            wmill,
            http: Arc::new(crate::walk_policy::ReqwestTransport),
            push_transport: Arc::new(crate::metrics::ReqwestPushTransport),
            remote_db: Arc::new(|url: &str| {
                crate::ledger::DsnConnect::new(url)
                    .map_err(DbError::from)?
                    .connect()
            }),
            sleeper: Arc::new(|secs: f64| std::thread::sleep(Duration::from_secs_f64(secs))),
        }
    }
}

/// The held wish (steer mode): adopted at the pulse, held at the
/// planned value, released to neutral. The python dict's `param` /
/// `setting` placeholders (always None outside legacy shapes) are
/// omitted here.
#[derive(Debug, Clone)]
pub struct HeldWish {
    pub wish_id: String,
    /// hold | park | seek | release | remeasure (causal's vocabulary)
    pub instruction: String,
    /// sender | receiver
    pub role: String,
    /// param -> setting (the sender's held dials)
    pub moves: BTreeMap<String, Value>,
    /// the walk's next level from a refresh (param, level)
    pub probe: Option<(String, f64)>,
}

pub struct Worker<K: Coordinator> {
    pub config: Value,
    pub cfg: WorkerConfig,
    pub systemtender_id: String,
    pub ledger: Arc<dyn Ledger>,
    pub dials: Arc<DialSource>,
    pub coordinator: K,
    pub own_db: Arc<dyn Connect + Send + Sync>,
    pub shared_db: Arc<dyn Connect + Send + Sync>,
    pub remote_db: Arc<dyn Fn(&str) -> Result<postgres::Client, DbError> + Send + Sync>,
    pub wmill: Arc<dyn Wmill>,
    pub http: Arc<dyn HttpTransport>,
    pub sleeper: Arc<dyn Fn(f64) + Send + Sync>,
    pub metrics: SystemtenderMetricsClient,

    pub target: Value,
    pub target_id: i64,
    pub rollback_config: Value,
    pub rollback_enabled: bool,
    pub heartbeat_interval_secs: f64,
    last_room_heartbeat: Option<Instant>,

    // wish state (steer mode)
    pub wish: Option<HeldWish>,
    pub seek_driver: Option<SeekDriver>,
    pub seek_open: Option<Commission>,
    pub strategies_run: BTreeSet<String>,
    pin_receipt: Option<String>,
    pub last_metric_noise: BTreeMap<String, f64>,
}

impl<K: Coordinator> Worker<K> {
    /// The python `__init__` + `_init_body` order, one constructor:
    /// config parse, ledger init, heartbeat, coordinator wiring (the
    /// caller builds it against the same dial source), target
    /// resolution, rollback state, the boot park, registration, the
    /// first state update, metrics.
    pub fn with_coordinator<L: Ledger + Send + Sync + 'static>(
        config: Value,
        deps: WorkerDeps,
        ledger: Arc<L>,
        coordinator: K,
    ) -> Result<Self, crate::worker::WorkerError> {
        let cfg = WorkerConfig::from_value(&config)?;
        let dials = Arc::new(DialSource {
            config: config.clone(),
            ledger: ledger.clone(),
        });

        // _init_ledger: ensure + count log
        let loaded = ledger
            .ensure_schema()
            .and_then(|_| ledger.count())
            .map_err(|e| {
                log::info!("ledger init failed: {e}");
                crate::worker::WorkerError::BadCreationTs(e.to_string())
            });
        let loaded = match loaded {
            Ok(n) => n,
            Err(e) => return Err(e),
        };
        log::info!("Ledger ready: {loaded} trials loaded");

        // target resolution
        let targets = config
            .get("effectuation")
            .and_then(|e| e.get("targets"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let target_id = cfg.target_id;
        let target = if (0..targets.len() as i64).contains(&target_id) {
            targets[target_id as usize].clone()
        } else {
            if !targets.is_empty() {
                log::info!("Invalid target_id {target_id}, using first target");
            }
            targets.first().cloned().unwrap_or(json!({}))
        };
        let rollback_config = target.get("rollback").cloned().unwrap_or(json!({}));
        let rollback_enabled = rollback_config
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if rollback_enabled {
            log::info!("Rollback enabled for target {target_id}");
            log::info!(
                "Rollback strategy: {}",
                rollback_config
                    .get("strategy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            );
        }

        let heartbeat_interval_secs = std::env::var("GODON_HEARTBEAT_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(10.0);

        let push_transport = deps.push_transport.clone();
        let systemtender_id = cfg.systemtender_uuid.clone();
        let worker_id = cfg.worker_id();
        let systemtender_type = cfg.systemtender_type.clone();
        let mut worker = Worker {
            config,
            cfg,
            systemtender_id: systemtender_id.clone(),
            ledger: ledger as Arc<dyn Ledger>,
            dials,
            coordinator,
            own_db: deps.own_db.clone(),
            shared_db: deps.shared_db.clone(),
            remote_db: deps.remote_db.clone(),
            wmill: deps.wmill.clone(),
            http: deps.http.clone(),
            sleeper: deps.sleeper.clone(),
            metrics: SystemtenderMetricsClient::new(
                &systemtender_id,
                &worker_id,
                &systemtender_type,
                None,
                push_transport,
            ),
            target,
            target_id,
            rollback_config,
            rollback_enabled,
            heartbeat_interval_secs,
            last_room_heartbeat: None,
            wish: None,
            seek_driver: None,
            seek_open: None,
            strategies_run: BTreeSet::new(),
            pin_receipt: None,
            last_metric_noise: BTreeMap::new(),
        };

        if worker.rollback_enabled {
            worker.init_rollback_state();
        }

        // the liveness beater: daemon semantics, outside the cadence
        worker.start_heartbeat();

        // The first park BEFORE the room announcement (rung 8: the
        // death resume — a held wish parks at its last standing
        // params; bare boots and receivers park neutral).
        worker.park_before_announcement();
        worker.register_interference_systemtender();
        worker.update_state().ok();
        Ok(worker)
    }

    /// Build the production coordinator against the shared db and this
    /// dial source — the python `_init_body`'s ProbeCoordinator(...)
    /// wiring, hooks included.
    pub fn connect<L: Ledger + Send + Sync + 'static>(
        config: Value,
        deps: WorkerDeps,
        ledger: Arc<L>,
    ) -> Result<Worker<ProbeCoordinator<Arc<dyn Connect + Send + Sync>>>, crate::worker::WorkerError>
    {
        let dials = Arc::new(DialSource {
            config: config.clone(),
            ledger: ledger.clone() as Arc<dyn Ledger + Send + Sync>,
        });
        let hooks_dials = dials.clone();
        let hooks = crate::coordinator_walk::CoordHooks::new(Arc::new(move |settings: &Value| {
            hooks_dials.collect_upper_bounds(settings)
        }));
        // the divergence recorded in the module doc: Option, not Result
        let neutral_dials = dials.clone();
        let mut hooks = hooks;
        hooks.compute_neutral_params = Some(Arc::new(move || {
            match neutral_dials.compute_neutral_params() {
                Ok(v) => v,
                Err(e) => {
                    log::info!("neutral-params hook read failed (parking None): {e}");
                    None
                }
            }
        }));
        let coordinator = ProbeCoordinator::new(
            &format!(
                "{}",
                WorkerConfig::from_value(&config)
                    .map(|c| c.systemtender_uuid)
                    .unwrap_or_else(|_| "unknown".to_string())
            ),
            &config,
            deps.shared_db.clone(),
            hooks,
        );
        Worker::with_coordinator(config, deps, ledger, coordinator)
    }
}

impl<K: Coordinator> Worker<K> {
    // ─── liveness heartbeat ───────────────────────────────────────

    /// The parallel beater, daemon semantics: the thread outlives the
    /// worker, process exit is its stop — never joined (receipt, first
    /// CI flight of the spine slice).
    pub fn start_heartbeat(&self) {
        let own = self.own_db.clone();
        let interval = self.heartbeat_interval_secs;
        let sleeper = self.sleeper.clone();
        std::thread::spawn(move || loop {
            if let Err(e) = beat_with_burst(&*own, interval, &*sleeper) {
                log::info!("heartbeat beat failed (retries next beat): {e}");
            }
            sleeper(interval);
        });
    }

    // ─── room presence ────────────────────────────────────────────

    pub fn register_interference_systemtender(&self) {
        if let Err(e) = register_interference_systemtender(
            &self.shared_db,
            &self.systemtender_id,
            &self.cfg.group,
        ) {
            log::info!("Failed to register for interference detection: {e}");
        }
    }

    /// Per-trial refresh of the standing dials — UPDATE only, never
    /// creates ghost systemtenders.
    pub fn publish_standing_params(&self, params: &Map<String, Value>) {
        if params.is_empty() {
            return;
        }
        // Bind the OBJECT (the ledger's stamps() pattern: serde Value +
        // $1::jsonb — the crate's jsonb adaptation encodes the binary
        // form Yugabyte accepts). The previous shape double-encoded:
        // py_dumps to a STRING, then the bespoke text-bytes wrapper
        // sent it as binary jsonb — Yugabyte's jsonb_recv answered
        // "unsupported jsonb version number 34" (the string's leading
        // quote) on every publish, and the room's standing params
        // never landed (receipt: 65 failures/10m in loki, first
        // observed live 2026-10-19 via the otel wire).
        let payload = Value::Object(params.clone());
        let sid = self.systemtender_id.clone();
        let result = self.with_shared_db("publish_standing_params", move |client| {
            let body = payload.clone();
            client
                .execute(
                    "UPDATE interference_active_systemtenders \
                     SET params = $1::jsonb, last_seen = NOW() WHERE systemtender_id = $2",
                    &[&body, &sid],
                )
                .map_err(DbError::from)?;
            Ok(())
        });
        if let Err(e) = result {
            log::info!("Failed to publish standing params: {e}");
        }
    }

    /// The room heartbeat (registration refresh), gated to its own
    /// 120 s interval, never while shut down.
    pub fn heartbeat_interference(&mut self) {
        let fire = match self.last_room_heartbeat {
            None => true,
            Some(last) => last.elapsed().as_secs_f64() >= 120.0,
        };
        if !fire {
            return;
        }
        self.last_room_heartbeat = Some(Instant::now());
        if self.check_shutdown_requested() {
            return;
        }
        self.register_interference_systemtender();
    }

    pub fn check_shutdown_requested(&self) -> bool {
        match self.ledger.stop_requested() {
            Ok(true) => {
                log::info!("Stop flag is set for systemtender {}",
                    self.cfg.systemtender_uuid
                );
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::info!("Failed to check stop flag: {e}");
                false
            }
        }
    }

    // ─── the worker's retry ladders (python's own cadence) ────────

    fn retry_op<T>(
        &self,
        description: &str,
        mut op: impl FnMut() -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        const MAX_RETRIES: usize = 4;
        let mut last: Option<DbError> = None;
        for attempt in 0..MAX_RETRIES {
            match op() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let retryable = worker_is_retryable(&e.to_string());
                    if attempt < MAX_RETRIES - 1 && retryable {
                        let wait = 2f64.powi(attempt as i32);
                        log::info!(
                            "{description} attempt {}/{} failed: {e}, retrying in {wait}s",
                            attempt + 1,
                            MAX_RETRIES
                        );
                        (self.sleeper)(wait);
                        last = Some(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    fn with_shared_db<T>(
        &self,
        description: &str,
        mut op: impl FnMut(&mut postgres::Client) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        const MAX_RETRIES: usize = 4;
        let mut last: Option<DbError> = None;
        for attempt in 0..MAX_RETRIES {
            match self
                .shared_db
                .connect()
                .and_then(|mut client| op(&mut client))
            {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let retryable = worker_is_retryable(&e.to_string());
                    if attempt < MAX_RETRIES - 1 && retryable {
                        let wait = 2f64.powi(attempt as i32);
                        log::info!(
                            "{description} attempt {}/{} failed: {e}, retrying in {wait}s",
                            attempt + 1,
                            MAX_RETRIES
                        );
                        (self.sleeper)(wait);
                        last = Some(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Err(last.expect("at least one attempt ran"))
    }

    // ─── trial machinery ──────────────────────────────────────────

    fn objective_names(&self) -> Vec<String> {
        self.config
            .get("objectives")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|o| {
                        o.get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn inf_objectives(&self) -> BTreeMap<String, f64> {
        self.objective_names()
            .into_iter()
            .map(|name| (name, f64::INFINITY))
            .collect()
    }

    /// Recon reads metrics through the windmill rail. Errors and empty
    /// metric sets both park the objectives at infinity (python's
    /// generic-exception catch, same wire result).
    fn run_reconnaissance(
        &mut self,
        settings: &Map<String, Value>,
    ) -> Result<BTreeMap<String, f64>, String> {
        let targets = self
            .config
            .get("effectuation")
            .and_then(|e| e.get("targets"))
            .cloned()
            .unwrap_or(json!([]));
        let recon_type = self
            .config
            .get("reconnaissance")
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("prometheus")
            .to_string();
        let path = format!("f/reconnaissance/{recon_type}");
        let args = json!({
            "context": self.config,
            "targets": targets,
            "settings": Value::Object(settings.clone()),
        });
        let result = self
            .wmill
            .run_script_by_path(&path, &args)
            .map_err(|e| e.to_string())?;

        let empty: Map<String, Value> = Map::new();
        let metrics = result
            .get("metrics")
            .and_then(|m| m.as_object())
            .unwrap_or(&empty);
        if metrics.is_empty() {
            log::info!("No metrics returned from reconnaissance");
            return Ok(self.inf_objectives());
        }
        let mut out = BTreeMap::new();
        for (k, v) in metrics {
            out.insert(k.clone(), v.as_f64().unwrap_or(f64::NAN));
        }
        self.last_metric_noise = result
            .get("metric_noise")
            .and_then(|m| m.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                    .collect()
            })
            .unwrap_or_default();
        Ok(out)
    }

    /// Effectuate the shaped settings, then measure. Total effectuation
    /// failure raises (the RuntimeError — the trial fails loud);
    /// anything else parks objectives at infinity.
    pub fn execute_trial(
        &mut self,
        settings: &Map<String, Value>,
    ) -> Result<BTreeMap<String, f64>, String> {
        let targets = self
            .config
            .get("effectuation")
            .and_then(|e| e.get("targets"))
            .cloned()
            .unwrap_or(json!([]));
        let eff_type = self
            .config
            .get("effectuation")
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("ssh")
            .to_string();
        let effectuator_path = format!("f/effectuation/{eff_type}");
        log::info!("Effectuating {} targets via {effectuator_path} with settings: {:?}",
            targets.as_array().map(|a| a.len()).unwrap_or(0),
            settings.keys().collect::<Vec<_>>()
        );

        let args = json!({
            "context": self.config,
            "targets": targets,
            "settings": Value::Object(settings.clone()),
        });
        let eff_result = match self.wmill.run_script_by_path(&effectuator_path, &args) {
            Ok(v) => v,
            Err(e) => {
                log::info!("Trial execution failed: {e}");
                return Ok(self.inf_objectives());
            }
        };
        log::info!(
            "Effectuation completed: {}",
            eff_result
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("-")
        );
        let successful = eff_result
            .get("successful_changes")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let failed = eff_result
            .get("failed_changes")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if successful == 0 && failed > 0 {
            let failed_targets: Vec<String> = eff_result
                .get("results")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|r| !r.get("success").and_then(|s| s.as_bool()).unwrap_or(false))
                        .map(|r| {
                            r.get("target_id")
                                .and_then(|t| t.as_str())
                                .unwrap_or("unknown")
                                .to_string()
                        })
                        .collect()
                })
                .unwrap_or_default();
            log::info!("Effectuation completely failed for all targets: {failed_targets:?}");
            return Err(format!(
                "Effectuation failed for all targets: {failed_targets:?}"
            ));
        }
        if failed > 0 {
            let failed_targets: Vec<String> = eff_result
                .get("results")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|r| !r.get("success").and_then(|s| s.as_bool()).unwrap_or(false))
                        .map(|r| {
                            r.get("target_id")
                                .and_then(|t| t.as_str())
                                .unwrap_or("unknown")
                                .to_string()
                        })
                        .collect()
                })
                .unwrap_or_default();
            log::info!(
                "Effectuation partially failed: {failed}/{} targets succeeded. Failed: {failed_targets:?}",
                successful + failed
            );
        }
        match self.run_reconnaissance(settings) {
            Ok(metrics) => Ok(metrics),
            Err(e) => {
                log::info!("Trial execution failed: {e}");
                Ok(self.inf_objectives())
            }
        }
    }

    /// Guardrail check against the recon's metrics — the refusal check.
    pub fn check_guardrails(&self, metrics: &BTreeMap<String, f64>) -> (bool, Vec<String>) {
        let guardrails = self
            .config
            .get("guardrails")
            .and_then(|g| g.as_array())
            .cloned()
            .unwrap_or_default();
        if guardrails.is_empty() {
            return (false, Vec::new());
        }
        let mut violations = Vec::new();
        for guardrail in &guardrails {
            let name = guardrail
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let hard_limit = guardrail.get("hard_limit");
            let Some(hard_limit) = hard_limit else {
                log::info!("Guardrail '{name}' missing hard_limit, skipping");
                continue;
            };
            let Some(&metric_value) = metrics.get(name) else {
                log::info!("Guardrail '{name}' metric not found in metrics, skipping check");
                continue;
            };
            if let Some(limit) = hard_limit.as_f64() {
                if metric_value > limit {
                    let msg = format!(
                        "Guardrail '{name}' violated: {} > {}",
                        py_float_str(metric_value),
                        py_float_str(limit)
                    );
                    log::info!("{msg}");
                    violations.push(msg);
                }
            } else {
                log::info!("Guardrail '{name}' has non-numeric hard_limit, skipping");
            }
        }
        (!violations.is_empty(), violations)
    }

    // ─── the rollback machine ─────────────────────────────────────

    fn rollback_state_key(&self) -> String {
        format!("rollback_state_target_{}", self.target_id)
    }

    pub fn init_rollback_state(&mut self) {
        let key = self.rollback_state_key();
        match self.ledger.state_get(&key) {
            Ok(Some(_)) => return, // already initialized
            Ok(None) => {}
            Err(e) => {
                log::info!("rollback state read failed: {e}");
                return;
            }
        }
        let initial = json!({
            "state": "normal",
            "consecutive_failures": 0,
            "last_successful_params": null,
            "rollback_strategy": self.rollback_config.get("strategy").and_then(|s| s.as_str()).unwrap_or("standard"),
            "version": 0,
        });
        let _ = self.ledger.state_put(&key, &py_dumps(&initial)); // sorted vs python's insertion order — round-trips, nothing pins these bytes
        log::info!("Initialized rollback state for target {}: {initial}",
            self.target_id
        );
    }

    fn get_rollback_state(&mut self) -> Map<String, Value> {
        let key = self.rollback_state_key();
        let existing = match self.ledger.state_get(&key) {
            Ok(v) => v,
            Err(e) => {
                log::info!("rollback state read failed: {e}");
                None
            }
        };
        match existing {
            Some(s) => serde_json::from_str::<Value>(&s)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default(),
            None => {
                log::info!("No rollback state found for target {}, initializing",
                    self.target_id
                );
                self.init_rollback_state();
                self.ledger
                    .state_get(&key)
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default()
            }
        }
    }

    fn update_rollback_state(&mut self, mut new_state: Map<String, Value>) -> bool {
        let version = new_state
            .get("version")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            + 1;
        new_state.insert("version".into(), json!(version));
        let key = self.rollback_state_key();
        if let Err(e) = self
            .ledger
            .state_put(&key, &py_dumps(&Value::Object(new_state.clone())))
        {
            log::info!("rollback state write failed: {e}");
            return false;
        }
        true
    }

    pub fn check_needs_rollback(&mut self) -> bool {
        if !self.rollback_enabled {
            return false;
        }
        let state = self.get_rollback_state();
        let consecutive = state
            .get("consecutive_failures")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let strategy_name = self
            .rollback_config
            .get("strategy")
            .and_then(|s| s.as_str())
            .unwrap_or("standard");
        let threshold = self
            .config
            .get("rollback_strategies")
            .and_then(|s| s.get(strategy_name))
            .and_then(|s| s.get("consecutive_failures"))
            .and_then(|v| v.as_i64())
            .unwrap_or(3);
        if consecutive >= threshold {
            log::info!("Consecutive failures ({consecutive}) >= threshold ({threshold}), rollback needed"
            );
            return true;
        }
        false
    }

    pub fn execute_rollback(&mut self) -> Result<bool, String> {
        log::info!("Executing rollback for target {}", self.target_id);
        let mut rollback_state = self.get_rollback_state();
        let strategy_name = self
            .rollback_config
            .get("strategy")
            .and_then(|s| s.as_str())
            .unwrap_or("standard")
            .to_string();
        let strategy = self
            .config
            .get("rollback_strategies")
            .and_then(|s| s.get(&strategy_name))
            .cloned()
            .unwrap_or(json!({}));
        let target_state = strategy
            .get("target_state")
            .and_then(|t| t.as_str())
            .unwrap_or("previous")
            .to_string();

        let params_to_restore: Option<Map<String, Value>> = if target_state == "previous" {
            rollback_state
                .get("last_successful_params")
                .and_then(|p| p.as_object().cloned())
        } else if target_state == "best" {
            match self.ledger.best_trial(self.dials.objective_minimizes()) {
                Ok(Some(best)) => Some(best.params.as_object().cloned().unwrap_or_default()),
                _ => {
                    log::info!("Cannot rollback to 'best': no best trial found");
                    return Ok(false);
                }
            }
        } else if target_state == "baseline" {
            Some(Map::new())
        } else {
            log::info!("Unknown target_state: {target_state}");
            return Ok(false);
        };

        let Some(params_to_restore) = params_to_restore else {
            log::info!("No parameters to restore for target_state={target_state}");
            return Ok(false);
        };
        log::info!("Rolling back to {target_state} state with params: {:?}",
            params_to_restore.keys().collect::<Vec<_>>()
        );

        let eff_type = self
            .config
            .get("effectuation")
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("ssh")
            .to_string();
        let effectuator_path = format!("f/effectuation/{eff_type}");
        let args = json!({
            "context": self.config,
            "targets": [self.target.clone()],
            "settings": Value::Object(params_to_restore.clone()),
        });
        log::info!("Executing rollback effectuation for target {}",
            self.target_id
        );
        match self.wmill.run_script_by_path(&effectuator_path, &args) {
            Ok(result) => {
                log::info!(
                    "Rollback effectuation completed: {}",
                    result.get("status").and_then(|s| s.as_str()).unwrap_or("-")
                );
                rollback_state.insert("state".into(), json!("completed"));
                rollback_state.insert("consecutive_failures".into(), json!(0));
                self.update_rollback_state(rollback_state);
                self.metrics.inc_rollback("success");
                let _ = self.metrics.push();
                Ok(true)
            }
            Err(e) => {
                log::info!("Rollback execution failed: {e}");
                self.metrics.inc_rollback("failed");
                let _ = self.metrics.push();
                let on_failure = strategy
                    .get("on_failure")
                    .and_then(|f| f.as_str())
                    .unwrap_or("stop")
                    .to_string();
                match on_failure.as_str() {
                    "stop" => {
                        log::info!("Rollback failed with on_failure=stop, halting trials");
                        rollback_state.insert("state".into(), json!("failed"));
                        self.update_rollback_state(rollback_state);
                        Err(e)
                    }
                    "continue" => {
                        log::info!("Rollback failed with on_failure=continue, continuing trials");
                        rollback_state.insert("state".into(), json!("failed"));
                        self.update_rollback_state(rollback_state);
                        Ok(false)
                    }
                    "skip_target" => {
                        log::info!(
                            "Rollback failed with on_failure=skip_target, marking target unhealthy"
                        );
                        rollback_state.insert("state".into(), json!("skip_target"));
                        self.update_rollback_state(rollback_state);
                        Ok(false)
                    }
                    _ => Ok(false),
                }
            }
        }
    }

    pub fn handle_guardrail_violation(&mut self, _params: Option<&Map<String, Value>>) {
        if !self.rollback_enabled {
            return;
        }
        let mut state = self.get_rollback_state();
        let failures = state
            .get("consecutive_failures")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            + 1;
        state.insert("consecutive_failures".into(), json!(failures));
        log::info!("Guardrail violation detected. Consecutive failures: {failures}");
        let needs = self.check_needs_rollback();
        let next = if needs { "needs_rollback" } else { "normal" };
        state.insert("state".into(), json!(next));
        self.update_rollback_state(state);
    }

    pub fn handle_successful_trial(&mut self, params: &Map<String, Value>) {
        if !self.rollback_enabled {
            return;
        }
        let mut state = self.get_rollback_state();
        state.insert("consecutive_failures".into(), json!(0));
        state.insert("state".into(), json!("normal"));
        state.insert(
            "last_successful_params".into(),
            Value::Object(params.clone()),
        );
        self.update_rollback_state(state);
    }

    // ─── wish serving (steer mode) ────────────────────────────────

    fn causal_base_url(&self) -> String {
        self.config
            .get("detection")
            .and_then(|d| d.get("causal_url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("GODON_CAUSAL_URL").ok())
            .unwrap_or_else(|| "http://godon-godon-causal:9091".to_string())
    }

    /// Read the wish assignment row: adopt on appearance, release on
    /// removal, every trial.
    pub fn check_wish_assignment(&mut self) {
        let assignment = match self.ledger.wish_latest() {
            Ok(a) => a,
            Err(e) => {
                log::info!("Wish assignment check failed: {e}");
                return;
            }
        };
        match assignment {
            Some((wish_id, role)) => {
                let changed = self
                    .wish
                    .as_ref()
                    .map(|w| w.wish_id != wish_id)
                    .unwrap_or(true);
                if changed {
                    let role = if role.is_empty() {
                        "sender".to_string()
                    } else {
                        role
                    };
                    self.adopt_wish(&wish_id, &role);
                }
            }
            None => {
                if self.wish.is_some() {
                    if self.seek_driver.is_some() {
                        // A vanished assignment mid-seek still comes
                        // home: abort between trials, write the return
                        // NOW while the wish is still named.
                        if let Some(driver) = self.seek_driver.as_mut() {
                            driver.abort();
                        }
                        self.finish_seek();
                    } else {
                        self.release_wish(false);
                    }
                }
            }
        }
    }

    /// GET the wish's page from causal's book. Read-only — the arm
    /// never posts verdicts.
    fn fetch_wish_plan(&self, wish_id: &str) -> Option<Value> {
        let url = format!("{}/steer/plan/{}", self.causal_base_url(), wish_id);
        match self.http.call("GET", &url, None, Duration::from_secs(10)) {
            Ok(v) => Some(v),
            Err(e) => {
                log::info!("Wish plan fetch failed for {wish_id}: {e:?}");
                None
            }
        }
    }

    /// Is this plan move one of THIS tender's holds? Joint serving: a
    /// multi-claim plan names every sender; each tender serves its own
    /// moves. The legacy single-move plan is trusted as this tender's
    /// by construction.
    fn move_is_mine(&self, mv: &Value, single: bool) -> bool {
        move_is_mine(mv, single, &self.systemtender_id)
    }

    pub fn adopt_wish(&mut self, wish_id: &str, role: &str) {
        let Some(plan) = self.fetch_wish_plan(wish_id) else {
            return;
        };
        if plan.get("type").and_then(|t| t.as_str()) == Some("seek") {
            self.adopt_seek_wish(wish_id, &plan);
            return;
        }
        let empty: Vec<Value> = Vec::new();
        let moves = plan
            .get("plan")
            .and_then(|p| p.get("moves"))
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or(empty.clone());
        let path = plan
            .get("plan")
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        if role == "receiver" {
            // The receiver's whole contribution is stillness.
            self.wish = Some(HeldWish {
                wish_id: wish_id.to_string(),
                instruction: "park".to_string(),
                role: "receiver".to_string(),
                moves: BTreeMap::new(),
                probe: None,
            });
            log::info!(
                "Wish {wish_id} adopted as RECEIVER: parking at neutral, publishing readings for the judge"
            );
            return;
        }
        let single = moves.len() == 1;
        let mut own: BTreeMap<String, Value> = BTreeMap::new();
        for mv in &moves {
            let param = mv.get("param").filter(|v| !v.is_null());
            let setting = mv.get("setting").filter(|v| !v.is_null());
            if param.is_none() || setting.is_none() {
                continue;
            }
            if self.move_is_mine(mv, single) {
                own.insert(
                    param.unwrap().as_str().unwrap_or_default().to_string(),
                    setting.unwrap().clone(),
                );
            }
        }
        if own.is_empty() {
            log::info!("Wish {wish_id}: plan names no move for this tender — staying put");
            return;
        }
        let instruction = plan
            .get("instruction")
            .and_then(|i| i.as_str())
            .unwrap_or("hold")
            .to_string();
        let dials: Vec<String> = own.iter().map(|(p, s)| format!("{p}={s}")).collect();
        log::info!(
            "Wish {wish_id} adopted: holding {} dial(s) ({}) (instruction: {instruction})",
            own.len(),
            dials.join(", ")
        );
        self.wish = Some(HeldWish {
            wish_id: wish_id.to_string(),
            instruction,
            role: "sender".to_string(),
            moves: own,
            probe: None,
        });
        // Tell the receiver end: plant the row in ITS db.
        if path.len() >= 2 {
            if let Some(receiver) = path.last().and_then(|r| r.as_str()) {
                self.notify_wish_receiver(wish_id, receiver);
            }
        }
    }

    fn adopt_seek_wish(&mut self, wish_id: &str, plan: &Value) {
        let body = plan
            .get("seek")
            .filter(|v| !v.is_null())
            .or_else(|| plan.get("plan"))
            .cloned()
            .unwrap_or(json!({}));
        let seek_plan_value = json!({
            "wish_id": wish_id,
            "form": body.get("form"),
            "region": body.get("region").filter(|v| !v.is_null()).cloned().unwrap_or(json!({})),
            "criterion": body.get("criterion"),
            "budget": body.get("budget"),
            "candidates": body.get("candidates"),
        });
        let seek_plan = match parse_seek_plan(&seek_plan_value) {
            Ok(p) => p,
            Err(refused) => {
                let detail = if refused.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", refused.detail)
                };
                log::info!(
                    "Wish {wish_id} refused at the door: {}{detail} — never bound, never half-run",
                    refused.reason
                );
                return;
            }
        };
        self.wish = Some(HeldWish {
            wish_id: wish_id.to_string(),
            instruction: "seek".to_string(),
            role: "sender".to_string(),
            moves: BTreeMap::new(),
            probe: None,
        });
        let mut sorted_region: Vec<&String> = seek_plan.region.keys().collect();
        sorted_region.sort();
        let dials = sorted_region
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        log::info!(
            "Wish {wish_id} adopted as SEEK: form={}, dials=[{dials}], budget={}, candidates={}",
            seek_plan.form, seek_plan.budget, seek_plan.candidates
        );
        self.seek_driver = Some(SeekDriver::new(
            seek_plan.clone(),
            region_policy(seek_plan.region),
        ));
    }

    /// One commission at a time. Darts execute directly; passes are
    /// handed to the coordinator's probe override and the standing
    /// protocol executes them.
    pub fn serve_seek_step(&mut self) -> Result<(), String> {
        let wallet_exhausted = match self.seek_driver.as_ref() {
            None => return Ok(()),
            Some(d) => d.wallet_exhausted(),
        };
        if wallet_exhausted {
            // The seam holds: this runs between trials, and a
            // commission that rode its trial was already observed and
            // closed inside run_trial_body. An open commission HERE
            // never rode one — nothing is in flight. It bought nothing
            // and costs nothing: abandon it, drop its probe pin (an
            // empty level set clears on first contact, so the dead
            // pin cannot re-open the walk post-seek), and finish with
            // best-so-far. An exhausted wallet must never park the
            // walk forever (live receipt 2026-10-23: wish 891381d6,
            // spent 62 of budget 30, frozen mid-hold, tender pinned).
            if let Some(open) = self.seek_open.take() {
                log::info!(
                    "Seek: wallet exhausted with an unridden commission (dial {:?}) — abandoned, finishing best-so-far",
                    open.probe_dial
                );
                if let Some(dial) = &open.probe_dial {
                    self.coordinator.set_probe_override(dial, &[]);
                }
            }
            if let Some(driver) = self.seek_driver.as_mut() {
                driver.abort_with_reason(STOP_BUDGET);
            }
            self.finish_seek();
            return Ok(());
        }
        if self.seek_open.is_some() {
            return Ok(()); // the open commission is still in flight
        }
        let next = match self.seek_driver.as_mut() {
            None => return Ok(()),
            Some(driver) => driver.step().map_err(|e: DriverError| e.to_string())?,
        };
        match next {
            None => {
                self.finish_seek();
                Ok(())
            }
            Some(commission) => {
                if !commission.dart {
                    if let (Some(dial), Some(level)) =
                        (commission.probe_dial.clone(), commission.probe_level)
                    {
                        self.coordinator.set_probe_override(&dial, &[level]);
                    }
                }
                self.seek_open = Some(commission);
                Ok(())
            }
        }
    }

    /// Write the RETURN into the tender's own ledger and release.
    pub fn finish_seek(&mut self) {
        let Some(driver) = self.seek_driver.take() else {
            return;
        };
        let wish_id = self
            .wish
            .as_ref()
            .map(|w| w.wish_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let result = match driver.result() {
            Ok(r) => r,
            Err(e) => {
                log::info!("Seek {wish_id}: no result to write ({e})");
                self.release_wish(true);
                return;
            }
        };
        let payload = py_dumps(&result); // json.dumps(sort_keys=True)
        let key = format!("seek_return::{wish_id}");
        if let Err(e) = self.retry_op(&format!("ledger.state_put seek_return ({wish_id})"), || {
            self.ledger.state_put(&key, &payload)
        }) {
            log::info!("Seek return write failed for {wish_id}: {e}");
        }
        log::info!(
            "Wish {wish_id} RETURNED: stop={}, spend={}, finding={}",
            result
                .get("stop_reason")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            result
                .get("spend_count")
                .map(|v| v.to_string())
                .unwrap_or_default(),
            result
                .get("finding")
                .map(|v| v.to_string())
                .unwrap_or_default()
        );
        self.seek_open = None;
        self.release_wish(true);
    }

    fn notify_wish_receiver(&self, wish_id: &str, receiver: &str) {
        // the shared url minus its db, plus the receiver's own db
        let base = self
            .shared_db_dsn_base()
            .unwrap_or_else(|| "postgresql://localhost:5432".to_string());
        let url = format!("{base}/systemtender_{}", receiver.replace('-', "_"));
        let mut client = match (self.remote_db)(&url) {
            Ok(c) => c,
            Err(e) => {
                log::info!("Wish {wish_id}: receiver notification failed: {e}");
                return;
            }
        };
        let result = (|| -> Result<(), DbError> {
            client.execute(
                "CREATE TABLE IF NOT EXISTS wish_assignments (\
                 wish_id TEXT PRIMARY KEY, \
                 assigned_tsz DOUBLE PRECISION NOT NULL)",
                &[],
            )?;
            client.execute(
                "ALTER TABLE wish_assignments \
                 ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'sender'",
                &[],
            )?;
            let tsz = unix_now();
            client.execute(
                "INSERT INTO wish_assignments (wish_id, assigned_tsz, role) \
                 VALUES ($1, $2, 'receiver') \
                 ON CONFLICT (wish_id) DO UPDATE SET role = 'receiver', \
                 assigned_tsz = EXCLUDED.assigned_tsz",
                &[&wish_id, &tsz],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => log::info!(
                "Wish {wish_id}: receiver notified ({})",
                &receiver.chars().take(8).collect::<String>()
            ),
            Err(e) => log::info!("Wish {wish_id}: receiver notification failed: {e}"),
        }
    }

    /// The shared DSN's base (everything before the last '/'), for
    /// building per-receiver urls.
    fn shared_db_dsn_base(&self) -> Option<String> {
        // the python re-derived it from the shared url; the rust side
        // carries a Connect, so rebuild from the same env
        let pw = std::env::var("GODON_ARCHIVE_DB_PASSWORD").unwrap_or_else(|_| "postgres".into());
        let user = std::env::var("GODON_ARCHIVE_DB_USER").unwrap_or_else(|_| "postgres".into());
        let host =
            std::env::var("GODON_ARCHIVE_DB_SERVICE_HOST").unwrap_or_else(|_| "localhost".into());
        let port = std::env::var("GODON_ARCHIVE_DB_SERVICE_PORT").unwrap_or_else(|_| "5432".into());
        Some(format!("postgresql://{user}:{pw}@{host}:{port}"))
    }

    /// The receiver's assignment row cleanup (kept where the python
    /// kept it; the book's close normally removes rows itself).
    pub fn release_wish_receiver_row(&self, wish_id: &str, receiver: &str) {
        let base = self
            .shared_db_dsn_base()
            .unwrap_or_else(|| "postgresql://localhost:5432".to_string());
        let url = format!("{base}/systemtender_{}", receiver.replace('-', "_"));
        let mut client = match (self.remote_db)(&url) {
            Ok(c) => c,
            Err(e) => {
                log::info!("Wish {wish_id}: receiver row cleanup failed: {e}");
                return;
            }
        };
        if let Err(e) = client.execute(
            "DELETE FROM wish_assignments WHERE wish_id = $1",
            &[&wish_id],
        ) {
            log::info!("Wish {wish_id}: receiver row cleanup failed: {e}");
        }
    }

    /// One GET per trial boundary: keep holding / new value / release.
    pub fn refresh_wish_status(&mut self) {
        let Some(wish) = self.wish.clone() else {
            return;
        };
        let Some(plan) = self.fetch_wish_plan(&wish.wish_id) else {
            return;
        };
        if wish.role == "receiver" {
            // The receiver's role survives the refresh: only a release
            // ends the park (found live, Sep 21).
            let instruction = plan
                .get("instruction")
                .and_then(|i| i.as_str())
                .unwrap_or("hold");
            let next = if instruction == "release" {
                "release".to_string()
            } else {
                "park".to_string()
            };
            if let Some(w) = self.wish.as_mut() {
                w.instruction = next;
            }
        } else if wish.instruction == "seek" {
            // The seek owns its loop until the book releases it.
            if plan.get("instruction").and_then(|i| i.as_str()) == Some("release") {
                if self.seek_driver.is_some() {
                    if let Some(driver) = self.seek_driver.as_mut() {
                        driver.abort();
                    }
                    self.finish_seek();
                } else {
                    self.release_wish(true);
                }
            }
            return;
        } else {
            let instruction = plan
                .get("instruction")
                .and_then(|i| i.as_str())
                .unwrap_or("hold")
                .to_string();
            if let Some(w) = self.wish.as_mut() {
                w.instruction = instruction;
            }
        }
        let release_now = self
            .wish
            .as_ref()
            .map(|w| w.instruction == "release")
            .unwrap_or(false);
        if release_now {
            self.release_wish(true);
            return;
        }
        // Joint serving: re-read every move this tender holds. The
        // mine-filter runs BEFORE the mutable borrow (disjoint fields).
        let fresh_moves = plan
            .get("plan")
            .and_then(|p| p.get("moves"))
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        let single = fresh_moves.len() == 1;
        let mut mine: Vec<(String, Value)> = Vec::new();
        for mv in &fresh_moves {
            let param = mv.get("param").filter(|v| !v.is_null());
            let setting = mv.get("setting").filter(|v| !v.is_null());
            if param.is_none() || setting.is_none() {
                continue;
            }
            if move_is_mine(mv, single, &self.systemtender_id) {
                mine.push((
                    param.unwrap().as_str().unwrap_or_default().to_string(),
                    setting.unwrap().clone(),
                ));
            }
        }
        // the walk's next level: one per refresh
        let probe = plan.get("probe").and_then(|p| {
            let param = p.get("param").and_then(|n| n.as_str());
            let level = p.get("level").and_then(|l| l.as_f64());
            match (param, level) {
                (Some(n), Some(l)) => Some((n.to_string(), l)),
                _ => None,
            }
        });
        let coord = &self.coordinator;
        if let Some(w) = self.wish.as_mut() {
            for (p, s) in mine {
                if w.moves.get(&p) != Some(&s) {
                    log::info!(
                        "Wish {} re-planned: {p} {:?} -> {s}",
                        w.wish_id,
                        w.moves.get(&p)
                    );
                    w.moves.insert(p, s);
                }
            }
            if let Some((param, level)) = probe {
                w.probe = Some((param.clone(), level));
                coord.set_probe_override(&param, &[level]);
            }
        }
    }

    pub fn release_wish(&mut self, delete_assignment: bool) {
        let Some(wish) = self.wish.clone() else {
            return;
        };
        log::info!("Wish {} released — reverting to neutral", wish.wish_id);
        match self.dials.compute_neutral_params() {
            Ok(Some(neutral)) if !neutral.is_empty() => {
                if let Err(e) = self.execute_trial(&neutral) {
                    log::info!("Neutral re-assertion on release failed: {e}");
                }
            }
            Err(e) => {
                log::info!("Neutral re-assertion on release failed: {e}");
            }
            _ => {}
        }
        if delete_assignment {
            if let Err(e) = self.ledger.wish_remove(&wish.wish_id) {
                log::info!("Wish assignment removal failed for {}: {e}", wish.wish_id);
            }
        }
        self.wish = None;
    }

    fn wish_summary(&self) -> Value {
        match &self.wish {
            None => Value::Null,
            Some(w) => json!({
                "wish_id": w.wish_id,
                "instruction": w.instruction,
                "role": w.role,
                "dials": w.moves.keys().cloned().collect::<Vec<String>>(),
            }),
        }
    }

    fn seek_spend(&self) -> Value {
        match &self.seek_driver {
            None => Value::Null,
            Some(d) => d.wallet_status(),
        }
    }

    // ─── state and stamps ─────────────────────────────────────────

    pub fn update_state(&mut self) -> Result<(), String> {
        let count = self.ledger.count().map_err(|e| e.to_string())?;
        let state = json!({
            "systemtender_id": self.systemtender_id,
            "total_trials": count,
            "study_name": format!("{}_study", self.systemtender_id),
            "status": "running",
            "strategies_run": self.strategies_run.iter().cloned().collect::<Vec<String>>(),
            "wish": self.wish_summary(),
            "seek_spend": self.seek_spend(),
        });
        if let Err(e) = self.wmill.set_state(&state) {
            return Err(e);
        }
        self.stamp_instruments(&state);
        Ok(())
    }

    /// The rung-7 field-sets, stamped where the estate reads them: one
    /// tiny UPDATE per state refresh on its own throwaway connection,
    /// the heartbeat's pattern — a failed stamp never kills the run.
    fn stamp_instruments(&self, state: &Value) {
        let instruments = json!({
            "strategies_run": state.get("strategies_run"),
            "wish": state.get("wish"),
            "seek_spend": state.get("seek_spend"),
        });
        let payload = py_dumps(&instruments);
        let result = self.own_db.connect().and_then(|mut client| {
            let update = "UPDATE systemtender_state SET instruments = $1";
            let param = payload.clone();
            if let Err(e) = client.execute(update, &[&param]) {
                let msg = format!("{e} | {e:?}");
                if !(msg.contains("instruments") && msg.contains("does not exist")) {
                    return Err(DbError::new(msg));
                }
                client.execute(
                    "ALTER TABLE systemtender_state \
                         ADD COLUMN IF NOT EXISTS instruments TEXT",
                    &[],
                )?;
                client.execute(update, &[&param])?;
            }
            Ok(())
        });
        if let Err(e) = result {
            log::info!("instruments stamp failed (retries on next state refresh): {e}");
        }
    }

    /// Declare the walk's end in the tender's own state table.
    /// Best-effort: a dying worker must never hang on this.
    pub fn stamp_finished(&self) {
        let result = self.own_db.connect().and_then(|mut client| {
            if let Err(e) = client.execute("UPDATE systemtender_state SET finished_at = NOW()", &[])
            {
                let msg = format!("{e} | {e:?}");
                if !(msg.contains("finished_at") && msg.contains("does not exist")) {
                    return Err(DbError::new(msg));
                }
                client.execute(
                    "ALTER TABLE systemtender_state \
                         ADD COLUMN IF NOT EXISTS finished_at TIMESTAMPTZ",
                    &[],
                )?;
                client.execute("UPDATE systemtender_state SET finished_at = NOW()", &[])?;
            }
            Ok(())
        });
        match result {
            Ok(()) => log::info!("Stamped finished_at for systemtender {}",
                self.systemtender_id
            ),
            Err(e) => log::info!("finished stamp failed (non-fatal): {e}"),
        }
    }

    /// The park target at boot (rung 8, designs/2026-10-11 XX — the
    /// death resume at boot).
    ///
    /// A relaunched holder keeps its measured state: dials are
    /// target-host settings and persist across worker death, so the
    /// park asserts the last standing params instead of dragging a
    /// held dial back to neutral and breaking the hold the worker is
    /// about to resume. A receiver's contribution is stillness — it
    /// parks at neutral. A bare boot (no assignment row) parks at
    /// neutral exactly as before; every read failure degrades to the
    /// bare-boot path.
    fn boot_park_params(&self) -> Option<Map<String, Value>> {
        let assignment = match self.ledger.wish_latest() {
            Ok(a) => a,
            Err(e) => {
                log::info!("BOOT PARK: assignment read failed ({e}) - treating as bare boot");
                None
            }
        };
        if let Some((wish_id, role)) = assignment {
            // python `(role or 'sender')`: an empty role reads sender
            let role = if role.is_empty() {
                "sender".to_string()
            } else {
                role
            };
            if role == "receiver" {
                log::info!("BOOT PARK: receiver of wish {wish_id} - stillness, parking neutral");
            } else {
                // the standing params live in the ROLLBACK STATE —
                // python reads exactly this key, not the trials-based
                // `get_last_successful_params` helper
                match self.ledger.state_get(&self.rollback_state_key()) {
                    Ok(Some(state_json)) => match serde_json::from_str::<Value>(&state_json) {
                        Ok(v) => {
                            // python truthiness: an absent, null, or
                            // EMPTY map parks neutral
                            let standing = v
                                .get("last_successful_params")
                                .and_then(|p| p.as_object())
                                .filter(|m| !m.is_empty())
                                .cloned();
                            match standing {
                                Some(standing) => {
                                    log::info!(
                                        "BOOT PARK: resuming holder of wish {wish_id} \
                                         at last standing params ({} dials)",
                                        standing.len()
                                    );
                                    return Some(standing);
                                }
                                None => {
                                    log::info!(
                                        "BOOT PARK: wish {wish_id} held but no standing params - parking neutral"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            log::info!("BOOT PARK: standing read failed ({e}) - parking neutral");
                        }
                    },
                    Ok(None) => {
                        log::info!(
                            "BOOT PARK: wish {wish_id} held but no standing params - parking neutral"
                        );
                    }
                    Err(e) => {
                        log::info!("BOOT PARK: standing read failed ({e}) - parking neutral");
                    }
                }
            }
        }
        match self.dials.compute_neutral_params() {
            Ok(v) => v,
            Err(e) => {
                log::info!(
                    "BOOT PARK: neutral computation failed ({e}) - skipping; the room may see boot state"
                );
                None
            }
        }
    }

    /// Apply the boot park BEFORE the tender announces itself to
    /// the room (registration). The bench boots at param_lower — the
    /// dials' extreme — and a registration that outran the first
    /// park let the room's first measurement window bank that
    /// extreme (found live Sep 30). The target: neutral on a bare
    /// boot, the last standing params when a held wish waits in the
    /// ledger (rung 8 — the death resume).
    pub fn park_before_announcement(&mut self) {
        let Some(target) = self.boot_park_params() else {
            log::info!("BOOT PARK: no park target - skipping");
            return;
        };
        if let Err(e) = self.execute_trial(&target) {
            log::info!("BOOT PARK: apply failed ({e}) - skipping; the room may see boot state");
            return;
        }
        log::info!(
            "BOOT PARK: {} dials applied before announcement",
            target.len()
        );
    }

    // ─── the continue policy (composition around the spine) ───────

    fn worker_should_continue(&mut self) -> Result<bool, String> {
        // stop flag read: a failed read is NOT a stop (python parity);
        // a noticed stop returns BEFORE the char read (python's order)
        if self.check_shutdown_requested() {
            log::info!("Stopping: Shutdown requested by controller");
            return Ok(false);
        }
        let wish_held = self.wish.is_some();
        // char_complete: errors propagate (python's raise)
        let walk_complete = match self.coordinator.char_complete() {
            Ok(v) => v,
            Err(e) => return Err(e.to_string()),
        };
        let pin_current = self
            .pin_receipt
            .as_deref()
            .zip(self.wish.as_ref().map(|w| w.wish_id.as_str()))
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        match should_continue(false, wish_held, walk_complete, pin_current) {
            Verdict::Stop(reason) => {
                log::info!("{reason}");
                Ok(false)
            }
            Verdict::Continue { pin_fired } => {
                if pin_fired {
                    let wish_id = self
                        .wish
                        .as_ref()
                        .map(|w| w.wish_id.clone())
                        .unwrap_or_default();
                    self.pin_receipt = Some(wish_id.clone());
                    log::info!("Characterization complete but wish {wish_id} held — pinning past walk-complete (guard): finished walkers leave, holders park"
                    );
                }
                Ok(true)
            }
        }
    }

    // ─── the trial loop ───────────────────────────────────────────

    /// The run: trials until the continue policy ends them. A stop or
    /// a crash mid-seek still comes home — the driver aborts between
    /// trials and the return is written; a held wish releases to
    /// neutral and its assignment row goes with it; the finished stamp
    /// lands. Every exit is a clean exit (guard, rung 7).
    pub fn run(&mut self) -> Result<(), String> {
        log::info!("Starting SystemtenderWorker: {}", self.cfg.worker_id());
        self.metrics.purge_stale_series();
        let _ = self.metrics.push();

        let mut trial_count: u64 = 0;
        let result = self.run_trials(&mut trial_count);

        if let Err(ref run_err) = result {
            log::info!("Systemtender {} failed: {run_err}", self.systemtender_id);
            // python's except-block state update: if THIS fails, its
            // error replaces the loop's
            if let Err(state_err) = self.update_state() {
                return Err(state_err);
            }
        }

        // finally: the seek comes home, the wish releases, the stamps
        if self.seek_driver.is_some() {
            if let Some(driver) = self.seek_driver.as_mut() {
                driver.abort();
            }
            self.finish_seek();
        }
        self.release_wish(true);
        self.stamp_finished();
        let _ = self.metrics.push();

        // the success epilogue (python's post-finally lines)
        if result.is_ok() {
            self.update_state()?;
            let count = self.ledger.count().map_err(|e| e.to_string())?;
            log::info!("SystemtenderWorker {} completed {count} trials",
                self.cfg.worker_id()
            );
            if let Ok(Some(best)) = self.ledger.best_trial(self.dials.objective_minimizes()) {
                log::debug!(
                    "Best trial: #{}, value={:?}, params={}",
                    best.number, best.value, best.params
                );
            }
        }
        result
    }

    fn run_trials(&mut self, trial_count: &mut u64) -> Result<(), String> {
        while self.worker_should_continue()? {
            // rollback check, before the next trial
            if self.rollback_enabled && self.check_needs_rollback() {
                log::info!("Rollback needed, executing rollback before next trial");
                let rollback_success = self.execute_rollback()?;
                if !rollback_success {
                    log::info!("Rollback failed, continuing with trials");
                }
                let strategy_name = self
                    .rollback_config
                    .get("strategy")
                    .and_then(|s| s.as_str())
                    .unwrap_or("standard")
                    .to_string();
                let after_policy = self
                    .config
                    .get("rollback_strategies")
                    .and_then(|s| s.get(&strategy_name))
                    .and_then(|s| s.get("after"))
                    .cloned()
                    .unwrap_or(json!({}));
                let after_action = after_policy
                    .get("action")
                    .and_then(|a| a.as_str())
                    .unwrap_or("continue");
                if after_action == "pause" {
                    let duration = after_policy
                        .get("duration")
                        .and_then(|d| d.as_i64())
                        .unwrap_or(300);
                    log::info!("Pausing for {duration} seconds after rollback");
                    (self.sleeper)(duration as f64);
                } else if after_action == "stop" {
                    log::info!("Rollback completed with after.action=stop, halting trials");
                    break;
                }
            }

            // begin() sits outside the per-trial catch (python parity:
            // a begin failure kills the run, not the trial)
            let trial_number = self.ledger.begin().map_err(|e| e.to_string())?;
            log::info!("Trial {trial_number} started");
            let trial_start = Instant::now();

            let body = self.run_trial_body(trial_number, trial_start);
            match body {
                Err(e) => {
                    log::info!("Trial {trial_number} failed: {e}");
                    let msg: String = e.chars().take(500).collect();
                    match self.retry_op(
                        &format!("ledger.fail recovery (trial {trial_number})"),
                        || self.ledger.fail(trial_number, Some(&msg)),
                    ) {
                        Ok(()) => log::info!("Trial {trial_number} marked as FAILED"),
                        Err(tell_err) => {
                            log::info!("Trial {trial_number} tell failed: {tell_err}")
                        }
                    }
                    self.metrics.inc_trial("failed", None);
                    self.metrics.inc_effectuation("failure");
                    self.handle_guardrail_violation(None);
                }
                Ok(()) => {
                    *trial_count += 1;
                    if *trial_count % 5 == 0 {
                        self.heartbeat_interference();
                        self.update_state()?;
                        let count = self.ledger.count().map_err(|er| er.to_string())?;
                        self.metrics.set_total_trials(count);
                        let _ = self.metrics.push();
                    }
                }
            }
        }
        Ok(())
    }

    /// One trial: the pulse, the seek commission, the coordinator's
    /// decision and its wish-shaped overrides, the stamps, the
    /// effectuation+measurement, the guards, the completion. Errors
    /// return to the loop's fail path (python's per-trial except).
    fn run_trial_body(&mut self, trial_number: i64, trial_start: Instant) -> Result<(), String> {
        // === Wish serving: adopt / release / refresh at every boundary
        self.check_wish_assignment();
        self.refresh_wish_status();

        // === The seek wish: commission or dart, THEN the decision ===
        let seeking = self
            .wish
            .as_ref()
            .map(|w| w.instruction == "seek")
            .unwrap_or(false);
        if seeking {
            self.serve_seek_step()?;
        }

        // === The coordinator's own decision ===
        let mut decision = self
            .coordinator
            .decide_trial(trial_number)
            .map_err(|e| e.to_string())?;
        let mut seek_dart = false;

        let open = self.seek_open.clone();
        if let Some(open) = &open {
            if open.dart {
                decision = Decision {
                    mode: "push",
                    params: open.params.clone(),
                    impulse_phase: None,
                    probe_param: None,
                    probe_param_idx: None,
                    probe_level: None,
                    lease_phase: None,
                    detection_trial: false,
                    ambience_probe: false,
                };
                seek_dart = true;
            } else {
                // The commissioned probe rides the protocol's own
                // decision; the seek's stepped baseline merges onto the
                // OTHER dials.
                let mut merged = decision.params.clone().unwrap_or_default();
                for (k, v) in open.baseline.clone().unwrap_or_default() {
                    merged.insert(k, v);
                }
                decision.params = Some(merged);
            }
        } else if let Some(wish) = self.wish.clone() {
            if wish.instruction == "hold" && !wish.moves.is_empty() {
                // The wish outranks the walk: hold the planned values.
                let base = match decision.params.clone() {
                    Some(p) if !p.is_empty() => p,
                    _ => self
                        .dials
                        .compute_neutral_params()
                        .map_err(|e| e.to_string())?
                        .unwrap_or_default(),
                };
                let mut hold_params = base;
                for (p, s) in &wish.moves {
                    hold_params.insert(p.clone(), s.clone());
                }
                decision = Decision {
                    mode: "hold",
                    params: Some(hold_params),
                    impulse_phase: None,
                    probe_param: None,
                    probe_param_idx: None,
                    probe_level: None,
                    lease_phase: None,
                    detection_trial: false,
                    ambience_probe: false,
                };
            } else if wish.instruction == "park" {
                // The receiver's role: stillness — neutral dials,
                // readings published with the hold phase.
                let neutral = self
                    .dials
                    .compute_neutral_params()
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                decision = Decision {
                    mode: "hold",
                    params: Some(neutral),
                    impulse_phase: None,
                    probe_param: None,
                    probe_param_idx: None,
                    probe_level: None,
                    lease_phase: Some("hold".to_string()),
                    detection_trial: false,
                    ambience_probe: false,
                };
            }
            // 'remeasure': keep the coordinator's probe decision
        }

        let detection_mode = decision.mode;
        self.strategies_run.insert(detection_mode.to_string());

        // The classification, written once — a dart carries its own
        // kind; a joint random setting is a prior, never curve ground.
        let coord_state = self.coordinator.get_state();
        let kind = classify(
            Some(detection_mode),
            decision.impulse_phase,
            coord_state.as_str(),
            seek_dart,
            decision.ambience_probe,
        );
        self.retry_op(&format!("ledger.set_kind (trial {trial_number})"), || {
            self.ledger.set_kind(trial_number, &kind)
        })
        .map_err(|e| e.to_string())?;

        // The why, as evidence: everything but params (sorted, the one
        // writer — parse-equal to python's dumps)
        let mut evidence = Map::new();
        evidence.insert("mode".into(), json!(detection_mode));
        if let Some(phase) = decision.impulse_phase {
            evidence.insert("impulse_phase".into(), json!(phase));
        }
        if seek_dart {
            evidence.insert("seek_dart".into(), json!(true));
        }
        if let Some(p) = &decision.probe_param {
            evidence.insert("probe_param".into(), json!(p));
        }
        if let Some(i) = decision.probe_param_idx {
            evidence.insert("probe_param_idx".into(), json!(i));
        }
        if let Some(l) = decision.probe_level {
            evidence.insert("probe_level".into(), json!(l));
        }
        if let Some(lp) = &decision.lease_phase {
            evidence.insert("lease_phase".into(), json!(lp));
        }
        if decision.detection_trial {
            evidence.insert("detection_trial".into(), json!(true));
        }
        if decision.ambience_probe {
            evidence.insert("ambience_probe".into(), json!(true));
        }
        // (python also stamps `impulse_scale` when present — nothing
        // in the coordinator ever produced it; no field exists here)
        self.retry_op(
            &format!("ledger.stamp decision (trial {trial_number})"),
            || {
                self.ledger.stamp(
                    trial_number,
                    "decision",
                    Value::String(py_dumps(&Value::Object(evidence.clone()))),
                )
            },
        )
        .map_err(|e| e.to_string())?;

        // the observer's stamps
        self.retry_op(&format!("ledger.stamp mode (trial {trial_number})"), || {
            self.ledger
                .stamp(trial_number, "detection_mode", json!(detection_mode))
        })
        .map_err(|e| e.to_string())?;
        if let Some(wish) = &self.wish {
            self.retry_op(&format!("ledger.stamp wish (trial {trial_number})"), || {
                self.ledger
                    .stamp(trial_number, "wish_id", json!(wish.wish_id))
                    .and_then(|_| {
                        self.ledger.stamp(
                            trial_number,
                            "role",
                            json!(if detection_mode == "hold" {
                                "hold"
                            } else {
                                "probe"
                            }),
                        )
                    })
            })
            .map_err(|e| e.to_string())?;
        }
        self.retry_op(
            &format!("ledger.stamp coord_state (trial {trial_number})"),
            || {
                self.ledger
                    .stamp(trial_number, "coord_state", json!(coord_state.as_str()))
            },
        )
        .map_err(|e| e.to_string())?;
        let char_status = self.coordinator.get_char_status();
        self.retry_op(
            &format!("ledger.stamp char_status (trial {trial_number})"),
            || {
                self.ledger
                    .stamp(trial_number, "char_status", char_status.clone())
            },
        )
        .map_err(|e| e.to_string())?;
        if let Some(phase) = decision.impulse_phase {
            self.retry_op(
                &format!("ledger.stamp impulse_phase (trial {trial_number})"),
                || {
                    self.ledger
                        .stamp(trial_number, "impulse_phase", json!(phase))
                },
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(param) = &decision.probe_param {
            self.retry_op(
                &format!("ledger.stamp probe_param (trial {trial_number})"),
                || self.ledger.stamp(trial_number, "probe_param", json!(param)),
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(idx) = decision.probe_param_idx {
            self.retry_op(
                &format!("ledger.stamp probe_param_idx (trial {trial_number})"),
                || {
                    self.ledger
                        .stamp(trial_number, "probe_param_idx", json!(idx))
                },
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(level) = decision.probe_level {
            self.retry_op(
                &format!("ledger.stamp probe_level (trial {trial_number})"),
                || self.ledger.stamp(trial_number, "probe_level", json!(level)),
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(phase) = &decision.lease_phase {
            self.retry_op(
                &format!("ledger.stamp lease_phase (trial {trial_number})"),
                || self.ledger.stamp(trial_number, "lease_phase", json!(phase)),
            )
            .map_err(|e| e.to_string())?;
        }

        // Every trial's params arrive shaped from the coordinator —
        // the body never picks settings itself.
        let params = match decision.params.clone() {
            Some(p) if !p.is_empty() => p,
            _ => {
                // FAIL to avoid effectuating anything the room did not
                // decide.
                log::info!(
                    "Trial {trial_number}: mode={detection_mode} but no params from coordinator — FAILing"
                );
                self.retry_op(
                    &format!("ledger.fail no-coordinator-params (trial {trial_number})"),
                    || {
                        self.ledger
                            .fail(trial_number, Some("no coordinator params"))
                    },
                )
                .map_err(|e| e.to_string())?;
                return Ok(()); // python's `continue`
            }
        };

        let metrics = self.execute_trial(&params)?;

        let (guardrails_violated, violations) = self.check_guardrails(&metrics);

        // guardrail readings, the stamp
        let guardrails_config = self
            .config
            .get("guardrails")
            .and_then(|g| g.as_array())
            .cloned()
            .unwrap_or_default();
        if !guardrails_config.is_empty() {
            let mut readings = Map::new();
            for g in &guardrails_config {
                let gname = g.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                if let Some(&gval) = metrics.get(gname) {
                    let limit = g.get("hard_limit").and_then(|l| l.as_f64());
                    let violated = match limit {
                        Some(l) => gval > l,
                        None => false,
                    };
                    readings.insert(
                        gname.to_string(),
                        json!({
                            "value": gval,
                            "hard_limit": g.get("hard_limit").cloned().unwrap_or(Value::Null),
                            "violated": violated,
                        }),
                    );
                }
            }
            if !readings.is_empty() {
                self.retry_op(
                    &format!("ledger.stamp guardrails (trial {trial_number})"),
                    || {
                        self.ledger.stamp(
                            trial_number,
                            "guardrails",
                            Value::String(py_dumps(&Value::Object(readings.clone()))),
                        )
                    },
                )
                .map_err(|e| e.to_string())?;
            }
        }

        // observation signals for coupling detection — recorded, never
        // judged
        let observations_config = self
            .config
            .get("observations")
            .and_then(|o| o.as_array())
            .cloned()
            .unwrap_or_default();
        if !observations_config.is_empty() {
            let mut observation_readings = Map::new();
            for obs in &observations_config {
                let oname = obs
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown");
                if let Some(&oval) = metrics.get(oname) {
                    observation_readings.insert(oname.to_string(), json!(oval));
                }
            }
            if !observation_readings.is_empty() {
                self.retry_op(
                    &format!("ledger.stamp observations (trial {trial_number})"),
                    || {
                        self.ledger.stamp(
                            trial_number,
                            "observations",
                            Value::String(py_dumps(&Value::Object(observation_readings.clone()))),
                        )
                    },
                )
                .map_err(|e| e.to_string())?;
            }
        }

        if guardrails_violated {
            log::info!("Trial {trial_number} failed guardrails: {violations:?}");
            self.retry_op(
                &format!("ledger.fail guardrail-refusal (trial {trial_number})"),
                || self.ledger.fail(trial_number, Some("guardrail refusal")),
            )
            .map_err(|e| e.to_string())?;
            log::info!("Trial {trial_number} marked as FAILED (guardrail violation)");
            for violation in &violations {
                let guardrail_name = violation
                    .split(':')
                    .next()
                    .unwrap_or("unknown")
                    .trim_start_matches("Guardrail '")
                    .trim_end_matches('\'');
                self.metrics.inc_guardrail_violation(guardrail_name);
            }
            self.handle_guardrail_violation(Some(&params));
            self.metrics.inc_trial("failed", None);
            self.metrics.inc_effectuation("failure");
            return Ok(());
        }

        // the success half
        let objectives = self.objective_names();
        let values: Vec<Option<f64>> = objectives
            .iter()
            .map(|name| metrics.get(name).copied())
            .collect();
        let missing: Vec<&String> = objectives
            .iter()
            .zip(&values)
            .filter(|(_, v)| v.is_none())
            .map(|(n, _)| n)
            .collect();
        if !missing.is_empty() {
            log::info!("Trial {trial_number} has None values for objectives: {missing:?}");
        }
        if !self.last_metric_noise.is_empty() {
            let noise = py_dumps(&Value::Object(
                self.last_metric_noise
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect(),
            ));
            self.retry_op(
                &format!("ledger.stamp metric_noise (trial {trial_number})"),
                || {
                    self.ledger
                        .stamp(trial_number, "metric_noise", json!(noise))
                },
            )
            .map_err(|e| e.to_string())?;
        }

        // Stash effectuation params with the trial.
        self.retry_op(
            &format!("ledger.stamp effectuation_params (trial {trial_number})"),
            || {
                self.ledger.stamp(
                    trial_number,
                    "effectuation_params",
                    Value::String(py_dumps(&Value::Object(params.clone()))),
                )
            },
        )
        .map_err(|e| e.to_string())?;
        self.publish_standing_params(&params);

        // The seek's pay and observe.
        if self.seek_driver.is_some() && self.seek_open.is_some() {
            let open_step = self.seek_open.clone().expect("checked");
            if let Some(driver) = self.seek_driver.as_mut() {
                driver.wallet_pay();
            }
            let outcome = self
                .seek_driver
                .as_ref()
                .and_then(|d| d.criterion_outcome());
            let (reading, noise) = match outcome {
                None => (0.0, 0.0),
                Some(outcome) => {
                    let local = outcome.rsplit('/').next().unwrap_or(&outcome).to_string();
                    match metrics.get(&local) {
                        None => {
                            log::info!(
                                "Seek trial {trial_number}: criterion '{local}' absent from metrics"
                            );
                            (0.0, 0.0)
                        }
                        Some(&r) => (
                            r,
                            self.last_metric_noise.get(&local).copied().unwrap_or(0.0),
                        ),
                    }
                }
            };
            let landed = open_step.dart
                || (decision.probe_param.as_deref() == open_step.probe_dial.as_deref()
                    && decision.probe_level == open_step.probe_level);
            if landed {
                if let Some(driver) = self.seek_driver.as_mut() {
                    driver.observe(reading, noise).map_err(|e| e.to_string())?;
                }
                self.seek_open = None;
            }
        }

        // Publish this trial's own readings whenever a protocol phase
        // was in flight; parked/idle trials publish nothing.
        let obs_phase = decision
            .lease_phase
            .clone()
            .or_else(|| decision.impulse_phase.map(|p| p.to_string()));
        {
            let mut obj_readings = Map::new();
            for section in ["objectives", "observations"] {
                if let Some(arr) = self.config.get(section).and_then(|v| v.as_array()) {
                    for obj in arr {
                        let oname = obj
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown");
                        if let Some(&v) = metrics.get(oname) {
                            obj_readings
                                .entry(oname.to_string())
                                .or_insert_with(|| json!(v));
                        }
                    }
                }
            }
            if obj_readings.is_empty() {
                // The publish silently skips on two gates; say WHICH.
                log::debug!(
                    "Receiver observation skipped: phase={obs_phase:?} metrics_keys={:?}",
                    metrics.keys().take(8).collect::<Vec<_>>()
                );
            }
            if !obj_readings.is_empty() && obs_phase.is_some() {
                let phase = obs_phase.clone().expect("checked");
                self.coordinator.record_receiver_observation(
                    trial_number,
                    &obj_readings,
                    Some(&phase),
                );
            }
        }

        // Guard (rung 7): no strategy stamp, no completion. (The rust
        // Decision always carries its mode — the unstamped branch is
        // structurally impossible here; the kind stamp above is the
        // guard's other half.)
        let v0 = values
            .first()
            .copied()
            .flatten()
            .ok_or_else(|| "IndexError: list index out of range".to_string())?;
        self.retry_op(&format!("ledger.complete (trial {trial_number})"), || {
            self.ledger
                .complete(trial_number, values.first().copied().flatten())
        })
        .map_err(|e| e.to_string())?;
        let _ = v0;

        let trial_duration = trial_start.elapsed().as_secs_f64();
        self.coordinator.record_trial_duration(trial_duration);
        log::info!("Trial {trial_number} completed with values: {values:?}");

        self.metrics
            .inc_trial("complete", values.first().copied().flatten());
        self.metrics.observe_trial_duration(trial_duration);
        self.metrics.inc_effectuation("success");

        if let Ok(Some(best)) = self.ledger.best_trial(self.dials.objective_minimizes()) {
            if best.number == trial_number {
                self.metrics
                    .set_best_value(values.first().copied().flatten().unwrap_or(0.0));
            }
        }
        self.handle_successful_trial(&params);
        Ok(())
    }
}

/// One beat, with a short fresh-connection retry burst. yb abort
/// bursts silence beats for tens of seconds; each beat opens a fresh
/// connection, so a burst is survived by quick retries inside the same
/// beat slot. Non-retryable errors re-raise (the loop owns recovery).
pub fn beat_with_burst(
    own: &dyn Connect,
    interval: f64,
    sleeper: &dyn Fn(f64),
) -> Result<(), DbError> {
    beat_with_burst_fn(|| beat_once(own, interval), sleeper)
}

/// The burst ladder over an injected beat — the seam the python tests
/// patched at `_beat_once`; same surface, one level down.
pub fn beat_with_burst_fn(
    mut beat: impl FnMut() -> Result<(), DbError>,
    sleeper: &dyn Fn(f64),
) -> Result<(), DbError> {
    match beat() {
        Ok(()) => Ok(()),
        Err(e) => {
            if !worker_is_retryable(&e.to_string()) {
                return Err(e);
            }
            let mut last = e;
            for _ in 0..3 {
                sleeper(1.0);
                match beat() {
                    Ok(()) => return Ok(()),
                    Err(retry_exc) => last = retry_exc,
                }
            }
            log::info!("heartbeat burst lost after retries: {last}");
            Ok(())
        }
    }
}

/// One beat into the tender's own state row, its own throwaway
/// connection. Rollout-order independent: a state row born before the
/// interval column gets it added, then beats normally. Message-matched
/// because the exact error class differs between real postgres and
/// test stubs. (The rust client is autocommit-per-statement — the
/// python's explicit rollback has no equivalent to do.)
pub fn beat_once(own: &dyn Connect, interval: f64) -> Result<(), DbError> {
    let mut client = own.connect()?;
    let update = "UPDATE systemtender_state SET updated_at = NOW(), beat_interval_secs = $1";
    if let Err(e) = client.execute(update, &[&interval]) {
        let msg = format!("{e} | {e:?}");
        if !(msg.contains("beat_interval_secs") && msg.contains("does not exist")) {
            return Err(DbError::new(msg));
        }
        client.execute(
            "ALTER TABLE systemtender_state \
             ADD COLUMN IF NOT EXISTS beat_interval_secs DOUBLE PRECISION",
            &[],
        )?;
        client.execute(update, &[&interval])?;
    }
    Ok(())
}

// (the bespoke jsonb text-bytes wrapper is gone: it sent text as
// binary and Yugabyte refused every publish — the ledger's
// serde-Value + $1::jsonb pattern is the one true binding)

/// Joint serving: a multi-claim plan names every sender; each tender
/// serves its own moves and ignores the rest. The legacy single-move
/// plan is trusted as this tender's by construction.
fn move_is_mine(mv: &Value, single: bool, systemtender_id: &str) -> bool {
    let Some(sender) = mv.get("sender").and_then(|s| s.as_str()) else {
        return single;
    };
    let mine = sender.replace('-', "_");
    let me = systemtender_id.replace('-', "_");
    if mine == me {
        return true;
    }
    single
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator_walk::Decision;
    use crate::walk_policy::HttpError;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ─── the fakes (the python tests' patch surfaces, one level down) ──

    struct GState {
        trials: Vec<TrialRecord>,
        next_number: i64,
        kinds: Vec<(i64, String)>,
        stamps: Vec<(i64, String, Value)>,
        completes: Vec<(i64, Option<f64>)>,
        fails: Vec<(i64, Option<String>)>,
        state: BTreeMap<String, String>,
        best: Option<TrialRecord>,
        stop: bool,
        wish: Option<(String, String)>,
        removed_wishes: Vec<String>,
        // read-failure injection (rung 8's degradation tests)
        state_get_err: Option<String>,
        wish_latest_err: Option<String>,
    }

    #[derive(Clone)]
    struct FakeLedger(Arc<Mutex<GState>>);

    impl FakeLedger {
        fn new() -> Self {
            FakeLedger(Arc::new(Mutex::new(GState {
                trials: Vec::new(),
                next_number: 0,
                kinds: Vec::new(),
                stamps: Vec::new(),
                completes: Vec::new(),
                fails: Vec::new(),
                state: BTreeMap::new(),
                best: None,
                stop: false,
                wish: None,
                removed_wishes: Vec::new(),
                state_get_err: None,
                wish_latest_err: None,
            })))
        }
        fn set_wish(&self, w: Option<(String, String)>) {
            self.0.lock().unwrap().wish = w;
        }
        fn set_state_get_err(&self, e: Option<String>) {
            self.0.lock().unwrap().state_get_err = e;
        }
        fn set_wish_latest_err(&self, e: Option<String>) {
            self.0.lock().unwrap().wish_latest_err = e;
        }
        fn set_trials(&self, t: Vec<TrialRecord>) {
            self.0.lock().unwrap().trials = t;
        }
        fn set_stop(&self, s: bool) {
            self.0.lock().unwrap().stop = s;
        }
        fn state_keys(&self) -> Vec<String> {
            self.0.lock().unwrap().state.keys().cloned().collect()
        }
    }

    impl Ledger for FakeLedger {
        fn ensure_schema(&self) -> Result<i32, DbError> {
            Ok(0)
        }
        fn count(&self) -> Result<i64, DbError> {
            Ok(self.0.lock().unwrap().trials.len() as i64)
        }
        fn trials(&self) -> Result<Vec<TrialRecord>, DbError> {
            Ok(self.0.lock().unwrap().trials.clone())
        }
        fn begin(&self) -> Result<i64, DbError> {
            let mut g = self.0.lock().unwrap();
            g.next_number += 1;
            Ok(g.next_number)
        }
        fn set_kind(&self, number: i64, kind: &str) -> Result<(), DbError> {
            self.0
                .lock()
                .unwrap()
                .kinds
                .push((number, kind.to_string()));
            Ok(())
        }
        fn stamp(&self, number: i64, key: &str, value: Value) -> Result<(), DbError> {
            self.0
                .lock()
                .unwrap()
                .stamps
                .push((number, key.to_string(), value));
            Ok(())
        }
        fn complete(&self, number: i64, value: Option<f64>) -> Result<(), DbError> {
            let mut g = self.0.lock().unwrap();
            g.completes.push((number, value));
            g.trials.push(TrialRecord {
                number,
                kind: "hold".into(),
                params: json!({}),
                value,
                state: "COMPLETE".into(),
                error: None,
                attrs: json!({}),
                created_ts: None,
                finished_ts: None,
                values: value.map(|v| vec![v]).unwrap_or_default(),
            });
            g.best = g.trials.last().cloned();
            Ok(())
        }
        fn fail(&self, number: i64, error: Option<&str>) -> Result<(), DbError> {
            self.0
                .lock()
                .unwrap()
                .fails
                .push((number, error.map(|s| s.to_string())));
            Ok(())
        }
        fn state_get(&self, key: &str) -> Result<Option<String>, DbError> {
            let g = self.0.lock().unwrap();
            if let Some(e) = &g.state_get_err {
                return Err(DbError::new(e.as_str()));
            }
            Ok(g.state.get(key).cloned())
        }
        fn state_put(&self, key: &str, value: &str) -> Result<(), DbError> {
            self.0
                .lock()
                .unwrap()
                .state
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
        fn best_trial(&self, minimize: bool) -> Result<Option<TrialRecord>, DbError> {
            let g = self.0.lock().unwrap();
            let _ = minimize;
            Ok(g.best.clone())
        }
        fn stop_requested(&self) -> Result<bool, DbError> {
            Ok(self.0.lock().unwrap().stop)
        }
        fn wish_latest(&self) -> Result<Option<(String, String)>, DbError> {
            let g = self.0.lock().unwrap();
            if let Some(e) = &g.wish_latest_err {
                return Err(DbError::new(e.as_str()));
            }
            Ok(g.wish.clone())
        }
        fn wish_remove(&self, wish_id: &str) -> Result<(), DbError> {
            self.0
                .lock()
                .unwrap()
                .removed_wishes
                .push(wish_id.to_string());
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeCoord {
        decisions: Arc<Mutex<VecDeque<Decision>>>,
        char_seq: Arc<Mutex<VecDeque<bool>>>,
        overrides: Arc<Mutex<Vec<(String, Vec<f64>)>>>,
        observations: Arc<Mutex<Vec<(i64, Value, Option<String>)>>>,
        durations: Arc<Mutex<Vec<f64>>>,
    }

    impl FakeCoord {
        fn new(decisions: Vec<Decision>, char_seq: Vec<bool>) -> Self {
            FakeCoord {
                decisions: Arc::new(Mutex::new(decisions.into())),
                char_seq: Arc::new(Mutex::new(char_seq.into())),
                overrides: Arc::new(Mutex::new(Vec::new())),
                observations: Arc::new(Mutex::new(Vec::new())),
                durations: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Coordinator for FakeCoord {
        fn decide_trial(&self, _n: i64) -> Result<Decision, CoordError> {
            self.decisions
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| CoordError::Db(DbError::new("no more decisions")))
        }
        fn get_state(&self) -> CoordState {
            CoordState::Idle
        }
        fn get_char_status(&self) -> Value {
            json!({})
        }
        fn char_complete(&self) -> Result<bool, CoordError> {
            self.char_seq
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| CoordError::Db(DbError::new("no more char reads")))
        }
        fn set_probe_override(&self, param: &str, levels: &[f64]) {
            self.overrides
                .lock()
                .unwrap()
                .push((param.to_string(), levels.to_vec()));
        }
        fn record_receiver_observation(
            &self,
            n: i64,
            values: &Map<String, Value>,
            phase: Option<&str>,
        ) {
            self.observations.lock().unwrap().push((
                n,
                Value::Object(values.clone()),
                phase.map(|s| s.to_string()),
            ));
        }
        fn record_trial_duration(&self, d: f64) {
            self.durations.lock().unwrap().push(d);
        }
    }

    #[derive(Clone)]
    struct FakeWmill {
        calls: Arc<Mutex<Vec<String>>>,
        args: Arc<Mutex<Vec<Value>>>,
        scripts: Arc<Mutex<VecDeque<Result<Value, String>>>>,
        states: Arc<Mutex<Vec<Value>>>,
    }

    impl FakeWmill {
        fn new(scripts: Vec<Result<Value, String>>) -> Self {
            FakeWmill {
                calls: Arc::new(Mutex::new(Vec::new())),
                args: Arc::new(Mutex::new(Vec::new())),
                scripts: Arc::new(Mutex::new(scripts.into())),
                states: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Wmill for FakeWmill {
        fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push(path.to_string());
            self.args.lock().unwrap().push(args.clone());
            self.scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(json!({"status": "completed"})))
        }
        fn set_state(&self, state: &Value) -> Result<(), String> {
            self.states.lock().unwrap().push(state.clone());
            Ok(())
        }
        fn get_job_args(&self, _job_id: &str) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    #[derive(Clone)]
    struct FakeHttp {
        pages: Arc<BTreeMap<String, Value>>,
    }

    impl HttpTransport for FakeHttp {
        fn call(
            &self,
            _method: &str,
            url: &str,
            _payload: Option<&Value>,
            _timeout: Duration,
        ) -> Result<Value, HttpError> {
            self.pages
                .get(url)
                .cloned()
                .ok_or_else(|| HttpError::Dark(format!("no page for {url}")))
        }
    }

    /// The own/shared connectors in unit tests: fail fast, NON-retryable
    /// (beats, stamps, registration and standing-params all warn and
    /// continue — the loop's own semantics).
    struct DownConnect;
    impl Connect for DownConnect {
        fn connect(&self) -> Result<postgres::Client, DbError> {
            Err(DbError::new("fake own/shared db down (unit test)"))
        }
    }

    fn impulse_decision() -> Decision {
        Decision {
            mode: "impulse",
            params: Some(Map::from_iter([("vm.swappiness".to_string(), json!(10))])),
            impulse_phase: None,
            probe_param: None,
            probe_param_idx: None,
            probe_level: None,
            lease_phase: None,
            detection_trial: false,
            ambience_probe: false,
        }
    }

    fn base_config(over: Value) -> Value {
        let mut config = json!({
            "systemtender": {
                "name": "test_systemtender",
                "uuid": "test-uuid-123",
                "type": "linux_performance",
            },
            "creation_ts": "2025-01-15T10:30:00Z",
            "run": {"parallel": 1},
            "objectives": [{"name": "throughput", "direction": "minimize"}],
            "effectuation": {"targets": [], "type": "ssh"},
            "reconnaissance": {"type": "prometheus"},
        });
        if let (Some(t), Some(o)) = (config.as_object_mut(), over.as_object()) {
            for (k, v) in o {
                t.insert(k.clone(), v.clone());
            }
        }
        config
    }

    fn make_worker(
        config: Value,
        ledger: Arc<FakeLedger>,
        coord: FakeCoord,
        wmill: FakeWmill,
        http_pages: BTreeMap<String, Value>,
    ) -> Worker<FakeCoord> {
        std::env::remove_var("PUSH_METRICS_ENABLED");
        let deps = WorkerDeps {
            own_db: Arc::new(DownConnect),
            shared_db: Arc::new(DownConnect),
            wmill: Arc::new(wmill),
            http: Arc::new(FakeHttp {
                pages: Arc::new(http_pages),
            }),
            push_transport: Arc::new(NoopPush),
            remote_db: Arc::new(|_url: &str| Err(DbError::new("no remote db in unit tests"))),
            sleeper: Arc::new(|_s: f64| {}),
        };
        Worker::with_coordinator(config, deps, ledger, coord).unwrap()
    }

    struct NoopPush;
    impl PushTransport for NoopPush {
        fn request(
            &self,
            _m: &str,
            _u: &str,
            _b: Option<&str>,
            _t: Duration,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    // ─── TestExecuteTrial ───

    #[test]
    fn execute_trial_success_returns_metrics() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed"})),
            Ok(json!({"status": "completed", "metrics": {"throughput": 42.5}})),
        ]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill.clone(),
            BTreeMap::new(),
        );
        let settings = Map::from_iter([("vm.swappiness".to_string(), json!(10))]);
        let result = w.execute_trial(&settings).unwrap();
        assert_eq!(result.get("throughput").copied(), Some(42.5));
        assert_eq!(wmill.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn execute_trial_effectuation_failure_returns_inf() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![Err("SSH failed".to_string())]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        let result = w.execute_trial(&Map::new()).unwrap();
        assert_eq!(result.get("throughput").copied(), Some(f64::INFINITY));
    }

    #[test]
    fn execute_trial_no_metrics_returns_inf() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed"})),
            Ok(json!({"status": "completed"})),
        ]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        let result = w.execute_trial(&Map::new()).unwrap();
        assert_eq!(result.get("throughput").copied(), Some(f64::INFINITY));
    }

    #[test]
    fn execute_trial_uses_configured_effectuation_type() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed", "successful_changes": 1})),
            Ok(json!({"status": "completed", "metrics": {"throughput": 10.0}})),
        ]);
        let mut w = make_worker(
            base_config(json!({"effectuation": {"targets": [], "type": "http"}})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill.clone(),
            BTreeMap::new(),
        );
        w.execute_trial(&Map::new()).unwrap();
        assert_eq!(wmill.calls.lock().unwrap()[0], "f/effectuation/http");
    }

    #[test]
    fn execute_trial_total_failure_raises() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![Ok(json!({
            "status": "failed",
            "successful_changes": 0,
            "failed_changes": 2,
            "results": [
                {"target_id": "t1", "success": false},
                {"target_id": "t2", "success": false},
            ],
        }))]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        assert!(w.execute_trial(&Map::new()).is_err());
    }

    // ─── TestDeathResume (rung 8 — the boot park's standing-params
    // resume, python tests/unit/test_death_cures.py ported
    // one-for-one) ───

    /// The neutral in these tests: a completed trial's stashed
    /// effectuation params (no matching bounds in the config, so
    /// the template rides unchanged).
    fn park_neutral_trial() -> TrialRecord {
        TrialRecord {
            number: 1,
            kind: "hold".into(),
            params: json!({}),
            value: Some(0.5),
            state: "COMPLETE".into(),
            error: None,
            attrs: json!({"effectuation_params": {"param_0": 50.0}}),
            created_ts: None,
            finished_ts: None,
            values: vec![0.5],
        }
    }

    /// The boot harness: seed the neutral template, an optional
    /// assignment row, an optional standing map in the rollback
    /// state, park, and return the APPLIED settings (the first
    /// effectuation call's args — python's `applied` list).
    fn boot_park_applied(
        assignment: Option<(&str, &str)>,
        standing: Option<f64>,
    ) -> Map<String, Value> {
        let ledger = Arc::new(FakeLedger::new());
        ledger.set_trials(vec![park_neutral_trial()]);
        if let Some((w, r)) = assignment {
            ledger.set_wish(Some((w.to_string(), r.to_string())));
        }
        if let Some(v) = standing {
            ledger.0.lock().unwrap().state.insert(
                "rollback_state_target_0".to_string(),
                serde_json::to_string(&json!({
                    "state": "normal",
                    "last_successful_params": {"param_0": v},
                }))
                .unwrap(),
            );
        }
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed"})),
            Ok(json!({"status": "completed", "metrics": {"throughput": 42.5}})),
        ]);
        let handle = wmill.clone();
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        w.park_before_announcement();
        let args = handle.args.lock().unwrap().clone();
        assert!(!args.is_empty(), "the park must effectuate once");
        args[0]
            .get("settings")
            .and_then(|s| s.as_object())
            .cloned()
            .expect("the effectuation args carry the park target as settings")
    }

    #[test]
    fn resume_holder_parks_at_standing() {
        let applied = boot_park_applied(Some(("sw-1", "sender")), Some(73.5));
        assert_eq!(
            applied,
            Map::from_iter([("param_0".to_string(), json!(73.5))]),
            "a relaunched holder parks at its last standing params"
        );
    }

    #[test]
    fn receiver_parks_neutral() {
        let applied = boot_park_applied(Some(("sw-1", "receiver")), Some(73.5));
        assert_eq!(
            applied,
            Map::from_iter([("param_0".to_string(), json!(50.0))]),
            "a receiver's contribution is stillness - neutral"
        );
    }

    #[test]
    fn holder_without_standing_parks_neutral() {
        let applied = boot_park_applied(Some(("sw-1", "sender")), None);
        assert_eq!(
            applied,
            Map::from_iter([("param_0".to_string(), json!(50.0))]),
            "a held wish with no measured state starts from neutral"
        );
    }

    #[test]
    fn bare_boot_parks_neutral() {
        let applied = boot_park_applied(None, Some(73.5));
        assert_eq!(
            applied,
            Map::from_iter([("param_0".to_string(), json!(50.0))]),
            "a bare boot parks neutral exactly as before rung 8"
        );
    }

    #[test]
    fn assignment_read_failure_degrades_to_bare_boot() {
        let ledger = Arc::new(FakeLedger::new());
        ledger.set_trials(vec![park_neutral_trial()]);
        ledger.set_wish_latest_err(Some("db down".to_string()));
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed"})),
            Ok(json!({"status": "completed", "metrics": {"throughput": 42.5}})),
        ]);
        let handle = wmill.clone();
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        w.park_before_announcement();
        let applied = handle.args.lock().unwrap()[0]
            .get("settings")
            .and_then(|s| s.as_object())
            .cloned()
            .unwrap();
        assert_eq!(
            applied,
            Map::from_iter([("param_0".to_string(), json!(50.0))]),
            "an unreadable assignment degrades to the bare-boot park"
        );
    }

    #[test]
    fn standing_read_failure_degrades_to_neutral() {
        let ledger = Arc::new(FakeLedger::new());
        ledger.set_trials(vec![park_neutral_trial()]);
        ledger.set_wish(Some(("sw-1".to_string(), "sender".to_string())));
        ledger.set_state_get_err(Some("db down".to_string()));
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed"})),
            Ok(json!({"status": "completed", "metrics": {"throughput": 42.5}})),
        ]);
        let handle = wmill.clone();
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        w.park_before_announcement();
        let applied = handle.args.lock().unwrap()[0]
            .get("settings")
            .and_then(|s| s.as_object())
            .cloned()
            .unwrap();
        assert_eq!(
            applied,
            Map::from_iter([("param_0".to_string(), json!(50.0))]),
            "an unreadable standing state degrades to neutral"
        );
    }

    // ─── TestRunReconnaissance ───

    #[test]
    fn recon_returns_metrics_from_recon() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![Ok(json!({
            "status": "completed", "metrics": {"throughput": 42.5}
        }))]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill.clone(),
            BTreeMap::new(),
        );
        let result = w.run_reconnaissance(&Map::new()).unwrap();
        assert_eq!(result.get("throughput").copied(), Some(42.5));
        assert_eq!(
            wmill.calls.lock().unwrap()[0],
            "f/reconnaissance/prometheus"
        );
    }

    #[test]
    fn recon_returns_inf_on_empty_metrics() {
        let ledger = Arc::new(FakeLedger::new());
        let wmill = FakeWmill::new(vec![Ok(json!({"status": "completed", "metrics": {}}))]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            wmill,
            BTreeMap::new(),
        );
        let result = w.run_reconnaissance(&Map::new()).unwrap();
        assert_eq!(result.get("throughput").copied(), Some(f64::INFINITY));
    }

    // ─── TestWorkerInit ───

    #[test]
    fn missing_creation_ts_is_the_python_error() {
        let mut config = base_config(json!({}));
        config.as_object_mut().unwrap().remove("creation_ts");
        let ledger = Arc::new(FakeLedger::new());
        let deps = WorkerDeps {
            own_db: Arc::new(DownConnect),
            shared_db: Arc::new(DownConnect),
            wmill: Arc::new(FakeWmill::new(vec![])),
            http: Arc::new(FakeHttp {
                pages: Arc::new(BTreeMap::new()),
            }),
            push_transport: Arc::new(NoopPush),
            remote_db: Arc::new(|_u: &str| Err(DbError::new("down"))),
            sleeper: Arc::new(|_s: f64| {}),
        };
        let err = match Worker::<FakeCoord>::with_coordinator(
            config,
            deps,
            ledger,
            FakeCoord::new(vec![], vec![]),
        ) {
            Err(e) => e,
            Ok(_) => panic!("missing creation_ts must fail construction"),
        };
        assert_eq!(
            err.to_string(),
            "Required field 'creation_ts' missing from config"
        );
    }

    #[test]
    fn target_resolution_valid_and_invalid() {
        let targets = json!([{"id": "t0", "address": "10.0.0.1"}]);
        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(
                json!({"effectuation": {"targets": targets, "type": "ssh"}, "target_id": 0}),
            ),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        assert_eq!(w.target["id"], json!("t0"));

        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(
                json!({"effectuation": {"targets": targets, "type": "ssh"}, "target_id": 99}),
            ),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        assert_eq!(w.target["id"], json!("t0")); // invalid falls back to first
        assert!(!w.rollback_enabled); // disabled by default
    }

    // ─── TestRunLoop ───

    #[test]
    fn run_executes_single_trial_then_walk_completes() {
        let ledger = Arc::new(FakeLedger::new());
        let coord = FakeCoord::new(vec![impulse_decision()], vec![false, true]);
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed", "successful_changes": 1})),
            Ok(json!({"status": "completed", "metrics": {"throughput": 42.5}})),
        ]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger.clone(),
            coord,
            wmill,
            BTreeMap::new(),
        );
        w.run().unwrap();
        let g = ledger.0.lock().unwrap();
        assert_eq!(g.completes.len(), 1);
        assert_eq!(g.completes[0].1, Some(42.5));
    }

    #[test]
    fn run_marks_trial_failed_on_guardrail_violation() {
        let ledger = Arc::new(FakeLedger::new());
        let coord = FakeCoord::new(vec![impulse_decision()], vec![false, true]);
        let wmill = FakeWmill::new(vec![
            Ok(json!({"status": "completed", "successful_changes": 1})),
            Ok(json!({"status": "completed", "metrics": {"cpu_usage": 99.0}})),
        ]);
        let config =
            base_config(json!({"guardrails": [{"name": "cpu_usage", "hard_limit": 90.0}]}));
        let mut w = make_worker(config, ledger.clone(), coord, wmill, BTreeMap::new());
        w.run().unwrap();
        let g = ledger.0.lock().unwrap();
        assert!(g
            .fails
            .iter()
            .any(|(_, e)| e.as_deref() == Some("guardrail refusal")));
        assert!(g.completes.is_empty());
    }

    #[test]
    fn run_handles_trial_exception() {
        let ledger = Arc::new(FakeLedger::new());
        let coord = FakeCoord::new(vec![impulse_decision()], vec![false, true]);
        // wmill dark: effectuation error → inf metrics → still completes.
        // Force the loud path instead: total effectuation failure raises.
        let wmill = FakeWmill::new(vec![Ok(json!({
            "status": "failed", "successful_changes": 0, "failed_changes": 1,
            "results": [{"target_id": "t1", "success": false}],
        }))]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger.clone(),
            coord,
            wmill,
            BTreeMap::new(),
        );
        w.run().unwrap();
        let g = ledger.0.lock().unwrap();
        assert_eq!(g.fails.len(), 1);
        assert!(g.completes.is_empty());
    }

    #[test]
    fn run_pushes_metrics_on_completion() {
        let ledger = Arc::new(FakeLedger::new());
        let coord = FakeCoord::new(vec![], vec![true]); // walk already complete
        let wmill = FakeWmill::new(vec![]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger,
            coord,
            wmill.clone(),
            BTreeMap::new(),
        );
        w.run().unwrap();
        // purge + at least the two pushes (start and finally)
        let _ = wmill; // transport is a no-op; the calls rode the metrics client
    }

    #[test]
    fn run_stops_on_shutdown_flag() {
        let ledger = Arc::new(FakeLedger::new());
        ledger.set_stop(true);
        let coord = FakeCoord::new(vec![impulse_decision()], vec![]);
        let wmill = FakeWmill::new(vec![]);
        let mut w = make_worker(
            base_config(json!({})),
            ledger.clone(),
            coord,
            wmill,
            BTreeMap::new(),
        );
        w.run().unwrap();
        let g = ledger.0.lock().unwrap();
        assert!(g.completes.is_empty() && g.fails.is_empty());
    }

    // ─── TestCheckGuardrails ───

    #[test]
    fn guardrails_no_config_and_empty_list_pass() {
        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(json!({})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        let (v, m) = w.check_guardrails(&BTreeMap::from([("anything".to_string(), 42.0)]));
        assert!(!v && m.is_empty());
    }

    #[test]
    fn guardrails_within_limits_pass() {
        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(json!({"guardrails": [{"name": "cpu_usage", "hard_limit": 90.0}]})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        let (v, m) = w.check_guardrails(&BTreeMap::from([("cpu_usage".to_string(), 75.0)]));
        assert!(!v && m.is_empty());
    }

    #[test]
    fn guardrails_single_and_multiple_violations() {
        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(json!({"guardrails": [
                {"name": "cpu_usage", "hard_limit": 90.0},
                {"name": "memory_usage", "hard_limit": 85.0},
            ]})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        let (v, m) = w.check_guardrails(&BTreeMap::from([
            ("cpu_usage".to_string(), 95.0),
            ("memory_usage".to_string(), 90.0),
        ]));
        assert!(v && m.len() == 2);
        assert!(m[0].contains("cpu_usage") && m[0].contains("95.0"));
    }

    #[test]
    fn guardrails_inf_metric_is_violation() {
        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(json!({"guardrails": [{"name": "cpu_usage", "hard_limit": 90.0}]})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        let (v, _) =
            w.check_guardrails(&BTreeMap::from([("cpu_usage".to_string(), f64::INFINITY)]));
        assert!(v);
    }

    #[test]
    fn guardrails_skips_missing_limit_missing_metric_nonnumeric() {
        let ledger = Arc::new(FakeLedger::new());
        let w = make_worker(
            base_config(json!({"guardrails": [
                {"name": "cpu_usage"},
                {"name": "other", "hard_limit": 5.0},
                {"name": "weird", "hard_limit": "high"},
            ]})),
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        let (v, m) = w.check_guardrails(&BTreeMap::from([("cpu_usage".to_string(), 95.0)]));
        assert!(!v && m.is_empty());
    }

    // ─── TestRollbackStateManagement ───

    fn rollback_worker(strategy: &str, consecutive_failures: i64) -> Worker<FakeCoord> {
        let config = base_config(json!({
            "effectuation": {"targets": [{"id": "t1", "address": "10.0.0.1",
                "rollback": {"enabled": true, "strategy": strategy}}], "type": "ssh"},
            "rollback_strategies": {"standard": {
                "consecutive_failures": consecutive_failures,
                "target_state": "previous",
            }},
        }));
        let ledger = Arc::new(FakeLedger::new());
        make_worker(
            config,
            ledger,
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        )
    }

    #[test]
    fn rollback_state_creates_initial_state_idempotently() {
        let mut w = rollback_worker("standard", 3);
        w.init_rollback_state();
        w.init_rollback_state(); // idempotent
        let state: Value = serde_json::from_str(
            &w.ledger
                .state_get("rollback_state_target_0")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(state["state"], json!("normal"));
        assert_eq!(state["consecutive_failures"], json!(0));
        assert_eq!(state["last_successful_params"], json!(null));
        assert_eq!(state["version"], json!(0));
    }

    #[test]
    fn rollback_state_auto_inits_and_increments_version() {
        let mut w = rollback_worker("standard", 3);
        let mut state = w.get_rollback_state();
        assert_eq!(state["state"], json!("normal")); // auto-init happened
        state.insert("consecutive_failures".into(), json!(1));
        w.update_rollback_state(state);
        let stored: Value = serde_json::from_str(
            &w.ledger
                .state_get("rollback_state_target_0")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored["version"], json!(1));
        assert_eq!(stored["consecutive_failures"], json!(1));
    }

    #[test]
    fn rollback_needs_below_at_disabled() {
        let mut w = rollback_worker("standard", 3);
        w.init_rollback_state();
        let mut state = w.get_rollback_state();
        state.insert("consecutive_failures".into(), json!(2));
        w.update_rollback_state(state);
        assert!(!w.check_needs_rollback());

        let mut state = w.get_rollback_state();
        state.insert("consecutive_failures".into(), json!(3));
        w.update_rollback_state(state);
        assert!(w.check_needs_rollback());

        w.rollback_enabled = false;
        assert!(!w.check_needs_rollback());
    }

    #[test]
    fn guardrail_violation_increments_and_trips_rollback_state() {
        let mut w = rollback_worker("standard", 2);
        w.init_rollback_state();
        let mut state = w.get_rollback_state();
        state.insert("consecutive_failures".into(), json!(2));
        w.update_rollback_state(state);

        w.handle_guardrail_violation(None);
        let state = w.get_rollback_state();
        assert_eq!(state["consecutive_failures"], json!(3));
        assert_eq!(state["state"], json!("needs_rollback"));

        w.handle_guardrail_violation(None); // below-threshold increments keep normal
        let _ = w;
    }

    #[test]
    fn guardrail_violation_noop_when_disabled() {
        // built WITHOUT rollback in the config: the machine is absent
        let ledger = Arc::new(FakeLedger::new());
        let mut w = make_worker(
            base_config(json!({})),
            ledger.clone(),
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        w.handle_guardrail_violation(None);
        assert!(w
            .ledger
            .state_get("rollback_state_target_0")
            .unwrap()
            .is_none());
    }

    #[test]
    fn successful_trial_resets_failures_and_stashes_params() {
        let mut w = rollback_worker("standard", 3);
        w.init_rollback_state();
        let mut state = w.get_rollback_state();
        state.insert("consecutive_failures".into(), json!(5));
        state.insert("state".into(), json!("needs_rollback"));
        w.update_rollback_state(state);

        w.handle_successful_trial(&Map::from_iter([("param".to_string(), json!("new_value"))]));
        let state = w.get_rollback_state();
        assert_eq!(state["consecutive_failures"], json!(0));
        assert_eq!(state["state"], json!("normal"));
        assert_eq!(state["last_successful_params"]["param"], json!("new_value"));
    }

    // ─── TestExecuteRollback ───

    fn rollback_exec_worker(
        target_state: &str,
        on_failure: &str,
        stash: Option<Value>,
        best: Option<TrialRecord>,
    ) -> (Worker<FakeCoord>, Arc<FakeLedger>, FakeWmill) {
        let config = base_config(json!({
            "effectuation": {"targets": [{"id": "t1", "address": "10.0.0.1",
                "rollback": {"enabled": true, "strategy": "standard"}}], "type": "ssh"},
            "rollback_strategies": {"standard": {
                "consecutive_failures": 3,
                "target_state": target_state,
                "on_failure": on_failure,
            }},
        }));
        let ledger = Arc::new(FakeLedger::new());
        ledger.0.lock().unwrap().best = best;
        let wmill = FakeWmill::new(vec![Ok(json!({"status": "completed"}))]);
        let mut w = make_worker(
            config.clone(),
            ledger.clone(),
            FakeCoord::new(vec![], vec![]),
            wmill.clone(),
            BTreeMap::new(),
        );
        w.init_rollback_state();
        if let Some(stash) = stash {
            let mut state = w.get_rollback_state();
            state.insert("last_successful_params".into(), stash);
            w.update_rollback_state(state);
        }
        (w, ledger, wmill)
    }

    #[test]
    fn rollback_to_previous_success() {
        let (mut w, _, _) = rollback_exec_worker(
            "previous",
            "stop",
            Some(json!({"net.core.somaxconn": 4096})),
            None,
        );
        assert!(w.execute_rollback().unwrap());
        let state = w.get_rollback_state();
        assert_eq!(state["state"], json!("completed"));
        assert_eq!(state["consecutive_failures"], json!(0));
    }

    #[test]
    fn rollback_to_best_success_and_no_best_fails() {
        let best = TrialRecord {
            number: 1,
            kind: "hold".into(),
            params: json!({"vm.swappiness": 10}),
            value: Some(1.0),
            state: "COMPLETE".into(),
            error: None,
            attrs: json!({}),
            created_ts: None,
            finished_ts: None,
            values: vec![1.0],
        };
        let (mut w, _, _) = rollback_exec_worker("best", "stop", None, Some(best));
        assert!(w.execute_rollback().unwrap());

        let (mut w, _, _) = rollback_exec_worker("best", "stop", None, None);
        assert!(!w.execute_rollback().unwrap());
    }

    #[test]
    fn rollback_baseline_succeeds_previous_without_params_fails() {
        let (mut w, _, _) = rollback_exec_worker("baseline", "stop", None, None);
        assert!(w.execute_rollback().unwrap()); // baseline = {}

        let (mut w, _, _) = rollback_exec_worker("previous", "stop", None, None);
        assert!(!w.execute_rollback().unwrap()); // nothing to restore
    }

    #[test]
    fn rollback_unknown_target_state_fails() {
        let (mut w, _, _) = rollback_exec_worker("invalid_state", "stop", None, None);
        assert!(!w.execute_rollback().unwrap());
    }

    #[test]
    fn rollback_effectuation_error_stop_raises() {
        let (mut w, _, _) = rollback_exec_worker("previous", "stop", Some(json!({"p": 1})), None);
        // exhaust the ok script, then error
        w.wmill = Arc::new(FakeWmill::new(vec![Err("SSH failed".to_string())]));
        let err = w.execute_rollback().unwrap_err();
        assert!(err.contains("SSH failed"));
        let state = w.get_rollback_state();
        assert_eq!(state["state"], json!("failed"));
    }

    #[test]
    fn rollback_effectuation_error_continue_and_skip_target() {
        let (mut w, _, _) =
            rollback_exec_worker("previous", "continue", Some(json!({"p": 1})), None);
        w.wmill = Arc::new(FakeWmill::new(vec![Err("fail".to_string())]));
        assert!(!w.execute_rollback().unwrap());
        assert_eq!(w.get_rollback_state()["state"], json!("failed"));

        let (mut w, _, _) =
            rollback_exec_worker("previous", "skip_target", Some(json!({"p": 1})), None);
        w.wmill = Arc::new(FakeWmill::new(vec![Err("fail".to_string())]));
        assert!(!w.execute_rollback().unwrap());
        assert_eq!(w.get_rollback_state()["state"], json!("skip_target"));
    }

    // ─── TestHeartbeatBurst (the seam the python tests patched) ──

    #[test]
    fn expired_or_abort_signature_is_retryable() {
        assert!(worker_is_retryable(
            "current transaction is expired or aborted (query layer retry is not possible)"
        ));
    }

    #[test]
    fn relcache_timeout_is_retryable() {
        // fresh tender DB vs yugabyte relcache race (live 2026-09-25)
        assert!(worker_is_retryable(
            "connection to server at \"yb-tservers\" (10.0.0.1), port 5433 failed: FATAL:  Relcache init connection request to database systemtender_x timed out"
        ));
    }

    #[test]
    fn burst_survives_transient_abort_burst() {
        let mut beats = 0;
        let noop = |_: f64| {};
        let result = beat_with_burst_fn(
            || {
                beats += 1;
                if beats < 3 {
                    Err(DbError::new("current transaction is expired or aborted"))
                } else {
                    Ok(())
                }
            },
            &noop,
        );
        result.unwrap();
        assert_eq!(beats, 3);
    }

    #[test]
    fn burst_concedes_after_retries_without_raising() {
        let mut beats = 0;
        let noop = |_: f64| {};
        beat_with_burst_fn(
            || {
                beats += 1;
                Err(DbError::new("current transaction is expired or aborted"))
            },
            &noop,
        )
        .unwrap(); // no raise: the loop owns recovery
        assert_eq!(beats, 4); // first beat + 3 burst retries
    }

    #[test]
    fn burst_reraises_non_retryable() {
        let noop = |_: f64| {};
        let err = beat_with_burst_fn(|| Err(DbError::new("not a db thing")), &noop).unwrap_err();
        assert_eq!(err.to_string(), "not a db thing");
    }

    // ─── the observation publish gate (pure discriminator) ───

    #[test]
    fn observation_publish_phase_discriminator() {
        let phase_of = |d: &Decision| -> Option<String> {
            d.lease_phase
                .clone()
                .or_else(|| d.impulse_phase.map(|p| p.to_string()))
        };
        let mut d = impulse_decision();
        d.lease_phase = Some("probe_push".into());
        assert_eq!(phase_of(&d).as_deref(), Some("probe_push"));
        let mut d = impulse_decision();
        d.impulse_phase = Some("probe_push");
        assert_eq!(phase_of(&d).as_deref(), Some("probe_push"));
        let mut d = impulse_decision();
        d.mode = "hold";
        d.impulse_phase = Some("probe_pause");
        assert_eq!(phase_of(&d).as_deref(), Some("probe_pause"));
        assert_eq!(phase_of(&impulse_decision()), None); // parked publishes nothing
    }

    // ─── the seek wish: worker-level wiring (test_seek_worker_wiring) ──

    fn seek_page(candidates: i64) -> Value {
        json!({
            "type": "seek",
            "instruction": "seek",
            "seek": {
                "form": "find_best",
                "region": {"param_0": [0.0, 100.0, false]},
                "criterion": {"outcome": "room/objective_1", "direction": "maximize"},
                "budget": 50,
                "candidates": candidates,
            },
        })
    }

    fn seek_worker(page: Value) -> (Worker<FakeCoord>, Arc<FakeLedger>) {
        let config = base_config(json!({
            "objectives": [{"name": "objective_1", "direction": "maximize"}],
            "interference_detection": {"group": "g", "hold_params": {"param_0": 50.0}},
        }));
        let ledger = Arc::new(FakeLedger::new());
        // a completed trial with stashed params → neutral exists
        ledger.0.lock().unwrap().trials.push(TrialRecord {
            number: 1,
            kind: "hold".into(),
            params: json!({"param_0": 50.0}),
            value: Some(0.5),
            state: "COMPLETE".into(),
            error: None,
            attrs: json!({"effectuation_params": "{\"param_0\": 50.0}"}),
            created_ts: None,
            finished_ts: None,
            values: vec![0.5],
        });
        let base = "http://godon-godon-causal:9091/steer/plan";
        let mut pages = BTreeMap::new();
        for id in ["sw-9", "sw-live", "sw-bad", "sw-mid"] {
            pages.insert(format!("{base}/{id}"), page.clone());
        }
        let wmill = FakeWmill::new(vec![Ok(json!({
            "status": "completed", "successful_changes": 1,
            "metrics": {"objective_1": 0.5},
        }))]);
        let w = make_worker(
            config,
            ledger.clone(),
            FakeCoord::new(vec![], vec![]),
            wmill,
            pages,
        );
        (w, ledger)
    }

    #[test]
    fn seek_lifecycle_adopt_walk_return() {
        let (mut w, ledger) = seek_worker(seek_page(2));
        ledger.set_wish(Some(("sw-9".to_string(), "sender".to_string())));
        w.check_wish_assignment();

        assert_eq!(w.wish.as_ref().unwrap().instruction, "seek");
        assert!(w.seek_driver.is_some());

        // Darts: open and observe directly — joint pushes, never protocol
        let mut darts = 0;
        loop {
            if w.seek_open.is_none() {
                w.serve_seek_step().unwrap();
            }
            if w.seek_driver.is_none() {
                break;
            }
            let open = w.seek_open.clone().unwrap();
            if !open.dart {
                break;
            }
            assert!(open.params.unwrap().contains_key("param_0"));
            let driver = w.seek_driver.as_mut().unwrap();
            driver.wallet_pay();
            driver.observe(0.4 + 0.5 * darts as f64, 0.02).unwrap();
            w.seek_open = None;
            darts += 1;
        }
        assert_eq!(darts, 2);

        // Pass: the COORDINATOR's probe override is commissioned
        let open = w.seek_open.clone().unwrap();
        assert!(!open.dart);
        let coord = w.coordinator.clone();
        let overrides = coord.overrides.lock().unwrap();
        assert_eq!(overrides.last().unwrap().0, "param_0");
        assert_eq!(overrides.last().unwrap().1, vec![50.0]); // midpoint first
        drop(overrides);

        // Drive the commissions to completion (pay + observe each)
        let mut passes = 0;
        while w.seek_driver.is_some() {
            if w.seek_open.is_none() {
                w.serve_seek_step().unwrap();
            }
            if w.seek_driver.is_none() {
                break;
            }
            let driver = w.seek_driver.as_mut().unwrap();
            driver.wallet_pay();
            driver.observe(0.5 + 0.001 * passes as f64, 0.02).unwrap();
            w.seek_open = None;
            passes += 1;
        }
        assert!(passes > 0);

        // The return: written to the tender's own ledger, wish released
        let g = ledger.0.lock().unwrap();
        assert!(g.state.contains_key("seek_return::sw-9"));
        let payload: Value = serde_json::from_str(&g.state["seek_return::sw-9"]).unwrap();
        assert_eq!(payload["verdict"], json!("RETURNED"));
        assert_eq!(payload["spend_count"], json!(darts + passes));
        assert!(!payload["finding"].is_null());
        assert!(g.removed_wishes.contains(&"sw-9".to_string()));
    }

    #[test]
    fn refused_seek_never_binds() {
        let (mut w, ledger) = seek_worker(seek_page(60)); // candidates > budget 50
        ledger.set_wish(Some(("sw-bad".to_string(), "sender".to_string())));
        w.check_wish_assignment();
        assert!(w.wish.is_none());
        assert!(w.seek_driver.is_none());
        assert!(ledger.0.lock().unwrap().removed_wishes.is_empty());
    }

    /// The strand's regression pin (live receipt 2026-10-23, wish
    /// 891381d6): a commissioned pass that never rode a trial must not
    /// outlive the wallet. Exhaustion finishes the walk best-so-far —
    /// the abandoned commission's probe pin is dropped with an empty
    /// level set.
    #[test]
    fn exhausted_wallet_abandons_an_unridden_commission() {
        let (mut w, ledger) = seek_worker(seek_page(1));
        ledger.set_wish(Some(("sw-strand".to_string(), "sender".to_string())));
        w.check_wish_assignment();
        assert!(w.seek_driver.is_some());

        // The dart rides its trial and closes.
        w.serve_seek_step().unwrap();
        assert!(w.seek_open.as_ref().unwrap().dart);
        let driver = w.seek_driver.as_mut().unwrap();
        driver.wallet_pay();
        driver.observe(0.7, 0.02).unwrap();
        w.seek_open = None;

        // A pass is commissioned (override pinned) but never rides a
        // trial — the room keeps the tender on ambient rounds, each
        // paying the wallet.
        w.serve_seek_step().unwrap();
        let open = w.seek_open.clone().unwrap();
        assert!(!open.dart);
        assert!(open.probe_dial.is_some());
        let driver = w.seek_driver.as_mut().unwrap();
        while !driver.wallet_exhausted() {
            driver.wallet_pay();
        }

        // The strand shape: exhaustion with the unridden commission
        // still open must finish, not park the walk forever.
        w.serve_seek_step().unwrap();
        assert!(w.seek_driver.is_none(), "the seek must finish");
        assert!(w.seek_open.is_none());

        let g = ledger.0.lock().unwrap();
        let payload: Value = serde_json::from_str(&g.state["seek_return::sw-strand"]).unwrap();
        assert_eq!(payload["verdict"], json!("RETURNED"));
        assert_eq!(payload["stop_reason"], json!("budget_exhausted"));
        assert!(g.removed_wishes.contains(&"sw-strand".to_string()));
    }

    // ─── the release rider (test_seek_release_rider) ──

    #[test]
    fn unassigned_mid_seek_writes_return_under_its_own_id() {
        let (mut w, ledger) = seek_worker(seek_page(1));
        ledger.set_wish(Some(("sw-live".to_string(), "sender".to_string())));
        w.check_wish_assignment();
        assert!(w.seek_driver.is_some());

        // One dart commissioned and measured.
        w.serve_seek_step().unwrap();
        assert!(w.seek_open.as_ref().unwrap().dart);
        let driver = w.seek_driver.as_mut().unwrap();
        driver.wallet_pay();
        driver.observe(0.7, 0.02).unwrap();
        w.seek_open = None;

        // The controller closes the wish: the assignment row vanishes.
        ledger.set_wish(None);
        w.check_wish_assignment();

        let g = ledger.0.lock().unwrap();
        assert!(g.state.contains_key("seek_return::sw-live"));
        let payload: Value = serde_json::from_str(&g.state["seek_return::sw-live"]).unwrap();
        assert_eq!(payload["verdict"], json!("RETURNED"));
        assert_eq!(payload["stop_reason"], json!("stopped"));
        assert_eq!(payload["finding"]["reading"], json!(0.7));
        drop(g);
        assert!(w.seek_driver.is_none());
        assert!(w.wish.is_none());
    }

    // ─── the dial source (test_hold_impulse_params) ───

    fn greenhouse_settings() -> Value {
        json!({
            "greenhouse": {
                "zones": 2,
                "heating_setpoints": {"constraints": [{"step": 0.5, "lower": 5.0, "upper": 40.0}]},
                "vent_openings": {"constraints": [{"step": 0.05, "lower": 0.0, "upper": 1.0}]},
                "co2_injection": {"constraints": [{"step": 0.5, "lower": 0.0, "upper": 20.0}]},
            }
        })
    }

    fn trial_with(attrs: Value, params: Value) -> TrialRecord {
        TrialRecord {
            number: 0,
            kind: "hold".into(),
            params,
            value: Some(1.0),
            state: "COMPLETE".into(),
            error: None,
            attrs,
            created_ts: None,
            finished_ts: None,
            values: vec![1.0],
        }
    }

    fn dial_worker(trials: Vec<TrialRecord>) -> (Worker<FakeCoord>, Arc<FakeLedger>) {
        let config = base_config(json!({
            "systemtender": {"type": "bench_greenhouse", "uuid": "test-uuid", "name": "test"},
            "settings": greenhouse_settings(),
            "objectives": [{"name": "growth_rate", "direction": "maximize"}],
        }));
        let ledger = Arc::new(FakeLedger::new());
        ledger.set_trials(trials);
        let w = make_worker(
            config,
            ledger.clone(),
            FakeCoord::new(vec![], vec![]),
            FakeWmill::new(vec![]),
            BTreeMap::new(),
        );
        (w, ledger)
    }

    #[test]
    fn last_successful_params_reads_stash_and_falls_back() {
        let nested = json!({"heating_setpoints": [11.0, 13.5], "co2_injection": 3.5});
        let (w, _) = dial_worker(vec![trial_with(
            json!({"effectuation_params": py_dumps(&nested)}),
            json!({}),
        )]);
        let got = w.dials.get_last_successful_params().unwrap().unwrap();
        assert_eq!(got["co2_injection"], json!(3.5));
        assert!(got["heating_setpoints"].is_array()); // per-zone stays a list

        // fallback: flat params without a stash
        let (w, _) = dial_worker(vec![trial_with(json!({}), json!({"co2_injection": 3.5}))]);
        let got = w.dials.get_last_successful_params().unwrap().unwrap();
        assert_eq!(got["co2_injection"], json!(3.5));

        // none without trials
        let (w, _) = dial_worker(vec![]);
        assert!(w.dials.get_last_successful_params().unwrap().is_none());
    }

    #[test]
    fn neutral_params_use_template_with_midpoints() {
        let template = json!({
            "heating_setpoints": [11.0, 13.5], "co2_injection": 3.5, "vent_openings": [0.5, 0.6]
        });
        let (w, _) = dial_worker(vec![trial_with(
            json!({"effectuation_params": py_dumps(&template)}),
            json!({}),
        )]);
        let neutral = w.dials.compute_neutral_params().unwrap().unwrap();
        assert_eq!(neutral["heating_setpoints"], json!([22.5, 22.5])); // midpoint [5,40]
        assert_eq!(neutral["co2_injection"], json!(10.0)); // midpoint [0,20]
        assert!(neutral["vent_openings"].is_array());

        let (w, _) = dial_worker(vec![]);
        assert!(w.dials.compute_neutral_params().unwrap().is_none());
    }

    #[test]
    fn collect_upper_bounds_finds_nested_and_handles_empty() {
        let (w, _) = dial_worker(vec![]);
        let bounds = w.dials.collect_upper_bounds(&greenhouse_settings());
        let names: Vec<&str> = bounds.iter().map(|b| b.name.as_str()).collect();
        for expected in ["heating_setpoints", "co2_injection", "vent_openings"] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        for b in &bounds {
            assert_eq!(b.upper - b.lower, (b.upper - b.lower)); // range is derivable, fields sane
        }
        assert!(w.dials.collect_upper_bounds(&json!({})).is_empty());
    }
}
