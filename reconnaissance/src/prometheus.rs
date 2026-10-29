//! Port of `reconnaissance/prometheus.py` — Prometheus
//! reconnaissance: scalar PromQL queries per objective/guardrail,
//! stabilization wait, multi-sampling with aggregation, the
//! exponential-backoff retry, and NaN handled as a null
//! sample. Statement-by-statement against the python original; the
//! tests mirror `tests/unit/test_reconnaissance_logic.py`.
//!
//! The connection trait ([`PromConnect`]) ports the script's TESTED
//! contract: `custom_query` answers the scalar result
//! `{resultType: "scalar", result: [ts, "value" | "NaN"]}` — the
//! shape the script's own code reads and checks. The real transport
//! ([`ReqwestPromConnect`]) ports the python client's construction:
//! a plain HTTP GET at `{url}/api/v1/query` (the port-plan ruling:
//! the prometheus python client → plain HTTP queries), the urllib3
//! Retry(total=3, backoff 0.5) the python built into every
//! connection, `raise_on_status` (non-2xx is a retryable error), and
//! an API-level `status: "error"` body mapping to the client's
//! exception class (retryable at the script's retry ladder). The
//! result returned is the API body's `data` object.

use serde_json::{json, Map, Value};

/// The exception classes the script's retry ladder caught, versus
/// everything else (which raised through).
#[derive(Clone)]
pub enum PromErr {
    Retryable(String),
    Fatal(String),
}

/// The connection trait — the python tests patched the whole
/// `custom_query`; the rust tests fake this surface.
pub trait PromConnect: Send + Sync {
    fn custom_query(&self, url: &str, query: &str) -> Result<Value, PromErr>;
}

/// The production edge: one GET per query, the urllib3-style retry
/// ladder inside (3 retries, 0.5s backoff doubling — sleeps 0.5,
/// 1.0, 2.0 between the four attempts).
pub struct ReqwestPromConnect;

impl ReqwestPromConnect {
    fn one_query(url: &str, query: &str) -> Result<Value, PromErr> {
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| PromErr::Fatal(e.to_string()))?;
        let resp = client
            .get(format!("{}/api/v1/query", url.trim_end_matches('/')))
            .query(&("query", query))
            .send()
            .map_err(|e| PromErr::Retryable(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // raise_on_status=True — the status error is retry-class
            return Err(PromErr::Retryable(format!("HTTP {status}")));
        }
        let text = resp.text().map_err(|e| PromErr::Retryable(e.to_string()))?;
        let body: Value = serde_json::from_str(&text).map_err(|e| PromErr::Fatal(e.to_string()))?;
        if body.get("status").and_then(|s| s.as_str()) == Some("error") {
            // the python client raised PrometheusApiClientException
            // on API-level errors — retryable at the script's ladder
            return Err(PromErr::Retryable(
                body.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("prometheus api error")
                    .to_string(),
            ));
        }
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    }
}

impl PromConnect for ReqwestPromConnect {
    fn custom_query(&self, url: &str, query: &str) -> Result<Value, PromErr> {
        // urllib3 Retry(total=3, backoff_factor=0.5): the original
        // attempt plus three retries
        let mut last: Option<String> = None;
        for attempt in 0..4u32 {
            match Self::one_query(url, query) {
                Ok(v) => return Ok(v),
                Err(PromErr::Retryable(e)) => {
                    if attempt < 3 {
                        let delay = 0.5 * (2u32.pow(attempt) as f64);
                        std::thread::sleep(std::time::Duration::from_secs_f64(delay));
                    }
                    last = Some(e);
                }
                Err(fatal) => return Err(fatal),
            }
        }
        Err(PromErr::Retryable(
            last.unwrap_or_else(|| "prometheus query failed".into()),
        ))
    }
}

/// `extract_scalar_value` — the second element of the scalar result;
/// "NaN" and null read as None; anything else parses as float.
pub fn extract_scalar_value(query_result: &Value) -> Result<Option<f64>, String> {
    let empty = vec![];
    let result = query_result
        .get("result")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty);
    if result.len() < 2 {
        return Err(format!("Invalid scalar result format: {query_result}"));
    }
    let value = &result[1];
    if value == "NaN" || value.is_null() {
        return Ok(None);
    }
    match value {
        Value::Number(n) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("could not convert {value} to float")),
        Value::String(s) => s
            .parse::<f64>()
            .map(Some)
            .map_err(|_| format!("could not convert '{s}' to float")),
        other => Err(format!("could not convert {other} to float")),
    }
}

