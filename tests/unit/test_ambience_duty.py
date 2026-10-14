#!/usr/bin/env python3
"""
Unit tests for rung 5's robots side — the ambience walk mode.

The same walk, re-anchored and re-addressed: standing duty's local
ground rides the probe override (anchor + epoch aboard), the ledger
kind-stamps the fenced trials, and the DONE-boundary consult names
its levels from the map's debt. No DB, no causal service — the debt
answers are seeded or the HTTP seam is captured.
"""

import sys
import os
import time
import types
import logging
from types import SimpleNamespace
from datetime import datetime

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))

# Stub Windmill runtime imports before importing coordinator
if 'f' not in sys.modules:
    f_mod = types.ModuleType('f')
    systemtender_mod = types.ModuleType('f.systemtender')
    shared_mod = types.ModuleType('f.systemtender.shared')
    otel_mod = types.ModuleType('f.systemtender.shared.otel_logging')
    otel_mod.get_logger = lambda name: logging.getLogger(name)
    f_mod.systemtender = systemtender_mod
    systemtender_mod.shared = shared_mod
    shared_mod.otel_logging = otel_mod
    sys.modules['f'] = f_mod
    sys.modules['f.systemtender'] = systemtender_mod
    sys.modules['f.systemtender.shared'] = shared_mod
    sys.modules['f.systemtender.shared.otel_logging'] = otel_mod

from engine.probe_coordinator import ProbeCoordinator
from engine.trial_ledger import classify


def _noop_db(fn, desc):
    pass


def _config(params=None, **overrides):
    params = params or {
        'param_0': {'constraints': [{'lower': 0.0, 'upper': 100.0}]},
    }
    cfg = {
        'systemtender': {'type': 'bench_generic', 'uuid': 'test-1'},
        'settings': {'generic': params},
        'interference_detection': {
            'group': 'test',
            'hold_params': {name: 50.0 for name in params},
            'push_block_size': 3,
            'pause_block_size': 3,
            'cooldown_trials': 1,
            'convergence_threshold': 0.02,
            'refinement_depth': 3,
            'walk_policy': 'ladder',
        },
    }
    cfg['interference_detection'].update(overrides)
    return cfg


def _make_coordinator(config=None, params=None, **overrides):
    config = config or _config(params=params, **overrides)
    coord = ProbeCoordinator(
        systemtender_id='test-sender-1',
        config=config,
        shared_db_fn=_noop_db,
        collect_upper_bounds_fn=lambda settings: _fake_upper_bounds(settings),
    )
    coord._initialized = True
    return coord


def _fake_upper_bounds(settings):
    gen = settings.get('generic', settings)
    bounds = []
    for name, cfg in gen.items():
        if not isinstance(cfg, dict) or 'constraints' not in cfg:
            continue
        for c in cfg['constraints']:
            bounds.append({
                'name': name,
                'lower': c.get('lower', 0.0),
                'upper': c.get('upper', 100.0),
                'step': c.get('step', 20.0),
                'range': c.get('upper', 100.0) - c.get('lower', 0.0),
                'is_int': isinstance(c.get('lower', 0), int) and isinstance(c.get('upper', 100), int),
            })
    return bounds


def _done_walk():
    """A coverage walk with nothing left to give."""
    return SimpleNamespace(
        next_probe=lambda skip: None,
        can_probe=lambda skip: False,
        status=lambda: {},
        refine=lambda p: None,
    )


def _debt(reason='unmeasured_operating_point', param='param_0',
          anchor=50.0, levels=(50.0,), gaps=None):
    gaps = gaps if gaps is not None else [
        {'from_level': 50.0, 'to_level': 60.0, 'unresolved': True,
         'ignorance': 5.0, 'bars_sum': 0.5},
        {'from_level': 40.0, 'to_level': 50.0, 'unresolved': True,
         'ignorance': 2.0, 'bars_sum': 0.5},
    ]
    return [{
        'param': param, 'anchor': anchor, 'baseline': '{"test-sender-1":{"param_0":50.0}}',
        'window': [anchor - 10.0, anchor + 10.0], 'epoch': 0,
        'reason': reason, 'levels': list(levels), 'gaps': gaps,
    }]


# ─── the kind stamp ───────────────────────────────────────────────

def test_classify_slice_is_its_own_kind():
    print("\n=== test_classify_slice_is_its_own_kind ===")
    assert classify('impulse', 'probe_push', 'probe_push', ambience=True) == 'ambience'
    # phase words never leak into a slice trial's kind
    assert classify('impulse', 'probe_push', 'probe_pause', ambience=True) == 'ambience'
    # non-slice probes classify exactly as before
    assert classify('impulse', 'probe_push', 'probe_push') == 'push'
    assert classify('impulse', 'probe_push', 'probe_push', dart=True) == 'seek_prior'
    print("  PASS")


