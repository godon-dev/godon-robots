
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
Systemtender Metrics Client

Thin wrapper around prometheus_client for Godon systemtenders.
Simplifies pushing metrics to Prometheus Push Gateway.

Dependencies:
    pip install prometheus_client

Usage:
    from f.systemtender.engine.systemtender_metrics_client import SystemtenderMetricsClient

    metrics = SystemtenderMetricsClient(systemtender_id='abc-123', worker_id='worker_1', systemtender_type='linux_performance')
    metrics.mark_running()
    metrics.inc_trial('complete', value=0.85)
    metrics.push()
"""

import os
from typing import Optional
from prometheus_client import CollectorRegistry, Gauge, Counter, Histogram, push_to_gateway

from f.systemtender.shared.otel_logging import get_logger

logger = get_logger(__name__)


class SystemtenderMetricsClient:

    def __init__(self, systemtender_id: str, worker_id: str, systemtender_type: str,
                 pushgateway_url: Optional[str] = None):
        self.systemtender_id = systemtender_id
        self.worker_id = worker_id
        self.systemtender_type = systemtender_type

        self.enabled = os.getenv("PUSH_METRICS_ENABLED", "true").lower() == "true"
        self.pushgateway_url = pushgateway_url or os.getenv("PUSH_GATEWAY_URL", "http://pushgateway:9091")

        if not self.enabled:
            logger.info("Prometheus metrics pushing disabled via PUSH_METRICS_ENABLED=false")
            return

        self.registry = CollectorRegistry()
        self._init_metrics()

        logger.debug(f"Initialized {self.__class__.__name__} for {systemtender_id}/{worker_id}")

    def _init_metrics(self):
        self._worker_status = Gauge(
            'godon_systemtender_worker_status',
            'Systemtender worker running status',
            ['systemtender_id', 'worker_id', 'systemtender_type', 'status'],
            registry=self.registry
        )

        self._trial_count = Counter(
            'godon_systemtender_trials_total',
            'Total trials executed',
            ['systemtender_id', 'worker_id', 'systemtender_type', 'state'],
            registry=self.registry
        )

        self._best_value = Gauge(
            'godon_systemtender_best_value',
            'Best objective value achieved',
            ['systemtender_id', 'worker_id', 'systemtender_type'],
            registry=self.registry
        )

        self._last_trial_value = Gauge(
            'godon_systemtender_last_trial_value',
            'Most recent trial value',
            ['systemtender_id', 'worker_id', 'systemtender_type'],
            registry=self.registry
        )

        self._total_trials = Gauge(
            'godon_systemtender_total_trials',
            'Total number of trials in study',
            ['systemtender_id', 'worker_id', 'systemtender_type'],
            registry=self.registry
        )

        self._trial_duration = Histogram(
            'godon_systemtender_trial_duration_seconds',
            'Trial execution time',
            ['systemtender_id', 'worker_id', 'systemtender_type'],
            buckets=[1, 5, 10, 30, 60, 120, 300, 600, 1800],
            registry=self.registry
        )

        self._effectuation_count = Counter(
            'godon_systemtender_effectuation_total',
            'Effectuation executions',
            ['systemtender_id', 'worker_id', 'systemtender_type', 'status'],
            registry=self.registry
        )

        self._guardrail_violations = Counter(
            'godon_systemtender_guardrail_violations_total',
            'Safety guardrail violations',
            ['systemtender_id', 'worker_id', 'systemtender_type', 'guardrail_name'],
            registry=self.registry
        )

        self._rollback_count = Counter(
            'godon_systemtender_rollbacks_total',
            'Number of rollbacks performed',
            ['systemtender_id', 'worker_id', 'systemtender_type', 'status'],
            registry=self.registry
        )

        self._trials_shared = Counter(
            'godon_systemtender_trials_shared_total',
            'Trials shared with other systemtenders',
            ['systemtender_id', 'worker_id', 'systemtender_type', 'strategy'],
            registry=self.registry
        )

    def push(self) -> bool:
        if not self.enabled:
            return False

        try:
            push_to_gateway(
                self.pushgateway_url,
                job=f'systemtender_{self.systemtender_id}',
                registry=self.registry
            )
            logger.debug(f"Pushed metrics to {self.pushgateway_url}")
            return True
        except Exception as e:
            logger.warning(f"Failed to push metrics to {self.pushgateway_url}: {e}")
            return False

    def mark_running(self):
        if not self.enabled:
            return
        self._worker_status.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            status='running'
        ).set(1)
        self._worker_status.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            status='stopped'
        ).set(0)

    def mark_stopped(self):
        if not self.enabled:
            return
        self._worker_status.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            status='running'
        ).set(0)
        self._worker_status.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            status='stopped'
        ).set(1)

    def inc_trial(self, state: str, value: Optional[float] = None):
        if not self.enabled:
            return

        self._trial_count.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            state=state
        ).inc()

        if value is not None:
            self._last_trial_value.labels(
                systemtender_id=self.systemtender_id,
                worker_id=self.worker_id,
                systemtender_type=self.systemtender_type
            ).set(value)

    def set_best_value(self, value: float):
        if not self.enabled:
            return
        self._best_value.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type
        ).set(value)

    def set_total_trials(self, count: int):
        if not self.enabled:
            return
        self._total_trials.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type
        ).set(count)

    def observe_trial_duration(self, duration_seconds: float):
        if not self.enabled:
            return
        self._trial_duration.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type
        ).observe(duration_seconds)

    def inc_effectuation(self, status: str):
        if not self.enabled:
            return
        self._effectuation_count.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            status=status
        ).inc()

    def inc_guardrail_violation(self, guardrail_name: str):
        if not self.enabled:
            return
        self._guardrail_violations.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            guardrail_name=guardrail_name
        ).inc()

    def inc_rollback(self, status: str):
        if not self.enabled:
            return
        self._rollback_count.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            status=status
        ).inc()

    def inc_trial_shared(self, strategy: str):
        if not self.enabled:
            return
        self._trials_shared.labels(
            systemtender_id=self.systemtender_id,
            worker_id=self.worker_id,
            systemtender_type=self.systemtender_type,
            strategy=strategy
        ).inc()