/// python `statistics.median` — mean of the middle two on evens.
fn median(mut vals: Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = vals.len();
    if n % 2 == 1 {
        vals[n / 2]
    } else {
        (vals[n / 2 - 1] + vals[n / 2]) / 2.0
    }
}

/// `aggregate_samples` — duplicated from the http component by the
/// python original's own hand (both files carry the same function);
/// kept duplicated for the same reason.
pub fn aggregate_samples(samples: &[Option<f64>], method: &str) -> f64 {
    let valid: Vec<f64> = samples
        .iter()
        .filter_map(|s| s.filter(|v| v.is_finite()))
        .collect();
    if valid.is_empty() {
        return f64::INFINITY;
    }
    match method {
        "median" => median(valid),
        "mean" => valid.iter().sum::<f64>() / valid.len() as f64,
        "min" => valid.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => valid.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        _ => median(valid),
    }
}

/// `prometheus_query_with_retry` — the script's own ladder over the
/// connection: three attempts, initial delay five seconds, doubling.
pub fn prometheus_query_with_retry(
    conn: &dyn PromConnect,
    sleep: &mut dyn FnMut(f64),
    url: &str,
    query: &str,
    max_retries: u32,
    initial_delay: f64,
) -> Result<Value, String> {
    let mut last_exception: Option<String> = None;
    for attempt in 0..max_retries {
        match conn.custom_query(url, query) {
            Ok(v) => return Ok(v),
            Err(PromErr::Retryable(e)) => {
                if attempt < max_retries - 1 {
                    let delay = initial_delay * (2u32.pow(attempt) as f64);
                    log::info!("Query failed (attempt {attempt}/{max_retries}): {e}. Retrying in {delay}s...");
                    sleep(delay);
                } else {
                    log::info!("Query failed after {max_retries} retries: {e}");
                }
                last_exception = Some(e);
            }
            Err(PromErr::Fatal(e)) => {
                log::info!("Non-retryable query error: {e}");
                return Err(e);
            }
        }
    }
    Err(format!(
        "Prometheus query failed after {max_retries} retries: {}",
        last_exception.unwrap_or_default()
    ))
}

/// python `range(count)` semantics — see the http component's helper.
fn sample_count(raw: Option<&Value>, default: u64) -> Result<u64, String> {
    match raw {
        None => Ok(default),
        Some(v) => {
            if let Some(i) = v.as_i64() {
                Ok(i.max(0) as u64)
            } else if v.is_u64() {
                Ok(v.as_u64().unwrap())
            } else {
                Err(format!("sample count must be an integer, got {v}"))
            }
        }
    }
}

/// `_gather_single_metric` — the scalar query ladder verbatim: the
/// long stabilization wait (120s default), the sample count with
/// intervals, the scalar-type check, NaN as a null sample, and the
/// aggregation. Failures land as inf, exactly as the python did.
pub fn gather_single_metric(
    conn: &dyn PromConnect,
    sleep: &mut dyn FnMut(f64),
    url: &str,
    metric_name: &str,
    recon_config: &Value,
) -> f64 {
    let recon_service = recon_config.get("service").and_then(|s| s.as_str());
    if recon_service != Some("prometheus") {
        log::info!("Unsupported reconnaissance service: {recon_service:?}");
        return f64::INFINITY;
    }
    match gather_inner(conn, sleep, url, metric_name, recon_config) {
        Ok(v) => v,
        Err(e) => {
            log::info!("Failed to gather metric {metric_name}: {e}");
            f64::INFINITY
        }
    }
}

