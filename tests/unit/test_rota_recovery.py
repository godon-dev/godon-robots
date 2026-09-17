#
# Copyright (c) 2019 Matthias Tafelmeier.
#
# AGPL-3.0 — see godon-systemtenders/LICENSE.
#
"""Rota recovery tests — the mutual-invisibility deadlock.

Run 35206141133 (edgedrift, Sep 17): a worker died mid-walk; its
corpse-held lease blocked the group for the heartbeat-staleness window
while the survivor parked in HOLD — and HOLD refreshes no presence, so
after the active window BOTH census rows went stale, the
`< 2 active systemtenders` gate idled everyone before any acquire
attempt, and the rota froze permanently (curves frozen at 1-2 points,
`walking` in the metrics for two hours, poll rode the watchdog).

These tests pin the three cures:
  1. HOLD trials refresh presence (a parked member is still a member).
  2. Lease liveness keys on the HOLDER's presence, not only on the
     lease row's own heartbeat clock — a dead holder's lease is
     claimable once its census row goes stale.
  3. A cooldown-path acquire logs (the silent re-entry cost a full
     forensics session).
"""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

import pytest

from engine.probe_coordinator import ProbeCoordinator
from engine import probe_coordinator as _pc_module


def _config():
    params = {'param_0': {'constraints': [{'lower': 0.0, 'upper': 100.0}]}}
    return {
        'systemtender': {'type': 'bench_generic', 'uuid': 'rota-1'},
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


class _RecordingCursor:
    def __init__(self, statements):
        self._statements = statements
        self.rowcount = 0

    def execute(self, sql, params=None):
        self._statements.append((sql, params))

    def fetchone(self):
        return (0,)

    def close(self):
        pass


class _RecordingDB:
    """Captures every op closure's SQL + params, answers benignly."""

    def __init__(self):
        self.statements = []

    def __call__(self, op, label=None):
        conn = type('C', (), {})()
        conn.cursor = lambda: _RecordingCursor(self.statements)
        return op(conn)


def _coordinator(shared_db=None):
    return ProbeCoordinator(
        systemtender_id='A1',
        config=_config(),
        shared_db_fn=shared_db or _RecordingDB(),
        collect_upper_bounds_fn=lambda settings: [
            {'name': 'param_0', 'lower': 0.0, 'upper': 100.0,
             'is_int': False, 'idx': 0},
        ],
        compute_neutral_params_fn=lambda: {'param_0': 50.0},
    )


class TestHoldRefreshesPresence:
    def test_hold_trial_refreshes_presence(self):
        coord = _coordinator()
        coord.state = coord.HOLD
        coord._has_active_sender = lambda: True
        coord._hold_count = 0
        calls = []
        coord._refresh_presence = lambda: calls.append(1)
        coord._handle_hold(trial=None)
        assert calls, "a parked member must keep its census row alive"

    def test_refresh_presence_sql_contract(self):
        db = _RecordingDB()
        coord = _coordinator(shared_db=db)
        coord._refresh_presence()
        sql = [s for s, _ in db.statements if 'last_seen = NOW()' in s]
        assert sql, "presence refresh must touch last_seen"
        assert 'walk_pending' not in sql[0], \
            "presence refresh must not touch the demand flag"
        for s, p in db.statements:
            if 'last_seen = NOW()' in s:
                assert len(p or ()) == s.count('%s'), "arity contract"


class TestLeaseLivenessKeysOnHolderPresence:
    def test_has_active_sender_requires_live_holder(self):
        db = _RecordingDB()
        coord = _coordinator(shared_db=db)
        coord._has_active_sender()
        sql = db.statements[0][0]
        assert 'interference_active_systemtenders' in sql, \
            "lease liveness must join the holder's census row"

    def test_acquire_claims_dead_holders_lease(self):
        db = _RecordingDB()
        coord = _coordinator(shared_db=db)
        coord._char_walk = type('W', (), {
            'can_probe': staticmethod(lambda skip: True)})()
        coord._try_acquire_lease(coord.PROBE_PUSH)
        acquire_sql = [s for s, _ in db.statements
                       if 'UPDATE sender_lease' in s][0]
        assert 'interference_active_systemtenders' in acquire_sql, \
            "the free-lease condition must include holder-presence death"

    def test_arity_contracts(self):
        for meth, phase in ((lambda c: c._has_active_sender(), None),
                            (lambda c: c._try_acquire_lease(c.PROBE_PUSH), None)):
            db = _RecordingDB()
            coord = _coordinator(shared_db=db)
            coord._char_walk = type('W', (), {
                'can_probe': staticmethod(lambda skip: True)})()
            meth(coord)
            for s, p in db.statements:
                if 'sender_lease' in s:
                    assert len(p or ()) == s.count('%s'), \
                        f"arity contract violated: {s[:60]}"


class TestCooldownAcquireIsLoud:
    def test_cooldown_reacquire_logs(self):
        coord = _coordinator()
        coord._char_walk = type('W', (), {
            'can_probe': staticmethod(lambda skip: True)})()
        coord._param_names = ['param_0']
        coord.state = coord.COOLDOWN
        coord._cooldown_count = 4  # next trial completes cooldown
        acquired = []
        coord._try_acquire_lease = lambda phase: acquired.append(phase) or True
        coord._handle_probe_push = lambda trial: {'mode': 'hold'}

        # A recording logger, swapped in for the duration: the assertion
        # is about the call site, not the logging plumbing (suite order
        # resets levels/handlers on the real logger).
        lines = []

        class _RecLogger:
            def info(self, msg, *a, **k):
                lines.append(msg % a if a else msg)

            def warning(self, msg, *a, **k):
                lines.append(msg)

            def error(self, msg, *a, **k):
                lines.append(msg)

        real_logger = _pc_module.logger
        _pc_module.logger = _RecLogger()
        try:
            coord._handle_cooldown(trial=None)
        finally:
            _pc_module.logger = real_logger
        assert acquired, "cooldown must attempt the re-acquire"
        assert any('Acquired lease' in m for m in lines), \
            "a silent re-entry is invisible in forensics"
