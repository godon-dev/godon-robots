
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
"""
Unit tests for SystemtenderMetricsClient

Tests the Prometheus metrics wrapper without requiring actual
Prometheus Push Gateway or Windmill infrastructure.
"""

import sys
import os
from unittest.mock import MagicMock, patch, call
import pytest

# Mock prometheus_client before importing the code under test
sys.modules['prometheus_client'] = MagicMock()

from engine.systemtender_metrics_client import SystemtenderMetricsClient


class TestSystemtenderMetricsClientInitialization:
    """Test SystemtenderMetricsClient initialization and configuration"""

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    def test_initialization_enabled(self, mock_registry, mock_push):
        """Test client initializes correctly when enabled"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance',
            pushgateway_url='http://test-pushgateway:9091'
        )

        assert client.systemtender_id == 'test-systemtender-123'
        assert client.worker_id == 'test-worker-1'
        assert client.systemtender_type == 'linux_performance'
        assert client.pushgateway_url == 'http://test-pushgateway:9091'
        assert client.enabled is True

        # Verify registry was created
        mock_registry.assert_called_once()

    @patch.dict(os.environ, {'PUSH_METRICS_ENABLED': 'false'})
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    def test_initialization_disabled(self, mock_registry):
        """Test client respects PUSH_METRICS_ENABLED=false"""
        import os

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        assert client.enabled is False

        # Registry should not be created when disabled
        mock_registry.assert_not_called()


class TestMetricCreation:
    """Test that Prometheus metrics are created with correct configuration"""

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Gauge')
    @patch('engine.systemtender_metrics_client.Counter')
    @patch('engine.systemtender_metrics_client.Histogram')
    def test_worker_status_metric_created(self, mock_histogram, mock_counter, mock_gauge, mock_registry, mock_push):
        """Test worker status Gauge metric is created"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_gauge_instance = MagicMock()
        mock_gauge.return_value = mock_gauge_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Verify Gauge was created for worker status
        assert mock_gauge.call_count >= 1  # At least one Gauge created

        # Check that worker status gauge was created with correct labels
        gauge_calls = [str(call) for call in mock_gauge.call_args_list]
        assert any('godon_systemtender_worker_status' in str(call) for call in gauge_calls)

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Counter')
    def test_trial_counter_metric_created(self, mock_counter, mock_registry, mock_push):
        """Test trial counter is created"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Verify Counter was created
        counter_calls = [str(call) for call in mock_counter.call_args_list]
        assert any('godon_systemtender_trials_total' in str(call) for call in counter_calls)


class TestMetricMethods:
    """Test that metric methods correctly update metrics"""

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Counter')
    def test_inc_trial(self, mock_counter, mock_registry, mock_push):
        """Test inc_trial increments counter"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_counter_instance = MagicMock()
        mock_counter.return_value = mock_counter_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Call inc_trial
        client.inc_trial('complete', value=0.85)

        # Verify counter was incremented
        mock_counter_instance.labels.assert_called_once()
        mock_counter_instance.labels.return_value.inc.assert_called_once()

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Gauge')
    def test_set_best_value(self, mock_gauge, mock_registry, mock_push):
        """Test set_best_value updates gauge"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_gauge_instance = MagicMock()
        mock_gauge.return_value = mock_gauge_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Set best value
        client.set_best_value(0.123)

        # Verify gauge was set
        mock_gauge_instance.labels.assert_called_once()
        mock_gauge_instance.labels.return_value.set.assert_called_with(0.123)

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Gauge')
    def test_mark_running_stopped(self, mock_gauge, mock_registry, mock_push):
        """Test mark_running and mark_stopped"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_gauge_instance = MagicMock()
        mock_gauge.return_value = mock_gauge_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Mark as running
        client.mark_running()

        # Verify running=1, stopped=0
        assert mock_gauge_instance.labels.call_count == 2  # running and stopped

        # Mark as stopped
        client.mark_stopped()

        # Should have 4 more calls (running=0, stopped=1)
        assert mock_gauge_instance.labels.call_count == 4


