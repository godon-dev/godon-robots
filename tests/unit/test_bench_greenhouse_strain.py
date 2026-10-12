"""Greenhouse strain contract: configuration validation.

The suggestion half of the strain interface died with the optimizer
era (rung 2 deleted the only caller; rung 3 deleted the interface) —
params arrive shaped from the coordinator. What a strain still owns:
its parameter registry, preflight, and validation.
"""

import pytest
import sys
from unittest.mock import MagicMock, patch

from strains.bench_greenhouse.strain import validate_config


class TestValidateConfig:
    def test_delegates_to_preflight(self):
        config = {'settings': {'greenhouse': {'zones': 2}}}
        mock_preflight = MagicMock()
        mock_preflight.main.return_value = {'result': 'SUCCESS'}
        parent_mod = sys.modules['f.systemtender.strains.bench_greenhouse']
        with patch.object(parent_mod, 'preflight', mock_preflight, create=True):
            result = validate_config(config)
            mock_preflight.main.assert_called_once_with(config)
            assert result['result'] == 'SUCCESS'
