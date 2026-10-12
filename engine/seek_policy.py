"""The seek's level policy — the walk family's placement at the seam.

Farthest-point placement over the seek's region, the same rule
coverage_walk applies (midpoint first, then the farthest candidate,
ties to center): the seek's per-dial passes place levels exactly as
the standing walk does. Stateless at this seam by design — the DRIVER
owns the measured list, so a fresh round re-asks honestly at the new
baseline instead of inheriting a stale placement state.

A dial is done when no candidate clears the resolution floor (floats)
or the enumeration is exhausted (ints). The notebook-steered policy —
gap-price steering, priced retirement, the unchanged WalkPolicy — is
the named follow-on; when it lands, this file is replaced at this one
seam and the driver never knows.
"""

from typing import Any, Callable, Dict, List, Optional, Tuple

Bounds = Dict[str, Tuple[float, float, bool]]


def region_policy(region: Bounds) -> Callable[[str, List[Any]], Optional[Any]]:
    """policy(dial, measured) -> next level or None (dial done)."""

    def policy(dial: str, measured: List[Any]) -> Optional[Any]:
        lo, hi, is_int = region[dial]
        meas = list(measured)
        if is_int:
            cands = [float(v) for v in range(int(round(lo)), int(round(hi)) + 1)
                     if float(v) not in meas]
            if not cands:
                return None
            center = (lo + hi) / 2.0
            return max(cands, key=lambda c: (-abs(c - center), c))

        anchors = sorted(list(meas) + [lo, hi])
        cands = {round(lo, 6), round(hi, 6)}
        for a, b in zip(anchors, anchors[1:]):
            cands.add(round((a + b) / 2.0, 6))
        cands = [c for c in cands if c not in meas]

        def dist(c: float) -> float:
            if not meas:
                return hi - lo
            return min(abs(c - m) for m in meas)

        # The floor bites only once something is measured (the first
        # probe is the midpoint regardless) — coverage_walk's rule.
        if meas:
            floor = (hi - lo) / 4.0
            cands = [c for c in cands if dist(c) >= floor]
        if not cands:
            return None
        center = (lo + hi) / 2.0
        return max(cands, key=lambda c: (dist(c), -abs(c - center), c))

    return policy
