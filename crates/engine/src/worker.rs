//! The worker's lifecycle spine — the port of `systemtender_worker.py`'s
//! config parse, continue-policy, and heartbeat shape. The deep body
//! (probe coordinator, rollback machine, wish serving, trials) lands in
//! its own slices against these seams; nothing here is faked.

use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

use crate::ledger::TrialLedger;

#[derive(Debug)]
pub enum WorkerError {
    MissingCreationTs,
    BadCreationTs(String),
    /// unknown strain type — `load_strain`'s ValueError, raised at
    /// boot between the group read and the creation_ts check
    /// (python's own ordering, so it wins when both are wrong)
    UnknownStrain(String),
    /// ledger/schema init failure (python let psycopg2's own exception
    /// out of __init__; the typed rust surface names it here)
    Init(String),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerError::MissingCreationTs => {
                write!(f, "Required field 'creation_ts' missing from config")
            }
            WorkerError::BadCreationTs(s) => {
                write!(f, "unparseable creation_ts: {s}")
            }
            WorkerError::UnknownStrain(msg) => write!(f, "{msg}"),
            WorkerError::Init(s) => write!(f, "worker init failed: {s}"),
        }
    }
}

/// creation_ts arrives offset-aware since the async-create executor
/// reads it from the registry row (postgres TIMESTAMPTZ); legacy
/// stamps were naive. Normalize to aware-UTC once, at the boundary:
/// every later clock comparison matches Utc::now().
/// (Receipt 10-10, godon#402: a naive now() once crashed every walker
/// at a clock check.)
pub fn parse_creation_ts(raw: &str) -> Result<DateTime<Utc>, WorkerError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&Utc));
    }
    // legacy naive stamps: assume UTC, exactly like the python
    // boundary (replace tzinfo=utc)
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f"))
        .map_err(|_| WorkerError::BadCreationTs(raw.to_string()))?;
    Ok(Utc.from_utc_datetime(&naive))
}

/// The worker's config surface, mirroring the python `__init__` parse
/// one-to-one: type/uuid from the systemtender section (uuid falls
/// back to name, then 'unknown'), the detection group with its
/// legacy-key fallbacks, creation_ts REQUIRED.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub systemtender_type: String,
    pub systemtender_uuid: String,
    pub group: String,
    pub creation_ts: DateTime<Utc>,
    pub run_id: i64,
    pub target_id: i64,
}

