//! The tender's trial ledger — the estate's own tables.
//!
//! Two tables in the tender's own database, one direct connection per
//! operation — slice 2 ports the SQL side whole: every verb below is
//! the full write and read surface the worker had in python
//! (`engine/trial_ledger.py`), on per-verb short connections behind a
//! retry ladder. Carried from day one, unchanged: the classification
//! function (pinned against the rust reader's `from_trials`), the
//! schema DDL (byte-identical tables — the readers in godon-images
//! read them), and the transient-failure patterns that take a retry,
//! never a lost trial.
//!
//! Port discipline: SQL strings are byte-identical to the python
//! original, embedded whitespace included; psycopg2's `%s`
//! placeholders become the postgres crate's `$N` — the one dialect
//! delta, values bound in the same order. Multi-statement verbs run
//! in one transaction, as psycopg2's implicit BEGIN/COMMIT gave the
//! python side. JSON that crosses the wire is emitted through the
//! `pyjson` writer, so every string this body hands out (stamps
//! payloads, `state_get` returns) is byte-shaped like python's
//! `json.dumps(..., sort_keys=True)` — the readers compare strings,
//! not parsed trees.

use std::fmt;

use serde_json::{Map, Value};

/// The one classification — pinned against the rust reader's
/// `from_trials`. push/pause/hold_calib come from the impulse phase
/// (probe_-prefix normalized away: a rename must never re-starve a
/// reader), receiver hold from detection mode + coordinator state.
///
/// A seek dart is its OWN kind: a joint random setting is
/// unattributable, so no reader may ever bank one as curve ground —
/// darts are priors, never map entries.
///
/// An ambience trial is its OWN kind: a fenced own-effect walk at the
/// resting spot — ambience entries at a named baseline, never chain
/// ground (the map keeps the two scopes apart).
///
/// Byte-compatible with the python original (see
/// `tests/unit/test_trial_ledger.py`, the golden mapping):
/// `None`/empty inputs behave exactly as the python `or`-chains do.
pub fn classify(
    mode: Option<&str>,
    impulse_phase: Option<&str>,
    coord_state: &str,
    dart: bool,
    ambience: bool,
) -> String {
    if dart {
        return "seek_prior".to_string();
    }
    if ambience {
        return "ambience".to_string();
    }
    let phase_raw = impulse_phase.unwrap_or("");
    let phase = phase_raw.strip_prefix("probe_").unwrap_or(phase_raw);
    if matches!(phase, "push" | "pause" | "hold_calib") {
        return phase.to_string();
    }
    if mode == Some("hold") && coord_state != "hold_calib" {
        return "hold".to_string();
    }
    match mode {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => "unclassified".to_string(),
    }
}

/// Schema version 1 — the two tables, byte-identical to the python
/// original's migrations. The readers in godon-images depend on these
/// shapes; any change is an estate motion, not a code decision.
pub const SCHEMA_VERSION: i64 = 1;

pub const MIGRATIONS_V1: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS trials (
            number      BIGSERIAL PRIMARY KEY,
            kind        TEXT NOT NULL DEFAULT 'unclassified',
            params      JSONB NOT NULL DEFAULT '{}',
            value       DOUBLE PRECISION,
            state       TEXT NOT NULL DEFAULT 'RUNNING',
            error       TEXT,
            attrs       JSONB NOT NULL DEFAULT '{}',
            created_ts  TIMESTAMPTZ NOT NULL DEFAULT now(),
            finished_ts TIMESTAMPTZ
        )",
    "CREATE TABLE IF NOT EXISTS ledger_state (
            key        TEXT PRIMARY KEY,
            value_json JSONB NOT NULL,
            updated_ts TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
];

/// Transient failures take a retry (per-verb short connections),
/// never a lost trial. Substrings, exactly as the python side matches
/// them.
pub const RETRYABLE_DB_ERROR_PATTERNS: [&str; 5] = [
    "SerializationFailure",
    "40001",
    "expired or aborted",
    "OperationalError",
    "server closed the connection",
];

/// The python original matched these substrings against `str(exc)`;
/// here the same match runs against the error's full text.
pub fn is_retryable(text: &str) -> bool {
    RETRYABLE_DB_ERROR_PATTERNS.iter().any(|p| text.contains(p))
}

