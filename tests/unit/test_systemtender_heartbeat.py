"""The tender's liveness beater: a parallel thread, outside the trial
cadence, touching the tender's own state row.

Liveness is data-plane truth on a FIXED short interval: a wedged trial
loop must not slow the beat (false-alive), and a slow-but-working phase
must not read as dead (false-dead). The beater touches only the
tender's own state row over its own connection — no study, no
coordination tables, nothing else.
"""

from unittest.mock import MagicMock, patch

from engine.systemtender_worker import SystemtenderWorker


def _bare_worker():
    """A worker skeleton: no loop, no config — only what the beater needs."""
    w = object.__new__(SystemtenderWorker)
    w.systemtender_uuid = "t-1"
    return w


def test_beat_once_touches_state_row_with_declared_interval():
    print("\n=== test_beat_once_touches_state_row_with_declared_interval ===")
    w = _bare_worker()

    cur = MagicMock()
    conn = MagicMock()
    conn.cursor.return_value.__enter__.return_value = cur
    with patch.object(w, '_get_db_url', return_value='postgresql://t-1'), \
            patch('psycopg2.connect', return_value=conn) as connect:
        w._beat_once()

    connect.assert_called_once_with('postgresql://t-1')
    sql = str(cur.execute.call_args[0][0])
    assert 'UPDATE systemtender_state' in sql
    assert 'updated_at = NOW()' in sql
    assert 'beat_interval_secs' in sql
    assert cur.execute.call_args[0][1] == (w.HEARTBEAT_INTERVAL_SECS,)
    conn.commit.assert_called_once()
    conn.close.assert_called_once()

    print("  one beat: own connection, state row touched, interval declared")
    print("  PASS")


def test_heartbeat_loop_survives_beat_failures():
    print("\n=== test_heartbeat_loop_survives_beat_failures ===")
    # a failed beat is logged and retried next interval — the thread
    # must survive db hiccups, or liveness dies with the first blip
    w = _bare_worker()
    beats, sleeps = [], []

    class Done(Exception):
        pass

    def fake_beat():
        beats.append(1)
        raise RuntimeError('db gone')

    def fake_sleep(secs):
        sleeps.append(secs)
        if len(sleeps) >= 3:
            raise Done()

    with patch.object(w, '_beat_once', side_effect=fake_beat), \
            patch.object(w, 'HEARTBEAT_INTERVAL_SECS', 0), \
            patch('engine.systemtender_worker.time.sleep', side_effect=fake_sleep):
        try:
            w._heartbeat_loop()
        except Done:
            pass

    assert len(beats) == 3, "the loop kept beating past failures"
    assert len(sleeps) == 3, "each failed beat still waits its interval"

    print("  three beats, two failures, loop alive throughout")
    print("  PASS")


def test_beater_thread_never_blocks_exit():
    print("\n=== test_beater_thread_never_blocks_exit ===")
    # the beat must never hold the process open — when the trial loop
    # ends (shutdown, quiet bench), the thread dies with it
    w = _bare_worker()
    started = {}

    class FakeThread:
        def __init__(self, target=None, daemon=None, name=None):
            started['daemon'] = daemon
            started['target'] = target

        def start(self):
            started['started'] = True

    with patch('engine.systemtender_worker.threading.Thread', FakeThread):
        w.start_heartbeat()

    assert started.get('daemon') is True, "the beater must be a daemon"
    assert started.get('started') is True

    print("  daemon beater: dies with the process, never blocks exit")
    print("  PASS")


def test_beat_ensures_interval_column_on_old_rows():
    print("\n=== test_beat_ensures_interval_column_on_old_rows ===")
    # rollout-order independence: a state row born before the interval
    # column gets it added by the first beat, then beats normally
    w = _bare_worker()

    cur = MagicMock()
    conn = MagicMock()
    conn.cursor.return_value.__enter__.return_value = cur
    cur.execute.side_effect = [
        Exception('column "beat_interval_secs" does not exist'),
        None,
        None,
    ]
    with patch.object(w, '_get_db_url', return_value='postgresql://t-1'), \
            patch('psycopg2.connect', return_value=conn):
        w._beat_once()

    statements = [str(c[0][0]) for c in cur.execute.call_args_list]
    assert any('ADD COLUMN IF NOT EXISTS beat_interval_secs' in s
               for s in statements), "the first beat must ensure the column"
    assert sum(1 for s in statements if 'UPDATE systemtender_state' in s) == 2
    conn.rollback.assert_called_once()
    conn.commit.assert_called_once()

    print("  old row: column ensured, beat retried, exactly one rollback")
    print("  PASS")
