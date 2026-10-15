"""Steer-mode tender duties: the release instruction, the walk's
one-level probe hint, and joint serving (one wish, many dials).

The tender stays a dumb arm — it never judges. But instructions
demand acts: 'release' must revert the dial(s) to neutral AND delete
this tender's own assignment row (or the pulse's adopt-on-appearance
would re-adopt the corpse on the next boundary), a miss's probe hint
is ONE level per refresh, handed to the coordinator's override, and a
multi-claim plan names every sender — this tender adopts exactly its
own moves, never another tender's.
"""

from unittest.mock import MagicMock, patch

from engine.systemtender_worker import SystemtenderWorker


def _bare_worker(systemtender_id='tender-aaa'):
    """A worker skeleton: no loop, no config — only the steer fields."""
    w = object.__new__(SystemtenderWorker)
    w._wish = None
    w._probe_coordinator = None
    w.systemtender_id = systemtender_id
    return w


def _fake_ledger():
    """A ledger mock whose removals we can assert on."""
    return MagicMock()


def test_release_instruction_reverts_and_deletes_row():
    print("\n=== test_release_instruction_reverts_and_deletes_row ===")
    w = _bare_worker()
    w._wish = {'wish_id': 'w-1', 'moves': {'valve': 0.62},
               'instruction': 'hold'}
    w.ledger = _fake_ledger()

    plan = {'status': 'released', 'instruction': 'release', 'plan': None}
    with patch.object(w, '_fetch_wish_plan', return_value=plan), \
            patch.object(w, '_compute_neutral_params', return_value={'valve': 0.5}), \
            patch.object(w, '_execute_trial') as exec_trial:
        w._refresh_wish_status()

    assert w._wish is None, "the wish must be dropped"
    assert exec_trial.call_count == 1, "the dial(s) revert to neutral once"
    reverted = exec_trial.call_args[0][0]
    assert reverted == {'valve': 0.5}
    w.ledger.wish_remove.assert_called_once_with('w-1')

    print("  instruction release -> neutral + row deleted + wish dropped")
    print("  PASS")


def test_hold_still_holds_and_release_by_row_stays():
    print("\n=== test_hold_still_holds_and_release_by_row_stays ===")
    w = _bare_worker()
    w._wish = {'wish_id': 'w-2', 'moves': {'valve': 0.62},
               'instruction': 'hold'}

    plan = {'status': 'serving', 'instruction': 'hold',
            'plan': {'moves': [{'param': 'valve', 'setting': 0.74}]}}
    with patch.object(w, '_fetch_wish_plan', return_value=plan):
        w._refresh_wish_status()

    assert w._wish is not None, "hold keeps the wish"
    assert w._wish['moves']['valve'] == 0.74, "re-plan updates the held setting"
    assert w._wish['instruction'] == 'hold'

    # row removal (the owner's close) still releases the plain way
    w.ledger = _fake_ledger()
    with patch.object(w, '_compute_neutral_params', return_value={'valve': 0.5}), \
            patch.object(w, '_execute_trial'):
        w._release_wish()  # delete_assignment defaults False
    assert w._wish is None
    w.ledger.wish_remove.assert_not_called()

    print("  hold re-plans in place; row removal releases without DELETE")
    print("  PASS")


def test_miss_hint_is_one_level_to_the_override():
    print("\n=== test_miss_hint_is_one_level_to_the_override ===")
    w = _bare_worker()
    w._wish = {'wish_id': 'w-3', 'moves': {'valve': 0.62},
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


def test_adopt_serves_exactly_this_tenders_moves():
    print("\n=== test_adopt_serves_exactly_this_tenders_moves ===")
    w = _bare_worker(systemtender_id='tender-aaa')
    plan = {
        'status': 'planned', 'instruction': 'hold',
        'plan': {
            'moves': [
                {'sender': 'tender-aaa', 'param': 'valve', 'setting': 0.62},
                {'sender': 'tender-bbb', 'param': 'pump', 'setting': 40.0},
                {'sender': 'tender-aaa', 'param': 'vent', 'setting': 3.5},
            ],
            'path': ['tender-aaa', 'tender-ccc'],
        },
    }
    with patch.object(w, '_fetch_wish_plan', return_value=plan), \
            patch.object(w, '_notify_wish_receiver') as notify:
        w._adopt_wish('w-4', role='sender')

    assert w._wish is not None
    assert w._wish['moves'] == {'valve': 0.62, 'vent': 3.5}, \
        "this tender holds exactly its own moves, never another sender's"
    assert 'pump' not in w._wish['moves']
    notify.assert_called_once_with('w-4', 'tender-ccc')

    print("  multi-move plan -> own dials held, other sender's ignored")
    print("  PASS")


def test_adopt_legacy_single_move_without_sender_still_binds():
    print("\n=== test_adopt_legacy_single_move_without_sender_still_binds ===")
    w = _bare_worker(systemtender_id='tender-zzz')
    plan = {'status': 'planned', 'instruction': 'hold',
            'plan': {'moves': [{'param': 'valve', 'setting': 0.7}]}}
    with patch.object(w, '_fetch_wish_plan', return_value=plan):
        w._adopt_wish('w-5', role='sender')
    assert w._wish is not None
    assert w._wish['moves'] == {'valve': 0.7}, \
        "the single-move plan binds by construction (its assignment)"

    print("  legacy single-move plans adopt unchanged")
    print("  PASS")


def test_refresh_replans_one_dial_and_keeps_the_other():
    print("\n=== test_refresh_replans_one_dial_and_keeps_the_other ===")
    w = _bare_worker(systemtender_id='tender-aaa')
    w._wish = {'wish_id': 'w-6',
               'moves': {'valve': 0.62, 'vent': 3.5},
               'instruction': 'hold', 'role': 'sender'}
    plan = {'status': 'serving', 'instruction': 'hold',
            'plan': {'moves': [
                {'sender': 'tender-aaa', 'param': 'valve', 'setting': 0.80},
                {'sender': 'tender-aaa', 'param': 'vent', 'setting': 3.5},
            ]}}
    with patch.object(w, '_fetch_wish_plan', return_value=plan):
        w._refresh_wish_status()

    assert w._wish is not None
    assert w._wish['moves'] == {'valve': 0.80, 'vent': 3.5}, \
        "the moved dial updates in place; the unmoved one keeps its hold"

    print("  per-dial re-plan, no wholesale reset")
    print("  PASS")