/// Four attempts, sleeping 0.5 · 1.0 · 1.5 · 2.0s — after every
/// failed attempt, the fourth included: the python original's loop
/// slept after its last failure too, before raising (`max_retries=4`,
/// `0.5 * (attempt + 1)`).
pub const MAX_RETRIES: usize = 4;
pub const RETRY_SLEEP_BASE_SECS: f64 = 0.5;

/// The error the ledger speaks. `Display` is the whole text the retry
/// ladder matches on: the postgres error's `Display` and its `Debug`
/// appended — the Debug half carries the SQLSTATE word (e.g.
/// `SerializationFailure`) the way psycopg2's exception class names
/// rode python's errors.
#[derive(Debug, Clone)]
pub struct DbError {
    pub message: String,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        DbError {
            message: message.into(),
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

impl From<postgres::Error> for DbError {
    fn from(e: postgres::Error) -> Self {
        DbError {
            message: format!("{e} | {e:?}"),
        }
    }
}

/// Payloads are produced by `pyjson` one line above the parse; a
/// failure here is a writer bug, not a database fault — but it
/// surfaces through the same error type the verbs speak.
impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        DbError {
            message: e.to_string(),
        }
    }
}

/// The connection source — one short connection per verb attempt,
/// exactly as the python original called `psycopg2.connect` inside
/// every `_with_conn` attempt. Tests swap this to inject transient
/// failures at the connection edge.
pub trait Connect {
    fn connect(&self) -> Result<postgres::Client, DbError>;
}

/// The production source: the DSN string, parsed at connect time —
/// psycopg2's own shape (its `connect(dsn)` parsed on every call).
#[derive(Clone, Debug)]
pub struct DsnConnect {
    dsn: String,
}

impl DsnConnect {
    pub fn new(dsn: &str) -> Result<Self, DbError> {
        Ok(DsnConnect {
            dsn: dsn.to_string(),
        })
    }
}

impl Connect for DsnConnect {
    fn connect(&self) -> Result<postgres::Client, DbError> {
        postgres::Client::connect(self.dsn.as_str(), postgres::NoTls).map_err(DbError::from)
    }
}

/// The retry ladder — a transient takes a retry, never a lost trial:
/// four attempts, the error's text decides (see [`is_retryable`]),
/// non-transient errors stand on first contact, exhaustion raises the
/// LAST error raw (the python original raised `last`, unwrapped —
/// its interpolated per-verb descriptions reached only a
/// never-fired RuntimeError branch, so they are deliberately not
/// ported). The sleep is injected: production verbs sleep for real,
/// the ladder tests run without wall-clock waits.
pub fn retry_ladder<T>(
    sleep: &mut dyn FnMut(f64),
    mut attempt: impl FnMut() -> Result<T, DbError>,
) -> Result<T, DbError> {
    let mut last: Option<DbError> = None;
    for i in 0..MAX_RETRIES {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) => {
                if !is_retryable(&err.message) {
                    return Err(err);
                }
                sleep(RETRY_SLEEP_BASE_SECS * (i as f64 + 1.0));
                last = Some(err);
            }
        }
    }
    Err(last.expect("four attempts always record the last error"))
}

/// One trial row as the read verbs return it — the shape the python
/// original built per row (params/attrs coerced to objects, `values`
/// a one-element list when judged).
#[derive(Debug, Clone)]
pub struct TrialRecord {
    pub number: i64,
    pub kind: String,
    pub params: Value,
    pub value: Option<f64>,
    pub state: String,
    pub error: Option<String>,
    pub attrs: Value,
    pub created_ts: Option<String>,
    pub finished_ts: Option<String>,
    pub values: Vec<f64>,
}

impl TrialRecord {
    fn from_row(row: &postgres::Row) -> Result<Self, DbError> {
        let params: Value = row.try_get(2)?;
        let attrs: Value = row.try_get(6)?;
        let value: Option<f64> = row.try_get(3)?;
        Ok(TrialRecord {
            number: row.try_get(0)?,
            kind: row.try_get(1)?,
            params: object_or_empty(params),
            value,
            state: row.try_get(4)?,
            error: row.try_get(5)?,
            attrs: object_or_empty(attrs),
            created_ts: row.try_get(7)?,
            finished_ts: row.try_get(8)?,
            values: value.map(|v| vec![v]).unwrap_or_default(),
        })
    }
}

fn object_or_empty(v: Value) -> Value {
    if v.is_object() {
        v
    } else {
        Value::Object(Map::new())
    }
}

