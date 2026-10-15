"""Rung 7 — the four exit guards (design §X item 7): a held wish pins
the holder past walk-complete; every loop exit reverts the dial to
neutral and drops the assignment row; no trial completes without its
strategy stamp; the status payload carries field-sets, not new status
words."""

import sys
import os
from types import SimpleNamespace
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

import engine.systemtender_worker as worker_mod
from engine.systemtender_worker import SystemtenderWorker


def _config():
    return {
        'systemtender': {'name': 'test_systemtender', 'uuid': 'test_uuid_123'},
        'creation_ts': '2025-01-15T10:30:00Z',
        'run': {'parallel': 1},
        'objectives': [{'name': 'test_obj', 'direction': 'maximize'}],
        'interference_detection': {
            'group': 'g',
            'hold_params': {'param_0': 50.0},
            'push_block_size': 3,
            'pause_block_size': 3,
        },
        'effectuation': {'type': 'ssh', 'targets': [{'target_id': 't1'}]},
    }


def _worker():
    with patch.object(SystemtenderWorker, '_init_ledger', lambda self: setattr(self, 'ledger', MagicMock())), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_compute_neutral_params', return_value={'param_0': 50.0}), \
         patch.object(SystemtenderWorker, '_execute_trial'), \
         patch.object(SystemtenderWorker, '_register_interference_systemtender'):
        return SystemtenderWorker(_config())


def _char_done_coord():
    """A coordinator whose characterization walk is complete."""
    return SimpleNamespace(char_complete=lambda: True)


HELD_WISH = {
    'wish_id': 'w-1',
    'instruction': 'hold',
    'role': 'sender',
    'moves': {'param_0': 55.0},
}


# ─── Guard a: the pin ───────────────────────────────────────────────

def test_pin_holds_past_walk_complete():
    """Characterization complete, wish held: the lever would fire, the
    pin holds — the tender parks instead of leaving. (Shutdown patched
    False: the mocked ledger's stop_requested() is MagicMock-truthy
    and would answer before the pin ever runs.)"""
    worker = _worker()
    worker._probe_coordinator = _char_done_coord()
    worker._wish = dict(HELD_WISH)
    with patch.object(worker, '_check_shutdown_requested', return_value=False):
        assert worker._should_continue() is True


def test_walk_complete_without_wish_exits():
    """No wish held: the quiet-bench lever works exactly as before.
    (Shutdown patched False so the green proves the LEVER fired, not
    an accidental shutdown.)"""
    worker = _worker()
    worker._probe_coordinator = _char_done_coord()
    worker._wish = None
    with patch.object(worker, '_check_shutdown_requested', return_value=False):
        assert worker._should_continue() is False


def test_stop_beats_pin():
    """The controller's stop flag outranks the pin: a noticed exit is
    the one exit a holder takes — the finally then releases the dial."""
    worker = _worker()
    worker._probe_coordinator = _char_done_coord()
    worker._wish = dict(HELD_WISH)
    with patch.object(worker, '_check_shutdown_requested', return_value=True):
        assert worker._should_continue() is False


def test_pin_receipt_once_per_wish():
    """The pin receipt logs once per held wish, not once per loop."""
    worker = _worker()
    worker._probe_coordinator = _char_done_coord()
    worker._wish = dict(HELD_WISH)
    with patch.object(worker, '_check_shutdown_requested', return_value=False):
        worker._should_continue()
        assert worker._pin_receipt == 'w-1'
        worker._should_continue()
        assert worker._pin_receipt == 'w-1'


# ─── Guard b: the release ───────────────────────────────────────────

def test_run_finally_releases_held_wish_on_exit():
    """Any loop exit — stop, fault, or plain completion — releases a
    held wish fully: dial back to neutral, assignment row gone."""
    worker = _worker()
    worker._wish = dict(HELD_WISH)
    with patch.object(worker, '_should_continue', return_value=False), \
         patch.object(worker, '_release_wish') as release, \
         patch.object(worker, '_update_state'), \
         patch.object(worker, '_stamp_finished'):
        worker.run()
    release.assert_called_once_with(delete_assignment=True)


