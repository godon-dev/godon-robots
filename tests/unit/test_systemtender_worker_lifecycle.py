
#
# Copyright (c) 2019 Matthias Tafelmeier.
#
# This file is part of godon
#
# godon is free software: you can redistribute it and/or modify
# it under the terms of the GNU Affero General Public License as
# published by the Free Software Foundation, either version 3 of the
# License, or (at your option) any later version.
#
# godon is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU Affero General Public License for more details.
#
# You should have received a copy of the GNU Affero General Public License
# along with this godon. If not, see <http://www.gnu.org/licenses/>.
#
import pytest
import sys
import os
from unittest.mock import MagicMock, patch, call
import datetime

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '../..'))

sys.modules['wmill'] = MagicMock()

# Mock Windmill package namespace so systemtender_worker's internal imports resolve
sys.modules['f'] = MagicMock()
sys.modules['f.systemtender'] = MagicMock()
sys.modules['f.systemtender.engine'] = MagicMock()
sys.modules['f.systemtender.engine.probe_coordinator'] = MagicMock()
sys.modules['f.systemtender.engine.probe_coordinator'].ProbeCoordinator = MagicMock
sys.modules['f.systemtender.engine.systemtender_metrics_client'] = MagicMock()
sys.modules['f.systemtender.engine.systemtender_metrics_client'].SystemtenderMetricsClient = MagicMock
sys.modules['f.systemtender.engine.strain_loader'] = MagicMock()
sys.modules['f.systemtender.engine.strain_loader'].load_strain = MagicMock(return_value=MagicMock())
sys.modules['f.systemtender.shared'] = MagicMock()
sys.modules['f.systemtender.shared.otel_logging'] = MagicMock()
sys.modules['f.systemtender.shared.otel_logging'].get_logger = MagicMock(return_value=MagicMock())

from engine.systemtender_worker import SystemtenderWorker


def _base_config(**overrides):
    config = {
        'systemtender': {
            'name': 'test_systemtender',
            'uuid': 'test-uuid-123',
            'type': 'linux_performance',
        },
        'creation_ts': '2025-01-15T10:30:00Z',
        'run': {'parallel': 1},
        'objectives': [{'name': 'throughput', 'direction': 'minimize'}],
        'effectuation': {'targets': [], 'type': 'ssh'},
        'reconnaissance': {'type': 'prometheus'},
    }
    config.update(overrides)
    return config


def _mock_ledger(trials=None, best=None):
    ledger = MagicMock()
    ledger.trials.return_value = trials or []
    ledger.count.return_value = len(trials or [])
    ledger.best_trial.return_value = best
    ledger.state_get.return_value = None
    return ledger


def _create_worker(**config_overrides):
    config = _base_config(**config_overrides)
    ledger = _mock_ledger()

    with patch.object(SystemtenderWorker, '_init_ledger',
                      lambda self: setattr(self, 'ledger', ledger)), \
         patch.object(SystemtenderWorker, '_update_state'), \
         patch('engine.systemtender_worker.load_strain', return_value=MagicMock()):
        worker = SystemtenderWorker(config)
    worker._check_shutdown_requested = lambda: False
    return worker


class TestExecuteTrial:
    def setup_method(self):
        sys.modules['wmill'].reset_mock()

    def test_success_returns_metrics(self):
        worker = _create_worker()
        settings = {'vm.swappiness': 10}
        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.side_effect = [
            {'status': 'completed'},
            {'status': 'completed', 'metrics': {'throughput': 42.5}},
        ]

        result = worker._execute_trial(settings)
        assert result == {'throughput': 42.5}
        assert mock_wmill.run_script_by_path.call_count == 2

    def test_effectuation_failure_returns_inf(self):
        worker = _create_worker()
        settings = {'vm.swappiness': 10}
        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.side_effect = Exception('SSH failed')
        result = worker._execute_trial(settings)
        assert result == {'throughput': float('inf')}

    def test_no_metrics_returns_inf(self):
        worker = _create_worker()
        settings = {}

        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.side_effect = [
            {'status': 'completed'},
            {'status': 'completed'},
        ]

        result = worker._execute_trial(settings)
        assert result == {'throughput': float('inf')}

    def test_uses_configured_effectuation_type(self):
        worker = _create_worker(effectuation={'targets': [], 'type': 'http'})
        settings = {}

        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.side_effect = [
            {'status': 'completed'},
            {'status': 'completed', 'metrics': {'throughput': 10.0}},
        ]
        worker._execute_trial(settings)
        eff_call = mock_wmill.run_script_by_path.call_args_list[0]
        assert eff_call[0][0] == 'f/effectuation/http'


