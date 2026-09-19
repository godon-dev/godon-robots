"""Steer-mode tender duties: the release instruction and the walk's
one-level probe hint.

The tender stays a dumb arm — it never judges. But two instructions
demand acts: 'release' must revert the dial to neutral AND delete this
tender's own assignment row (or the pulse's adopt-on-appearance would
re-adopt the corpse on the next boundary), and a miss's probe hint is
ONE level per refresh, handed to the coordinator's override.
"""

from unittest.mock import MagicMock, patch

from engine.systemtender_worker import SystemtenderWorker


def _bare_worker():
    """A worker skeleton: no loop, no config — only the steer fields."""
    w = object.__new__(SystemtenderWorker)
    w._wish = None
    w._probe_coordinator = None
    return w


def _fake_engine():
    """Context-managed engine whose DELETE we can assert on."""
    conn = MagicMock()
    engine = MagicMock()
    engine.connect.return_value.__enter__.return_value = conn
    return engine, conn


def test_release_instruction_reverts_and_deletes_row():
    print("\n=== test_release_instruction_reverts_and_deletes_row ===")
    w = _bare_worker()
    w._wish = {'wish_id': 'w-1', 'param': 'valve', 'setting': 0.62,
               'instruction': 'hold'}
    engine, conn = _fake_engine()
    w.study = MagicMock()
    # The unwrap path: _CachedStorage wraps RDBStorage — the engine
    # lives on the backend (a bare storage would expose it directly).
    w.study._storage._backend.engine = engine
    w.study._storage.engine = engine

    plan = {'status': 'released', 'instruction': 'release', 'plan': None}
    with patch.object(w, '_fetch_wish_plan', return_value=plan), \
            patch.object(w, '_compute_neutral_params', return_value={'valve': 0.5}), \
            patch.object(w, '_execute_trial') as exec_trial:
        w._refresh_wish_status()

    assert w._wish is None, "the wish must be dropped"
    assert exec_trial.call_count == 1, "the dial reverts to neutral once"
    reverted = exec_trial.call_args[0][0]
    assert reverted == {'valve': 0.5}
    deleted = conn.execute.call_args[0][0]
    assert 'DELETE FROM wish_assignments' in str(deleted)
    assert conn.execute.call_args[0][1] == {'wid': 'w-1'}

    print("  instruction release -> neutral + row deleted + wish dropped")
    print("  PASS")


def test_hold_still_holds_and_release_by_row_stays():
    print("\n=== test_hold_still_holds_and_release_by_row_stays ===")
    w = _bare_worker()
    w._wish = {'wish_id': 'w-2', 'param': 'valve', 'setting': 0.62,
               'instruction': 'hold'}

    plan = {'status': 'serving', 'instruction': 'hold',
            'plan': {'moves': [{'param': 'valve', 'setting': 0.74}]}}
    with patch.object(w, '_fetch_wish_plan', return_value=plan):
        w._refresh_wish_status()

    assert w._wish is not None, "hold keeps the wish"
    assert w._wish['setting'] == 0.74, "re-plan updates the held setting"
    assert w._wish['instruction'] == 'hold'

    # row removal (the owner's close) still releases the plain way
    engine, conn = _fake_engine()
    w.study = MagicMock()
    w.study._storage.engine = engine
    with patch.object(w, '_compute_neutral_params', return_value={'valve': 0.5}), \
            patch.object(w, '_execute_trial'):
        w._release_wish()  # delete_assignment defaults False
    assert w._wish is None
    conn.execute.assert_not_called()

    print("  hold re-plans in place; row removal releases without DELETE")
    print("  PASS")


def test_miss_hint_is_one_level_to_the_override():
    print("\n=== test_miss_hint_is_one_level_to_the_override ===")
    w = _bare_worker()
    w._wish = {'wish_id': 'w-3', 'param': 'valve', 'setting': 0.62,
               'instruction': 'hold'}
    coord = MagicMock()
    w._probe_coordinator = coord

    plan = {'status': 'missed', 'instruction': 'remeasure',
            'plan': {'moves': [{'param': 'valve', 'setting': 0.62}]},
            'probe': {'param': 'valve', 'level': 0.6978, 'phase': 'slope'}}
    with patch.object(w, '_fetch_wish_plan', return_value=plan):
        w._refresh_wish_status()

    coord.set_probe_override.assert_called_once_with('valve', [0.6978])
    assert w._wish['instruction'] == 'remeasure'

    print("  one level per refresh reaches the coordinator override")
    print("  PASS")
