"""The seek wish's pure machinery — the sealed §XIV rulings as tests.

Every test names the ruling it pins:
- admission refusals fire at the DOOR (candidates over budget never
  starts, undeclared budget/criterion never starts)
- dice are reproducible, in-bounds, and int-clean; they are launch
  points, nothing more
- the wallet counts feelers and never lies about exhaustion
- ranking follows the DECLARED direction, nothing chosen
- the return is RETURNED even on budget exhaustion — no miss-state
  exists for seeks
"""

import random

import pytest

from engine.seek_wish import (
    COVER_NOW,
    FIND_BEST,
    MAXIMIZE,
    MINIMIZE,
    RETURNED,
    SeekRefused,
    Finding,
    Wallet,
    build_return,
    launch_seed,
    parse_seek_plan,
    rank,
    roll_candidates,
)


def base_plan(**over):
    plan = {
        'wish_id': 'sw-1',
        'form': FIND_BEST,
        'region': {
            'dial_a': (0.0, 100.0, False),
            'dial_b': (1, 8, True),
        },
        'criterion': {'outcome': 'room/objective_1', 'direction': MAXIMIZE},
        'budget': 200,
        'candidates': 5,
    }
    plan.update(over)
    return plan


class TestAdmission:
    """Refusals are admission events — raised before any trial runs."""

    def test_candidates_over_budget_refuses_at_the_door(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(candidates=201, budget=200))
        assert exc.value.reason == 'candidates_exceed_budget'

    def test_undeclared_budget_refuses(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(budget=0))
        assert exc.value.reason == 'undeclared_budget'

    def test_undeclared_criterion_refuses(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(criterion=None))
        assert exc.value.reason == 'undeclared_criterion'

    def test_unknown_direction_refuses(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(
                criterion={'outcome': 'x', 'direction': 'toward'}))
        assert exc.value.reason == 'unknown_direction'

    def test_estate_direction_words_normalize(self):
        # The estate's objectives speak maximize/minimize; the door
        # normalizes both spellings onto the engine's compass words.
        plan = parse_seek_plan(base_plan(
            criterion={'outcome': 'x', 'direction': 'maximize'}))
        assert plan.criterion['direction'] == MAXIMIZE
        plan = parse_seek_plan(base_plan(
            criterion={'outcome': 'x', 'direction': 'minimize'}))
        assert plan.criterion['direction'] == MINIMIZE

    def test_missing_form_refuses_no_omission(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(form=None))
        assert exc.value.reason == 'undeclared_form'

    def test_unknown_form_refuses(self):
        with pytest.raises(SeekRefused):
            parse_seek_plan(base_plan(form='wander'))

    def test_empty_range_refuses_named(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(region={'dial_a': (5.0, 5.0, False),
                                              'dial_b': (1, 8, True)}))
        assert exc.value.reason == 'empty_range'
        assert exc.value.detail == 'dial_a'

    def test_cover_now_admits_without_search_fields(self):
        plan = parse_seek_plan({
            'wish_id': 'sw-2',
            'form': COVER_NOW,
            'region': {'dial_a': (0.0, 100.0, False)},
        })
        assert plan.candidates == 0
        assert plan.budget == 0
        assert plan.criterion is None

    def test_non_integer_int_range_refuses(self):
        with pytest.raises(SeekRefused) as exc:
            parse_seek_plan(base_plan(region={'dial_b': (1.5, 8, True),
                                              'dial_a': (0.0, 1.0, False)}))
        assert exc.value.reason == 'non_integer_range'


class TestLaunchDice:
    """Darts choose the launch point: reproducible, in-bounds, int-clean."""

    def test_dice_are_deterministic_per_wish(self):
        plan = parse_seek_plan(base_plan())
        first = roll_candidates(plan)
        second = roll_candidates(parse_seek_plan(base_plan()))
        assert first == second

    def test_dice_respect_bounds_and_integrity(self):
        plan = parse_seek_plan(base_plan())
        for draw in roll_candidates(plan):
            assert 0.0 <= draw['dial_a'] <= 100.0
            assert isinstance(draw['dial_b'], int)
            assert 1 <= draw['dial_b'] <= 8

    def test_dice_count_matches_declaration(self):
        plan = parse_seek_plan(base_plan(candidates=7))
        assert len(roll_candidates(plan)) == 7

    def test_no_candidates_declared_no_dice(self):
        plan = parse_seek_plan(base_plan(candidates=0))
        assert roll_candidates(plan) == []

    def test_same_seed_same_draws(self):
        plan = parse_seek_plan(base_plan())
        assert roll_candidates(plan, random.Random(42)) == \
            roll_candidates(plan, random.Random(42))

    def test_launch_seed_is_stable_across_processes(self):
        assert launch_seed('sw-1') == launch_seed('sw-1')
        assert launch_seed('sw-1') != launch_seed('sw-2')


class TestWallet:
    """Feelers counted; the wallet never lies about exhaustion."""

    def test_pay_counts_units(self):
        w = Wallet(budget=10)
        w.pay(3)
        assert w.spent == 3
        assert w.remaining == 7

    def test_exhausted_reads_true_at_zero_remaining(self):
        w = Wallet(budget=2)
        w.pay(2)
        assert w.exhausted
        assert w.remaining == 0

    def test_negative_payment_refused(self):
        with pytest.raises(ValueError):
            Wallet(budget=5).pay(-1)


class TestRanking:
    """The compass is declared; ranking only compares numbers."""

    def test_maximize_picks_highest_reading(self):
        findings = [Finding({'a': 1}, 0.5, 0.02),
                    Finding({'a': 2}, 0.9, 0.02),
                    Finding({'a': 3}, 0.1, 0.02)]
        best = rank(findings, {'direction': MAXIMIZE})
        assert best is not None
        assert best.params == {'a': 2}

    def test_minimize_picks_lowest_reading(self):
        findings = [Finding({'a': 1}, 0.5, 0.02),
                    Finding({'a': 2}, 0.9, 0.02)]
        best = rank(findings, {'direction': MINIMIZE})
        assert best is not None
        assert best.params == {'a': 1}

    def test_no_findings_ranks_none(self):
        assert rank([], {'direction': MAXIMIZE}) is None


class TestReturn:
    """Verdict kind RETURNED — no miss-state exists for seeks."""

    def test_budget_exhaustion_still_returns(self):
        w = Wallet(budget=5)
        w.pay(5)
        ret = build_return(Finding({'a': 2}, 0.9, 0.02), w,
                           'budget_exhausted')
        assert ret['verdict'] == RETURNED
        assert ret['stop_reason'] == 'budget_exhausted'
        assert ret['spend_count'] == 5
        assert ret['finding']['reading'] == 0.9

    def test_return_carries_bars_with_the_finding(self):
        ret = build_return(Finding({'a': 2}, 0.9, 0.05), Wallet(5), 'done')
        assert ret['finding']['bars'] == 0.05

    def test_empty_seek_returns_finding_none(self):
        ret = build_return(None, Wallet(budget=5), 'budget_exhausted')
        assert ret['verdict'] == RETURNED
        assert ret['finding'] is None