/// The trial ledger — per-verb short connections; a transient takes a
/// retry, never a lost trial.
#[derive(Clone, Debug)]
pub struct TrialLedger<C: Connect = DsnConnect> {
    connect: C,
}

impl TrialLedger<DsnConnect> {
    pub fn new(dsn: &str) -> Result<Self, DbError> {
        Ok(TrialLedger {
            connect: DsnConnect::new(dsn)?,
        })
    }
}

impl<C: Connect> TrialLedger<C> {
    /// One attempt = connect, one transaction, op, commit — the
    /// python original's `connect / cursor / fn / commit / close`
    /// shape; the connection closes on drop either way.
    fn with_conn<T>(
        &self,
        op: impl FnOnce(&mut postgres::Transaction) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut op = Some(op);
        retry_ladder(
            &mut |secs| std::thread::sleep(std::time::Duration::from_secs_f64(secs)),
            || {
                let op = op.take().expect("one op per attempt");
                let mut conn = self.connect.connect()?;
                let mut tx = conn.transaction().map_err(DbError::from)?;
                let result = op(&mut tx)?;
                tx.commit().map_err(DbError::from)?;
                Ok(result)
            },
        )
    }

    // ─── schema ──────────────────────────────────────────────────

    pub fn ensure_schema(&self) -> Result<i32, DbError> {
        self.with_conn(|tx| {
            tx.execute(
                "CREATE TABLE IF NOT EXISTS schema_version (version INT NOT NULL)",
                &[],
            )
            .map_err(DbError::from)?;
            let rows = tx
                .query("SELECT version FROM schema_version", &[])
                .map_err(DbError::from)?;
            let current = match rows.iter().next() {
                Some(row) => row.try_get::<_, i32>(0)?,
                None => {
                    tx.execute("INSERT INTO schema_version (version) VALUES (0)", &[])
                        .map_err(DbError::from)?;
                    0
                }
            };
            for version in [1i32] {
                if version > current {
                    for stmt in MIGRATIONS_V1 {
                        tx.execute(stmt, &[]).map_err(DbError::from)?;
                    }
                    tx.execute("UPDATE schema_version SET version = $1", &[&version])
                        .map_err(DbError::from)?;
                }
            }
            Ok(current)
        })
    }

    // ─── write verbs ─────────────────────────────────────────────

    /// Append a trial row; returns its number.
    pub fn begin(&self) -> Result<i64, DbError> {
        self.with_conn(|tx| {
            let row = tx
                .query_one(
                    "INSERT INTO trials (kind) VALUES ('unclassified') RETURNING number",
                    &[],
                )
                .map_err(DbError::from)?;
            row.try_get::<_, i64>(0).map_err(DbError::from)
        })
    }

    pub fn set_kind(&self, number: i64, kind: &str) -> Result<(), DbError> {
        self.with_conn(|tx| {
            tx.execute(
                "UPDATE trials SET kind = $1 WHERE number = $2",
                &[&kind, &number],
            )
            .map_err(DbError::from)?;
            Ok(())
        })
    }

    pub fn stamp(&self, number: i64, key: &str, value: Value) -> Result<(), DbError> {
        let mut values = Map::new();
        values.insert(key.to_string(), value);
        self.stamps(number, &values)
    }

    /// Merge keys into attrs. Values as given (see the module
    /// docstring's encoding rules): strings land as JSON strings,
    /// floats as JSON numbers, pre-serialized JSON strings stay
    /// strings.
    pub fn stamps(&self, number: i64, values: &Map<String, Value>) -> Result<(), DbError> {
        let payload = pyjson::py_dumps(&Value::Object(values.clone()));
        // $1::jsonb types the prepared parameter as jsonb, so the bind
        // is a Value (psycopg2 sent text and let the server cast).
        let payload: Value = serde_json::from_str(&payload)?;
        self.with_conn(|tx| {
            tx.execute(
                "UPDATE trials SET attrs = attrs || $1::jsonb WHERE number = $2",
                &[&payload, &number],
            )
            .map_err(DbError::from)?;
            Ok(())
        })
    }

    pub fn complete(&self, number: i64, value: f64) -> Result<(), DbError> {
        self.with_conn(|tx| {
            tx.execute(
                "UPDATE trials SET state = 'COMPLETE', value = $1, finished_ts = now() WHERE number = $2",
                &[&value, &number],
            )
            .map_err(DbError::from)?;
            Ok(())
        })
    }

