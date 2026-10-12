"""The ledger's own tests — three classes the study era could not have.

1. classify(): the pinned reader contract as a pure function — every
   row of the mapping the rust reader will bucket by.
2. TrialRef: the stamp delegation.
3. The golden fixture (requires a real postgres, LEDGER_TEST_DSN): a
   representative trial run written through the real module, dumped in
   canonical form, compared byte-for-byte against the committed
   fixture. The rust readers in godon-images test against the SAME
   artifact — writer and readers agree or CI is red somewhere.
"""

import json
import os
import pytest
from unittest.mock import MagicMock

from engine.trial_ledger import TrialLedger, TrialRef, classify


class TestClassify:
    """The pinned mapping — causal's from_trials, written once."""

    @pytest.mark.parametrize("phase,expected", [
        ("push", "push"),
        ("probe_push", "push"),          # the rename that starved a reader
        ("pause", "pause"),
        ("probe_pause", "pause"),
        ("hold_calib", "hold_calib"),
    ])
    def test_phase_trials(self, phase, expected):
        assert classify("impulse", phase, "probe_push") == expected

    def test_receiver_hold(self):
        assert classify("hold", "hold", "idle") == "hold"

    def test_own_hold_calib_is_not_receiver_hold(self):
        assert classify("hold", "hold_calib", "hold_calib") == "hold_calib"

    def test_mode_passthrough(self):
        assert classify("park", None, "idle") == "park"
        assert classify("idle", "", "idle") == "idle"

    def test_unknown_falls_back(self):
        assert classify("", None, "") == "unclassified"


class TestTrialRef:
    def test_stamp_delegates_to_ledger(self):
        ledger = MagicMock()
        ref = TrialRef(ledger, 7)
        ref.set_user_attr("coord_state", "idle")
        ledger.stamp.assert_called_once_with(7, "coord_state", "idle")

    def test_number_exposed(self):
        assert TrialRef(MagicMock(), 7).number == 7


DSN = os.environ.get("LEDGER_TEST_DSN")


def _canonical_rows(tl, conn_kwargs):
    psycopg2 = tl.psycopg2
    extras = tl.psycopg2.extras
    out = {}
    conn = psycopg2.connect(**conn_kwargs)
    with conn.cursor(cursor_factory=extras.RealDictCursor) as cur:
        cur.execute("SELECT number, kind, params, value, state, error, attrs "
                    "FROM trials ORDER BY number")
        rows = []
        for r in cur.fetchall():
            r = dict(r)
            for k in ("params", "attrs"):
                v = r[k]
                r[k] = v if isinstance(v, str) else json.dumps(v, sort_keys=True)
            rows.append(r)
        out["trials"] = rows
        cur.execute("SELECT key, value_json FROM ledger_state ORDER BY key")
        state = []
        for r in cur.fetchall():
            r = dict(r)
            v = r["value_json"]
            r["value_json"] = v if isinstance(v, str) else json.dumps(v, sort_keys=True)
            state.append(r)
        out["ledger_state"] = state
    conn.close()
    return out


@pytest.mark.skipif(not DSN, reason="LEDGER_TEST_DSN not set")
class TestGoldenFixture:
    """The writer side of the cross-language contract. The suite mocks
    psycopg2 globally (conftest); this test swaps the real module in,
    reloads the ledger against it, and restores the mock after."""

    def _real_ledger(self):
        import importlib
        import sys
        saved = {k: sys.modules.get(k) for k in
                 ('psycopg2', 'psycopg2.extras', 'psycopg2.extensions')}
        for k in list(saved):
            sys.modules.pop(k, None)
        import psycopg2  # noqa: F401 — the real one
        import psycopg2.extras  # noqa: F401
        import engine.trial_ledger as tl
        importlib.reload(tl)
        return tl, saved

    def _restore(self, saved):
        import importlib
        import sys
        import engine.trial_ledger as tl
        for k, v in saved.items():
            if v is not None:
                sys.modules[k] = v
            else:
                sys.modules.pop(k, None)
        importlib.reload(tl)

    def test_golden_roundtrip(self):
        from urllib.parse import urlparse
        u = urlparse(DSN)
        tl, saved = self._real_ledger()
        try:
            ledger = tl.TrialLedger(DSN)
            ledger.ensure_schema()
            import psycopg2
            conn = psycopg2.connect(DSN)
            with conn.cursor() as cur:
                cur.execute("TRUNCATE trials RESTART IDENTITY")
                cur.execute("TRUNCATE ledger_state")
            conn.commit()
            conn.close()

            # A representative trial: the stamps the worker writes,
            # pre-serialized JSON exactly as the call sites do it.
            n = ledger.begin()
            ledger.set_kind(n, tl.classify("impulse", "probe_push", "probe_push"))
            ref = tl.TrialRef(ledger, n)
            ref.set_user_attr("detection_mode", "impulse")
            ref.set_user_attr("coord_state", "probe_push")
            ref.set_user_attr("impulse_phase", "probe_push")
            ref.set_user_attr("impulse_scale", 1.5)
            ref.set_user_attr("probe_param", "param_0")
            ref.set_user_attr("probe_level", 75.0)
            ref.set_user_attr("observations", json.dumps([0.51, 0.49]))
            ref.set_user_attr("effectuation_params", json.dumps({"param_0": 75.0}))
            ledger.complete(n, 0.512)

            # A failed one: park without neutral params.
            n2 = ledger.begin()
            ledger.set_kind(n2, tl.classify("hold", "hold", "idle"))
            ledger.fail(n2, "park without neutral params")

            # State machine on the same connection pattern.
            ledger.state_put("rollback_state_target_0", json.dumps(
                {"state": "normal", "version": 1}, sort_keys=True))
            assert json.loads(ledger.state_get("rollback_state_target_0"))["version"] == 1

            got = _canonical_rows(tl, dict(
                host=u.hostname, port=u.port or 5432,
                user=u.username, password=u.password,
                dbname=u.path.lstrip("/")))
        finally:
            self._restore(saved)

        fixture = json.dumps(got, indent=1, sort_keys=True)

        here = os.path.dirname(__file__)
        path = os.path.join(here, "fixtures", "ledger_golden.json")
        if os.environ.get("UPDATE_FIXTURE"):
            open(path, "w").write(fixture + "\n")
        expected = open(path).read().strip()
        assert fixture == expected, (
            "golden fixture drift — regenerate with UPDATE_FIXTURE=1 "
            "and copy the artifact to the rust readers' tests")
