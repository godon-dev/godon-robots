#
# Copyright (c) 2019 Matthias Tafelmeier.
#
# AGPL-3.0 — see godon-systemtenders/LICENSE.
#
"""Walk-view survival tests — a slow notebook must not kill a tender.

Forensics on run 35224443449 (job 01a0af75, died 13:10:47): the worker
loop asked "are we done?" every trial — char_complete -> can_probe ->
next_probe -> view(), a plain HTTP read of causal's walk-view with a
2-second timeout. One slow answer raised TimeoutError unhandled through
run() and the whole worker job FAILED; nothing re-queues it, so the
tender died, its lease and presence froze. Three deaths today, one
cause.

The doctrine split these tests pin:
  - view() failures surface as WalkViewUnavailable (transient class
    only — timeout/connection; a 4xx stays LOUD: a wrong page is a
    config break, not a blip).
  - init's reachability check stays loud (creation-time).
  - the three run-loop surfaces survive a dark view: char_complete
    answers False (not complete), _walk_pending answers True (the
    tender stays engaged), the push-path ask skips one boundary.
"""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

import pytest

from engine.probe_coordinator import ProbeCoordinator
from engine.walk_policy import WalkPolicy, WalkViewUnavailable


def _config():
    params = {'param_0': {'constraints': [{'lower': 0.0, 'upper': 100.0}]}}
    return {
        'systemtender': {'type': 'bench_generic', 'uuid': 'view-1'},
        'settings': {'generic': params},
        'interference_detection': {
            'group': 'bench-characterization',
            'hold_params': {'param_0': 50.0},
            'push_block_size': 10,
            'pause_block_size': 10,
            'cooldown_trials': 5,
            'convergence_threshold': 0.005,
            'refinement_depth': 3,
            'walk_policy': 'ladder',
            'quantum_cycles': 2,
        },
    }


def _coordinator():
    return ProbeCoordinator(
        systemtender_id='V1',
        config=_config(),
        shared_db_fn=lambda op, label=None: None,
        collect_upper_bounds_fn=lambda settings: [
            {'name': 'param_0', 'lower': 0.0, 'upper': 100.0,
             'is_int': False, 'idx': 0},
        ],
        compute_neutral_params_fn=lambda: {'param_0': 50.0},
    )


class TestViewFailureTyping:
    def _policy_with_transport(self, exc):
        return WalkPolicy(
            causal_url='http://causal', group_id='g',
            systemtender_id='V1', refinement_depth=3,
            param_bounds={'param_0': (0.0, 100.0, False)},
            transport=lambda m, u, payload=None: (_ for _ in ()).throw(exc))

    def test_timeout_becomes_walk_view_unavailable(self):
        policy = self._policy_with_transport(TimeoutError("timed out"))
        with pytest.raises(WalkViewUnavailable):
            policy.view('param_0')

    def test_connection_error_becomes_walk_view_unavailable(self):
        policy = self._policy_with_transport(ConnectionError("refused"))
        with pytest.raises(WalkViewUnavailable):
            policy.view('param_0')


class TestRunLoopSurvivesDarkView:
    def _coord_with_dark_view(self):
        coord = _coordinator()
        dark = WalkPolicy(
            causal_url='http://causal', group_id='g',
            systemtender_id='V1', refinement_depth=3,
            param_bounds={'param_0': (0.0, 100.0, False)},
            transport=lambda m, u, payload=None: (_ for _ in ()).throw(
                TimeoutError("view dark")))
        coord._char_walk = dark
        coord._param_names = ['param_0']
        return coord

    def test_char_complete_answers_not_complete(self):
        coord = self._coord_with_dark_view()
        assert coord.char_complete() is False, \
            "a dark view must not read as walk-complete (nor kill the loop)"

    def test_walk_pending_stays_engaged(self):
        coord = self._coord_with_dark_view()
        assert coord._walk_pending() is True, \
            "a dark view must keep the tender engaged, not park it"

    def test_push_ask_skips_one_boundary(self):
        coord = self._coord_with_dark_view()
        coord.state = coord.PROBE_PUSH
        coord._push_count = 0
        coord._current_probe = None
        result = coord._handle_probe_push(trial=None)
        assert result.get('mode') in ('hold', 'optimize'), \
            "a dark view must park one trial, not die and not advance"