# ─── the override ride ────────────────────────────────────────────

def test_override_carries_slice_identity():
    print("\n=== test_override_carries_slice_identity ===")
    coord = _make_coordinator()
    coord._init_characterization()
    coord.set_probe_override('param_0', [55.0, 45.0],
                             ambience_anchor=50.0, ambience_epoch=0)
    probe = coord._override_next_probe()
    assert probe is not None
    assert probe['ambience_anchor'] == 50.0
    assert probe['ambience_epoch'] == 0
    # the plain remeasure ride stays unmarked (chain ground)
    coord.set_probe_override('param_0', [60.0])
    probe = coord._override_next_probe()
    assert probe is not None
    assert 'ambience_anchor' not in probe
    print("  PASS")


def test_boundary_walks_ambience_debt_before_done():
    print("\n=== test_boundary_walks_ambience_debt_before_done ===")
    coord = _make_coordinator()
    coord._init_characterization()
    coord._char_walk = _done_walk()
    coord.state = coord.PROBE_PUSH
    coord._push_count = 0
    coord._ambience_debt = lambda force=False: _debt()

    decision = coord._handle_probe_push(SimpleNamespace(number=1))
    assert decision['mode'] == 'impulse'
    assert decision['ambience_probe'] is True
    assert decision['probe_param'] == 'param_0'
    # fattest gap first: [50,60] prices 5.0 -> midpoint 55.0
    assert decision['probe_level'] == 55.0
    # the batch rides the override, anchor + epoch aboard
    ov = coord._probe_override
    assert ov['ambience_anchor'] == 50.0
    assert ov['levels'] == [55.0, 45.0]
    print("  PASS")


def test_boundary_done_when_map_names_no_debt():
    print("\n=== test_boundary_done_when_map_names_no_debt ===")
    coord = _make_coordinator()
    coord._init_characterization()
    coord._char_walk = _done_walk()
    coord.state = coord.PROBE_PUSH
    coord._push_count = 0
    coord._ambience_debt = lambda force=False: []

    decision = coord._handle_probe_push(SimpleNamespace(number=1))
    assert coord.state == coord.COOLDOWN  # DONE handed off, walk ended
    assert 'ambience_probe' not in decision
    print("  PASS")


def test_boundary_parks_on_dark_consult():
    print("\n=== test_boundary_parks_on_dark_consult ===")
    from engine.walk_policy import WalkViewUnavailable
    coord = _make_coordinator()
    coord._init_characterization()
    coord._char_walk = _done_walk()
    coord.state = coord.PROBE_PUSH
    coord._push_count = 0
    coord._ambience_debt = lambda force=False: None  # dark: not an answer

    decision = coord._handle_probe_push(SimpleNamespace(number=1))
    assert coord.state == coord.PROBE_PUSH  # parked, not ended
    assert decision.get('mode') != 'impulse'
    print("  PASS")


# ─── placement (the dumb stateless legs) ──────────────────────────

def test_placement_skips_banked_and_caps_batch():
    print("\n=== test_placement_skips_banked_and_caps_batch ===")
    gaps = [
        {'from_level': 50.0, 'to_level': 52.5, 'unresolved': True,
         'ignorance': 9.0, 'bars_sum': 0.5},
        {'from_level': 47.5, 'to_level': 50.0, 'unresolved': True,
         'ignorance': 8.0, 'bars_sum': 0.5},
        {'from_level': 45.0, 'to_level': 47.5, 'unresolved': True,
         'ignorance': 7.0, 'bars_sum': 0.5},
        {'from_level': 42.5, 'to_level': 45.0, 'unresolved': True,
         'ignorance': 6.0, 'bars_sum': 0.5},
        {'from_level': 40.0, 'to_level': 42.5, 'unresolved': True,
         'ignorance': 5.0, 'bars_sum': 0.5},
        {'from_level': 37.5, 'to_level': 40.0, 'unresolved': True,
         'ignorance': 4.0, 'bars_sum': 0.5},
        {'from_level': 35.0, 'to_level': 37.5, 'unresolved': True,
         'ignorance': 3.0, 'bars_sum': 0.5},
        {'from_level': 32.5, 'to_level': 35.0, 'unresolved': True,
         'ignorance': 2.0, 'bars_sum': 0.5},
        {'from_level': 30.0, 'to_level': 32.5, 'unresolved': True,
         'ignorance': 1.0, 'bars_sum': 0.5},
        # priced out: its own bars say nothing hides here
        {'from_level': 27.5, 'to_level': 30.0, 'unresolved': True,
         'ignorance': 0.1, 'bars_sum': 0.5},
    ]
    coord = _make_coordinator()
    coord._init_characterization()
    coord._ambience_debt = lambda force=False: _debt(
        levels=(51.25,), gaps=gaps)

    probe = coord._ambience_duty_probe()
    assert probe is not None
    ov = coord._probe_override
    assert len(ov['levels']) == coord.AMBIENCE_WALK_BATCH  # capped at 8
    # the banked midpoint (51.25) never repeats
    assert all(abs(l - 51.25) > 1e-9 for l in ov['levels'])
    # priced-out gaps never place
    assert all(abs(l - 28.75) > 1e-9 for l in ov['levels'])
    # ignorance order, banked midpoints skipped: the 9.0-gap's midpoint
    # (51.25) is banked, so the 8.0-gap [47.5,50] places first -> 48.75
    assert ov['levels'][0] == 48.75
    print("  PASS")