class TestPushToGateway:
    """Test that metrics are pushed to Push Gateway correctly"""

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    def test_push_calls_pushgateway(self, mock_registry, mock_push):
        """Test push() calls prometheus push_to_gateway"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance',
            pushgateway_url='http://test-pushgateway:9091'
        )

        # Push metrics
        result = client.push()

        # Verify push_to_gateway was called
        mock_push.assert_called_once_with(
            'http://test-pushgateway:9091',
            job='systemtender_test-systemtender-123',
            registry=mock_registry_instance
        )
        assert result is True

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    def test_push_disabled(self, mock_registry, mock_push):
        """Test push() does nothing when disabled"""
        # Create client with PUSH_METRICS_ENABLED=false via env
        import os
        with patch.dict(os.environ, {'PUSH_METRICS_ENABLED': 'false'}):
            client = SystemtenderMetricsClient(
                systemtender_id='test-systemtender-123',
                worker_id='test-worker-1',
                systemtender_type='linux_performance'
            )

            # Push metrics
            result = client.push()

            # Verify push_to_gateway was NOT called
            mock_push.assert_not_called()
            assert result is False

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    def test_push_handles_errors(self, mock_registry, mock_push):
        """Test push() handles Push Gateway errors gracefully"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        # Simulate Push Gateway error
        mock_push.side_effect = Exception("Connection refused")

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Push should not raise exception
        result = client.push()

        # Should return False on error
        assert result is False


class TestRollbackCounter:
    """Test rollback counter functionality"""

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Counter')
    def test_inc_rollback_success(self, mock_counter, mock_registry, mock_push):
        """Test rollback counter increments for successful rollback"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_counter_instance = MagicMock()
        mock_counter.return_value = mock_counter_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Increment rollback success
        client.inc_rollback('success')

        # Verify counter was incremented
        mock_counter_instance.labels.assert_called_once()
        labels_call = mock_counter_instance.labels.call_args
        assert 'success' in str(labels_call)

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Counter')
    def test_inc_rollback_failed(self, mock_counter, mock_registry, mock_push):
        """Test rollback counter increments for failed rollback"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_counter_instance = MagicMock()
        mock_counter.return_value = mock_counter_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-123',
            worker_id='test-worker-1',
            systemtender_type='linux_performance'
        )

        # Increment rollback failed
        client.inc_rollback('failed')

        # Verify counter was incremented
        mock_counter_instance.labels.assert_called_once()
        labels_call = mock_counter_instance.labels.call_args
        assert 'failed' in str(labels_call)


class TestMetricLabels:
    """Test that metrics have correct labels"""

    @patch('engine.systemtender_metrics_client.push_to_gateway')
    @patch('engine.systemtender_metrics_client.CollectorRegistry')
    @patch('engine.systemtender_metrics_client.Gauge')
    def test_metrics_include_common_labels(self, mock_gauge, mock_registry, mock_push):
        """Test all metrics include systemtender_id, worker_id, systemtender_type labels"""
        mock_registry_instance = MagicMock()
        mock_registry.return_value = mock_registry_instance

        mock_gauge_instance = MagicMock()
        mock_gauge.return_value = mock_gauge_instance

        client = SystemtenderMetricsClient(
            systemtender_id='test-systemtender-abc',
            worker_id='test-worker-xyz',
            systemtender_type='linux_performance'
        )

        # Set best value (triggers labels())
        client.set_best_value(0.5)

        # Verify labels include systemtender_id, worker_id, systemtender_type
        labels_call = mock_gauge_instance.labels.call_args
        args, kwargs = labels_call

        # Check keyword arguments (label names and values)
        assert 'systemtender_id' in kwargs
        assert kwargs['systemtender_id'] == 'test-systemtender-abc'
        assert 'worker_id' in kwargs
        assert kwargs['worker_id'] == 'test-worker-xyz'
        assert 'systemtender_type' in kwargs
        assert kwargs['systemtender_type'] == 'linux_performance'


if __name__ == '__main__':
    pytest.main([__file__, '-v'])
