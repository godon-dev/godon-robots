"""Rung 9 — the instruments door's write side: _update_state stamps
the rung-7 field-sets (strategies_run, wish, seek_spend) into the
tender's own systemtender_state row — the row the controller's
verdict already reads — on a throwaway connection with the
heartbeat's rollout-order guard. A failed stamp never kills the run."""

import sys
import os
import json
from unittest.mock import MagicMock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

sys.modules.setdefault('wmill', MagicMock())
sys.modules.setdefault('optuna', MagicMock())
sys.modules.setdefault('optuna.storages', MagicMock())
sys.modules.setdefault('optuna.trial', MagicMock())

sys.modules['f'] = MagicMock()
sys.modules['f.systemtender'] = MagicMock()
sys.modules['f.systemtender.shared'] = MagicMock()
sys.modules['f.systemtender.shared.otel_logging'] = MagicMock()
sys.modules['f.systemtender.shared.otel_logging'].get_logger = lambda name: MagicMock()
sys.modules['f.systemtender.engine'] = MagicMock()
sys.modules['f.systemtender.engine.probe_coordinator'] = MagicMock()
sys.modules['f.systemtender.engine.probe_coordinator'].ProbeCoordinator = MagicMock
sys.modules['f.systemtender.engine.systemtender_metrics_client'] = MagicMock()
sys.modules['f.systemtender.engine.systemtender_metrics_client'].SystemtenderMetricsClient = MagicMock
sys.modules['f.systemtender.engine.strain_loader'] = MagicMock()
sys.modules['f.systemtender.engine.strain_loader'].load_strain = MagicMock(return_value=MagicMock())

from engine.systemtender_worker import SystemtenderWorker


def _config():
    return {
        'systemtender': {'name': 'test_systemtender', 'uuid': 'test_uuid_123'},
        'creation_ts': '2025-01-15T10:30:00Z',
        'run': {'parallel': 1},
    }


def _worker():
    with patch.object(SystemtenderWorker, '_init_ledger', lambda self: setattr(self, 'ledger', MagicMock())), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_compute_neutral_params', return_value={'param_0': 50.0}), \
         patch.object(SystemtenderWorker, '_execute_trial'), \
         patch.object(SystemtenderWorker, '_register_interference_systemtender'):
        return SystemtenderWorker(_config())


def _cursor_conn():
    cur = MagicMock()
    conn = MagicMock()
    conn.cursor.return_value.__enter__.return_value = cur
    return cur, conn


def test_update_state_stamps_instruments():
    """The state refresh hands its payload to the door's stamp."""
    worker = _worker()
    with patch.object(worker, '_stamp_instruments') as stamp:
        worker._update_state()
    stamp.assert_called_once()
    stamped = stamp.call_args[0][0]
    for key in ('strategies_run', 'wish', 'seek_spend'):
        assert key in stamped


def test_stamp_writes_the_three_keys():
    """One UPDATE, payload carrying exactly the three field-sets."""
    worker = _worker()
    cur, conn = _cursor_conn()
    with patch('psycopg2.connect', return_value=conn):
        worker._stamp_instruments(
            {'strategies_run': ['hold'], 'wish': None, 'seek_spend': None})
    executed = [c[0][0] for c in cur.execute.call_args_list]
    assert any('UPDATE systemtender_state SET instruments' in s for s in executed)
    update_call = [c for c in cur.execute.call_args_list
                   if 'UPDATE systemtender_state SET instruments' in c[0][0]][0]
    payload = json.loads(update_call[0][1][0])
    assert payload == {'strategies_run': ['hold'], 'wish': None, 'seek_spend': None}
    conn.commit.assert_called_once()


def test_stamp_guards_rollout_order():
    """A row born before the column: ensure the column, then stamp —
    message-matched, the heartbeat's own guard pattern."""
    worker = _worker()
    cur, conn = _cursor_conn()
    cur.execute.side_effect = [
        Exception('column "instruments" of relation "systemtender_state" does not exist'),
        None, None]
    with patch('psycopg2.connect', return_value=conn):
        worker._stamp_instruments(
            {'strategies_run': [], 'wish': None, 'seek_spend': None})
    executed = [c[0][0] for c in cur.execute.call_args_list]
    assert any('ALTER TABLE systemtender_state' in s and 'instruments' in s
               for s in executed)
    assert any('UPDATE systemtender_state SET instruments' in s for s in executed)


def test_stamp_failure_never_kills():
    """The stamp is instrumentation, not machinery: a dead DB logs and
    yields — the run continues, the next refresh retries."""
    worker = _worker()
    with patch('psycopg2.connect', side_effect=Exception('db down')):
        worker._stamp_instruments(
            {'strategies_run': [], 'wish': None, 'seek_spend': None})


def test_stamp_swallows_unrelated_sql_fault():
    """A non-column fault (message mismatch) must not take the ALTER
    path — it logs and yields to the next refresh."""
    worker = _worker()
    cur, conn = _cursor_conn()
    cur.execute.side_effect = Exception('connection refused')
    with patch('psycopg2.connect', return_value=conn):
        worker._stamp_instruments(
            {'strategies_run': [], 'wish': None, 'seek_spend': None})
    executed = [c[0][0] for c in cur.execute.call_args_list]
    assert not any('ALTER TABLE' in s for s in executed)