def test_placement_fences_to_param_bounds():
    print("\n=== test_placement_fences_to_param_bounds ===")
    gaps = [
        {'from_level': 95.0, 'to_level': 105.0, 'unresolved': True,
         'ignorance': 9.0, 'bars_sum': 0.5},
    ]
    coord = _make_coordinator()
    coord._init_characterization()
    coord._ambience_debt = lambda force=False: _debt(
        anchor=95.0, levels=(95.0,), gaps=gaps)

    probe = coord._ambience_duty_probe()
    assert probe is not None
    assert coord._probe_override['levels'] == [100.0]  # clamped to bound
    print("  PASS")


# ─── the wire ─────────────────────────────────────────────────────

def test_probe_result_payload_carries_slice_fields(monkeypatch):
    print("\n=== test_probe_result_payload_carries_slice_fields ===")
    import urllib.request

    coord = _make_coordinator()
    coord._round_push_start = datetime.now()
    coord._round_pause_end = datetime.now()

    captured = {}

    class FakeResp:
        def read(self):
            return b'{"shift": 0.0, "delta": 0.0, "converged": true}'
        def __enter__(self):
            return self
        def __exit__(self, *a):
            return False

    def fake_urlopen(req, timeout=None):
        captured['url'] = req.full_url
        captured['data'] = req.data
        return FakeResp()

    monkeypatch.setattr(urllib.request, 'urlopen', fake_urlopen)

    probe = {'param_name': 'param_0', 'level': 55.0,
             'ambience_anchor': 50.0, 'ambience_epoch': 0}
    coord._query_causal_probe_result(probe)
    import json
    payload = json.loads(captured['data'])
    assert payload['ambience_anchor'] == 50.0
    assert payload['ambience_epoch'] == 0

    # plain probes bank exactly as before — no slice residue
    probe = {'param_name': 'param_0', 'level': 60.0}
    coord._query_causal_probe_result(probe)
    payload = json.loads(captured['data'])
    assert 'ambience_anchor' not in payload
    print("  PASS")


# ─── the pending gates ────────────────────────────────────────────

def test_unmeasured_debt_holds_the_tender():
    print("\n=== test_unmeasured_debt_holds_the_tender ===")
    coord = _make_coordinator()
    coord._init_characterization()
    coord._char_walk = _done_walk()

    coord._ambience_debt_cache = _debt()
    coord._ambience_debt_at = time.monotonic()
    assert coord._walk_pending() is True
    assert coord.char_complete() is False

    coord._ambience_debt_cache = []  # patches exist: aging-only world
    coord._ambience_debt_at = time.monotonic()
    assert coord._walk_pending() is False
    assert coord.char_complete() is True
    print("  PASS")


if __name__ == '__main__':
    class _Mp:
        def setattr(self, obj, name, value):
            setattr(obj, name, value)

    test_classify_slice_is_its_own_kind()
    test_override_carries_slice_identity()
    test_boundary_walks_ambience_debt_before_done()
    test_boundary_done_when_map_names_no_debt()
    test_boundary_parks_on_dark_consult()
    test_placement_skips_banked_and_caps_batch()
    test_placement_fences_to_param_bounds()
    test_probe_result_payload_carries_slice_fields(monkeypatch=_Mp())
    test_unmeasured_debt_holds_the_tender()
    print("\nall ambience duty tests passed")
