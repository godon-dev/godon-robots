"""Rung 8 - the death resume at boot (designs/2026-10-11 XX).

A relaunched holder keeps its measured state: dials persist on the
target hosts across worker death, so the boot park asserts the last
standing params instead of dragging a held dial back to neutral.
Receivers park neutral (stillness); bare boots park neutral exactly
as before; every read failure degrades to the bare-boot path.
"""

import sys
import os
import json
from unittest.mock import MagicMock, patch, call

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

sys.modules.setdefault('wmill', MagicMock())

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


NEUTRAL = {'param_0': 50.0}
STANDING = {'param_0': 73.5}


def _config():
    return {
        'systemtender': {'name': 't', 'uuid': 'u'},
        'creation_ts': '2025-01-15T10:30:00Z',
        'objectives': [{'name': 'objective_1', 'direction': 'maximize'}],
        'interference_detection': {
            'group': 'g',
            'hold_params': {'param_0': 50.0},
            'push_block_size': 3,
            'pause_block_size': 3,
        },
        'effectuation': {'type': 'ssh', 'targets': [{'target_id': 't1'}]},
    }


def _boot(assignment=None, standing=NEUTRAL, wish_latest=None, state_get=None):
    """Construct a worker and return (worker, applied_params list).

    assignment: the (wish_id, role) the ledger answers, or None.
    standing: the last_successful_params in the rollback state, or None.
    wish_latest / state_get: optional overrides (e.g. raising callables)
    to exercise the read-failure paths.
    """
    ledger = MagicMock()
    if wish_latest is not None:
        ledger.wish_latest.side_effect = wish_latest
    else:
        ledger.wish_latest.return_value = assignment
    if state_get is not None:
        ledger.state_get.side_effect = state_get
    else:
        rollback = {'state': 'normal', 'last_successful_params': standing}
        ledger.state_get.return_value = json.dumps(rollback)

    applied = []

    def record_trial(params):
        applied.append(params)
        return {'objective_1': 0.5}

    with patch.object(SystemtenderWorker, '_init_ledger',
                      lambda self: setattr(self, 'ledger', ledger)), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_compute_neutral_params',
                      return_value=dict(NEUTRAL)), \
         patch.object(SystemtenderWorker, '_execute_trial',
                      side_effect=record_trial), \
         patch.object(SystemtenderWorker, '_register_interference_systemtender'):
        worker = SystemtenderWorker(_config())
    return worker, applied


def test_resume_holder_parks_at_standing():
    worker, applied = _boot(assignment=('sw-1', 'sender'), standing=dict(STANDING))
    assert applied == [STANDING], "a relaunched holder parks at its last standing params"


def test_receiver_parks_neutral():
    worker, applied = _boot(assignment=('sw-1', 'receiver'), standing=dict(STANDING))
    assert applied == [NEUTRAL], "a receiver's contribution is stillness - neutral"


def test_holder_without_standing_parks_neutral():
    worker, applied = _boot(assignment=('sw-1', 'sender'), standing=None)
    assert applied == [NEUTRAL], "a held wish with no measured state starts from neutral"


def test_bare_boot_parks_neutral():
    worker, applied = _boot(assignment=None)
    assert applied == [NEUTRAL], "a bare boot parks neutral exactly as before rung 8"


def test_assignment_read_failure_degrades_to_bare_boot():
    worker, applied = _boot(wish_latest=RuntimeError("db down"))
    assert applied == [NEUTRAL], "an unreadable assignment degrades to the bare-boot park"


def test_standing_read_failure_degrades_to_neutral():
    worker, applied = _boot(assignment=('sw-1', 'sender'),
                            state_get=RuntimeError("db down"))
    assert applied == [NEUTRAL], "an unreadable standing state degrades to neutral"
