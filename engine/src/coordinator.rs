//! The probe coordinator's room machinery — the port of
//! `probe_coordinator.py`'s shared-database layer: the room tables,
//! the census, and the sender-lease machine. The walk/characterization
//! half of the coordinator lands in its own slice against these traits.
//!
//! The room's DB layer is deliberately forgiving, exactly like the
//! python original: room machinery must never crash a walker — ops the
//! python side swallows into a warning return their python defaults
//! here (demand/presence/cleanup: ignored; pending-peers: empty;
//! live-peer-count: 1, assume contention; acquire: false). The
//! deliberate defaults and guards carry their receipts in comments.
//! Interval literals are interpolated into the SQL text with the same
//! integer seconds the python interpolated — constants, not user input.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::coordinator_walk::{CharWalk, CoordHooks, CoordState, ParamBound, Probe, ProbeOverride};
use crate::ledger::{retry_ladder, Connect, DbError, DsnConnect};
use crate::walk_policy::HttpTransport;

pub const WORST_CASE_TRIAL_SECONDS: i64 = 600;
pub const STALE_SENDER_MULTIPLIER: i64 = 5;
pub const MAX_HOLD_TRIALS: i64 = 200;
pub const ACTIVE_SYSTEMTENDER_WINDOW_SECONDS: i64 = 360;

/// The unified detection + characterization coordinator: the room
/// side (tables, census, lease machine) lives here; the
/// walk/characterization half (its state fields are below, its
/// behavior in [`crate::coordinator_walk`]) lands against these traits.
pub struct ProbeCoordinator<C: Connect = DsnConnect> {
    pub systemtender_id: String,
    pub group_id: String,
    pub push_block_size: i64,
    pub pause_block_size: i64,
    pub cooldown_trials: i64,
    pub active_window: i64,
    pub(crate) conn: C,
    /// the held connection for the trial loop's coordination verbs
    /// (the trial-cost census cure — same one-tx-per-write shape, no
    /// handshake per verb; see [`crate::ledger::ConnSlot`]). None on
    /// the injected/test shape.
    pub(crate) pool: Option<Arc<crate::ledger::ConnSlot>>,
    /// the whole config, kept for the walk half's re-reads (hold_params,
    /// settings, walk_policy/walk_transport keys — python parity)
    pub(crate) config: Value,
    /// the lease token held, read back on acquire (0 = none held; the
    /// python original's `_lease_token`, same lifecycle)
    pub lease_token: RefCell<i32>,
    /// the worker's injected callbacks (the python constructor's
    /// `collect_upper_bounds_fn` / `compute_neutral_params_fn`, plus
    /// the rust replacement for the python tests' config-injected
    /// walk transport callable)
    pub hooks: CoordHooks,
    /// the causal client — one transport behind the coordinator's own
    /// adaptive-timeout calls and the notebook walk's fixed-2s views
    pub http: Arc<dyn HttpTransport>,

    // ── walk/characterization half state (the python object's own) ──
    /// Characterization precision — the one customer-facing knob
    /// (default 0.02, `convergence_threshold`).
    pub convergence_threshold: f64,
    /// Max recursive refinement insertions between two measured
    /// points (default 3).
    pub refinement_depth: i64,
    pub causal_url: String,
    /// Quantum: probe cycles one systemtender holds the lease before
    /// yielding to a walk-pending peer (default 3; solo walkers never
    /// yield — zero churn).
    pub quantum_cycles: i64,
    pub(crate) state: RefCell<CoordState>,
    pub(crate) initialized: RefCell<bool>,
    pub(crate) idle_count: RefCell<i64>,
    pub(crate) push_count: RefCell<i64>,
    pub(crate) pause_count: RefCell<i64>,
    pub(crate) cooldown_count: RefCell<i64>,
    pub(crate) hold_count: RefCell<i64>,
    /// params retired by causal's two-key verdict
    pub(crate) converged_params: RefCell<HashSet<String>>,
    /// round timestamps for causal probe_result calls
    pub(crate) round_push_start: RefCell<Option<DateTime<Utc>>>,
    pub(crate) round_pause_end: RefCell<Option<DateTime<Utc>>>,
    /// causal RTT history (seconds, rolling 10) and the system's own
    /// trial-duration history — the adaptive timeout's two inputs
    pub(crate) causal_rtt_history: RefCell<Vec<f64>>,
    pub(crate) trial_duration_history: RefCell<Vec<f64>>,
    /// the map's named ambience debt for THIS tender, memoized so
    /// per-trial polls never hammer the notebook. None = not
    /// consulted or the consult went dark (a dark view is not an
    /// answer — never cached, never walked on).
    pub(crate) ambience_debt_cache: RefCell<Option<(std::time::Instant, Vec<Value>)>>,
    pub(crate) stretch_cycles: RefCell<i64>,
    pub(crate) char_walk: RefCell<Option<CharWalk>>,
    pub(crate) refinement_level: RefCell<i64>,
    pub(crate) param_names: RefCell<Vec<String>>,
    pub(crate) param_bounds: RefCell<BTreeMap<String, ParamBound>>,
    pub(crate) current_probe: RefCell<Option<Probe>>,
    pub(crate) neutral_params: RefCell<Option<serde_json::Map<String, Value>>>,
    pub(crate) hold_params_from_config: RefCell<bool>,
    pub(crate) probe_override: RefCell<Option<ProbeOverride>>,
    pub(crate) last_observed_phase: RefCell<Option<String>>,
}

