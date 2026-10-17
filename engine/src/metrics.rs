//! Systemtender metrics client — port of
//! `engine/systemtender_metrics_client.py`.
//!
//! The python wrapped `prometheus_client` and pushed to the gateway.
//! The rust body carries the same families (same names, same labels,
//! same histogram buckets) as owned state plus a tiny exposition-text
//! renderer, and pushes over an injected transport — the same trait
//! discipline as the causal HttpTransport. The pushgateway wire
//! protocol is PUT text-exposition on `/metrics/job/{job}`, DELETE for
//! the purge; the python client's `_created` companion series (a
//! prometheus_client artifact) is not emitted — the gateway semantics
//! are identical without it, and nothing in the estate reads those
//! series (the state gauge was retired on the python side already).
//!
//! State gauges live on the pushgateway only as last-announcement
//! fossils (nothing overwrites them when a worker dies hard) — status
//! truth is the controller GET; counters stay here.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The pushgateway edge: method, url, optional text body, timeout.
/// The python used `push_to_gateway`/`urllib.request` directly; the
/// trait is the rust replacement for exactly that edge.
pub trait PushTransport: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> Result<(), String>;
}

/// Production transport: reqwest blocking, no TLS (the estate's
/// pushgateway is in-cluster plain http).
pub struct ReqwestPushTransport;

impl PushTransport for ReqwestPushTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        _timeout: Duration,
    ) -> Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| e.to_string())?;
        let mut req = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?,
            url,
        );
        if let Some(text) = body {
            req = req
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(text.to_string());
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("pushgateway answered {}", resp.status()));
        }
        Ok(())
    }
}

const TRIAL_DURATION_BUCKETS: [f64; 9] = [1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];

#[derive(Default)]
struct Registry {
    trials: BTreeMap<String, f64>,
    best_value: Option<f64>,
    last_trial_value: Option<f64>,
    total_trials: Option<f64>,
    duration_sum: f64,
    duration_count: u64,
    duration_buckets: [u64; TRIAL_DURATION_BUCKETS.len()],
    effectuations: BTreeMap<String, f64>,
    guardrail_violations: BTreeMap<String, f64>,
    rollbacks: BTreeMap<String, f64>,
    trials_shared: BTreeMap<String, f64>,
}

pub struct SystemtenderMetricsClient {
    pub systemtender_id: String,
    pub worker_id: String,
    pub systemtender_type: String,
    pub enabled: bool,
    pub pushgateway_url: String,
    registry: Registry,
    transport: Arc<dyn PushTransport>,
}

impl SystemtenderMetricsClient {
    pub fn new(
        systemtender_id: &str,
        worker_id: &str,
        systemtender_type: &str,
        pushgateway_url: Option<&str>,
        transport: Arc<dyn PushTransport>,
    ) -> Self {
        let enabled = std::env::var("PUSH_METRICS_ENABLED")
            .unwrap_or_else(|_| "true".to_string())
            .to_lowercase()
            == "true";
        SystemtenderMetricsClient {
            systemtender_id: systemtender_id.to_string(),
            worker_id: worker_id.to_string(),
            systemtender_type: systemtender_type.to_string(),
            enabled,
            pushgateway_url: pushgateway_url.map(|s| s.to_string()).unwrap_or_else(|| {
                std::env::var("PUSH_GATEWAY_URL")
                    .unwrap_or_else(|_| "http://pushgateway:9091".to_string())
            }),
            registry: Registry::default(),
            transport,
        }
    }

    /// The pushgateway job group URL for this tender.
    fn job_url(&self) -> String {
        format!(
            "{}/metrics/job/systemtender_{}",
            self.pushgateway_url.trim_end_matches('/'),
            self.systemtender_id
        )
    }

    pub fn push(&mut self) -> bool {
        if !self.enabled {
            return false;
        }
        let body = self.render_exposition();
        match self
            .transport
            .request("PUT", &self.job_url(), Some(&body), Duration::from_secs(10))
        {
            Ok(()) => true,
            Err(e) => {
                log::info!("Failed to push metrics to {}: {e}", self.pushgateway_url);
                false
            }
        }
    }

