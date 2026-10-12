"""The seek's level policy — the walk family's placement at the seam.

Pins the placement rule the per-dial passes use: midpoint first, then
the farthest candidate, ties to center; the resolution floor ends a
float dial; ints enumerate. Stateless: fresh rounds re-ask honestly
(the driver owns the measured list, not the policy).
"""

from engine.seek_policy import region_policy


class TestFloatPlacement:
    def test_first_probe_is_the_midpoint(self):
        policy = region_policy({'a': (0.0, 100.0, False)})
        assert policy('a', []) == 50.0

    def test_second_probe_is_an_edge_farthest_from_mid(self):
        policy = region_policy({'a': (0.0, 100.0, False)})
        assert policy('a', [50.0]) in (0.0, 100.0)

    def test_floor_ends_the_dial(self):
        # range 100 -> floor 25; the walk runs to quarter resolution
        # (>= floor passes), then every remaining candidate sits closer
        # than 25 to a measurement and the dial is done.
        policy = region_policy({'a': (0.0, 100.0, False)})
        measured = []
        while True:
            level = policy('a', measured)
            if level is None:
                break
            measured.append(level)
        assert sorted(measured) == [0.0, 25.0, 50.0, 75.0, 100.0]

    def test_placement_is_stateless_across_rounds(self):
        policy = region_policy({'a': (0.0, 100.0, False)})
        assert policy('a', []) == 50.0
        assert policy('a', []) == 50.0  # no hidden memory


class TestIntPlacement:
    def test_ints_enumerate_center_out(self):
        policy = region_policy({'n': (1, 5, True)})
        assert policy('n', []) == 3.0  # center
        measured = []
        while True:
            level = policy('n', measured)
            if level is None:
                break
            measured.append(level)
        assert sorted(measured) == [1.0, 2.0, 3.0, 4.0, 5.0]

    def test_ints_ignore_the_float_floor(self):
        policy = region_policy({'n': (1, 9, True)})
        measured = []
        while True:
            level = policy('n', measured)
            if level is None:
                break
            measured.append(level)
        assert len(measured) == 9  # full enumeration, always


class TestClassifyDart:
    def test_a_dart_is_its_own_kind(self):
        from engine.trial_ledger import classify
        assert classify('push', None, 'idle', dart=True) == 'seek_prior'

    def test_ordinary_trials_classify_unchanged(self):
        from engine.trial_ledger import classify
        assert classify('push', 'probe_push', 'idle') == 'push'
        assert classify('hold', None, 'hold') == 'hold'
        assert classify('push', None, 'idle') == 'push'
