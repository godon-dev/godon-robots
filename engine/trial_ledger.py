"""The tender's trial ledger — the estate's own tables.

Two tables in the tender's own database, one direct connection per
operation (the same short-lived psycopg2 pattern the room machinery
uses). No study, no storage library: the verbs below are the whole
write and read surface for the worker. The rust readers in
godon-images read the same tables through their own module; the
contract between the two sides is the golden fixture (see
tests/unit/test_trial_ledger.py), not shared code.

Encoding rules (canonical, both sides obey):
- attrs values are stored as given: strings as JSON strings, floats as
  JSON numbers, pre-serialized JSON (observations, guardrails,
  effectuation_params) as strings containing JSON — byte-identical to
  what the readers decoded from the study era.
- trial states are the uppercase COMPLETE / FAIL / RUNNING — the exact
  strings the observer's API served from the study era.
- kind is the trial's classification, written once by the worker via
  classify() below; readers bucket by kind, never by string-matching
  attr literals (a stamp rename once starved a reader silently for two
  days — kind is the cure for that class).
- params column is reserved: written empty until the read surface
  deliberately widens; the applied settings ride attrs under
  effectuation_params, exactly as before.
"""

import json
import time

try:
    import psycopg2
    import psycopg2.extras
except ImportError:  # test envs stub psycopg2 as a plain module
    psycopg2 = None

_RETRYABLE_DB_ERROR_PATTERNS = (
    'SerializationFailure',
    '40001',
    'expired or aborted',
    'OperationalError',
    'server closed the connection',
)

_SCHEMA_VERSION = 1

_MIGRATIONS = {
    1: [
        """
        CREATE TABLE IF NOT EXISTS trials (
            number      BIGSERIAL PRIMARY KEY,
            kind        TEXT NOT NULL DEFAULT 'unclassified',
            params      JSONB NOT NULL DEFAULT '{}',
            value       DOUBLE PRECISION,
            state       TEXT NOT NULL DEFAULT 'RUNNING',
            error       TEXT,
            attrs       JSONB NOT NULL DEFAULT '{}',
            created_ts  TIMESTAMPTZ NOT NULL DEFAULT now(),
            finished_ts TIMESTAMPTZ
        )
        """,
        """
        CREATE TABLE IF NOT EXISTS ledger_state (
            key        TEXT PRIMARY KEY,
            value_json JSONB NOT NULL,
            updated_ts TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        """,
    ],
}


def classify(mode, impulse_phase, coord_state):
    """The one classification — pinned against the rust reader's
    from_trials. push/pause/hold_calib come from the impulse phase
    (probe_-prefix normalized away: a rename must never re-starve a
    reader), receiver hold from detection mode + coordinator state."""
    phase = impulse_phase or ''
    if phase.startswith('probe_'):
        phase = phase[len('probe_'):]
    if phase in ('push', 'pause', 'hold_calib'):
        return phase
    if mode == 'hold' and coord_state != 'hold_calib':
        return 'hold'
    return mode or 'unclassified'


