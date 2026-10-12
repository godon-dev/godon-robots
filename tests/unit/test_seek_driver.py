"""The seek driver — the §XIV loop as tests.

Each test drives the state machine the way the worker will: step()
before a trial, observe() after, seams between trials only. The
policies and readings are fakes; the rulings they pin are real:

- darts are joint pushes, one per trial, then the seam
- wallet exhaustion between trials returns best-so-far (RETURNED,
  never a miss)
- a priced-out dial steps the baseline; the next dial probes over it
- a round that moves nothing converges
- cover_now walks the region once with no compass and returns no
  finding — the walked trials ARE the receipt
"""

import pytest

from engine.seek_driver import (
    DARTS,
    PASSES,
    STOP_BUDGET,
    STOP_CONVERGED,
    STOP_REGION,
    STOP_STOPPED,
    SeekDriver,
)
from engine.seek_wish import (
    COVER_NOW,
    FIND_BEST,
    MAXIMIZE,
    MINIMIZE,
    RETURNED,
    SeekRefused,
    parse_seek_plan,
)


def find_plan(budget=100, candidates=2):
    return parse_seek_plan({
        'wish_id': 'sw-t',
        'species': FIND_BEST,
        'region': {'a': (0.0, 10.0, False), 'b': (0.0, 10.0, False)},
        'criterion': {'outcome': 'room/obj', 'direction': MAXIMIZE},
        'budget': budget,
        'candidates': candidates,
    })


def queue_policy(queues):
    """Fake walk machinery: named levels per dial, then None (done)."""
    state = {d: list(q) for d, q in queues.items()}

    def policy(dial, measured):
        if state.get(dial):
            return state[dial].pop(0)
        return None
    return policy


def drive(driver, readings):
    """Step/observe through the whole seek; readings consumed in order."""
    steps = []
    for reading in readings:
        step = driver.step()
        if step is None:
            break
        driver.observe(reading)
        steps.append(step)
    while driver.step() is not None:
        pass  # drain any zero-measurement remainder
    return steps


class TestDarts:
    def test_darts_are_joint_pushes_one_per_trial(self):
        plan = find_plan(budget=10, candidates=2)
        driver = SeekDriver(plan, queue_policy({'a': [5.0], 'b': [1.0]}))
        first = driver.step()
        assert first is not None
        assert first['phase'] == DARTS and first['dart'] is True
        assert set(first['params']) == {'a', 'b'}
        driver.observe(0.4)
        second = driver.step()
        assert second is not None and second['dart'] is True
        driver.observe(0.9)
        # dart phase over: the next step is a pass probe
        third = driver.step()
        assert third is not None
        assert third['phase'] == PASSES and 'dart' not in third

    def test_darts_rank_into_the_return_when_budget_ends(self):
        plan = find_plan(budget=2, candidates=2)
        driver = SeekDriver(plan, queue_policy({'a': [], 'b': []}))
        driver.step(); driver.observe(0.4)
        driver.step(); driver.observe(0.9)
        assert driver.step() is None  # wallet empty at the seam
        result = driver.result()
        assert result['verdict'] == RETURNED
        assert result['stop_reason'] == STOP_BUDGET
        assert result['spend_count'] == 2
        assert result['finding']['reading'] == 0.9


