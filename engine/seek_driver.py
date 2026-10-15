"""The seek driver — one question per trial, the rung's whole loop.

The worker's trial loop asks this driver what the NEXT trial does; the
driver answers, the worker effectuates and measures, then hands the
reading back. Every dart and every pass level is therefore a real
ledger trial through the ordinary rails — stop flag, wish pulse,
classification — and the spend count is a count of real rows.

The loop (design §XIV, all rulings named where they bite):
- DARTS: one joint random setting per trial until the declared
  candidates are spent. Joint settings are unattributable — they rank
  but cannot teach. The worker classifies them as their OWN kind so no
  reader ever banks a dart as curve ground.
- PASSES: per dial, the injected policy names the next level (the
  unchanged walk machinery — farthest-point placement, gap-price
  steering); the trial pushes that level over the current baseline of
  every other dial. When the policy says the dial is done, the dial's
  best measured level BECOMES the baseline (stepping) and the next
  dial walks from there. Rounds repeat over all dials until the budget
  seam or a round moves nothing (converged).
- SEAMS: the wallet is checked between trials only — a measurement is
  never aborted halfway. Budget exhaustion returns best-so-far.
- cover_now is the same walk with the search fields empty: no darts,
  no criterion, one round — the region walked IS the receipt.
"""

from typing import Any, Callable, Dict, List, Optional, Tuple

from engine.seek_wish import (
    COVER_NOW,
    FIND_BEST,
    MAXIMIZE,
    Finding,
    SeekPlan,
    Wallet,
    build_return,
    rank,
    roll_candidates,
)

# policy(dial, measured_levels) -> next level or None when the dial is
# done — the unchanged walk machinery behind a seam, injected so the
# driver stays pure and the tests stay honest.
Policy = Callable[[str, List[Any]], Optional[Any]]

DARTS = 'darts'
PASSES = 'passes'
DONE = 'done'

STOP_BUDGET = 'budget_exhausted'
STOP_CONVERGED = 'converged'
STOP_REGION = 'region_walked'
STOP_STOPPED = 'stopped'


