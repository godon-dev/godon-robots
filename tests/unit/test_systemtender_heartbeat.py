"""The tender's heartbeat: every pulse touches its state row.

Liveness is data-plane truth: the controller reads the state row's age
before it delivers a wish here, so the pulse must keep the row warm —
a wedged loop stops touching it and reads as dead, whatever the
platform's job status claims.
"""

from unittest.mock import MagicMock

from engine.systemtender_worker import SystemtenderWorker


def _bare_worker():
    """A worker skeleton: no loop, no config — only the steer fields."""
    w = object.__new__(SystemtenderWorker)
    w._wish = None
    w._probe_coordinator = None
    return w


def _fake_engine():
    """Context-managed engine whose UPDATE we can assert on."""
    conn = MagicMock()
    engine = MagicMock()
    engine.connect.return_value.__enter__.return_value = conn
    return engine, conn


def test_shutdown_check_touches_the_heartbeat():
    print("\n=== test_shutdown_check_touches_the_heartbeat ===")
    w = _bare_worker()
    w.systemtender_uuid = "t-1"
    engine, conn = _fake_engine()
    w.study = MagicMock()
    w.study._storage._backend.engine = engine
    w.study._storage.engine = engine

    row = MagicMock()
    row.__getitem__.return_value = False  # no shutdown requested
    result = MagicMock()
    result.fetchone.return_value = row
    conn.execute.side_effect = [None, result]

    alive = w._check_shutdown_requested()

    assert alive is False, "a plain heartbeat beat is not a shutdown"
    statements = [str(c[0][0]) for c in conn.execute.call_args_list]
    assert any("UPDATE systemtender_state" in s and "updated_at" in s
               for s in statements), "the pulse must touch the state row"
    assert any("SELECT shutdown_requested" in s for s in statements), \
        "the shutdown flag read must survive unchanged"

    print("  one visit, two acts: heartbeat touch + shutdown read")
    print("  PASS")


def test_heartbeat_failure_stays_quiet():
    print("\n=== test_heartbeat_failure_stays_quiet ===")
    # a failing state row must not crash the pulse — the worker answers
    # 'not shutting down' and lives to beat again (pre-existing contract)
    w = _bare_worker()
    w.systemtender_uuid = "t-2"
    engine, conn = _fake_engine()
    w.study = MagicMock()
    w.study._storage._backend.engine = engine
    w.study._storage.engine = engine
    conn.execute.side_effect = RuntimeError("db gone")

    alive = w._check_shutdown_requested()

    assert alive is False, "a failed check is never a shutdown"

    print("  heartbeat failure degrades to 'keep running'")
    print("  PASS")