    pub fn fail(&self, number: i64, error: Option<&str>) -> Result<(), DbError> {
        self.with_conn(|tx| {
            tx.execute(
                "UPDATE trials SET state = 'FAIL', error = $1, finished_ts = now() WHERE number = $2",
                &[&error, &number],
            )
            .map_err(DbError::from)?;
            Ok(())
        })
    }

    // ─── state table (rollback machine and friends) ──────────────

    /// value is a string (call sites serialize); stored JSON-encoded
    /// so readers decode exactly as they decoded study attrs.
    pub fn state_put(&self, key: &str, value: &str) -> Result<(), DbError> {
        let payload = pyjson::py_dumps(&Value::String(value.to_string()));
        let payload: Value = serde_json::from_str(&payload)?;
        self.with_conn(|tx| {
            tx.execute(
                "INSERT INTO ledger_state (key, value_json, updated_ts) VALUES ($1, $2::jsonb, now()) ON CONFLICT (key) DO UPDATE SET value_json = EXCLUDED.value_json, updated_ts = now()",
                &[&key, &payload],
            )
            .map_err(DbError::from)?;
            Ok(())
        })
    }

    pub fn state_get(&self, key: &str) -> Result<Option<String>, DbError> {
        self.with_conn(|tx| {
            let row = tx
                .query_opt(
                    "SELECT value_json FROM ledger_state WHERE key = $1",
                    &[&key],
                )
                .map_err(DbError::from)?;
            match row {
                None => Ok(None),
                Some(row) => {
                    let value: Value = row.try_get(0)?;
                    match value {
                        Value::Null => Ok(None),
                        // a jsonb string scalar comes back raw — the
                        // python original's live isinstance-str branch
                        Value::String(s) => Ok(Some(s)),
                        v => Ok(Some(pyjson::py_dumps(&v))),
                    }
                }
            }
        })
    }

    // ─── read verbs (boot replays, metrics) ──────────────────────

    /// All trials, number order. attrs decoded to a value; `values`
    /// mirrors the reader contract: one-element list when judged.
    pub fn trials(&self) -> Result<Vec<TrialRecord>, DbError> {
        self.with_conn(|tx| {
            let rows = tx
                .query(
                    "SELECT number, kind, params, value, state, error, attrs, CAST(created_ts AS TEXT), CAST(finished_ts AS TEXT) FROM trials ORDER BY number",
                    &[],
                )
                .map_err(DbError::from)?;
            rows.iter().map(TrialRecord::from_row).collect()
        })
    }

    pub fn count(&self) -> Result<i64, DbError> {
        self.with_conn(|tx| {
            let row = tx
                .query_one("SELECT COUNT(*) AS n FROM trials", &[])
                .map_err(DbError::from)?;
            row.try_get::<_, i64>(0).map_err(DbError::from)
        })
    }

    /// The best judged trial — computed, no stored front. Params come
    /// back exactly as written (empty until the surface widens).
    pub fn best_trial(&self, minimize: bool) -> Result<Option<TrialRecord>, DbError> {
        let order = if minimize { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT number, kind, params, value, state, error, attrs, CAST(created_ts AS TEXT), CAST(finished_ts AS TEXT) FROM trials WHERE state = 'COMPLETE' AND value IS NOT NULL ORDER BY value {order}, number ASC LIMIT 1"
        );
        self.with_conn(|tx| {
            let row = tx.query_opt(&sql, &[]).map_err(DbError::from)?;
            row.as_ref().map(TrialRecord::from_row).transpose()
        })
    }

    // ─── side reads that rode the old storage engine ─────────────

    /// The tender's stop flag, same table the door writes.
    pub fn stop_requested(&self) -> Result<bool, DbError> {
        self.with_conn(|tx| {
            let row = tx
                .query_opt(
                    "SELECT shutdown_requested FROM systemtender_state LIMIT 1",
                    &[],
                )
                .map_err(DbError::from)?;
            match row {
                Some(row) => Ok(row.try_get::<_, bool>(0)?),
                None => Ok(false),
            }
        })
    }