class TrialLedger:
    """Per-verb short connections; a transient takes a retry, never a
    lost trial."""

    def __init__(self, dsn):
        self.dsn = dsn

    # ─── connection ────────────────────────────────────────────────

    def _with_conn(self, fn, description, max_retries=4):
        last = None
        for attempt in range(max_retries):
            conn = None
            try:
                conn = psycopg2.connect(self.dsn)
                with conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
                    result = fn(cur)
                conn.commit()
                return result
            except Exception as exc:
                last = exc
                text = str(exc)
                if not any(p in text for p in _RETRYABLE_DB_ERROR_PATTERNS):
                    raise
                time.sleep(0.5 * (attempt + 1))
            finally:
                if conn is not None:
                    try:
                        conn.close()
                    except Exception:
                        pass
        if last is None:
            raise RuntimeError(f"{description}: failed without an exception")
        raise last

    def ensure_schema(self):
        def apply(cur):
            cur.execute(
                "CREATE TABLE IF NOT EXISTS schema_version (version INT NOT NULL)")
            cur.execute("SELECT version FROM schema_version")
            row = cur.fetchone()
            current = row['version'] if row else 0
            if row is None:
                cur.execute("INSERT INTO schema_version (version) VALUES (0)")
                current = 0
            for version in sorted(_MIGRATIONS):
                if version > current:
                    for stmt in _MIGRATIONS[version]:
                        cur.execute(stmt)
                    cur.execute(
                        "UPDATE schema_version SET version = %s", (version,))
            return current
        return self._with_conn(apply, "ensure_schema")

    # ─── write verbs ───────────────────────────────────────────────

    def begin(self):
        """Append a trial row; returns its number."""
        def op(cur):
            cur.execute(
                "INSERT INTO trials (kind) VALUES ('unclassified') "
                "RETURNING number")
            return cur.fetchone()['number']
        return self._with_conn(op, "begin")

    def set_kind(self, number, kind):
        def op(cur):
            cur.execute(
                "UPDATE trials SET kind = %s WHERE number = %s",
                (kind, number))
        return self._with_conn(op, f"set_kind {number}")

    def stamp(self, number, key, value):
        return self.stamps(number, {key: value})

    def stamps(self, number, values):
        """Merge keys into attrs. Values as given (see module docstring):
        strings land as JSON strings, floats as JSON numbers,
        pre-serialized JSON strings stay strings."""
        payload = json.dumps(values, sort_keys=True)
        def op(cur):
            cur.execute(
                "UPDATE trials SET attrs = attrs || %s::jsonb "
                "WHERE number = %s", (payload, number))
        return self._with_conn(op, f"stamps {number}")

    def complete(self, number, value):
        def op(cur):
            cur.execute(
                "UPDATE trials SET state = 'COMPLETE', value = %s, "
                "finished_ts = now() WHERE number = %s", (value, number))
        return self._with_conn(op, f"complete {number}")

    def fail(self, number, error=None):
        def op(cur):
            cur.execute(
                "UPDATE trials SET state = 'FAIL', error = %s, "
                "finished_ts = now() WHERE number = %s", (error, number))
        return self._with_conn(op, f"fail {number}")

    # ─── state table (rollback machine and friends) ────────────────

    def state_put(self, key, value):
        """value is a string (call sites serialize); stored JSON-encoded
        so readers decode exactly as they decoded study attrs."""
        def op(cur):
            cur.execute(
                "INSERT INTO ledger_state (key, value_json, updated_ts) "
                "VALUES (%s, %s::jsonb, now()) "
                "ON CONFLICT (key) DO UPDATE SET value_json = EXCLUDED.value_json, "
                "updated_ts = now()", (key, json.dumps(value)))
        return self._with_conn(op, f"state_put {key}")

    def state_get(self, key):
        def op(cur):
            cur.execute(
                "SELECT value_json FROM ledger_state WHERE key = %s", (key,))
            row = cur.fetchone()
            if row is None or row['value_json'] is None:
                return None
            value = row['value_json']
            if isinstance(value, str):
                return value
            return json.dumps(value, sort_keys=True)
        return self._with_conn(op, f"state_get {key}")

    # ─── read verbs (boot replays, metrics) ────────────────────────

    def trials(self):
        """All trials, number order. attrs decoded to a dict; values
        mirror the reader contract: one-element list when judged."""
        def op(cur):
            cur.execute(
                "SELECT number, kind, params, value, state, error, attrs, "
                "CAST(created_ts AS TEXT), CAST(finished_ts AS TEXT) "
                "FROM trials ORDER BY number")
            rows = cur.fetchall()
            out = []
            for r in rows:
                rec = dict(r)
                rec['params'] = rec['params'] if isinstance(rec['params'], dict) else {}
                rec['attrs'] = rec['attrs'] if isinstance(rec['attrs'], dict) else {}
                rec['values'] = [rec['value']] if rec['value'] is not None else []
                out.append(rec)
            return out
        return self._with_conn(op, "trials")

    def count(self):
        def op(cur):
            cur.execute("SELECT COUNT(*) AS n FROM trials")
            return cur.fetchone()['n']
        return self._with_conn(op, "count")

    def best_trial(self, minimize=False):
        """The best judged trial — computed, no stored front. Params
        come back exactly as written (empty until the surface widens)."""
        order = 'ASC' if minimize else 'DESC'
        def op(cur):
            cur.execute(
                f"SELECT number, kind, params, value, state, error, attrs, "
                f"CAST(created_ts AS TEXT), CAST(finished_ts AS TEXT) "
                f"FROM trials WHERE state = 'COMPLETE' AND value IS NOT NULL "
                f"ORDER BY value {order}, number ASC LIMIT 1")
            row = cur.fetchone()
            if row is None:
                return None
            rec = dict(row)
            rec['params'] = rec['params'] if isinstance(rec['params'], dict) else {}
            rec['attrs'] = rec['attrs'] if isinstance(rec['attrs'], dict) else {}
            rec['values'] = [rec['value']] if rec['value'] is not None else []
            return rec
        return self._with_conn(op, "best_trial")

    # ─── side reads that rode the old storage engine ───────────────

    def stop_requested(self):
        """The tender's stop flag, same table the door writes."""
        def op(cur):
            cur.execute("SELECT shutdown_requested FROM systemtender_state LIMIT 1")
            row = cur.fetchone()
            return bool(row and row['shutdown_requested'])
        return self._with_conn(op, "stop_requested")

    def wish_latest(self):
        """The wish assignment row, if any: (wish_id, role) or None.
        Creates/migrates the table exactly as the worker did."""
        def op(cur):
            cur.execute(
                "CREATE TABLE IF NOT EXISTS wish_assignments ("
                "wish_id TEXT PRIMARY KEY, "
                "assigned_tsz DOUBLE PRECISION NOT NULL)")
            cur.execute(
                "ALTER TABLE wish_assignments "
                "ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'sender'")
            cur.execute(
                "SELECT wish_id, COALESCE(role, 'sender') AS role "
                "FROM wish_assignments ORDER BY assigned_tsz DESC LIMIT 1")
            row = cur.fetchone()
            if row is None:
                return None
            return (row['wish_id'], row['role'])
        return self._with_conn(op, "wish_latest")

    def wish_remove(self, wish_id):
        def op(cur):
            cur.execute(
                "DELETE FROM wish_assignments WHERE wish_id = %s", (wish_id,))
        return self._with_conn(op, f"wish_remove {wish_id}")


class TrialRef:
    """The worker-facing trial handle: a number and a stamp method.
    Keeps the trial-loop call sites shaped exactly as the study era
    wrote them; the stamps land in the ledger's attrs column."""

    def __init__(self, ledger, number):
        self._ledger = ledger
        self.number = number

    def set_user_attr(self, key, value):
        return self._ledger.stamp(self.number, key, value)


def to_attrs_json(value):
    """Reader-side canonicalization used by tests: whatever JSONB
    returned (already-decoded dict, or string) becomes the same python
    shape the study era produced."""
    if isinstance(value, str):
        return value
    return json.dumps(value, sort_keys=True)
