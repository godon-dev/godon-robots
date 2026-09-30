"""The boot-park ordering: a tender applies its neutral park BEFORE it
announces itself to the room. The bench boots at param_lower - the
dials' extreme - and a registration that outran the first park let the
room's first measurement window bank that extreme as whoever probed
first (found live Sep 30: a phantom -0.34 dip attributed to node-2's
first window while node-3's dial slept at boot-zero)."""

import sys
import os
from unittest.mock import MagicMock, patch, call

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

sys.modules.setdefault('wmill', MagicMock())
sys.modules.setdefault('optuna', MagicMock())
sys.modules.setdefault('optuna.storages', MagicMock())
sys.modules.setdefault('optuna.trial', MagicMock())
sys.modules.setdefault('optuna.samplers', MagicMock())

# Mock the Windmill package namespace so the worker's internal imports resolve
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
sys.modules['f.systemtender.engine.communication'] = MagicMock()
sys.modules['f.systemtender.engine.communication'].CommunicationCallback = MagicMock
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


def _worker_with_ordered_init():
    """Instantiate the worker with the DB and effectuation collaborators
    mocked, capturing the init-time call order of the boot park and the
    room announcement."""
    order = []
    with patch.object(SystemtenderWorker, '_load_or_create_study', return_value=MagicMock()), \
         patch.object(SystemtenderWorker, '_setup_communication', return_value=MagicMock()), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_compute_neutral_params',
                      return_value={'param_0': 50.0}), \
         patch.object(SystemtenderWorker, '_execute_trial',
                      side_effect=lambda params: order.append(('park', dict(params)))), \
         patch.object(SystemtenderWorker, '_register_interference_systemtender',
                      side_effect=lambda: order.append(('register', None))):
        worker = SystemtenderWorker(_config())
    return worker, order


def test_boot_park_precedes_registration():
    """Dials at neutral FIRST, then 'I'm ready' - the registration must
    never outrun the first park."""
    worker, order = _worker_with_ordered_init()
    parks = [i for i, (what, _) in enumerate(order) if what == 'park']
    regs = [i for i, (what, _) in enumerate(order) if what == 'register']
    assert parks and regs, f"expected a boot park and a registration, got {order}"
    assert parks[0] < regs[0], (
        f"the boot park must precede the registration, got {order}")
    print("  PASS")


def test_boot_park_applies_neutral_dials():
    """The boot park applies the configured neutral (hold_params), not
    the bench's boot defaults."""
    worker, order = _worker_with_ordered_init()
    parks = [params for what, params in order if what == 'park']
    assert parks and parks[0] == {'param_0': 50.0}, (
        f"the boot park must apply the neutral params, got {order}")
    print("  PASS")


def test_pure_optimizer_skips_boot_park():
    """A tender without an interference section owns its boot state: the
    boot park is a no-op - no dials moved on the room's behalf."""
    config = _config()
    del config['interference_detection']
    with patch.object(SystemtenderWorker, '_load_or_create_study', return_value=MagicMock()), \
         patch.object(SystemtenderWorker, '_setup_communication', return_value=MagicMock()), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch.object(SystemtenderWorker, '_execute_trial') as trial:
        worker = SystemtenderWorker(config)
        worker._park_before_announcement()  # explicit call: no-op, no raise
        trial.assert_not_called()
    print("  PASS")


if __name__ == "__main__":
    test_boot_park_precedes_registration()
    test_boot_park_applies_neutral_dials()
    test_pure_optimizer_skips_boot_park()