    /// The wish assignment row, if any: (wish_id, role) or None.
    /// Creates/migrates the table exactly as the worker did.
    pub fn wish_latest(&self) -> Result<Option<(String, String)>, DbError> {
        self.with_conn(|tx| {
            tx.execute(
                "CREATE TABLE IF NOT EXISTS wish_assignments (wish_id TEXT PRIMARY KEY, assigned_tsz DOUBLE PRECISION NOT NULL)",
                &[],
            )
            .map_err(DbError::from)?;
            tx.execute(
                "ALTER TABLE wish_assignments ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'sender'",
                &[],
            )
            .map_err(DbError::from)?;
            let row = tx
                .query_opt(
                    "SELECT wish_id, COALESCE(role, 'sender') AS role FROM wish_assignments ORDER BY assigned_tsz DESC LIMIT 1",
                    &[],
                )
                .map_err(DbError::from)?;
            match row {
                None => Ok(None),
                Some(row) => Ok(Some((row.try_get(0)?, row.try_get(1)?))),
            }
        })
    }

    pub fn wish_remove(&self, wish_id: &str) -> Result<(), DbError> {
        self.with_conn(|tx| {
            tx.execute(
                "DELETE FROM wish_assignments WHERE wish_id = $1",
                &[&wish_id],
            )
            .map_err(DbError::from)?;
            Ok(())
        })
    }
}

/// The worker-facing trial handle: a number and a stamp method.
/// Keeps the trial-loop call sites shaped exactly as the study era
/// wrote them; the stamps land in the ledger's attrs column.
#[derive(Clone, Debug)]
pub struct TrialRef<C: Connect = DsnConnect> {
    ledger: TrialLedger<C>,
    pub number: i64,
}

impl<C: Connect> TrialRef<C> {
    pub fn new(ledger: TrialLedger<C>, number: i64) -> Self {
        TrialRef { ledger, number }
    }

    pub fn set_user_attr(&self, key: &str, value: Value) -> Result<(), DbError> {
        self.ledger.stamp(self.number, key, value)
    }
}

/// Reader-side canonicalization: whatever JSONB returned (an already
/// decoded object, or a string) becomes the same shape the study era
/// produced — python's `to_attrs_json`.
pub fn to_attrs_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        v => pyjson::py_dumps(v),
    }
}

/// Python-shaped JSON, byte-for-byte: keys sorted, separators
/// `", "` and `": "`, non-ASCII escaped (`ensure_ascii`), floats in
/// python repr form (`75.0`, never `75`). One writer serves every
/// JSON string the body emits — stamps payloads, `state_get`
/// returns, and the golden fixture dump — so all seams carry the
/// same bytes. `serde_json`'s default (sorted) map underpins this,
/// but the writer sorts explicitly, so enabling `preserve_order`
/// elsewhere cannot silently unsort these strings.
mod pyjson {
    use serde_json::Value;

    pub fn py_dumps(v: &Value) -> String {
        let mut out = String::new();
        write_compact(v, &mut out);
        out
    }

    pub fn py_dumps_indent(v: &Value) -> String {
        let mut out = String::new();
        write_indented(v, &mut out, 0);
        out
    }