class TestShouldContinue:
    """The exit ladder after the budget cutover: the controller's stop
    flag and the walk-complete lever. No iteration floors or caps, no
    time budget, no quality exit — config absence means the role's
    natural exit."""

    def _worker(self, **overrides):
        worker = _create_worker(**overrides)
        worker._probe_coordinator = MagicMock()
        return worker

    def test_continues_with_no_budget_config(self):
        worker = self._worker()
        worker.ledger.trials.return_value = [MagicMock() for _ in range(500)]
        assert worker._should_continue() is True

    def test_stops_on_shutdown_requested(self):
        worker = self._worker()
        worker._check_shutdown_requested = lambda: True
        assert worker._should_continue() is False

    def test_stops_when_walk_complete(self):
        worker = self._worker()
        worker._probe_coordinator.char_complete = MagicMock(return_value=True)
        assert worker._should_continue() is False

    def test_continues_while_walk_open(self):
        worker = self._worker()
        worker._probe_coordinator.char_complete = MagicMock(return_value=False)
        worker.ledger.trials.return_value = [MagicMock() for _ in range(50)]
        assert worker._should_continue() is True

    def test_stub_char_complete_does_not_stop(self):
        # Defensive form: a stub coordinator answering a non-bool must
        # not read as complete.
        worker = self._worker()
        worker._probe_coordinator.char_complete = MagicMock(
            return_value='maybe')
        assert worker._should_continue() is True



class TestWorkerInit:
    def test_missing_creation_ts_raises(self):
        config = _base_config()
        del config['creation_ts']

        with pytest.raises(ValueError, match="creation_ts"):
            with patch.object(SystemtenderWorker, '_init_ledger'), \
                 patch.object(SystemtenderWorker, '_update_state'), \
                 patch('engine.systemtender_worker.load_strain', return_value=MagicMock()):
                SystemtenderWorker(config)

    def test_target_resolution_valid(self):
        worker = _create_worker(effectuation={
            'targets': [{'id': 't0', 'address': '10.0.0.1'}],
            'type': 'ssh',
        }, target_id=0)
        assert worker.target['id'] == 't0'

    def test_target_resolution_invalid_uses_first(self):
        worker = _create_worker(effectuation={
            'targets': [{'id': 't0', 'address': '10.0.0.1'}],
            'type': 'ssh',
        }, target_id=99)
        assert worker.target['id'] == 't0'

    def test_rollback_disabled_by_default(self):
        worker = _create_worker(effectuation={
            'targets': [{'id': 't0', 'address': '10.0.0.1'}],
            'type': 'ssh',
        })
        assert worker.rollback_enabled is False