def test_run_finally_releases_even_without_wish():
    """The release is a no-op when nothing is held — the finally never
    invents work."""
    worker = _worker()
    worker._wish = None
    with patch.object(worker, '_should_continue', return_value=False), \
         patch.object(worker, '_release_wish') as release, \
         patch.object(worker, '_update_state'), \
         patch.object(worker, '_stamp_finished'):
        worker.run()
    release.assert_called_once_with(delete_assignment=True)


# ─── Guard c: the stamp ─────────────────────────────────────────────

def _stamp_worker():
    w = SystemtenderWorker.__new__(SystemtenderWorker)
    w.ledger = MagicMock()
    w._retry_op = lambda fn, description, max_retries=4: fn()
    return w


def test_unstamped_trial_fails_not_completes():
    """A trial that reached completion without its strategy stamp is
    unauditable spend: it fails loud, the ledger row tells the truth."""
    w = _stamp_worker()
    trial = SimpleNamespace(number=7)
    w._complete_or_fail(trial, [0.5], None)
    w.ledger.fail.assert_called_once()
    assert w.ledger.fail.call_args[0][0] == 7
    assert 'unstamped' in w.ledger.fail.call_args[0][1]
    w.ledger.complete.assert_not_called()


def test_stamped_trial_completes():
    """The stamped path is unchanged: complete with the first value."""
    w = _stamp_worker()
    trial = SimpleNamespace(number=7)
    w._complete_or_fail(trial, [0.5], 'hold')
    w.ledger.complete.assert_called_once_with(7, 0.5)
    w.ledger.fail.assert_not_called()


# ─── Guard d: the status field-sets ─────────────────────────────────

def test_status_payload_carries_field_sets():
    """The payload grows named facts — strategy set, held wish, seek
    spend — and the status word stays the one it was."""
    w = SystemtenderWorker.__new__(SystemtenderWorker)
    w.systemtender_id = 'st-1'
    w.ledger = MagicMock()
    w.ledger.count.return_value = 5
    w._strategies_run = {'hold', 'push'}
    w._wish = {'wish_id': 'w-1', 'instruction': 'hold', 'role': 'sender',
               'moves': {'param_0': 55.0, 'param_1': 1.0}}
    w._seek_driver = SimpleNamespace(
        wallet_status={'budget': 10, 'spent': 3, 'remaining': 7})
    import wmill
    wmill.set_state.reset_mock()
    w._update_state()
    state = wmill.set_state.call_args[0][0]
    assert state['status'] == 'running'
    assert state['strategies_run'] == ['hold', 'push']
    assert state['wish'] == {'wish_id': 'w-1', 'instruction': 'hold',
                             'role': 'sender', 'dials': ['param_0', 'param_1']}
    assert state['seek_spend'] == {'budget': 10, 'spent': 3, 'remaining': 7}


def test_status_field_sets_none_when_empty():
    """Nothing held, no seek: the field-sets say None — absence is a
    fact, not a missing key."""
    w = SystemtenderWorker.__new__(SystemtenderWorker)
    w.systemtender_id = 'st-2'
    w.ledger = MagicMock()
    w.ledger.count.return_value = 0
    w._strategies_run = set()
    w._wish = None
    w._seek_driver = None
    import wmill
    wmill.set_state.reset_mock()
    w._update_state()
    state = wmill.set_state.call_args[0][0]
    assert state['strategies_run'] == []
    assert state['wish'] is None
    assert state['seek_spend'] is None


def test_wallet_status_read_seam():
    """The driver's wallet exposes a live read-only view: the RETURN's
    numbers, mid-flight."""
    from engine.seek_driver import SeekDriver
    from engine.seek_wish import Wallet
    d = SeekDriver.__new__(SeekDriver)
    d._wallet = Wallet(budget=5)
    d._wallet.pay(2)
    assert d.wallet_status == {'budget': 5, 'spent': 2, 'remaining': 3}