impl WorkerConfig {
    pub fn from_value(config: &Value) -> Result<Self, WorkerError> {
        let st = config
            .get("systemtender")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let systemtender_type = st
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_systemtender")
            .to_string();
        let systemtender_uuid = st
            .get("uuid")
            .or_else(|| st.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let det = config
            .get("interference_detection")
            .or_else(|| config.get("detection"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let group = det
            .get("group")
            .or_else(|| config.get("group"))
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();

        // python: strain_type = systemtender_config.get('type',
        // 'linux_performance'); self.strain = load_strain(strain_type)
        // — the boot-time refusal on unknown families sits BETWEEN the
        // group read and the creation_ts check, python's own order.
        let strain_type = crate::strains::strain_type_of(&st);
        crate::strains::load_strain(&strain_type).map_err(WorkerError::UnknownStrain)?;

        let creation_raw = config
            .get("creation_ts")
            .and_then(|v| v.as_str())
            .ok_or(WorkerError::MissingCreationTs)?;
        let creation_ts = parse_creation_ts(creation_raw)?;

        Ok(WorkerConfig {
            systemtender_type,
            systemtender_uuid,
            group,
            creation_ts,
            run_id: config.get("run_id").and_then(|v| v.as_i64()).unwrap_or(0),
            target_id: config
                .get("target_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        })
    }

    /// The ledger's database name, same derivation as the python side.
    pub fn db_name(&self) -> String {
        format!("systemtender_{}", self.systemtender_uuid.replace('-', "_"))
    }

    /// The worker identity string, same derivation.
    pub fn worker_id(&self) -> String {
        format!(
            "{}_worker_{}",
            self.systemtender_type, self.systemtender_uuid
        )
    }
}

/// The continue policy, pure: the ordering is the contract.
///
/// 1. The controller's stop flag ends the tender — nothing outranks a
///    noticed stop.
/// 2. A held wish PINS the holder: walk-complete is the walk's verdict
///    and stays true while holding, but a holder does not act on it —
///    the wish is a standing service on the wish's own budget, and
///    leaving would strand its assignment in a dead tender's ledger.
/// 3. Quiet-bench lever: with no wish held and the characterization
///    walk complete, the tender lets itself out — finished walkers
///    leave, the room goes still for whatever comes next.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// stop, with the reason the log line carries
    Stop(&'static str),
    /// continue; true = the wish pin fired this call (log the receipt
    /// once per held wish, not per loop)
    Continue { pin_fired: bool },
}

pub fn should_continue(
    stop_requested: bool,
    wish_held: bool,
    walk_complete: bool,
    pin_receipt_current: bool,
) -> Verdict {
    if stop_requested {
        return Verdict::Stop("Stopping: Shutdown requested by controller");
    }
    if wish_held {
        return Verdict::Continue {
            pin_fired: walk_complete && !pin_receipt_current,
        };
    }
    if walk_complete {
        return Verdict::Stop(
            "Stopping: characterization complete — walks done, leaving the bench quiet",
        );
    }
    Verdict::Continue { pin_fired: false }
}

/// One read of the verdict against the real ledger's stop flag — the
/// integration form of [`should_continue`].
pub fn should_continue_ledger(
    ledger: &TrialLedger,
    wish_held: bool,
    walk_complete: bool,
    pin_receipt_current: bool,
) -> Result<Verdict, crate::ledger::DbError> {
    let stop = ledger.stop_requested()?;
    Ok(should_continue(
        stop,
        wish_held,
        walk_complete,
        pin_receipt_current,
    ))
}

/// The liveness beater's shape: a parallel loop deliberately OUTSIDE
/// the trial cadence (trials can pause for minutes; a workload-bound
/// signal would make the "alive" window as slow as the slowest
/// phase). One beat per interval with a short fresh-try retry burst —
/// transient bursts are survived inside the same beat slot instead of
/// conceding the thump and waiting a full interval. The beat itself is
/// the caller's function (the python writes one tiny UPDATE into the
/// tender's own state row on its own throwaway connection; that
/// lands with the shared-db slice).
pub fn spawn_heartbeat<B>(mut beat: B, interval: Duration) -> JoinHandle<()>
where
    B: FnMut() -> Result<(), String> + Send + 'static,
{
    thread::spawn(move || loop {
        // one beat, with burst
        match beat() {
            Ok(()) => {}
            Err(first) => {
                // transient? quick retries inside this beat slot
                let mut last = first;
                for _ in 0..3 {
                    thread::sleep(Duration::from_secs(1));
                    match beat() {
                        Ok(()) => {
                            last.clear();
                            break;
                        }
                        Err(e) => last = e,
                    }
                }
                if !last.is_empty() {
                    // heartbeat beat failed (retries next beat)
                    eprintln!("heartbeat beat failed (retries next beat): {last}");
                }
            }
        }
        thread::sleep(interval);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_parse_defaults_and_fallbacks() {
        let cfg = WorkerConfig::from_value(&json!({
            "systemtender": {"type": "linux_performance", "name": "abc-1"},
            "creation_ts": "2026-10-10T12:00:00+00:00"
        }))
        .unwrap();
        assert_eq!(cfg.systemtender_type, "linux_performance");
        assert_eq!(cfg.systemtender_uuid, "abc-1"); // uuid falls back to name
        assert_eq!(cfg.group, "default");
        assert_eq!(cfg.db_name(), "systemtender_abc_1");
        assert_eq!(cfg.worker_id(), "linux_performance_worker_abc-1");
        assert_eq!(cfg.run_id, 0);
        assert_eq!(cfg.target_id, 0);
    }

    #[test]
    fn config_missing_creation_ts_is_the_python_error() {
        let err = WorkerConfig::from_value(&json!({"systemtender": {}})).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Required field 'creation_ts' missing from config"
        );
    }

    #[test]
    fn unknown_strain_type_is_the_loader_error() {
        let err = WorkerConfig::from_value(&json!({
            "systemtender": {"type": "nonexistent_strain"},
            "creation_ts": "2026-10-10T12:00:00+00:00"
        }))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unknown strain type: 'nonexistent_strain'. Available: \
             linux_performance, bench_greenhouse, bench_microgrid, bench_generic"
        );
    }

    #[test]
    fn unknown_strain_wins_over_missing_creation_ts() {
        // python raised load_strain BEFORE the creation_ts check —
        // both wrong means the strain error surfaces
        let err = WorkerConfig::from_value(&json!({
            "systemtender": {"type": "nonexistent_strain"}
        }))
        .unwrap_err();
        assert!(err.to_string().starts_with("Unknown strain type"));
    }

    #[test]
    fn creation_ts_aware_stays_legacy_naive_becomes_utc() {
        let aware = parse_creation_ts("2026-10-10T12:00:00+02:00").unwrap();
        assert_eq!(aware.to_rfc3339(), "2026-10-10T10:00:00+00:00");
        // legacy naive stamp (the study era's) — assumed UTC at the boundary
        let naive = parse_creation_ts("2026-10-10 12:00:00").unwrap();
        assert_eq!(naive.to_rfc3339(), "2026-10-10T12:00:00+00:00");
        assert!(parse_creation_ts("not a timestamp").is_err());
    }

    #[test]
    fn detection_group_legacy_keys() {
        let cfg = WorkerConfig::from_value(&json!({
            "detection": {"group": "legacy"},
            "creation_ts": "2026-10-10T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(cfg.group, "legacy");
    }

    /// The ordering IS the contract: stop beats the pin, the pin beats
    /// the quiet-bench lever.
    #[test]
    fn verdict_ordering() {
        use Verdict::{Continue, Stop};
        // stop flag outranks a held wish
        assert_eq!(
            should_continue(true, true, true, false),
            Stop("Stopping: Shutdown requested by controller")
        );
        // held wish pins past walk-complete; pin receipt fires once
        assert_eq!(
            should_continue(false, true, true, false),
            Continue { pin_fired: true }
        );
        assert_eq!(
            should_continue(false, true, true, true),
            Continue { pin_fired: false }
        );
        // no wish + walk complete: the quiet-bench lever
        assert_eq!(
            should_continue(false, false, true, false),
            Stop("Stopping: characterization complete — walks done, leaving the bench quiet")
        );
        // walk not complete, no wish: keep going
        assert_eq!(
            should_continue(false, false, false, false),
            Continue { pin_fired: false }
        );
    }

    #[test]
    fn heartbeat_beats_and_survives_transients() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let beats = Arc::new(AtomicUsize::new(0));
        let b = beats.clone();
        let mut calls = 0u32;
        // daemon semantics: the heartbeat thread outlives the test,
        // like python's daemon=True — never join it; the process exit
        // is its stop. (A join here hangs forever: receipt, first CI
        // flight of this slice.)
        let _handle = spawn_heartbeat(
            move || {
                calls += 1;
                b.fetch_add(1, Ordering::SeqCst);
                if calls == 1 {
                    // first call transiently fails exactly once — the
                    // burst must retry inside the same beat slot
                    Err("transient".to_string())
                } else {
                    Ok(())
                }
            },
            Duration::from_millis(50),
        );
        // window covers the burst retry's full 1s backoff (the python
        // shape) plus one interval: beat#1 (transient) -> +1s -> beat#2
        thread::sleep(Duration::from_millis(1800));
        assert!(beats.load(Ordering::SeqCst) >= 2, "heartbeat must beat");
    }
}