class TestRunLoop:
    def _run_loop_worker(self):
        worker = _create_worker(run={'parallel': 1})
        trial_counter = [0]
        worker.ledger.begin.side_effect = lambda: trial_counter[0] or trial_counter.__setitem__(0, trial_counter[0] + 1) or (trial_counter[0] - 1)

        real_trials = []

        def fake_complete(number, value):
            real_trials.append(number)

        def fake_fail(number, error=None):
            real_trials.append(number)

        worker.ledger.complete.side_effect = fake_complete
        worker.ledger.fail.side_effect = fake_fail
        worker.ledger.trials.return_value = real_trials
        worker._real_trials = real_trials

        # The walk ends the run: the fake coordinator reports walking
        # mode, and char_complete flips True at the second boundary —
        # or the loop never stops.
        worker._probe_coordinator.decide_trial = MagicMock(
            return_value={'mode': 'impulse',
                          'params': {'vm.swappiness': 10}})
        worker._probe_coordinator.char_complete = MagicMock(
            side_effect=[False, True])
        worker.ledger.best_trial.return_value = {'number': -1, 'params': {},
                                                  'value': None, 'values': []}

        return worker

    def test_run_executes_single_trial(self):
        worker = self._run_loop_worker()

        worker.strain = MagicMock()

        with patch.object(worker, '_execute_trial', return_value={'throughput': 42.5}), \
             patch.object(worker, '_check_guardrails', return_value=(False, [])), \
             patch.object(worker, 'metrics'):
            worker.run()

        assert len(worker.ledger.trials.return_value) == 1

    def test_run_marks_trial_failed_on_guardrail_violation(self):
        worker = self._run_loop_worker()

        worker.strain = MagicMock()

        with patch.object(worker, '_execute_trial', return_value={'cpu_usage': 99.0}), \
             patch.object(worker, '_check_guardrails', return_value=(True, ['cpu_usage violated'])), \
             patch.object(worker, '_handle_guardrail_violation'), \
             patch.object(worker, 'metrics'):
            worker.run()

        assert len(worker._real_trials) == 1

    def test_run_handles_trial_exception(self):
        worker = self._run_loop_worker()

        worker.strain = MagicMock()
        worker.strain = MagicMock()

        with patch.object(worker, 'metrics'):
            worker.run()

        assert len(worker._real_trials) == 1

    def test_run_pushes_metrics_on_completion(self):
        worker = _create_worker(run={'parallel': 1})
        worker._probe_coordinator.char_complete = MagicMock(return_value=True)

        with patch.object(worker, 'metrics') as mock_metrics:
            worker.run()
            mock_metrics.purge_stale_series.assert_called_once()
            mock_metrics.push.assert_called()

class TestRunReconnaissance:
    def setup_method(self):
        sys.modules['wmill'].reset_mock()
        sys.modules['wmill'].run_script_by_path.reset_mock()
        sys.modules['wmill'].run_script_by_path.side_effect = None

    def test_returns_metrics_from_recon(self):
        worker = _create_worker()
        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.return_value = {
            'status': 'completed',
            'metrics': {'throughput': 42.5}
        }

        result = worker._run_reconnaissance()
        assert result == {'throughput': 42.5}
        mock_wmill.run_script_by_path.assert_called_once()
        call_args = mock_wmill.run_script_by_path.call_args
        assert call_args[0][0] == 'f/reconnaissance/prometheus'

    def test_returns_inf_on_empty_metrics(self):
        worker = _create_worker()
        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.return_value = {
            'status': 'completed',
            'metrics': {}
        }

        result = worker._run_reconnaissance()
        assert result == {'throughput': float('inf')}


class TestExecuteTrialUsesRunRecon:
    def setup_method(self):
        sys.modules['wmill'].reset_mock()
        sys.modules['wmill'].run_script_by_path.reset_mock()
        sys.modules['wmill'].run_script_by_path.side_effect = None

    def test_execute_trial_calls_recon_through_shared_method(self):
        worker = _create_worker()
        mock_wmill = sys.modules['wmill']
        mock_wmill.run_script_by_path.side_effect = [
            {'status': 'completed', 'successful_changes': 1, 'failed_changes': 0},
            {'status': 'completed', 'metrics': {'throughput': 42.5}},
        ]

        result = worker._execute_trial({'vm.swappiness': 10})
        assert result == {'throughput': 42.5}
        assert mock_wmill.run_script_by_path.call_count == 2
        recon_call = mock_wmill.run_script_by_path.call_args_list[1]
        assert recon_call[0][0] == 'f/reconnaissance/prometheus'