    fn write_compact(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => write_number(n, out),
            Value::String(s) => write_escaped(s, out),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_compact(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                for (i, (k, x)) in sorted(map).iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_escaped(k, out);
                    out.push_str(": ");
                    write_compact(x, out);
                }
                out.push('}');
            }
        }
    }

    fn write_indented(v: &Value, out: &mut String, level: usize) {
        match v {
            Value::Array(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    push_indent(out, level + 1);
                    write_indented(item, out, level + 1);
                }
                out.push('\n');
                push_indent(out, level);
                out.push(']');
            }
            Value::Array(_) => out.push_str("[]"),
            Value::Object(map) if !map.is_empty() => {
                out.push_str("{\n");
                for (i, (k, x)) in sorted(map).iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    push_indent(out, level + 1);
                    write_escaped(k, out);
                    out.push_str(": ");
                    write_indented(x, out, level + 1);
                }
                out.push('\n');
                push_indent(out, level);
                out.push('}');
            }
            Value::Object(_) => out.push_str("{}"),
            scalar => write_compact(scalar, out),
        }
    }

    fn sorted(map: &serde_json::Map<String, Value>) -> Vec<(&String, &Value)> {
        let mut keys: Vec<(&String, &Value)> = map.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        keys
    }

    fn push_indent(out: &mut String, level: usize) {
        for _ in 0..level {
            out.push(' ');
        }
    }

    fn write_number(n: &serde_json::Number, out: &mut String) {
        if let Some(i) = n.as_i64() {
            out.push_str(&i.to_string());
        } else if let Some(u) = n.as_u64() {
            out.push_str(&u.to_string());
        } else if let Some(f) = n.as_f64() {
            write_f64(f, out);
        }
    }

    /// python's `repr` for the floats this body stores: shortest
    /// round-trip digits, always a fraction mark (`75.0`), the two
    /// non-finite spellings `json.dumps` allows. Rust's `Display`
    /// never uses exponent form; python's repr switches only at
    /// magnitudes far outside any measured value — a divergence that
    /// cannot reach the tables this ledger writes.
    fn write_f64(f: f64, out: &mut String) {
        if f.is_nan() {
            out.push_str("NaN");
            return;
        }
        if f.is_infinite() {
            out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
            return;
        }
        let mut s = format!("{f}");
        if !s.contains('.') && !s.contains('e') && !s.contains('E') {
            s.push_str(".0");
        }
        out.push_str(&s);
    }

    /// python's `ensure_ascii` escaping: the short forms for quote,
    /// backslash and whitespace, `\uXXXX` for the rest of the
    /// non-printable and non-ASCII range, surrogate pairs for the
    /// astral plane.
    fn write_escaped(s: &str, out: &mut String) {
        out.push('"');
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                c if (c as u32) > 0xFFFF => {
                    let cp = c as u32 - 0x10000;
                    let hi = 0xD800 + (cp >> 10);
                    let lo = 0xDC00 + (cp & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
}

#[cfg(test)]
mod tests {
    use super::classify;
    use super::{
        is_retryable, retry_ladder, Connect, DbError, DsnConnect, TrialLedger, TrialRef,
        MAX_RETRIES,
    };
    use serde_json::{json, Value};

    /// The pinned mapping — causal's from_trials, written once.
    /// Every row mirrors the python golden cases byte-for-byte.
    #[test]
    fn phase_trials() {
        for (phase, expected) in [
            ("push", "push"),
            // the rename that starved a reader
            ("probe_push", "push"),
            ("pause", "pause"),
            ("probe_pause", "pause"),
            ("hold_calib", "hold_calib"),
        ] {
            assert_eq!(
                classify(Some("impulse"), Some(phase), "probe_push", false, false),
                expected,
                "phase {phase}"
            );
        }
    }

    #[test]
    fn receiver_hold() {
        assert_eq!(
            classify(Some("hold"), Some("hold"), "idle", false, false),
            "hold"
        );
    }

    #[test]
    fn own_hold_calib_is_not_receiver_hold() {
        assert_eq!(
            classify(Some("hold"), Some("hold_calib"), "hold_calib", false, false),
            "hold_calib"
        );
    }

    #[test]
    fn mode_passthrough() {
        assert_eq!(classify(Some("park"), None, "idle", false, false), "park");
        assert_eq!(
            classify(Some("idle"), Some(""), "idle", false, false),
            "idle"
        );
    }

    #[test]
    fn unknown_falls_back() {
        assert_eq!(classify(Some(""), None, "", false, false), "unclassified");
        assert_eq!(classify(None, None, "", false, false), "unclassified");
    }

    #[test]
    fn dart_is_its_own_kind() {
        assert_eq!(
            classify(
                Some("impulse"),
                Some("probe_push"),
                "probe_push",
                true,
                false,
            ),
            "seek_prior"
        );
    }

    #[test]
    fn ambience_is_its_own_kind() {
        assert_eq!(
            classify(
                Some("impulse"),
                Some("probe_push"),
                "probe_push",
                false,
                true,
            ),
            "ambience"
        );
        // dart wins over ambience, per the python order
        assert_eq!(
            classify(Some("impulse"), None, "idle", true, true),
            "seek_prior"
        );
    }

    // ─── the retry ladder ────────────────────────────────────────

    #[test]
    fn retryable_patterns() {
        for text in [
            "db error: could not serialize access due to concurrent update | ... SerializationFailure ... 40001",
            "transaction expired or aborted by client",
            "OperationalError: ...",
            "server closed the connection unexpectedly",
        ] {
            assert!(is_retryable(text), "should retry: {text}");
        }
        assert!(!is_retryable("relation \"trials\" does not exist"));
        assert!(!is_retryable(
            "duplicate key value violates unique constraint"
        ));
    }

    #[test]
    fn ladder_retries_then_succeeds() {
        let mut attempts = 0u32;
        let mut sleeps: Vec<f64> = Vec::new();
        let result: Result<i64, DbError> = retry_ladder(&mut |secs| sleeps.push(secs), || {
            attempts += 1;
            if attempts < 3 {
                Err(DbError::new(
                    "OperationalError: server closed the connection",
                ))
            } else {
                Ok(7)
            }
        });
        assert_eq!(result.unwrap(), 7);
        assert_eq!(attempts, 3);
        assert_eq!(sleeps, vec![0.5, 1.0]);
    }

    #[test]
    fn ladder_non_retryable_stands_first() {
        let mut attempts = 0u32;
        let err: DbError = retry_ladder(&mut |_| {}, || {
            attempts += 1;
            Err::<(), _>(DbError::new("relation \"trials\" does not exist"))
        })
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(err.to_string(), "relation \"trials\" does not exist");
    }

    #[test]
    fn ladder_exhausts_after_four_attempts() {
        assert_eq!(MAX_RETRIES, 4);
        let mut attempts = 0u32;
        let mut sleeps: Vec<f64> = Vec::new();
        let err: DbError = retry_ladder(&mut |secs| sleeps.push(secs), || {
            attempts += 1;
            Err::<(), _>(DbError::new("SerializationFailure: SQLSTATE: 40001"))
        })
        .unwrap_err();
        assert_eq!(attempts, 4);
        // the python original sleeps after every failed attempt, the
        // fourth included, before raising the last error
        assert_eq!(sleeps, vec![0.5, 1.0, 1.5, 2.0]);
        assert_eq!(err.to_string(), "SerializationFailure: SQLSTATE: 40001");
    }

    // ─── the python-shaped JSON writer ───────────────────────────

    #[test]
    fn py_dumps_matches_python_shape() {
        let v: Value =
            serde_json::from_str("{\"b\": 1, \"a\": [0.51, 0.49], \"s\": \"x\"}").unwrap();
        assert_eq!(
            super::pyjson::py_dumps(&v),
            "{\"a\": [0.51, 0.49], \"b\": 1, \"s\": \"x\"}"
        );
        let f: Value = serde_json::from_str("{\"probe_level\": 75.0}").unwrap();
        assert_eq!(super::pyjson::py_dumps(&f), "{\"probe_level\": 75.0}");
        let e: Value = serde_json::from_str("{\"k\": \"a\\\"b\\\\c\\nd\"}").unwrap();
        assert_eq!(super::pyjson::py_dumps(&e), "{\"k\": \"a\\\"b\\\\c\\nd\"}");
        let u: Value = serde_json::from_str("{\"u\": \"\\u00e9\"}").unwrap();
        assert_eq!(super::pyjson::py_dumps(&u), "{\"u\": \"\\u00e9\"}");
    }

    #[test]
    fn py_dumps_indent_matches_python_shape() {
        let v = json!({"b": [], "a": {}});
        assert_eq!(
            super::pyjson::py_dumps_indent(&v),
            "{\n \"a\": {},\n \"b\": []\n}"
        );
    }

    #[test]
    fn to_attrs_json_reader_shape() {
        assert_eq!(super::to_attrs_json(&Value::String("raw".into())), "raw");
        let v: Value = serde_json::from_str("{\"z\": 1, \"a\": 2.5}").unwrap();
        assert_eq!(super::to_attrs_json(&v), "{\"a\": 2.5, \"z\": 1}");
    }

    // ─── the golden fixture (cross-language contract) ────────────
    //
    // Mirrors the python side's test_golden_roundtrip exactly: the
    // same representative run through the real verbs, dumped in
    // canonical form, compared byte-for-byte against the committed
    // artifact — the SAME file the python suite and the godon-images
    // readers test against. Requires a real postgres
    // (LEDGER_TEST_DSN; CI provides one).

    fn canonical_dump(dsn: &str) -> Value {
        let mut conn = DsnConnect::new(dsn).unwrap().connect().unwrap();
        let rows = conn
            .query(
                "SELECT number, kind, params, value, state, error, attrs FROM trials ORDER BY number",
                &[],
            )
            .unwrap();
        let mut trials = Vec::new();
        for r in &rows {
            let params: Value = r.get(2);
            let attrs: Value = r.get(6);
            trials.push(json!({
                "number": r.get::<_, i64>(0),
                "kind": r.get::<_, String>(1),
                // the python _canonical_rows kept jsonb string scalars
                // raw and re-serialized everything else, sorted - the
                // same rule as to_attrs_json
                "params": super::to_attrs_json(&params),
                "value": r.get::<_, Option<f64>>(3),
                "state": r.get::<_, String>(4),
                "error": r.get::<_, Option<String>>(5),
                "attrs": super::to_attrs_json(&attrs),
            }));
        }
        let state_rows = conn
            .query("SELECT key, value_json FROM ledger_state ORDER BY key", &[])
            .unwrap();
        let mut state = Vec::new();
        for r in &state_rows {
            let value_json: Value = r.get(1);
            state.push(json!({
                "key": r.get::<_, String>(0),
                "value_json": super::to_attrs_json(&value_json),
            }));
        }
        json!({"trials": trials, "ledger_state": state})
    }

    #[test]
    fn golden_roundtrip() {
        let Ok(dsn) = std::env::var("LEDGER_TEST_DSN") else {
            eprintln!("LEDGER_TEST_DSN not set — golden fixture test skipped");
            return;
        };
        let ledger = TrialLedger::new(&dsn).unwrap();
        ledger.ensure_schema().unwrap();
        {
            let mut setup = DsnConnect::new(&dsn).unwrap().connect().unwrap();
            setup
                .batch_execute("TRUNCATE trials RESTART IDENTITY;\nTRUNCATE ledger_state")
                .unwrap();
        }

        // A representative trial: the stamps the worker writes,
        // pre-serialized JSON exactly as the call sites do it.
        let n = ledger.begin().unwrap();
        ledger
            .set_kind(
                n,
                &classify(
                    Some("impulse"),
                    Some("probe_push"),
                    "probe_push",
                    false,
                    false,
                ),
            )
            .unwrap();
        let refer = TrialRef::new(ledger.clone(), n);
        refer
            .set_user_attr("detection_mode", json!("impulse"))
            .unwrap();
        refer
            .set_user_attr("coord_state", json!("probe_push"))
            .unwrap();
        refer
            .set_user_attr("impulse_phase", json!("probe_push"))
            .unwrap();
        refer.set_user_attr("impulse_scale", json!(1.5)).unwrap();
        refer
            .set_user_attr("probe_param", json!("param_0"))
            .unwrap();
        refer.set_user_attr("probe_level", json!(75.0)).unwrap();
        refer
            .set_user_attr("observations", json!("[0.51, 0.49]"))
            .unwrap();
        refer
            .set_user_attr("effectuation_params", json!("{\"param_0\": 75.0}"))
            .unwrap();
        ledger.complete(n, 0.512).unwrap();

        // A failed one: park without neutral params.
        let n2 = ledger.begin().unwrap();
        ledger
            .set_kind(
                n2,
                &classify(Some("hold"), Some("hold"), "idle", false, false),
            )
            .unwrap();
        ledger
            .fail(n2, Some("park without neutral params"))
            .unwrap();

        // State machine on the same connection pattern.
        ledger
            .state_put(
                "rollback_state_target_0",
                "{\"state\": \"normal\", \"version\": 1}",
            )
            .unwrap();
        let stored = ledger
            .state_get("rollback_state_target_0")
            .unwrap()
            .unwrap();
        let parsed: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(parsed["version"], json!(1));

        let dump = canonical_dump(&dsn);
        let produced = super::pyjson::py_dumps_indent(&dump);
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/unit/fixtures/ledger_golden.json");
        if std::env::var("UPDATE_FIXTURE").is_ok() {
            std::fs::write(&fixture_path, produced.clone() + "\n").unwrap();
            return;
        }
        let expected = include_str!("../../../tests/unit/fixtures/ledger_golden.json");
        assert_eq!(
            produced.trim(),
            expected.trim(),
            "golden fixture drift — regenerate with UPDATE_FIXTURE=1 (python side or here) \
             and copy the artifact to the rust readers' tests"
        );

        // The read verbs on the same run: shapes the worker replays
        // at boot and metrics read — after the fixture compare, so
        // the contract bytes above stay the first assertion.
        let rows = ledger.trials().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "push");
        assert_eq!(rows[0].state, "COMPLETE");
        assert_eq!(rows[0].values, vec![0.512]);
        assert!(rows[0].attrs.is_object());
        assert_eq!(rows[1].values, Vec::<f64>::new());
        assert_eq!(
            rows[1].error.as_deref(),
            Some("park without neutral params")
        );
        let best = ledger.best_trial(false).unwrap().unwrap();
        assert_eq!(best.number, 1);
        assert_eq!(best.value, Some(0.512));
        assert_eq!(ledger.count().unwrap(), 2);
        assert_eq!(ledger.wish_latest().unwrap(), None);
        ledger.wish_remove("none").unwrap();
    }
}