    /// Best-effort DELETE of this job's pushgateway group — clears
    /// fossils from earlier runs. Counters restart per run anyway, so
    /// the group loses nothing.
    pub fn purge_stale_series(&self) {
        if !self.enabled {
            return;
        }
        if let Err(e) =
            self.transport
                .request("DELETE", &self.job_url(), None, Duration::from_secs(5))
        {
            log::info!("pushgateway group purge skipped: {e}");
        }
    }

    pub fn inc_trial(&mut self, state: &str, value: Option<f64>) {
        if !self.enabled {
            return;
        }
        *self
            .registry
            .trials
            .entry(("state=".to_string() + state).clone())
            .or_insert(0.0) += 1.0;
        if let Some(v) = value {
            self.registry.last_trial_value = Some(v);
        }
    }

    pub fn set_best_value(&mut self, value: f64) {
        if !self.enabled {
            return;
        }
        self.registry.best_value = Some(value);
    }

    pub fn set_total_trials(&mut self, count: i64) {
        if !self.enabled {
            return;
        }
        self.registry.total_trials = Some(count as f64);
    }

    pub fn observe_trial_duration(&mut self, duration_seconds: f64) {
        if !self.enabled {
            return;
        }
        self.registry.duration_sum += duration_seconds;
        self.registry.duration_count += 1;
        // buckets are CUMULATIVE counts (obs ≤ bound), the exposition
        // and the python client's own shape
        for (i, bound) in TRIAL_DURATION_BUCKETS.iter().enumerate() {
            if duration_seconds <= *bound {
                self.registry.duration_buckets[i] += 1;
            }
        }
    }

    pub fn inc_effectuation(&mut self, status: &str) {
        if !self.enabled {
            return;
        }
        *self
            .registry
            .effectuations
            .entry("status=".to_string() + status)
            .or_insert(0.0) += 1.0;
    }

    pub fn inc_guardrail_violation(&mut self, guardrail_name: &str) {
        if !self.enabled {
            return;
        }
        *self
            .registry
            .guardrail_violations
            .entry("guardrail_name=".to_string() + guardrail_name)
            .or_insert(0.0) += 1.0;
    }

    pub fn inc_rollback(&mut self, status: &str) {
        if !self.enabled {
            return;
        }
        *self
            .registry
            .rollbacks
            .entry("status=".to_string() + status)
            .or_insert(0.0) += 1.0;
    }

    pub fn inc_trial_shared(&mut self, strategy: &str) {
        if !self.enabled {
            return;
        }
        *self
            .registry
            .trials_shared
            .entry("strategy=".to_string() + strategy)
            .or_insert(0.0) += 1.0;
    }

    /// The exposition text the push receives. Metric families and
    /// labels mirror the python client's registrations one-to-one.
    fn render_exposition(&self) -> String {
        let common = format!(
            "systemtender_id=\"{}\",worker_id=\"{}\",systemtender_type=\"{}\"",
            self.systemtender_id, self.worker_id, self.systemtender_type
        );
        let mut out = String::new();

        let family = |out: &mut String, name: &str, help: &str, kind: &str| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        };

        family(
            &mut out,
            "godon_systemtender_trials_total",
            "Total trials executed",
            "counter",
        );
        for (state, v) in &self.registry.trials {
            let (label, value) = state.split_once('=').expect("shaped above");
            out.push_str(&format!(
                "godon_systemtender_trials_total{{{common},{label}=\"{value}\"}} {v}\n"
            ));
        }

        family(
            &mut out,
            "godon_systemtender_best_value",
            "Best objective value achieved",
            "gauge",
        );
        if let Some(v) = self.registry.best_value {
            out.push_str(&format!("godon_systemtender_best_value{{{common}}} {v}\n"));
        }

        family(
            &mut out,
            "godon_systemtender_last_trial_value",
            "Most recent trial value",
            "gauge",
        );
        if let Some(v) = self.registry.last_trial_value {
            out.push_str(&format!(
                "godon_systemtender_last_trial_value{{{common}}} {v}\n"
            ));
        }