fn gather_inner(
    conn: &dyn PromConnect,
    sleep: &mut dyn FnMut(f64),
    url: &str,
    metric_name: &str,
    recon_config: &Value,
) -> Result<f64, String> {
    let recon_query = recon_config
        .get("query")
        .and_then(|q| q.as_str())
        .ok_or_else(|| "no query in reconnaissance config".to_string())?;

    let stabilization_seconds = recon_config
        .get("stabilization_seconds")
        .and_then(|s| s.as_f64())
        .unwrap_or(120.0);
    let samples = sample_count(recon_config.get("samples"), 1)?;
    let interval = recon_config
        .get("interval")
        .and_then(|s| s.as_f64())
        .unwrap_or(0.0);

    if stabilization_seconds > 0.0 {
        log::info!("Waiting {stabilization_seconds}s for network stabilization");
        sleep(stabilization_seconds);
        log::info!("Stabilization period completed");
    }
    log::info!("Collecting {samples} samples with {interval}s interval");

    let mut sample_values: Vec<Option<f64>> = Vec::new();
    for i in 0..samples {
        let query_result = prometheus_query_with_retry(conn, sleep, url, recon_query, 3, 5.0)?;
        if query_result.get("resultType").and_then(|t| t.as_str()) != Some("scalar") {
            return Err(format!(
                "Query must return scalar result, got: {}",
                query_result
                    .get("resultType")
                    .cloned()
                    .unwrap_or(Value::Null)
            ));
        }
        let value = extract_scalar_value(&query_result)?;
        sample_values.push(value);
        if i + 1 < samples && interval > 0.0 {
            sleep(interval);
        }
    }

    let aggregation_method = recon_config
        .get("aggregation")
        .and_then(|a| a.as_str())
        .unwrap_or("median");
    let final_value = aggregate_samples(&sample_values, aggregation_method);
    if final_value == f64::INFINITY {
        log::info!("All samples returned NaN for {metric_name}");
        Ok(f64::INFINITY)
    } else {
        let valid = sample_values.iter().filter(|s| s.is_some()).count();
        log::debug!(
            "Metric {metric_name}: {final_value} (using {aggregation_method} of {valid} samples)"
        );
        Ok(final_value)
    }
}

