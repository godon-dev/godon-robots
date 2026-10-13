"""The seek wish — declared intent that overrides the walk's priorities.

Two forms, one grammar (design §III/§XIV): cover_now walks a declared
region first; find_best searches it under a budget and RETURNS the best
it found. This module holds the pure machinery — plan validation, the
wallet, the launch dice, ranking, and the return. The worker drives it:
every dart and every pass it commissions is a real trial through the
ledger, so the spend count is a count of real rows.

Sealed rulings this module obeys (§XIV):
- Spend unit is feelers: one trial, one unit. The wallet only counts;
  the driver checks it at seams (round boundaries, after the darts),
  never mid-step.
- Candidates are declared-only, count against the same budget, and a
  plan declaring more candidates than budget refuses at admission —
  an impossible wish never starts.
- The dice choose the LAUNCH POINT only. A joint random setting is
  unattributable (every dial moved at once), so it ranks but cannot
  teach: darts are priors, never map entries. Dice are the joint-space
  counterpart of the walk's farthest-point placement.
- The verdict kind is RETURNED. Budget exhaustion returns best-so-far;
  there is no miss-state for seeks. Refusals are admission events.
"""

import hashlib
import random
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Tuple

COVER_NOW = 'cover_now'
FIND_BEST = 'find_best'
SPECIES = (COVER_NOW, FIND_BEST)

MAXIMIZE = 'max'
MINIMIZE = 'min'
DIRECTIONS = (MAXIMIZE, MINIMIZE)

RETURNED = 'RETURNED'


class SeekRefused(Exception):
    """Admission refusal — named, raised before any trial runs."""

    def __init__(self, reason: str, detail: str = ''):
        super().__init__(f'{reason}: {detail}' if detail else reason)
        self.reason = reason
        self.detail = detail


_DIRECTION_WORDS = {
    'max': MAXIMIZE, 'maximize': MAXIMIZE,
    'min': MINIMIZE, 'minimize': MINIMIZE,
}


@dataclass
class SeekPlan:
    """The admitted shape of a seek wish (design §XIV)."""

    wish_id: str
    form: str
    region: Dict[str, Tuple[float, float, bool]]  # name -> (lo, hi, is_int)
    criterion: Optional[Dict[str, str]] = None     # find_best: outcome + direction
    budget: int = 0                                # feelers; 0 = unbounded (cover_now)
    candidates: int = 0                            # launch dice; declared-only

    def validate(self) -> None:
        if self.form is None:
            raise SeekRefused('undeclared_form', 'the walk speaks its form (no omission)')
        if self.form not in SPECIES:
            raise SeekRefused('unknown_form', self.form)
        if not self.region:
            raise SeekRefused('empty_region')
        for name, (lo, hi, is_int) in self.region.items():
            if hi <= lo:
                raise SeekRefused('empty_range', name)
            if is_int and (int(lo) != lo or int(hi) != hi):
                raise SeekRefused('non_integer_range', name)
        if self.form == FIND_BEST:
            if not self.criterion or self.criterion.get('outcome') is None:
                raise SeekRefused('undeclared_criterion')
            # The estate's objectives speak maximize/minimize; the
            # engine's compass speaks max/min — normalize at the door.
            declared = self.criterion.get('direction')
            normalized = _DIRECTION_WORDS.get(str(declared))
            if normalized is None:
                raise SeekRefused('unknown_direction', str(declared))
            self.criterion['direction'] = normalized
            if self.budget <= 0:
                raise SeekRefused('undeclared_budget')
            if self.candidates < 0:
                raise SeekRefused('negative_candidates')
            # An impossible wish never starts: dice the budget cannot pay
            # for refuse at the door, not mid-flight.
            if self.candidates > self.budget:
                raise SeekRefused('candidates_exceed_budget',
                                  f'{self.candidates} > {self.budget}')


def parse_seek_plan(plan: Dict[str, Any]) -> SeekPlan:
    """Parse and admit a seek plan; raises SeekRefused at the door."""
    region = {}
    for name, spec in (plan.get('region') or {}).items():
        lo, hi = float(spec[0]), float(spec[1])
        is_int = bool(spec[2]) if len(spec) > 2 else False
        region[name] = (lo, hi, is_int)
    criterion = dict(plan.get('criterion')) if plan.get('criterion') else None
    parsed = SeekPlan(
        wish_id=str(plan['wish_id']),
        form=plan['form'],
        region=region,
        criterion=criterion,
        budget=int(plan.get('budget') or 0),
        candidates=int(plan.get('candidates') or 0),
    )
    parsed.validate()
    return parsed


def launch_seed(wish_id: str) -> int:
    """Deterministic per-wish seed — the dart trace is reproducible."""
    digest = hashlib.sha256(wish_id.encode('utf-8')).digest()
    return int.from_bytes(digest[:8], 'big')


def roll_candidates(plan: SeekPlan,
                    rng: Optional[random.Random] = None) -> List[Dict[str, Any]]:
    """N random joint settings — the launch dice.

    Spread, not opinion: nothing chooses these points by a belief about
    where the optimum lives. Each setting moves every dial at once, so
    the reading it produces ranks against other darts but can never
    attribute to a dial — priors, never map entries.
    """
    if plan.candidates <= 0:
        return []
    rng = rng or random.Random(launch_seed(plan.wish_id))
    settings: List[Dict[str, Any]] = []
    for _ in range(plan.candidates):
        draw = {}
        for name, (lo, hi, is_int) in plan.region.items():
            draw[name] = rng.randint(int(lo), int(hi)) if is_int \
                else rng.uniform(lo, hi)
        settings.append(draw)
    return settings


@dataclass
class Finding:
    """One measured candidate: a setting, its reading, its blur."""

    params: Dict[str, Any]
    reading: float
    bars: float


def rank(findings: List[Finding], criterion: Dict[str, str]) -> Optional[Finding]:
    """Best finding under the declared criterion.

    The compass is declared, never chosen: direction comes from the
    wish; this function only compares numbers.
    """
    if not findings:
        return None
    direction = criterion['direction']
    if direction == MAXIMIZE:
        return max(findings, key=lambda f: f.reading)
    return min(findings, key=lambda f: f.reading)


@dataclass
class Wallet:
    """The feeler counter. One trial, one unit; seams are the driver's."""

    budget: int
    spent: int = 0

    def pay(self, units: int = 1) -> None:
        if units < 0:
            raise ValueError('units must be non-negative')
        self.spent += units

    @property
    def remaining(self) -> int:
        return max(0, self.budget - self.spent)

    @property
    def exhausted(self) -> bool:
        return self.remaining <= 0


def build_return(best: Optional[Finding], wallet: Wallet,
                 stop_reason: str) -> Dict[str, Any]:
    """The RETURN — the seek's only verdict kind.

    Budget exhaustion still returns best-so-far; there is no miss-state
    for seeks. The finding carries its own bars so the declarer can
    judge what the answer is worth.
    """
    return {
        'verdict': RETURNED,
        'stop_reason': stop_reason,
        'spend_count': wallet.spent,
        'finding': None if best is None else {
            'params': best.params,
            'reading': best.reading,
            'bars': best.bars,
        },
    }