class TestPublishStandingParams:
    """Per-trial upsert of the systemtender's applied params — the standing
    dials causal stamps curve points with (the ambient of measurement)."""

    def setup_method(self):
        sys.modules['wmill'].reset_mock()

    def test_upserts_params_json(self):
        import json as _json
        worker = _create_worker(interference_detection={'group': 'g1'})
        cursor = MagicMock()
        conn = MagicMock()
        conn.cursor.return_value = cursor
        worker._with_shared_db = lambda op, desc=None: op(conn)

        worker._publish_standing_params({'param_0': 50.0})

        sql, args = cursor.execute.call_args[0]
        assert 'UPDATE interference_active_systemtenders' in sql
        assert 'params = %s' in sql
        assert 'last_seen = NOW()' in sql
        assert _json.loads(args[0]) == {'param_0': 50.0}
        assert args[1] == worker.systemtender_id

    def test_relcache_timeout_is_retryable(self):
        """Fresh tender DB vs yugabyte relcache race (live 2026-09-25,
        cell 36158079097): a coordination call timed out on relcache init
        ~61s into the walk and the worker died. The signature must be
        classified retryable so the gateway backs off instead."""
        err = Exception('connection to server at "yb-tservers" (10.0.0.1), '
                        'port 5433 failed: FATAL:  Relcache init connection '
                        'request to database systemtender_x timed out')
        assert SystemtenderWorker._is_retryable_error(err) is True

    def test_publishes_without_detection_section(self):
        """Standing params publish on every trial — room participation
        does not depend on the dress section being present."""
        worker = _create_worker()
        called = []
        worker._with_shared_db = lambda op, desc=None: called.append(desc)

        worker._publish_standing_params({'param_0': 50.0})

        assert called == ['publish_standing_params']

    def test_skips_on_empty_params(self):
        worker = _create_worker(interference_detection={'group': 'g1'})
        called = []
        worker._with_shared_db = lambda op, desc=None: called.append(desc)

        worker._publish_standing_params({})

        assert called == []


class TestObservationPublishGate:
    """Which trials publish their own readings: every trial with a
    protocol phase in flight (receiver hold, sender push, sender
    pause). Parked trials publish nothing."""

    def test_phase_discriminator(self):
        gate = lambda d: d.get('lease_phase') or d.get('impulse_phase')

        receiver_hold = {'mode': 'hold', 'lease_phase': 'probe_push'}
        sender_push = {'mode': 'impulse', 'impulse_phase': 'probe_push'}
        sender_pause = {'mode': 'hold', 'impulse_phase': 'probe_pause'}
        parked = {'mode': 'hold'}

        assert gate(receiver_hold) == 'probe_push'
        assert gate(sender_push) == 'probe_push'
        assert gate(sender_pause) == 'probe_pause'
        assert gate(parked) is None


class TestHeartbeatBurst:
    """yb abort bursts silenced fresh-connection beats for ~30s and two
    living workers were stamped presumed_dead (run 36261279551,
    2026-09-26). The beat now retries its burst before conceding."""

    def _worker(self):
        worker = SystemtenderWorker.__new__(SystemtenderWorker)
        worker.HEARTBEAT_INTERVAL_SECS = 10
        return worker

    def test_expired_or_abort_signature_is_retryable(self):
        worker = self._worker()
        assert worker._is_retryable_error(
            Exception('current transaction is expired or aborted '
                      '(query layer retry is not possible)'))

    @patch('engine.systemtender_worker.time.sleep', lambda s: None)
    def test_burst_survives_transient_abort_burst(self):
        worker = self._worker()
        beats = {'n': 0}

        def flaky_beat():
            beats['n'] += 1
            if beats['n'] < 3:
                raise Exception('current transaction is expired or aborted')
        with patch.object(worker, '_beat_once', side_effect=flaky_beat):
            worker._beat_with_burst()
        assert beats['n'] == 3

    @patch('engine.systemtender_worker.time.sleep', lambda s: None)
    def test_burst_concedes_after_retries_without_raising(self):
        worker = self._worker()
        beats = {'n': 0}

        def dead_beat():
            beats['n'] += 1
            raise Exception('current transaction is expired or aborted')
        with patch.object(worker, '_beat_once', side_effect=dead_beat):
            worker._beat_with_burst()  # no raise: the loop owns recovery
        assert beats['n'] == 4  # first beat + 3 burst retries

    @patch('engine.systemtender_worker.time.sleep', lambda s: None)
    def test_burst_reraises_non_retryable(self):
        worker = self._worker()
        with patch.object(worker, '_beat_once',
                          side_effect=ValueError('not a db thing')):
            with pytest.raises(ValueError):
                worker._beat_with_burst()




