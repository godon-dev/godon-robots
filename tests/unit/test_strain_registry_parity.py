#
# Copyright (c) 2019 Matthias Tafelmeier.
#
# This file is part of godon
#
# godon is free software: you can redistribute it and/or modify
# it under the terms of the GNU Affero General Public License as
# published by the Free Software Foundation, either version 3 of the
# License, or (at your option) any later version.
#
# godon is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU Affero General Public License for more details.
#
# You should have received a copy of the GNU Affero General Public License
# along with this godon. If not, see <http://www.gnu.org/licenses/>.
#
"""Strain registry parity (rung 10, the strains slice).

The golden fixtures carry the dial registries in python insertion
order — entries as [name, definition] arrays so the order rides the
file itself, definition keys sorted. The rust family crates embed the
same files (include_str!), so one fixture serves both suites: this
test holds the python side to the checked-in bytes, the rust side
parses them as its compiled-in data. Generated once via
`python3 tests/unit/test_strain_registry_parity.py`; regenerated with
UPDATE_FIXTURE=1 (the ledger fixture's own motion).
"""
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '../..'))

from strains.bench_greenhouse.parameter_registry import PARAMETER_REGISTRY as _GH
from strains.bench_microgrid.parameter_registry import PARAMETER_REGISTRY as _MG
from strains.linux_performance.parameter_registry import (
    PARAMETER_REGISTRY as _LNX,
    ETHTOOL_PARAMS,
)


def _entries(registry):
    return [[name, dict(defn)] for name, defn in registry.items()]


FIXTURES = {
    "bench_greenhouse": {"entries": _entries(_GH)},
    "bench_microgrid": {"entries": _entries(_MG)},
    "linux_performance": {
        "entries": _entries(_LNX),
        "ethtool_entries": _entries(ETHTOOL_PARAMS),
    },
}


class TestStrainRegistryParity:
    def test_fixtures_match_live_registries(self):
        here = os.path.dirname(__file__)
        for family, payload in sorted(FIXTURES.items()):
            path = os.path.join(here, "fixtures", "strain_registry_%s.json" % family)
            fixture = json.dumps(payload, indent=1, sort_keys=True)
            if os.environ.get("UPDATE_FIXTURE"):
                open(path, "w").write(fixture + "\n")
            expected = open(path).read().strip()
            assert fixture == expected, (
                "%s registry drifted from its golden fixture" % family
            )


if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    for family, payload in sorted(FIXTURES.items()):
        path = os.path.join(here, "fixtures", "strain_registry_%s.json" % family)
        fixture = json.dumps(payload, indent=1, sort_keys=True)
        with open(path, "w") as fh:
            fh.write(fixture + "\n")
        n = len(payload["entries"])
        extra = payload.get("ethtool_entries")
        suffix = " (+%d ethtool)" % len(extra) if extra else ""
        print("%s: %d entries%s -> %s" % (family, n, suffix, path))