class SeekDriver:
    """Per-trial state machine. step() issues a commission, observe()
    closes it; the WORKER pays the wallet once per trial (every trial
    the tender runs while the seek holds it is the seek's spend —
    darts and protocol trials alike, one payer, no double count)."""

    def __init__(self, plan: SeekPlan, policy: Policy):
        self._plan = plan
        self._policy = policy
        self._wallet = Wallet(budget=plan.budget)
        self._direction = (plan.criterion or {}).get('direction', MAXIMIZE)

        self._darts: List[Dict[str, Any]] = roll_candidates(plan)
        self._dart_findings: List[Finding] = []
        self._pass_findings: List[Finding] = []

        self._baseline: Dict[str, Any] = {}       # dial -> stepped level
        self._measured: Dict[str, List[Any]] = {}  # dial -> levels asked
        self._dial_readings: Dict[str, List[Tuple[Any, float, float]]] = {}
        self._dials: List[str] = list(plan.region.keys())
        self._dial_idx = 0
        self._pending: Optional[Dict[str, Any]] = None  # the issued commission
        self._phase = DARTS if self._darts else PASSES
        self._rounds_done = 0
        self._stop_reason: Optional[str] = None
        self._last_baseline: Optional[Dict[str, Any]] = None

    # ─── the loop ───────────────────────────────────────────────────

    @property
    def has_open_commission(self) -> bool:
        """A step was issued and its observe is still owed — the worker
        must not ask for the next step until the commissioned probe has
        actually been measured."""
        return self._pending is not None

    @property
    def wallet_exhausted(self) -> bool:
        return self._wallet.exhausted

    def wallet_pay(self, units: int = 1) -> None:
        """The worker pays here, once per trial — the single payer."""
        self._wallet.pay(units)

    @property
    def wallet_status(self) -> dict:
        """Guard (rung 7): read-only live view for the status payload —
        the wallet's numbers. The RETURN carries the same numbers at
        the end of the commission; this is the mid-flight read."""
        return {
            'budget': self._wallet.budget,
            'spent': self._wallet.spent,
            'remaining': self._wallet.remaining,
        }

    def step(self) -> Optional[Dict[str, Any]]:
        """The next commission, or None when the seek is complete.

        A DART is a direct joint push (the worker executes it itself —
        unattributable by design, never protocol). A PASS is a
        COMMISSION: the worker hands (dial, level) to the coordinator's
        probe override and lets the standing protocol execute it —
        full probe shape, observation publishing, curve banking. The
        seek pays walk prices and the map banks the ground.
        """
        if self._phase == DONE:
            return None
        if self._pending is not None:
            raise RuntimeError('observe() owed before the next step()')

        if self._phase == DARTS:
            if not self._darts:
                self._phase = PASSES  # the seam after the dart phase
            elif self._wallet.exhausted:
                self._finish(STOP_BUDGET)
                return None
            else:
                params = self._darts.pop(0)
                self._pending = {'phase': DARTS, 'mode': 'push',
                                 'params': params, 'dart': True}
                return dict(self._pending)

        while self._phase == PASSES:
            if self._dial_idx >= len(self._dials):
                ended = self._end_of_round()
                if ended:
                    return None
            dial = self._dials[self._dial_idx]
            if self._plan.form == FIND_BEST and self._wallet.exhausted:
                self._finish(STOP_BUDGET)
                return None
            level = self._policy(dial, self._measured.get(dial, []))
            if level is None:
                # The dial prices out: step the baseline to its best
                # measured level and move to the next dial.
                self._step_baseline(dial)
                self._dial_idx += 1
                continue
            self._measured.setdefault(dial, []).append(level)
            self._pending = {'phase': PASSES, 'mode': 'commission',
                             'probe_dial': dial, 'probe_level': level,
                             'baseline': dict(self._baseline)}
            return dict(self._pending)
        return None

    def observe(self, reading: float, bars: float = 0.0) -> None:
        """Hand the measured criterion reading for the open commission
        back: a dart ranked (joint params, unattributable), or a pass
        level measured (the commissioned dial at its level, over the
        stepped baseline)."""
        if self._pending is None:
            raise RuntimeError('observe() without an issued step')
        if self._pending['phase'] == DARTS:
            self._dart_findings.append(Finding(
                params=dict(self._pending['params']),
                reading=reading, bars=bars))
        else:
            dial = self._pending['probe_dial']
            params = dict(self._pending.get('baseline') or {})
            params[dial] = self._pending['probe_level']
            self._pass_findings.append(Finding(
                params=params, reading=reading, bars=bars))
            self._dial_readings.setdefault(dial, []).append(
                (self._pending['probe_level'], reading, bars))
        self._pending = None

    def abort(self, reason: str = STOP_STOPPED) -> None:
        """The operator's stop between trials: come home with what's held."""
        self._finish(reason)

    @property
    def criterion_outcome(self) -> Optional[str]:
        """The declared compass's outcome name; None for cover_now."""
        if self._plan.form == COVER_NOW or not self._plan.criterion:
            return None
        return self._plan.criterion.get('outcome')

    def result(self) -> Dict[str, Any]:
        """The RETURN — the seek's only verdict kind (§XIV)."""
        if self._phase != DONE:
            raise RuntimeError('result() before the seek completed')
        best = rank(self._dart_findings + self._pass_findings,
                    {'direction': self._direction}) \
            if self._plan.form == FIND_BEST else None
        return build_return(best, self._wallet,
                            self._stop_reason or STOP_CONVERGED)

    # ─── internals ──────────────────────────────────────────────────

    def _best_level(self, dial: str) -> Optional[Any]:
        """The dial's best measured level under the declared compass."""
        readings = self._dial_readings.get(dial) or []
        if not readings:
            return None
        pick = max if self._direction == MAXIMIZE else min
        best = pick(readings, key=lambda t: t[1])
        return best[0]

    def _step_baseline(self, dial: str) -> None:
        if self._plan.form == COVER_NOW:
            return  # no compass declared — nothing to step by
        level = self._best_level(dial)
        if level is not None:
            self._baseline[dial] = level

    def _end_of_round(self) -> bool:
        """Round seam: pay nothing, decide whether another round runs."""
        if self._plan.form == COVER_NOW:
            self._finish(STOP_REGION)
            return True
        if self._last_baseline is not None and \
                self._baseline == self._last_baseline:
            self._finish(STOP_CONVERGED)  # a round moved nothing
            return True
        if self._wallet.exhausted:
            self._finish(STOP_BUDGET)
            return True
        self._last_baseline = dict(self._baseline)
        self._rounds_done += 1
        self._dial_idx = 0
        self._measured = {d: [] for d in self._dials}  # fresh asks allowed
        return False

    def _finish(self, reason: str) -> None:
        self._stop_reason = reason
        self._phase = DONE
        self._pending = None