fn number_or_null(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// `main` — objectives then guardrails; NO observations section and
/// no noise readout in this component (the python asymmetry, kept
/// verbatim). Returns `{status, metrics}`.
pub fn main(
    context: &Value,
    _targets: &Value,
    _settings: Option<&Value>,
    conn: &dyn PromConnect,
    sleep: &mut dyn FnMut(f64),
) -> Value {
    log::info!("Starting reconnaissance via Prometheus");
    let prometheus_url = context
        .get("reconnaissance")
        .and_then(|r| r.get("prometheus"))
        .and_then(|p| p.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("http://localhost:9090");
    log::info!("Default Prometheus URL: {prometheus_url}");

    let empty = Vec::new();
    let mut metric_data = Map::new();

    for section in ["objectives", "guardrails"] {
        let items = context
            .get(section)
            .and_then(|s| s.as_array())
            .unwrap_or(&empty);
        for item in items {
            let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                log::info!("Unnamed {section} entry skipped (no name)");
                continue;
            };
            log::info!("Gathering {section} metric: {name}");
            let recon_config = item.get("reconnaissance").cloned().unwrap_or(json!({}));
            let entry_url = recon_config
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or(prometheus_url);
            if recon_config.get("url").is_some() {
                log::info!("Using per-entry Prometheus URL: {entry_url}");
            }
            let value = gather_single_metric(conn, sleep, entry_url, name, &recon_config);
            metric_data.insert(name.to_string(), number_or_null(value));
        }
    }

    log::info!(
        "Reconnaissance completed with {} metrics",
        metric_data.len()
    );
    json!({
        "status": "completed",
        "metrics": Value::Object(metric_data),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeConn {
        responses: Mutex<VecDeque<Result<Value, PromErr>>>,
        urls: Mutex<Vec<String>>,
        queries: Mutex<Vec<String>>,
    }

    impl FakeConn {
        fn new(responses: Vec<Result<Value, PromErr>>) -> Self {
            FakeConn {
                responses: Mutex::new(responses.into()),
                urls: Mutex::new(Vec::new()),
                queries: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.queries.lock().unwrap().len()
        }
    }

    impl PromConnect for FakeConn {
        fn custom_query(&self, url: &str, query: &str) -> Result<Value, PromErr> {
            self.urls.lock().unwrap().push(url.to_string());
            self.queries.lock().unwrap().push(query.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(PromErr::Fatal("no more responses".into())))
        }
    }

    fn scalar(v: &str) -> Result<Value, PromErr> {
        Ok(json!({"resultType": "scalar", "result": [1234567890, v]}))
    }
    fn conn_err() -> Result<Value, PromErr> {
        Err(PromErr::Retryable("refused".into()))
    }
    fn no_sleep(_: f64) {}

    fn cfg(query: &str) -> Value {
        json!({
            "service": "prometheus",
            "query": query,
            "stabilization_seconds": 0,
            "samples": 1,
            "interval": 0,
            "aggregation": "median",
        })
    }

    // ─── extract_scalar_value ────────────────────────────────────

    #[test]
    fn extract_valid_scalar() {
        let q = json!({"resultType": "scalar", "result": [1234567890, "42.5"]});
        assert_eq!(extract_scalar_value(&q).unwrap(), Some(42.5));
    }

    #[test]
    fn extract_nan_value() {
        let q = json!({"resultType": "scalar", "result": [1234567890, "NaN"]});
        assert_eq!(extract_scalar_value(&q).unwrap(), None);
    }

    #[test]
    fn extract_invalid_result_format_empty() {
        let q = json!({"resultType": "scalar", "result": []});
        let err = extract_scalar_value(&q).unwrap_err();
        assert!(err.contains("Invalid scalar result format"), "{err}");
    }

    // ─── aggregate_samples ───────────────────────────────────────

    #[test]
    fn aggregate_median_filters_outliers() {
        assert_eq!(
            aggregate_samples(
                &[Some(10.0), Some(11.0), Some(12.0), Some(100.0), Some(9.0)],
                "median"
            ),
            11.0
        );
    }

    #[test]
    fn aggregate_filters_none_values() {
        assert_eq!(
            aggregate_samples(&[Some(10.0), None, Some(12.0), None, Some(11.0)], "median"),
            11.0
        );
    }

    #[test]
    fn aggregate_all_none_returns_inf() {
        assert_eq!(
            aggregate_samples(&[None, None, None], "median"),
            f64::INFINITY
        );
    }

    // ─── gather_single_metric ────────────────────────────────────

    #[test]
    fn gather_single_sample_no_stabilization() {
        let c = FakeConn::new(vec![scalar("42.5")]);
        let out = gather_single_metric(
            &c,
            &mut no_sleep,
            "http://prom:9090",
            "test_metric",
            &cfg("rate(http_requests_total[5m])"),
        );
        assert_eq!(out, 42.5);
        assert_eq!(c.call_count(), 1);
    }

    #[test]
    fn gather_with_stabilization_wait() {
        let c = FakeConn::new(vec![scalar("100.0")]);
        let mut slept = Vec::new();
        let conf = json!({
            "service": "prometheus", "query": "rate(cpu_usage[5m])",
            "stabilization_seconds": 30, "samples": 1, "interval": 0, "aggregation": "median",
        });
        let out = gather_single_metric(&c, &mut |s| slept.push(s), "http://p", "cpu_metric", &conf);
        assert!(slept.contains(&30.0));
        assert_eq!(out, 100.0);
    }

    #[test]
    fn gather_multiple_samples_with_interval() {
        let c = FakeConn::new(vec![scalar("50.0"), scalar("50.0"), scalar("50.0")]);
        let mut slept = Vec::new();
        let conf = json!({
            "service": "prometheus", "query": "rate(memory_usage[5m])",
            "stabilization_seconds": 0, "samples": 3, "interval": 5, "aggregation": "median",
        });
        let out = gather_single_metric(
            &c,
            &mut |s| slept.push(s),
            "http://p",
            "memory_metric",
            &conf,
        );
        assert_eq!(c.call_count(), 3);
        assert_eq!(slept, vec![5.0, 5.0]);
        assert_eq!(out, 50.0);
    }

    #[test]
    fn gather_with_nan_samples_aggregates_the_rest() {
        let c = FakeConn::new(vec![scalar("NaN"), scalar("100.0"), scalar("200.0")]);
        let conf = json!({
            "service": "prometheus", "query": "rate(metric[5m])",
            "stabilization_seconds": 0, "samples": 3, "interval": 0, "aggregation": "median",
        });
        let out = gather_single_metric(&c, &mut no_sleep, "http://p", "test_metric", &conf);
        assert_eq!(out, 150.0); // median of [100.0, 200.0]
    }

    #[test]
    fn gather_all_nan_returns_inf() {
        let c = FakeConn::new(vec![scalar("NaN"), scalar("NaN"), scalar("NaN")]);
        let conf = cfg("rate(metric[5m])");
        let out = gather_single_metric(&c, &mut no_sleep, "http://p", "test_metric", &conf);
        assert_eq!(out, f64::INFINITY);
    }

    #[test]
    fn gather_query_failure_returns_inf() {
        let c = FakeConn::new(vec![conn_err(), conn_err(), conn_err()]);
        let out = gather_single_metric(
            &c,
            &mut no_sleep,
            "http://p",
            "test_metric",
            &cfg("rate(metric[5m])"),
        );
        assert_eq!(out, f64::INFINITY);
    }

    #[test]
    fn gather_unsupported_service_returns_inf() {
        let c = FakeConn::new(vec![]);
        let conf = json!({"service": "unsupported_service", "query": "some query"});
        let out = gather_single_metric(&c, &mut no_sleep, "http://p", "test_metric", &conf);
        assert_eq!(out, f64::INFINITY);
        assert_eq!(c.call_count(), 0);
    }

    #[test]
    fn gather_non_scalar_result_fails_to_inf() {
        let c = FakeConn::new(vec![Ok(json!({"resultType": "vector", "result": []}))]);
        let out = gather_single_metric(&c, &mut no_sleep, "http://p", "m", &cfg("up"));
        assert_eq!(out, f64::INFINITY);
    }

    // ─── prometheus_query_with_retry ─────────────────────────────

    #[test]
    fn query_retry_succeeds_first_attempt() {
        let c = FakeConn::new(vec![scalar("42.0")]);
        let out = prometheus_query_with_retry(&c, &mut no_sleep, "http://p", "up", 3, 5.0).unwrap();
        assert_eq!(out["result"][1], "42.0");
        assert_eq!(c.queries.lock().unwrap()[0], "up");
    }

    #[test]
    fn query_retry_retries_connection_error() {
        let c = FakeConn::new(vec![conn_err(), scalar("42.0")]);
        let mut slept = Vec::new();
        let out = prometheus_query_with_retry(&c, &mut |s| slept.push(s), "http://p", "up", 3, 1.0)
            .unwrap();
        assert_eq!(out["result"][1], "42.0");
        assert_eq!(c.call_count(), 2);
        assert_eq!(slept, vec![1.0]);
    }

    #[test]
    fn query_retry_raises_after_exhausted() {
        let c = FakeConn::new(vec![conn_err(), conn_err()]);
        let err =
            prometheus_query_with_retry(&c, &mut no_sleep, "http://p", "up", 2, 1.0).unwrap_err();
        assert!(err.contains("failed after 2 retries"), "{err}");
    }

    // ─── main ────────────────────────────────────────────────────

    #[test]
    fn main_gathers_objective_metrics() {
        let c = FakeConn::new(vec![scalar("42.5")]);
        let context = json!({
            "reconnaissance": {"prometheus": {"url": "http://prom:9090"}},
            "objectives": [{
                "name": "throughput",
                "reconnaissance": {"service": "prometheus", "query": "rate(http_total[5m])"}
            }],
            "guardrails": []
        });
        let out = main(&context, &json!([]), None, &c, &mut no_sleep);
        assert_eq!(out["status"], "completed");
        assert_eq!(out["metrics"]["throughput"], 42.5);
        assert_eq!(c.urls.lock().unwrap()[0], "http://prom:9090");
    }

    #[test]
    fn main_empty_objectives_and_guardrails() {
        let c = FakeConn::new(vec![]);
        let context = json!({
            "reconnaissance": {"prometheus": {"url": "http://prom:9090"}},
            "objectives": [],
            "guardrails": []
        });
        let out = main(&context, &json!([]), None, &c, &mut no_sleep);
        assert_eq!(out["status"], "completed");
        assert_eq!(out["metrics"], json!({}));
    }

    #[test]
    fn main_has_no_noise_and_no_observations_sections() {
        // the python asymmetry, kept: this component answers metrics only
        let c = FakeConn::new(vec![scalar("1.0")]);
        let context = json!({
            "objectives": [{"name": "a", "reconnaissance": {"service": "prometheus", "query": "up"}}],
            "guardrails": [],
            "observations": [{"name": "b", "reconnaissance": {"service": "prometheus", "query": "up"}}],
        });
        let out = main(&context, &json!([]), None, &c, &mut no_sleep);
        assert!(out.get("metric_noise").is_none());
        assert!(out["metrics"].as_object().unwrap().contains_key("a"));
        assert!(!out["metrics"].as_object().unwrap().contains_key("b"));
    }
}
