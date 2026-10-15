"""Pre-step prevention (design §XIX spillover): the walk's guards.

The page publishes, per region dial, the claims of other live wishes
that measured coupling connects to this tender's dial — band, reading
now, error margin, and the measured curve. The walk never steps a
level that would push a standing band out; a fully closed region comes
home with the numbers instead of walking.
"""

from engine.seek_wish import parse_seek_plan
from engine.seek_policy import region_policy
from engine.seek_driver import SeekDriver, STOP_GUARDS, DARTS


def _driver(region, initial_levels, guards, form='cover_now',
            criterion=None, budget=None):
    plan = parse_seek_plan({
        'wish_id': 'w-g',
        'form': form,
        'region': region,
        'criterion': criterion,
        'budget': budget,
        'candidates': None,
    })
    d = SeekDriver(plan, region_policy(plan.region),
                   initial_levels=initial_levels)
    d.set_guards(guards)
    return d


def _guard(lo, hi, now, curve, margin=0.0):
    return {
        'param': 'a', 'outcome': 'r/obj', 'lo': lo, 'hi': hi,
        'margin': margin, 'now': now, 'held_by': 'w-other',
        'curve': curve,
    }


def test_a_breaking_level_is_never_stepped():
    # shift(x) = x, dial now at 0, reading now 5.0, band [4, 6]:
    # safe levels are x in [0, 1] — the midpoint 5 must never issue.
    d = _driver({'a': [0.0, 10.0, False]}, {'a': 0.0},
                [_guard(4.0, 6.0, 5.0, [[0.0, 0.0], [10.0, 10.0]])])
    step = d.step()
    assert step is not None, 'a safe level exists — the walk proceeds'
    assert step['probe_level'] != 5.0, 'the breaker is never stepped'
    assert d._breaks('a', 5.0) is not None, '5.0 would read 10.0'
    assert d._breaks('a', 0.0) is None, '0.0 keeps the reading at 5.0'


def test_a_closed_region_comes_home_with_the_numbers():
    # band [5.05, 5.15] around reading 5.0 at dial 1.0: only x in
    # [1.05, 1.15] is legal — the placement's candidates never land
    # there, the dial closes without a single pass, and the region
    # ends with the guards on the record.
    d = _driver({'a': [0.0, 2.0, False]}, {'a': 1.0},
                [_guard(5.05, 5.15, 5.0, [[0.0, 0.0], [2.0, 2.0]])])
    assert d.step() is None, 'no legal step exists in the whole region'
    result = d.result()
    assert result['stop_reason'] == STOP_GUARDS
    assert result['guards'], 'the return carries the blocking guards'
    assert result['guards'][0]['held_by'] == 'w-other'
    assert 'never' not in result.get('detail', '')


def test_a_breaking_dart_is_dropped_not_run():
    d = _driver({'a': [0.0, 10.0, False]}, {'a': 0.0},
                [_guard(4.0, 6.0, 5.0, [[0.0, 0.0], [10.0, 10.0]])],
                form='find_best',
                criterion={'outcome': 'r/obj', 'direction': 'max'},
                budget=10)
    d._darts = [{'a': 5.0}, {'a': 0.5}]  # a breaker, then a safe one
    d._phase = DARTS
    step = d.step()
    assert step is not None and step.get('dart'), 'the safe dart runs'
    assert step['params'] == {'a': 0.5}, 'the breaking dart never runs'


def test_guards_refresh_on_every_serve():
    d = _driver({'a': [0.0, 10.0, False]}, {'a': 0.0},
                [_guard(4.0, 6.0, 5.0, [[0.0, 0.0], [10.0, 10.0]])])
    assert d._breaks('a', 0.0) is None, 'level 0 is safe under the guard'
    # the standing set moved: the band tightens past level 0
    d.set_guards([_guard(5.0, 6.0, 4.5, [[0.0, 0.0], [10.0, 10.0]])])
    assert d._breaks('a', 0.0) is not None, \
        'level 0 now reads 4.5 — outside the tightened band'


def test_unmeasured_ground_publishes_no_guard():
    # no guard at all: the placement is exactly the policy's own
    d = _driver({'a': [0.0, 10.0, False]}, {'a': 0.0}, [])
    step = d.step()
    assert step is not None and step['probe_level'] == 5.0, \
        'without a guard the midpoint stands — detection covers that ground'