        family(
            &mut out,
            "godon_systemtender_total_trials",
            "Total number of trials in study",
            "gauge",
        );
        if let Some(v) = self.registry.total_trials {
            out.push_str(&format!(
                "godon_systemtender_total_trials{{{common}}} {v}\n"
            ));
        }

        family(
            &mut out,
            "godon_systemtender_trial_duration_seconds",
            "Trial execution time",
            "histogram",
        );
        for (i, bound) in TRIAL_DURATION_BUCKETS.iter().enumerate() {
            out.push_str(&format!(
                "godon_systemtender_trial_duration_seconds_bucket{{{common},le=\"{bound}\"}} {}\n",
                self.registry.duration_buckets[i]
            ));
        }
        out.push_str(&format!(
            "godon_systemtender_trial_duration_seconds_bucket{{{common},le=\"+Inf\"}} {}\n",
            self.registry.duration_count
        ));
        out.push_str(&format!(
            "godon_systemtender_trial_duration_seconds_sum{{{common}}} {}\n",
            self.registry.duration_sum
        ));
        out.push_str(&format!(
            "godon_systemtender_trial_duration_seconds_count{{{common}}} {}\n",
            self.registry.duration_count
        ));

        family(
            &mut out,
            "godon_systemtender_effectuation_total",
            "Effectuation executions",
            "counter",
        );
        for (state, v) in &self.registry.effectuations {
            let (label, value) = state.split_once('=').expect("shaped above");
            out.push_str(&format!(
                "godon_systemtender_effectuation_total{{{common},{label}=\"{value}\"}} {v}\n"
            ));
        }

        family(
            &mut out,
            "godon_systemtender_guardrail_violations_total",
            "Safety guardrail violations",
            "counter",
        );
        for (state, v) in &self.registry.guardrail_violations {
            let (label, value) = state.split_once('=').expect("shaped above");
            out.push_str(&format!(
                "godon_systemtender_guardrail_violations_total{{{common},{label}=\"{value}\"}} {v}\n"
            ));
        }

        family(
            &mut out,
            "godon_systemtender_rollbacks_total",
            "Number of rollbacks performed",
            "counter",
        );
        for (state, v) in &self.registry.rollbacks {
            let (label, value) = state.split_once('=').expect("shaped above");
            out.push_str(&format!(
                "godon_systemtender_rollbacks_total{{{common},{label}=\"{value}\"}} {v}\n"
            ));
        }