class TestPasses:
    def test_priced_out_dial_steps_the_baseline(self):
        plan = find_plan(budget=50, candidates=0)
        policy = queue_policy({'a': [5.0, 7.0], 'b': [1.0]})
        driver = SeekDriver(plan, policy)
        driver.step(); driver.observe(0.5)   # a=5.0
        driver.step(); driver.observe(0.8)   # a=7.0  <- best for a
        # a prices out INSIDE the next step: the same step returns b's
        # first probe, pushed over the stepped baseline.
        b_probe = driver.step()
        assert b_probe is not None
        assert b_probe['probe_dial'] == 'b'
        assert b_probe['params']['a'] == 7.0  # stepped
        assert b_probe['params']['b'] == 1.0

    def test_minimize_direction_picks_the_lowest(self):
        plan = parse_seek_plan({
            'wish_id': 'sw-t', 'species': FIND_BEST,
            'region': {'a': (0.0, 10.0, False), 'b': (0.0, 10.0, False)},
            'criterion': {'outcome': 'r', 'direction': MINIMIZE},
            'budget': 100, 'candidates': 0,
        })
        driver = SeekDriver(plan, queue_policy({'a': [5.0, 7.0], 'b': [1.0]}))
        driver.step(); driver.observe(0.5)   # a=5.0
        driver.step(); driver.observe(0.2)   # a=7.0 lower
        # a prices out inside this step; it returns b's probe directly
        b = driver.step()
        assert b is not None and b['probe_dial'] == 'b'
        driver.observe(0.4)
        # b done; round 2 asks nothing new (queues empty) -> converged
        assert driver.step() is None
        result = driver.result()
        assert result['finding']['reading'] == 0.2

    def test_wallet_exhaustion_between_trials_only(self):
        plan = find_plan(budget=3, candidates=0)
        policy = queue_policy({'a': [1.0, 2.0, 3.0], 'b': [9.0]})
        driver = SeekDriver(plan, policy)
        driver.step(); driver.observe(0.1)
        driver.step(); driver.observe(0.2)
        driver.step(); driver.observe(0.3)    # wallet now empty
        assert driver.step() is None          # refused at the seam
        result = driver.result()
        assert result['stop_reason'] == STOP_BUDGET
        assert result['spend_count'] == 3


class TestRounds:
    def test_round_that_moves_nothing_converges(self):
        plan = find_plan(budget=100, candidates=0)
        # Both dials offer one level each round; best never changes ->
        # round two's baseline equals round one's -> converged.
        def policy(dial, measured):
            return 4.0 if not measured else None
        driver = SeekDriver(plan, policy)
        # Round 1: one probe per dial; round 2 re-asks both (fresh
        # measured), baseline unchanged by both -> converged at the seam.
        driver.step(); driver.observe(0.5)   # a=4.0
        driver.step(); driver.observe(0.5)   # b=4.0
        driver.step(); driver.observe(0.5)   # round 2: a=4.0 again
        driver.step(); driver.observe(0.5)   # round 2: b=4.0 again
        assert driver.step() is None
        assert driver.result()['stop_reason'] == STOP_CONVERGED


class TestCoverNow:
    def test_cover_now_walks_once_and_returns_no_finding(self):
        plan = parse_seek_plan({
            'wish_id': 'sw-c', 'species': COVER_NOW,
            'region': {'a': (0.0, 4.0, False), 'b': (0.0, 4.0, False)},
        })
        driver = SeekDriver(plan, queue_policy({'a': [2.0], 'b': [3.0]}))
        steps = drive(driver, [0.5, 0.6])
        assert len(steps) == 2
        result = driver.result()
        assert result['verdict'] == RETURNED
        assert result['stop_reason'] == STOP_REGION
        assert result['finding'] is None
        assert result['spend_count'] == 0  # cover_now carries no wallet

    def test_cover_now_never_steps_a_baseline(self):
        plan = parse_seek_plan({
            'wish_id': 'sw-c', 'species': COVER_NOW,
            'region': {'a': (0.0, 4.0, False), 'b': (0.0, 4.0, False)},
        })
        driver = SeekDriver(plan, queue_policy({'a': [2.0], 'b': [3.0]}))
        driver.step(); driver.observe(0.9)
        b = driver.step()
        assert b is not None
        assert 'a' not in b['params']  # no compass -> no stepping


class TestAbortAndContract:
    def test_abort_comes_home_returned(self):
        plan = find_plan(budget=100, candidates=1)
        driver = SeekDriver(plan, queue_policy({'a': [1.0], 'b': [1.0]}))
        driver.step(); driver.observe(0.7)
        driver.abort()
        assert driver.step() is None
        result = driver.result()
        assert result['verdict'] == RETURNED
        assert result['stop_reason'] == STOP_STOPPED
        assert result['finding']['reading'] == 0.7

    def test_step_demands_its_observe(self):
        driver = SeekDriver(find_plan(), queue_policy({'a': [], 'b': []}))
        driver.step()
        with pytest.raises(RuntimeError):
            driver.step()

    def test_observe_without_a_step_refused(self):
        driver = SeekDriver(find_plan(), queue_policy({'a': [], 'b': []}))
        with pytest.raises(RuntimeError):
            driver.observe(0.5)

    def test_result_before_completion_refused(self):
        driver = SeekDriver(find_plan(), queue_policy({'a': [], 'b': []}))
        with pytest.raises(RuntimeError):
            driver.result()
