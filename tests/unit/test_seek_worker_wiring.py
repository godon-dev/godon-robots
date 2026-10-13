"""The seek wish end-to-end at the worker level — adopt, walk, return.

The full lifecycle through the real methods: a seek plan page adopts
at the door, the driver's steps become decisions (a dart, then pass
probes over the stepped baseline), completion writes the RETURN into
the tender's own ledger and releases the wish. The measurement wiring
observe() calls stand in for the loop's success branch — the run-loop
arms themselves are proven by the suite staying green.
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


SEEK_PAGE = {
    'species': 'seek',
    'instruction': 'seek',
    'seek': {
        'species': 'find_best',
        'region': {
            'param_0': (0.0, 100.0, False),
        },
        'criterion': {'outcome': 'room/objective_1', 'direction': 'maximize'},
        'budget': 50,
        'candidates': 2,
    },
}


def _seek_worker():
    with patch.object(SystemtenderWorker, '_init_ledger',
                      lambda self: setattr(self, 'ledger', MagicMock())), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_compute_neutral_params',
                      return_value={'param_0': 50.0}), \
         patch.object(SystemtenderWorker, '_execute_trial',
                      return_value={'objective_1': 0.5}), \
         patch.object(SystemtenderWorker, '_register_interference_systemtender'):
        worker = SystemtenderWorker(_config())
    releases = []
    patch.object(worker, '_release_wish',
                 side_effect=lambda **kw: releases.append(kw)).start()
    return worker, releases


def test_seek_lifecycle_adopt_walk_return():
    worker, releases = _seek_worker()
    with patch.object(worker, '_fetch_wish_plan', return_value=SEEK_PAGE):
        worker._adopt_wish('sw-9', 'sender')

    assert worker._wish['instruction'] == 'seek'
    assert worker._seek_driver is not None

    # The coordinator as a mock from the start: darts never touch it,
    # so the only override call will be the first pass commission.
    worker._probe_coordinator = MagicMock()

    # Darts: the commissions open and the worker executes them
    # directly — joint pushes, never protocol.
    darts = 0
    while worker._seek_open is None or worker._seek_open.get('dart'):
        if worker._seek_open is None:
            worker._serve_seek_step()
            if worker._seek_open is None:
                break
        assert 'param_0' in worker._seek_open['params']
        worker._seek_driver.wallet_pay()
        worker._seek_driver.observe(0.4 + 0.5 * darts, 0.02)
        worker._seek_open = None
        darts += 1
        worker._serve_seek_step() if worker._seek_open is None else None
        if worker._seek_driver is None:
            break
    assert darts == 2

    # Pass: the COORDINATOR's probe override is commissioned — the
    # probe rides the standing protocol and banks as map ground.
    assert worker._seek_open is not None and not worker._seek_open.get('dart')
    ov = worker._probe_coordinator.set_probe_override.call_args
    assert ov.args[0] == 'param_0' and ov.args[1] == [50.0]  # midpoint first

    # Drive the commissions to completion (pay + observe each).
    passes = 0
    while worker._seek_driver is not None:
        if worker._seek_open is None:
            worker._serve_seek_step()
        open_step = worker._seek_open
        if open_step is None or worker._seek_driver is None:
            break
        worker._seek_driver.wallet_pay()
        worker._seek_driver.observe(0.5 + 0.001 * passes, 0.02)
        worker._seek_open = None
        passes += 1

    assert passes > 0
    # The return: written to the tender's own ledger, wish released.
    keys = [c.args[0] for c in worker.ledger.state_put.call_args_list]
    assert 'seek_return::sw-9' in keys
    payload = json.loads(
        [c.args[1] for c in worker.ledger.state_put.call_args_list
         if c.args[0] == 'seek_return::sw-9'][0])
    assert payload['verdict'] == 'RETURNED'
    assert payload['spend_count'] == darts + passes
    assert payload['finding'] is not None
    assert releases, 'the wish must be released after the return'


def test_refused_seek_never_binds():
    worker, releases = _seek_worker()
    impossible = json.loads(json.dumps(SEEK_PAGE))
    impossible['seek']['candidates'] = 60  # > budget 50
    with patch.object(worker, '_fetch_wish_plan', return_value=impossible):
        worker._adopt_wish('sw-bad', 'sender')
    assert worker._wish is None
    assert worker._seek_driver is None
    assert not releases


def test_release_mid_seek_comes_home_returned():
    worker, releases = _seek_worker()
    with patch.object(worker, '_fetch_wish_plan', return_value=SEEK_PAGE):
        worker._adopt_wish('sw-mid', 'sender')

    # One dart commissioned and measured.
    worker._serve_seek_step()
    assert worker._seek_open is not None and worker._seek_open.get('dart')
    worker._seek_driver.wallet_pay()
    worker._seek_driver.observe(0.7, 0.02)
    worker._seek_open = None

    # The controller closes the wish: the assignment row vanishes.
    worker.ledger.wish_latest.return_value = None
    worker._check_wish_assignment()

    keys = [c.args[0] for c in worker.ledger.state_put.call_args_list]
    assert 'seek_return::sw-mid' in keys
    payload = json.loads(
        [c.args[1] for c in worker.ledger.state_put.call_args_list
         if c.args[0] == 'seek_return::sw-mid'][0])
    assert payload['verdict'] == 'RETURNED'
    assert payload['stop_reason'] == 'stopped'
    assert payload['finding']['reading'] == 0.7
    assert releases