impl ProbeCoordinator<DsnConnect> {
    pub fn new_dsn(
        systemtender_id: &str,
        config: &Value,
        hooks: CoordHooks,
        dsn: &str,
    ) -> Result<Self, DbError> {
        Ok(Self::new(systemtender_id, config, DsnConnect::new(dsn)?, hooks).with_pool(dsn))
    }
}

impl<C: Connect> ProbeCoordinator<C> {
    pub fn new(systemtender_id: &str, config: &Value, conn: C, hooks: CoordHooks) -> Self {
        let det = config
            .get("interference_detection")
            .or_else(|| config.get("detection"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let group_id = det
            .get("group")
            .or_else(|| config.get("group"))
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let causal_url = det
            .get("causal_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| std::env::var("GODON_CAUSAL_URL").ok())
            .unwrap_or_else(|| "http://godon-godon-causal:9091".to_string());
        ProbeCoordinator {
            systemtender_id: systemtender_id.to_string(),
            group_id,
            push_block_size: det
                .get("push_block_size")
                .and_then(|v| v.as_i64())
                .unwrap_or(10),
            pause_block_size: det
                .get("pause_block_size")
                .and_then(|v| v.as_i64())
                .unwrap_or(10),
            cooldown_trials: det
                .get("cooldown_trials")
                .and_then(|v| v.as_i64())
                .unwrap_or(5),
            active_window: det
                .get("active_systemtender_window")
                .and_then(|v| v.as_i64())
                .unwrap_or(ACTIVE_SYSTEMTENDER_WINDOW_SECONDS),
            conn,
            pool: None,
            config: config.clone(),
            lease_token: RefCell::new(0),
            http: hooks
                .http
                .clone()
                .unwrap_or_else(|| Arc::new(crate::walk_policy::ReqwestTransport)),
            convergence_threshold: det
                .get("convergence_threshold")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.02),
            refinement_depth: det
                .get("refinement_depth")
                .and_then(|v| v.as_i64())
                .unwrap_or(3),
            causal_url,
            quantum_cycles: det
                .get("quantum_cycles")
                .and_then(|v| v.as_i64())
                .unwrap_or(3),
            hooks,
            state: RefCell::new(CoordState::Idle),
            initialized: RefCell::new(false),
            idle_count: RefCell::new(0),
            push_count: RefCell::new(0),
            pause_count: RefCell::new(0),
            cooldown_count: RefCell::new(0),
            hold_count: RefCell::new(0),
            converged_params: RefCell::new(HashSet::new()),
            round_push_start: RefCell::new(None),
            round_pause_end: RefCell::new(None),
            causal_rtt_history: RefCell::new(Vec::new()),
            trial_duration_history: RefCell::new(Vec::new()),
            ambience_debt_cache: RefCell::new(None),
            stretch_cycles: RefCell::new(0),
            char_walk: RefCell::new(None),
            refinement_level: RefCell::new(0),
            param_names: RefCell::new(Vec::new()),
            param_bounds: RefCell::new(BTreeMap::new()),
            current_probe: RefCell::new(None),
            neutral_params: RefCell::new(None),
            hold_params_from_config: RefCell::new(false),
            probe_override: RefCell::new(None),
            last_observed_phase: RefCell::new(None),
        }
    }

    /// Attach the held connection (the production dsn shape) — the
    /// trial loop's verbs stop paying a handshake each.
    pub fn with_pool(mut self, dsn: &str) -> Self {
        self.pool = Some(Arc::new(crate::ledger::ConnSlot::new(dsn)));
        self
    }

    /// Attach an already-built slot (the worker's shared-db pool —
    /// one conn for the standing-params publish and the coordination
    /// verbs, both trial-thread users).
    pub fn attach_pool(&mut self, pool: Arc<crate::ledger::ConnSlot>) {
        self.pool = Some(pool);
    }

    pub fn stale_interval(&self) -> i64 {
        STALE_SENDER_MULTIPLIER * WORST_CASE_TRIAL_SECONDS
    }

    pub(crate) fn exec<T>(
        &self,
        mut op: impl FnMut(&mut postgres::Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if let Some(pool) = &self.pool {
            return pool.run_tx(|tx| op(tx));
        }
        let mut sleeper = |s: f64| std::thread::sleep(Duration::from_secs_f64(s));
        retry_ladder(&mut sleeper, || {
            let mut client = self.conn.connect()?;
            let mut txn = client.transaction().map_err(DbError::from)?;
            let out = op(&mut txn)?;
            txn.commit().map_err(DbError::from)?;
            Ok(out)
        })
    }

    /// Read-only coordination verbs: autocommit on the held connection
    /// (or a per-verb one) — a lone SELECT in its own tx and an
    /// autocommit SELECT read the same committed state; the census
    /// (2026-10-31) found the BEGIN/COMMIT wrapper was round trips,
    /// not isolation. Write verbs keep [`Self::exec`]'s transaction.
    pub(crate) fn exec_read<T>(
        &self,
        mut op: impl FnMut(&mut postgres::Client) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if let Some(pool) = &self.pool {
            return pool.run_read(|client| op(client));
        }
        let mut sleeper = |s: f64| std::thread::sleep(Duration::from_secs_f64(s));
        retry_ladder(&mut sleeper, || {
            let mut client = self.conn.connect()?;
            op(&mut client)
        })
    }

    pub fn ensure_tables(&self) -> Result<(), DbError> {
        self.exec(|txn| {
            // legacy singleton → group-scoped migration (drop if the
            // old shape is present)
            let legacy = txn.query_opt(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = 'sender_lease' AND column_name = 'id'",
                &[],
            )?;
            if legacy.is_some() {
                txn.execute("DROP TABLE sender_lease", &[])?;
            }
            txn.execute(
                "CREATE TABLE IF NOT EXISTS sender_lease (\
                 group_id VARCHAR(255) PRIMARY KEY, \
                 holder VARCHAR(255), \
                 token INT DEFAULT 0, \
                 phase VARCHAR(50), \
                 push_remaining INT DEFAULT 0, \
                 pause_remaining INT DEFAULT 0, \
                 last_heartbeat TIMESTAMPTZ)",
                &[],
            )?;
            txn.execute(
                "INSERT INTO sender_lease \
                 (group_id, holder, token, phase, push_remaining, pause_remaining, last_heartbeat) \
                 VALUES ($1, NULL, 0, NULL, 0, 0, NULL) ON CONFLICT (group_id) DO NOTHING",
                &[&self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
            )?;
            txn.execute(
                "CREATE TABLE IF NOT EXISTS detection_readiness (\
                 systemtender_id VARCHAR(255) PRIMARY KEY, \
                 group_id VARCHAR(255) NOT NULL DEFAULT 'default', \
                 ready_for VARCHAR(50) NOT NULL, \
                 ready_at TIMESTAMPTZ DEFAULT NOW())",
                &[],
            )?;
            let has_group = txn.query_opt(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = 'detection_readiness' AND column_name = 'group_id'",
                &[],
            )?;
            if has_group.is_none() {
                txn.execute(
                    "ALTER TABLE detection_readiness \
                     ADD COLUMN IF NOT EXISTS group_id VARCHAR(255) NOT NULL DEFAULT 'default'",
                    &[],
                )?;
            }
            // Receiver observations — written by receiver during HOLD,
            // read by sender to compute coupling shift after each
            // probe round.
            txn.execute(
                "CREATE TABLE IF NOT EXISTS receiver_observations (\
                 id SERIAL PRIMARY KEY, \
                 group_id VARCHAR(255) NOT NULL, \
                 receiver_id VARCHAR(255) NOT NULL, \
                 trial_num INT, \
                 objective_values JSONB, \
                 lease_phase VARCHAR(50), \
                 written_at TIMESTAMPTZ DEFAULT NOW())",
                &[],
            )?;
            Ok(())
        })
    }

    pub fn cleanup_stale_state(&self) {
        let stale = self.stale_interval();
        let sql = format!(
            "UPDATE sender_lease \
             SET holder = NULL, phase = NULL, \
             push_remaining = 0, pause_remaining = 0 \
             WHERE group_id = $1 AND holder IS NOT NULL \
             AND (last_heartbeat IS NULL \
             OR last_heartbeat < NOW() - INTERVAL '{stale} seconds')"
        );
        if let Err(e) = self.exec(|txn| {
            txn.execute(
                &sql,
                &[&self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
            )
            .map(|_| ())
            .map_err(DbError::from)
        }) {
            log::info!("Failed to cleanup stale state: {e}");
            return;
        }
        if let Err(e) = self.exec(|txn| {
            txn.execute(
                "DELETE FROM detection_readiness \
                 WHERE ready_at < NOW() - INTERVAL '10 minutes'",
                &[],
            )
            .map(|_| ())
            .map_err(DbError::from)
        }) {
            log::info!("Failed to cleanup stale state: {e}");
        }
    }

    /// Publish (or clear) this systemtender's walk-demand flag.
    pub fn publish_demand(&self, want: bool) {
        if let Err(e) = self.exec(|txn| {
            txn.execute(
                "UPDATE interference_active_systemtenders \
                 SET walk_pending = $1, last_seen = NOW() \
                 WHERE systemtender_id = $2",
                &[
                    &want as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                ],
            )
            .map(|_| ())
            .map_err(DbError::from)
        }) {
            log::info!("Failed to publish demand: {e}");
        }
    }

    /// Live group members (other than us) that want to walk. On
    /// failure: assume none (the python default).
    pub fn walk_pending_peers(&self) -> Vec<String> {
        let window = self.active_window;
        let sql = format!(
            "SELECT systemtender_id FROM interference_active_systemtenders \
             WHERE group_id = $1 AND systemtender_id <> $2 \
             AND last_seen > NOW() - INTERVAL '{window} seconds' \
             AND walk_pending IS TRUE"
        );
        self.exec_read(|client| {
            let rows = client.query(
                &sql,
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                ],
            )?;
            Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
        })
        .unwrap_or_else(|e| {
            log::info!("walk_pending_peers query failed ({e}) — assuming none");
            Vec::new()
        })
    }

    /// Contention check: does another live member want the mic?
    pub fn peer_walk_pending(&self) -> bool {
        !self.walk_pending_peers().is_empty()
    }

    /// Live group members other than us — regardless of demand: peers
    /// in HOLD behind an active sender cannot publish walk_pending, so
    /// their EXISTENCE is the only signal the holder's yield check can
    /// rely on. On failure: assume contention (python default 1).
    pub fn live_peer_count(&self) -> i64 {
        let window = self.active_window;
        let sql = format!(
            "SELECT COUNT(*) FROM interference_active_systemtenders \
             WHERE group_id = $1 AND systemtender_id <> $2 \
             AND last_seen > NOW() - INTERVAL '{window} seconds'"
        );
        self.exec_read(|client| {
            let row = client.query_one(
                &sql,
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                ],
            )?;
            Ok(row.get::<_, i64>(0))
        })
        .unwrap_or_else(|e| {
            log::info!("live_peer_count query failed ({e}) — assuming contention");
            1
        })
    }

    /// Acquire the sender lease under fair-share turn taking: publish
    /// demand, then acquire only while no walk-pending live peer has
    /// had fewer lease turns, and only once every live peer has parked
    /// (params IS NOT NULL — benches boot at param_lower; a window
    /// opened before a peer's first park banks that ambient as our
    /// effect; receipt run 36393481120). Poll speed cannot beat the
    /// count: a starved peer always outranks a recently served one.
    /// On failure or denial: false (a silent denial is invisible — the
    /// block is named in the log; receipt seed-47, ~2090 denials with
    /// zero log lines).
    pub fn try_acquire_lease(&self, phase: &str) -> bool {
        let want = self.peer_walk_pending();
        let stale = self.stale_interval();
        let window = self.active_window;
        let sql = format!(
            "UPDATE sender_lease \
             SET holder = $1, token = token + 1, phase = $2, \
             push_remaining = 0, pause_remaining = 0, \
             last_heartbeat = NOW() \
             WHERE group_id = $3 AND (\
             holder IS NULL \
             OR last_heartbeat IS NULL \
             OR last_heartbeat < NOW() - INTERVAL '{stale} seconds' \
             OR NOT EXISTS (\
             SELECT 1 FROM interference_active_systemtenders h \
             WHERE h.systemtender_id = sender_lease.holder \
             AND h.group_id = $4 \
             AND h.last_seen > NOW() - INTERVAL '{window} seconds')) \
             AND NOT EXISTS (\
             SELECT 1 FROM interference_active_systemtenders p \
             WHERE p.group_id = $5 AND p.systemtender_id <> $6 \
             AND p.last_seen > NOW() - INTERVAL '{window} seconds' \
             AND p.walk_pending IS TRUE \
             AND COALESCE(p.acquire_count, 0) < COALESCE((\
             SELECT acquire_count FROM interference_active_systemtenders \
             WHERE group_id = $7 AND systemtender_id = $8), 0)) \
             AND NOT EXISTS (\
             SELECT 1 FROM interference_active_systemtenders u \
             WHERE u.group_id = $9 AND u.systemtender_id <> $10 \
             AND u.last_seen > NOW() - INTERVAL '{window} seconds' \
             AND u.params IS NULL)"
        );
        self.exec(|txn| {
            // Publish demand (I am a candidate while my walk has work).
            txn.execute(
                "UPDATE interference_active_systemtenders \
                 SET walk_pending = $1, last_seen = NOW() \
                 WHERE systemtender_id = $2",
                &[
                    &want as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                ],
            )?;
            let updated = txn.execute(
                &sql,
                &[
                    &self.systemtender_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &phase,
                    &self.group_id,
                    &self.group_id,
                    &self.group_id,
                    &self.systemtender_id,
                    &self.group_id,
                    &self.systemtender_id,
                    &self.group_id,
                    &self.systemtender_id,
                ],
            )?;
            if updated == 0 {
                // name the block (the seed-47 receipt)
                let lease = txn.query_opt(
                    "SELECT holder, EXTRACT(EPOCH FROM (NOW() - last_heartbeat))::FLOAT8 \
                     FROM sender_lease WHERE group_id = $1",
                    &[&self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
                )?;
                let unparked_sql = format!(
                    "SELECT systemtender_id FROM interference_active_systemtenders \
                     WHERE group_id = $1 AND systemtender_id <> $2 \
                     AND last_seen > NOW() - INTERVAL '{window} seconds' \
                     AND params IS NULL"
                );
                let unparked = txn.query(
                    &unparked_sql,
                    &[
                        &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                        &self.systemtender_id,
                    ],
                )?;
                let pending = txn.query(
                    "SELECT systemtender_id FROM interference_active_systemtenders \
                     WHERE group_id = $1 AND systemtender_id <> $2 \
                     AND walk_pending IS TRUE",
                    &[
                        &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                        &self.systemtender_id,
                    ],
                )?;
                let unparked_ids: Vec<String> =
                    unparked.iter().map(|r| r.get::<_, String>(0)).collect();
                let pending_ids: Vec<String> =
                    pending.iter().map(|r| r.get::<_, String>(0)).collect();
                log::info!(
                    "Lease acquire denied for {} (phase={}): holder={:?} \
                     heartbeat_age={:?} unparked_peers={:?} walk_pending_peers={:?}",
                    self.systemtender_id,
                    phase,
                    lease.as_ref().map(|r| r.get::<_, Option<String>>(0)),
                    lease
                        .as_ref()
                        .and_then(|r| r.get::<_, Option<f64>>(1))
                        .map(|v| v as i64),
                    unparked_ids,
                    pending_ids,
                );
                return Ok(false);
            }
            let row = txn.query_one(
                "SELECT token FROM sender_lease WHERE group_id = $1",
                &[&self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
            )?;
            *self.lease_token.borrow_mut() = row.get(0);
            txn.execute(
                "UPDATE interference_active_systemtenders \
                 SET acquire_count = COALESCE(acquire_count, 0) + 1 \
                 WHERE systemtender_id = $1",
                &[&self.systemtender_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
            )?;
            Ok(true)
        })
        .unwrap_or_else(|e| {
            log::info!("Failed to acquire lease: {e}");
            false
        })
    }

    pub fn heartbeat(&self) -> bool {
        self.exec(|txn| {
            let n = txn.execute(
                "UPDATE sender_lease SET last_heartbeat = NOW() \
                 WHERE group_id = $1 AND holder = $2 AND token = $3",
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                    &*self.lease_token.borrow(),
                ],
            )?;
            Ok(n > 0)
        })
        .unwrap_or_else(|e| {
            log::info!("Heartbeat failed: {e}");
            false
        })
    }

    pub fn release_lease(&self) {
        let token = *self.lease_token.borrow();
        let res = self.exec(|txn| {
            txn.execute(
                "UPDATE sender_lease \
                 SET holder = NULL, phase = NULL, \
                 push_remaining = 0, pause_remaining = 0 \
                 WHERE group_id = $1 AND holder = $2 AND token = $3",
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.systemtender_id,
                    &token,
                ],
            )
            .map(|_| ())
            .map_err(DbError::from)
        });
        match res {
            Ok(()) => log::info!("Released sender lease (token={token})"),
            Err(e) => log::info!("Failed to release lease: {e}"),
        }
    }

    /// Set the lease phase, optionally resetting the push/pause
    /// budgets — the python built its SET list dynamically; same
    /// shape, bound parameters.
    pub fn set_lease_phase(
        &self,
        phase: &str,
        push_budget: Option<i64>,
        pause_budget: Option<i64>,
    ) {
        let mut sets = vec!["phase = $1".to_string()];
        let mut n = 1;
        if push_budget.is_some() {
            n += 1;
            sets.push(format!("push_remaining = ${n}"));
        }
        if pause_budget.is_some() {
            n += 1;
            sets.push(format!("pause_remaining = ${n}"));
        }
        let sql = format!(
            "UPDATE sender_lease SET {} \
             WHERE group_id = ${} AND holder = ${} AND token = ${}",
            sets.join(", "),
            n + 1,
            n + 2,
            n + 3
        );
        let phase = phase.to_string();
        // the budget columns are INT — psycopg2 sent python ints; the
        // i64 write sent nothing (a swallowed warning) until the walk
        // half's state-machine tests asserted the phase write
        // (receipt: CI run 38938181276)
        let push_b = push_budget.map(|v| v as i32);
        let pause_b = pause_budget.map(|v| v as i32);
        if let Err(e) = self.exec(|txn| {
            let token = *self.lease_token.borrow();
            let mut params: Vec<&(dyn postgres::types::ToSql + std::marker::Sync)> =
                vec![&phase as &(dyn postgres::types::ToSql + std::marker::Sync)];
            if let Some(p) = push_b.as_ref() {
                params.push(p);
            }
            if let Some(p) = pause_b.as_ref() {
                params.push(p);
            }
            params.push(&self.group_id);
            params.push(&self.systemtender_id);
            params.push(&token);
            txn.execute(&sql, &params[..])
                .map(|_| ())
                .map_err(DbError::from)
        }) {
            log::info!("Failed to set lease phase: {e}");
        }
    }

    pub fn decrement_budget(&self, phase: &str, is_push: bool) {
        let col = if is_push {
            "push_remaining"
        } else {
            "pause_remaining"
        };
        let sql = format!(
            "UPDATE sender_lease SET phase = $1, \
             {col} = GREATEST({col} - 1, 0) \
             WHERE group_id = $2 AND holder = $3 AND token = $4"
        );
        let phase = phase.to_string();
        if let Err(e) = self.exec(|txn| {
            let token = *self.lease_token.borrow();
            txn.execute(
                &sql,
                &[
                    &phase as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.group_id,
                    &self.systemtender_id,
                    &token,
                ],
            )
            .map(|_| ())
            .map_err(DbError::from)
        }) {
            log::info!("Failed to decrement budget: {e}");
        }
    }

    /// Keep this member's census row alive while parked. HOLD trials
    /// touch nothing; a member that stops refreshing while alive
    /// starves the group's own count and the rota freezes (receipt run
    /// 35206141133). Presence only — the demand flag stays whatever
    /// the walk last published.
    pub fn refresh_presence(&self) {
        if let Err(e) = self.exec(|txn| {
            txn.execute(
                "UPDATE interference_active_systemtenders \
                 SET last_seen = NOW() \
                 WHERE systemtender_id = $1",
                &[&self.systemtender_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
            )
            .map(|_| ())
            .map_err(DbError::from)
        }) {
            log::info!("Presence refresh failed: {e}");
        }
    }

    /// Is a live sender holding the lease? A lease is live only while
    /// its HOLDER is: a worker that died mid-walk leaves the row held
    /// with a fresh-looking heartbeat clock for the whole staleness
    /// window, and its lost token means even its own restart cannot
    /// reclaim — the holder's census row is the death detector.
    /// On failure: false (the python degrades and logs LOUD).
    pub fn has_active_sender(&self) -> bool {
        let stale = self.stale_interval();
        let window = self.active_window;
        let sql = format!(
            "SELECT count(*) FROM sender_lease \
             WHERE group_id = $1 AND holder IS NOT NULL \
             AND last_heartbeat IS NOT NULL \
             AND last_heartbeat > NOW() - INTERVAL '{stale} seconds' \
             AND EXISTS (\
             SELECT 1 FROM interference_active_systemtenders h \
             WHERE h.systemtender_id = sender_lease.holder \
             AND h.group_id = $2 \
             AND h.last_seen > NOW() - INTERVAL '{window} seconds')"
        );
        self.exec_read(|client| {
            let row = client.query_one(
                &sql,
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.group_id,
                ],
            )?;
            Ok(row.get::<_, i64>(0) > 0)
        })
        .unwrap_or_else(|e| {
            log::info!(
                "COORDINATION DB FAILURE (has_active_sender): {e} — degrading, probe protocol compromised"
            );
            false
        })
    }

    /// The live holder's phase — same liveness rule as
    /// [`Self::has_active_sender`]: a dead holder's phase must not
    /// gate receivers either. On failure: None.
    pub fn get_lease_phase(&self) -> Option<String> {
        let stale = self.stale_interval();
        let window = self.active_window;
        let sql = format!(
            "SELECT phase FROM sender_lease \
             WHERE group_id = $1 AND holder IS NOT NULL \
             AND last_heartbeat IS NOT NULL \
             AND last_heartbeat > NOW() - INTERVAL '{stale} seconds' \
             AND EXISTS (\
             SELECT 1 FROM interference_active_systemtenders h \
             WHERE h.systemtender_id = sender_lease.holder \
             AND h.group_id = $2 \
             AND h.last_seen > NOW() - INTERVAL '{window} seconds')"
        );
        self.exec_read(|client| {
            let row = client.query_opt(
                &sql,
                &[
                    &self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync),
                    &self.group_id,
                ],
            )?;
            Ok(row.and_then(|r| r.get::<_, Option<String>>(0)))
        })
        .unwrap_or_else(|e| {
            log::info!(
                "COORDINATION DB FAILURE (get_lease_phase): {e} — degrading, probe protocol compromised"
            );
            None
        })
    }

    /// Live census count for this group (ourselves included). On
    /// failure: 1 — assume solo (if peers exist, coordination is
    /// LOST; the log says so, the python default stands).
    pub fn count_active_systemtenders(&self) -> i64 {
        let window = self.active_window;
        let sql = format!(
            "SELECT COUNT(*) FROM interference_active_systemtenders \
             WHERE group_id = $1 AND last_seen > NOW() - INTERVAL '{window} seconds'"
        );
        self.exec_read(|client| {
            let row = client.query_one(
                &sql,
                &[&self.group_id as &(dyn postgres::types::ToSql + std::marker::Sync)],
            )?;
            Ok(row.get::<_, i64>(0))
        })
        .unwrap_or_else(|e| {
            log::info!(
                "COORDINATION DB FAILURE (count_active_systemtenders): {e} — assuming solo; if peers exist, coordination is LOST"
            );
            1
        })
    }

    /// The walk half's current state (the python `get_state`).
    pub fn get_state(&self) -> CoordState {
        *self.state.borrow()
    }
}

/// The census registration — worker-owned in the python original
/// (`_register_interference_systemtender`), housed here until the
/// worker's shared-db slice lands. Same DDL, same upsert.
pub fn register_interference_systemtender<C: Connect>(
    conn: &C,
    systemtender_id: &str,
    group_id: &str,
) -> Result<(), DbError> {
    let mut sleeper = |s: f64| std::thread::sleep(Duration::from_secs_f64(s));
    retry_ladder(&mut sleeper, || {
        let mut client = conn.connect()?;
        let mut txn = client.transaction().map_err(DbError::from)?;
        txn.execute(
            "CREATE TABLE IF NOT EXISTS interference_active_systemtenders (\
             systemtender_id VARCHAR(255) PRIMARY KEY, \
             group_id VARCHAR(255) NOT NULL DEFAULT 'default', \
             last_seen TIMESTAMPTZ DEFAULT NOW())",
            &[],
        )?;
        let has_group = txn.query_opt(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'interference_active_systemtenders' \
             AND column_name = 'group_id'",
            &[],
        )?;
        if has_group.is_none() {
            txn.execute(
                "ALTER TABLE interference_active_systemtenders \
                 ADD COLUMN IF NOT EXISTS group_id VARCHAR(255) NOT NULL DEFAULT 'default'",
                &[],
            )?;
        }
        // Lease fairness bookkeeping: walk_pending declares demand;
        // acquire_count records turns held.
        txn.execute(
            "ALTER TABLE interference_active_systemtenders \
             ADD COLUMN IF NOT EXISTS walk_pending BOOLEAN",
            &[],
        )?;
        txn.execute(
            "ALTER TABLE interference_active_systemtenders \
             ADD COLUMN IF NOT EXISTS acquire_count INT DEFAULT 0",
            &[],
        )?;
        // Standing dials: this systemtender's applied params — read by
        // causal to stamp curve points with the ambient they were
        // measured under.
        txn.execute(
            "ALTER TABLE interference_active_systemtenders \
             ADD COLUMN IF NOT EXISTS params JSONB",
            &[],
        )?;
        txn.execute(
            "INSERT INTO interference_active_systemtenders (systemtender_id, group_id, last_seen) \
             VALUES ($1, $2, NOW()) ON CONFLICT (systemtender_id) DO UPDATE \
             SET group_id = $2, last_seen = NOW()",
            &[
                &systemtender_id.to_string() as &(dyn postgres::types::ToSql + std::marker::Sync),
                &group_id.to_string(),
            ],
        )?;
        txn.commit().map_err(DbError::from)?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) mod db_tests {
    use super::*;
    use crate::ledger::DsnConnect;

    fn dsn() -> Option<String> {
        std::env::var("LEDGER_TEST_DSN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// The room tests share one postgres and each rebuilds the room:
    /// concurrent DROP/CREATE trips postgres's catalog race
    /// (pg_type duplicate), so the room is serial.
    pub(crate) static ROOM: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_room() -> std::sync::MutexGuard<'static, ()> {
        ROOM.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn fresh_room(dsn: &str) -> (ProbeCoordinator<DsnConnect>, ProbeCoordinator<DsnConnect>) {
        fn hooks() -> crate::coordinator_walk::CoordHooks {
            // room tests never touch the walk half — empty collector
            crate::coordinator_walk::CoordHooks::new(std::sync::Arc::new(|_| vec![]))
        }
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
        let cfg = serde_json::json!({"detection": {"group": "rung10"}});
        let a = ProbeCoordinator::new_dsn("tender-a", &cfg, hooks(), dsn).unwrap();
        let b = ProbeCoordinator::new_dsn("tender-b", &cfg, hooks(), dsn).unwrap();
        a.ensure_tables().unwrap();
        register_interference_systemtender(&DsnConnect::new(dsn).unwrap(), "tender-a", "rung10")
            .unwrap();
        register_interference_systemtender(&DsnConnect::new(dsn).unwrap(), "tender-b", "rung10")
            .unwrap();
        (a, b)
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

    fn holder(dsn: &str) -> Option<String> {
        let mut client = postgres::Client::connect(dsn, postgres::NoTls).unwrap();
        let row = client
            .query_one("SELECT holder FROM sender_lease", &[])
            .unwrap();
        row.get::<_, Option<String>>(0)
    }

    #[test]
    fn unparked_peer_blocks_the_acquire() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = lock_room();
        let (a, _b) = fresh_room(&dsn);
        park(&dsn, "tender-a");
        // tender-b registered but never parked (params NULL)
        assert!(!a.try_acquire_lease("probe_push"));
        assert_eq!(holder(&dsn), None);
    }

    #[test]
    fn acquire_heartbeat_release_cycle() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = lock_room();
        let (a, _b) = fresh_room(&dsn);
        park(&dsn, "tender-a");
        park(&dsn, "tender-b");
        assert!(a.try_acquire_lease("probe_push"));
        assert_eq!(holder(&dsn).as_deref(), Some("tender-a"));
        assert!(*a.lease_token.borrow() >= 1);
        assert!(a.heartbeat());
        a.set_lease_phase("probe_push", Some(3), Some(2));
        a.decrement_budget("probe_push", true);
        a.release_lease();
        assert_eq!(holder(&dsn), None);
        // token-guarded: heartbeat after release no longer lands
        assert!(!a.heartbeat());
    }

    #[test]
    fn fair_share_denies_the_served_peer() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = lock_room();
        let (a, b) = fresh_room(&dsn);
        park(&dsn, "tender-a");
        park(&dsn, "tender-b");
        assert!(a.try_acquire_lease("probe_push")); // a served once
        a.release_lease();
        // b wants the mic and has fewer turns: a must be denied
        raw(
            &dsn,
            "UPDATE interference_active_systemtenders \
             SET walk_pending = TRUE WHERE systemtender_id = 'tender-b'",
        );
        assert!(!a.try_acquire_lease("probe_push"));
        // and b takes it
        assert!(b.try_acquire_lease("probe_push"));
        assert_eq!(holder(&dsn).as_deref(), Some("tender-b"));
    }

    #[test]
    fn stale_holder_is_reaped_and_replaced() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = lock_room();
        let (a, b) = fresh_room(&dsn);
        park(&dsn, "tender-a");
        park(&dsn, "tender-b");
        assert!(a.try_acquire_lease("probe_push"));
        // backdate far past the stale interval (5 x 600s)
        raw(
            &dsn,
            "UPDATE sender_lease SET last_heartbeat = NOW() - INTERVAL '4000 seconds'",
        );
        a.cleanup_stale_state();
        assert_eq!(holder(&dsn), None);
        assert!(b.try_acquire_lease("probe_push"));
    }

    #[test]
    fn presence_demand_and_peer_counts() {
        let Some(dsn) = dsn() else {
            eprintln!("skipping: LEDGER_TEST_DSN not set");
            return;
        };
        let _room = lock_room();
        let (a, b) = fresh_room(&dsn);
        park(&dsn, "tender-a");
        park(&dsn, "tender-b");
        assert_eq!(a.live_peer_count(), 1);
        assert!(a.walk_pending_peers().is_empty());
        b.publish_demand(true);
        assert!(a.peer_walk_pending());
        assert_eq!(a.walk_pending_peers(), vec!["tender-b".to_string()]);
        b.publish_demand(false);
        assert!(!a.peer_walk_pending());
        a.refresh_presence();
    }
}