        family(
            &mut out,
            "godon_systemtender_trials_shared_total",
            "Trials shared with other systemtenders",
            "counter",
        );
        for (state, v) in &self.registry.trials_shared {
            let (label, value) = state.split_once('=').expect("shaped above");
            out.push_str(&format!(
                "godon_systemtender_trials_shared_total{{{common},{label}=\"{value}\"}} {v}\n"
            ));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakePush {
        calls: Mutex<Vec<(String, String, Option<String>)>>,
        fail: bool,
    }

    impl PushTransport for FakePush {
        fn request(
            &self,
            method: &str,
            url: &str,
            body: Option<&str>,
            _timeout: Duration,
        ) -> Result<(), String> {
            if self.fail {
                return Err("Connection refused".to_string());
            }
            self.calls.lock().unwrap().push((
                method.to_string(),
                url.to_string(),
                body.map(|b| b.to_string()),
            ));
            Ok(())
        }
    }

    fn client(fail: bool) -> (SystemtenderMetricsClient, Arc<FakePush>) {
        let fake = Arc::new(FakePush {
            calls: Mutex::new(Vec::new()),
            fail,
        });
        (
            SystemtenderMetricsClient::new(
                "test-systemtender-123",
                "test-worker-1",
                "linux_performance",
                Some("http://test-pushgateway:9091"),
                fake.clone(),
            ),
            fake,
        )
    }

    // initialization: enabled by default, url respected
    #[test]
    fn initialization_enabled() {
        let (c, _) = client(false);
        assert!(c.enabled);
        assert_eq!(c.pushgateway_url, "http://test-pushgateway:9091");
        assert_eq!(c.systemtender_id, "test-systemtender-123");
    }

    // PUSH_METRICS_ENABLED=false disables everything
    #[test]
    fn initialization_disabled() {
        std::env::set_var("PUSH_METRICS_ENABLED", "false");
        let (mut c, fake) = client(false);
        assert!(!c.enabled);
        assert!(!c.push());
        c.inc_trial("complete", None);
        assert!(fake.calls.lock().unwrap().is_empty());
        std::env::remove_var("PUSH_METRICS_ENABLED");
    }

    // the state gauge is retired: no worker_status family anywhere
    #[test]
    fn state_gauge_not_rendered() {
        let (mut c, _) = client(false);
        c.inc_trial("complete", None);
        let text = c.render_exposition();
        assert!(!text.contains("godon_systemtender_worker_status"));
    }

    // inc_trial lands in the rendered family with the state label
    #[test]
    fn inc_trial_renders_counter() {
        let (mut c, _) = client(false);
        c.inc_trial("complete", Some(0.85));
        let text = c.render_exposition();
        assert!(text.contains("# TYPE godon_systemtender_trials_total counter"));
        assert!(text.contains("state=\"complete\""));
        assert!(text.contains("godon_systemtender_last_trial_value"));
        assert!(text.contains("0.85"));
    }

    // set_best_value sets the gauge
    #[test]
    fn set_best_value_renders_gauge() {
        let (mut c, _) = client(false);
        c.set_best_value(0.123);
        let text = c.render_exposition();
        assert!(text.contains("godon_systemtender_best_value"));
        assert!(text.contains("0.123"));
    }

    // purge DELETEs the job group; errors are swallowed
    #[test]
    fn purge_stale_series_deletes() {
        let (c, fake) = client(false);
        c.purge_stale_series();
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "DELETE");
        assert!(calls[0].1.contains("systemtender_test-systemtender-123"));

        drop(calls);
        let (dead, dead_fake) = client(true);
        dead.purge_stale_series(); // must not raise
        assert!(dead_fake.calls.lock().unwrap().is_empty());
    }

    // push PUTs the exposition to the job group url
    #[test]
    fn push_calls_pushgateway() {
        let (mut c, fake) = client(false);
        assert!(c.push());
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "PUT");
        assert_eq!(
            calls[0].1,
            "http://test-pushgateway:9091/metrics/job/systemtender_test-systemtender-123"
        );
        assert!(calls[0].2.as_deref().unwrap().contains("# HELP"));
    }

    // push errors are swallowed, return false
    #[test]
    fn push_handles_errors() {
        let (mut c, _) = client(true);
        assert!(!c.push());
    }

    // rollback counters carry the status label
    #[test]
    fn inc_rollback_success_and_failed() {
        let (mut c, _) = client(false);
        c.inc_rollback("success");
        c.inc_rollback("failed");
        let text = c.render_exposition();
        assert!(text.contains("godon_systemtender_rollbacks_total"));
        assert!(text.contains("status=\"success\""));
        assert!(text.contains("status=\"failed\""));
    }

    // every family carries the three common labels
    #[test]
    fn metrics_include_common_labels() {
        let (mut c, _) = client(false);
        c.set_best_value(0.5);
        let text = c.render_exposition();
        assert!(text.contains("systemtender_id=\"test-systemtender-123\""));
        assert!(text.contains("worker_id=\"test-worker-1\""));
        assert!(text.contains("systemtender_type=\"linux_performance\""));
    }

    // histogram buckets are cumulative with the python's bounds
    #[test]
    fn duration_histogram_renders_buckets() {
        let (mut c, _) = client(false);
        c.observe_trial_duration(7.0);
        c.observe_trial_duration(45.0);
        let text = c.render_exposition();
        assert!(text.contains("le=\"5\""));
        assert!(text.contains("le=\"1800\""));
        assert!(text.contains("le=\"+Inf\"} 2"));
        assert!(text.contains("godon_systemtender_trial_duration_seconds_count"));
    }
}
