"""The release rider: a vanished assignment mid-seek still comes home.

The controller's close removes the wish's assignment row; the tender's
pulse sees it gone. Without the rider, the orphaned driver wrote its
return at loop end under 'unknown' — the wish's id lost. With it, the
driver aborts between trials and the return lands under the wish's
OWN id, immediately.
"""

import sys
import os
import json
from unittest.mock import MagicMock, patch

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

SEEK_PAGE = {
    'species': 'seek',
    'instruction': 'seek',
    'seek': {
        'species': 'find_best',
        'region': {'param_0': [0.0, 100.0, False]},
        'criterion': {'outcome': 'room/objective_1', 'direction': 'maximize'},
        'budget': 50,
        'candidates': 1,
    },
}


def _worker():
    with patch.object(SystemtenderWorker, '_init_ledger',
                      lambda self: setattr(self, 'ledger', MagicMock())), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_compute_neutral_params',
                      return_value={'param_0': 50.0}), \
         patch.object(SystemtenderWorker, '_execute_trial',
                      return_value={'objective_1': 0.5}), \
         patch.object(SystemtenderWorker, '_register_interference_systemtender'):
        worker = SystemtenderWorker({
            'systemtender': {'name': 't', 'uuid': 'u'},
            'creation_ts': '2025-01-15T10:30:00Z',
            'objectives': [{'name': 'objective_1', 'direction': 'maximize'}],
            'interference_detection': {
                'group': 'g', 'hold_params': {'param_0': 50.0},
                'push_block_size': 3, 'pause_block_size': 3,
            },
            'effectuation': {'type': 'ssh', 'targets': [{'target_id': 't1'}]},
        })
    patch.object(worker, '_release_wish_receiver_row', lambda *a, **k: None).start()
    return worker


def test_unassigned_mid_seek_writes_return_under_its_own_id():
    worker = _worker()
    worker.ledger.wish_latest.return_value = ('sw-live', 'sender')
    with patch.object(worker, '_fetch_wish_plan', return_value=SEEK_PAGE):
        worker._check_wish_assignment()
    assert worker._seek_driver is not None

    # One dart measured.
    dart = worker._seek_decision()
    assert dart is not None and dart.get('seek_dart') is True
    worker._seek_driver.observe(0.7, 0.02)

    # The controller closes the wish: the assignment row vanishes.
    worker.ledger.wish_latest.return_value = None
    worker._check_wish_assignment()

    keys = [c.args[0] for c in worker.ledger.state_put.call_args_list]
    assert 'seek_return::sw-live' in keys, \
        f"the return must land under the wish's own id, got {keys}"
    payload = json.loads(
        [c.args[1] for c in worker.ledger.state_put.call_args_list
         if c.args[0] == 'seek_return::sw-live'][0])
    assert payload['verdict'] == 'RETURNED'
    assert payload['stop_reason'] == 'stopped'
    assert payload['finding']['reading'] == 0.7
    assert worker._seek_driver is None
    assert worker._wish is None
