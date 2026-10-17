//! Port of `reconnaissance/http.py` — generic HTTP reconnaissance:
//! one keyed value per objective/guardrail/observation, sampled with
//! a stabilization wait, an optional adaptive sample ladder (stop
//! early when the coefficient of variation settles), median/mean/
//! min/max aggregation, and an exponential-backoff retry on the GET.
//! Statement-by-statement against the python original; the tests
//! mirror `tests/unit/test_http_reconnaissance.py` case for case.

use serde_json::{json, Map, Value};

/// The HTTP edge. `Retryable` is the python retry set
/// (ConnectionError, Timeout, HTTPError from raise_for_status);
/// everything else — a body that is not JSON — raised through
/// immediately.
#[derive(Clone)]
pub enum ReconHttpErr {
    Retryable(String),
    Fatal(String),
}

/// The transport trait — the python patched `requests.get`; the rust
/// tests fake exactly this surface.
pub trait ReconHttp: Send + Sync {
    fn get_json(&self, url: &str, timeout_secs: f64) -> Result<Value, ReconHttpErr>;
}

/// The production edge: reqwest blocking, no TLS, per-call timeout
/// (the python passed `timeout=` per request).
pub struct ReqwestReconHttp;

impl ReconHttp for ReqwestReconHttp {
    fn get_json(&self, url: &str, timeout_secs: f64) -> Result<Value, ReconHttpErr> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs_f64(timeout_secs))
            .build()
            .map_err(|e| ReconHttpErr::Retryable(e.to_string()))?;
        let resp = client
            .get(url)
            .send()
            .map_err(|e| ReconHttpErr::Retryable(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // raise_for_status → HTTPError → retryable
            return Err(ReconHttpErr::Retryable(format!("HTTP {status}")));
        }
        let body = resp
            .text()
            .map_err(|e| ReconHttpErr::Retryable(e.to_string()))?;
        serde_json::from_str(&body).map_err(|e| ReconHttpErr::Fatal(e.to_string()))
    }
}

/// python `statistics.median` — the mean of the middle two on an
/// even count.
fn median(mut vals: Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = vals.len();
    if n % 2 == 1 {
        vals[n / 2]
    } else {
        (vals[n / 2 - 1] + vals[n / 2]) / 2.0
    }
}

fn mean(vals: &[f64]) -> f64 {
    vals.iter().sum::<f64>() / vals.len() as f64
}

/// python `round(x, 6)` carried through decimal formatting.
fn py_round6(x: f64) -> f64 {
    format!("{x:.6}").parse().unwrap_or(x)
}

/// `number_or_null` — the result-boundary rendering (inf/nan →
/// null); the same convention the worker's reader expects.
fn number_or_null(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// `_http_get_with_retry` — exponential backoff on the retryable
/// class, immediate raise on anything else, the exhaustion message
/// verbatim.
pub fn http_get_with_retry(
    transport: &dyn ReconHttp,
    sleep: &mut dyn FnMut(f64),
    url: &str,
    max_retries: u32,
    initial_delay: f64,
    timeout_secs: f64,
) -> Result<Value, String> {
    let mut last_exception: Option<String> = None;
    for attempt in 0..max_retries {
        match transport.get_json(url, timeout_secs) {
            Ok(v) => return Ok(v),
            Err(ReconHttpErr::Retryable(e)) => {
                if attempt < max_retries - 1 {
                    let delay = initial_delay * (2u32.pow(attempt) as f64);
                    eprintln!("HTTP GET failed (attempt {attempt}/{max_retries}): {e}. Retrying in {delay}s...");
                    sleep(delay);
                } else {
                    eprintln!("HTTP GET failed after {max_retries} retries: {e}");
                }
                last_exception = Some(e);
            }
            Err(ReconHttpErr::Fatal(e)) => {
                eprintln!("Non-retryable HTTP error: {e}");
                return Err(e);
            }
        }
    }
    Err(format!(
        "HTTP GET failed after {max_retries} retries: {}",
        last_exception.unwrap_or_default()
    ))
}

/// `_aggregate_samples` — None and inf drop out; no survivors → inf;
/// an unknown method falls back to median (python's else branch).
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
        "mean" => mean(&valid),
        "min" => valid.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => valid.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        _ => median(valid),
    }
}

/// `_compute_cv` — median-absolute-deviation over the median,
/// normalized; fewer than two finite samples → inf; a zero median
/// normalizes by the mean absolute value instead.
pub fn compute_cv(samples: &[Option<f64>]) -> f64 {
    let finite: Vec<f64> = samples
        .iter()
        .filter_map(|s| s.filter(|v| v.is_finite()))
        .collect();
    if finite.len() < 2 {
        return f64::INFINITY;
    }
    let med = median(finite.clone());
    if med == 0.0 {
        let mean_abs = mean(&finite.iter().map(|v| v.abs()).collect::<Vec<_>>());
        if mean_abs == 0.0 {
            return 0.0;
        }
        let mad = median(finite.iter().map(|v| (v - med).abs()).collect());
        return mad / mean_abs;
    }
    let mad = median(finite.iter().map(|v| (v - med).abs()).collect());
    mad / med.abs()
}

/// `_sprt_sample_stable` — three samples minimum, then the CV test.
pub fn sprt_sample_stable(samples: &[Option<f64>], threshold_cv: f64) -> bool {
    if samples.len() < 3 {
        return false;
    }
    compute_cv(samples) < threshold_cv
}

/// One gathered metric — the python `{'value', 'noise_cv'}` dict.
pub struct GatherOut {
    pub value: f64,
    pub noise_cv: Option<f64>,
}

impl GatherOut {
    fn failed() -> Self {
        GatherOut {
            value: f64::INFINITY,
            noise_cv: None,
        }
    }
}

/// `python float(value)` for the shapes a JSON response carries:
/// numbers as themselves, numeric strings parsed, bools as 1.0/0.0;
/// anything else is the TypeError path.
fn py_float(v: &Value) -> Result<f64, String> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| format!("could not convert {v} to float")),
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| format!("could not convert '{s}' to float")),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        other => Err(format!("could not convert {other} to float")),
    }
}

/// python `range(count)` semantics for the sample counts: an integer
/// JSON number (bool aside), non-negative; anything else is the
/// TypeError the python raised inside the try (the gather fails).
fn sample_count(raw: Option<&Value>, default: u64) -> Result<u64, String> {
    match raw {
        None => Ok(default),
        Some(v) => {
            if let Some(i) = v.as_i64() {
                if i >= 0 {
                    Ok(i as u64)
                } else {
                    Ok(0) // python range(-1) iterates zero times
                }
            } else if v.is_u64() {
                Ok(v.as_u64().unwrap())
            } else {
                Err(format!("sample count must be an integer, got {v}"))
            }
        }
    }
}

/// `_take_sample` — one keyed read; a missing key (or a non-object
/// body) samples None with the warning, exactly as python.
fn take_sample(
    transport: &dyn ReconHttp,
    sleep: &mut dyn FnMut(f64),
    url: &str,
    key: Option<&str>,
    timeout_secs: f64,
    _idx: usize,
) -> Result<Option<f64>, String> {
    let data = http_get_with_retry(transport, sleep, url, 3, 2.0, timeout_secs)?;
    let key = match key {
        Some(k) => k,
        None => {
            // python: `None not in data` → the warning, a None sample
            eprintln!("Key 'None' not found in response. Available keys: none");
            return Ok(None);
        }
    };
    let obj = match data.as_object() {
        Some(o) => o,
        None => {
            // python: `key in list_body` is a membership test → false
            // → the same missing-key warning path
            eprintln!("Key '{key}' not found in response. Available keys: []");
            return Ok(None);
        }
    };
    match obj.get(key) {
        None => {
            let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
            eprintln!("Key '{key}' not found in response. Available keys: {keys:?}");
            Ok(None)
        }
        Some(v) if v.is_null() => Ok(None),
        Some(v) => py_float(v).map(Some),
    }
}

/// `_gather_single_metric` — the sampling ladder verbatim: the
/// stabilization wait, the minimum samples with their interval, the
/// adaptive extension up to max_samples gated on the CV test, the
/// aggregation and the noise readout.
pub fn gather_single_metric(
    transport: &dyn ReconHttp,
    sleep: &mut dyn FnMut(f64),
    base_url: &str,
    metric_name: &str,
    recon_config: &Value,
) -> GatherOut {
    let recon_service = recon_config.get("service").and_then(|s| s.as_str());
    if recon_service != Some("http") {
        eprintln!("Unsupported reconnaissance service: {recon_service:?}");
        return GatherOut::failed();
    }

    let out = gather_inner(transport, sleep, base_url, metric_name, recon_config);
    match out {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to gather metric {metric_name}: {e}");
            GatherOut::failed()
        }
    }
}

fn gather_inner(
    transport: &dyn ReconHttp,
    sleep: &mut dyn FnMut(f64),
    base_url: &str,
    metric_name: &str,
    recon_config: &Value,
) -> Result<GatherOut, String> {
    let path = recon_config
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let key = recon_config.get("key").and_then(|k| k.as_str());
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);

    let stabilization_seconds = recon_config
        .get("stabilization_seconds")
        .and_then(|s| s.as_f64())
        .unwrap_or(2.0);
    let min_samples = sample_count(recon_config.get("samples"), 1)?;
    let max_samples = sample_count(recon_config.get("max_samples"), min_samples)?;
    let interval = recon_config
        .get("interval")
        .and_then(|s| s.as_f64())
        .unwrap_or(0.0);
    let timeout = recon_config
        .get("timeout_seconds")
        .and_then(|s| s.as_f64())
        .unwrap_or(30.0);
    let cv_threshold = recon_config
        .get("cv_threshold")
        .and_then(|s| s.as_f64())
        .unwrap_or(0.05);

    if stabilization_seconds > 0.0 {
        eprintln!("Waiting {stabilization_seconds}s for stabilization");
        sleep(stabilization_seconds);
    }
    eprintln!("Collecting {min_samples}-{max_samples} samples from {url} (key={key:?})");

    let mut sample_values: Vec<Option<f64>> = Vec::new();
    for i in 0..min_samples {
        sample_values.push(take_sample(
            transport, sleep, &url, key, timeout, i as usize,
        )?);
        if i + 1 < min_samples && interval > 0.0 {
            sleep(interval);
        }
    }
    while (sample_values.len() as u64) < max_samples {
        if sprt_sample_stable(&sample_values, cv_threshold) {
            eprintln!(
                "Sample stability reached at {} samples (CV < {cv_threshold})",
                sample_values.len()
            );
            break;
        }
        let idx = sample_values.len();
        sample_values.push(take_sample(transport, sleep, &url, key, timeout, idx)?);
        if interval > 0.0 {
            sleep(interval);
        }
    }

    let aggregation_method = recon_config
        .get("aggregation")
        .and_then(|a| a.as_str())
        .unwrap_or("median");
    let final_value = aggregate_samples(&sample_values, aggregation_method);
    let noise_cv = compute_cv(&sample_values);

    if final_value == f64::INFINITY {
        eprintln!("All samples returned invalid values for {metric_name}");
    } else {
        let valid_count = sample_values.iter().filter(|s| s.is_some()).count();
        eprintln!(
            "Metric {metric_name}: {final_value} (using {aggregation_method} of {valid_count} samples, CV={noise_cv:.4})"
        );
    }

    Ok(GatherOut {
        value: final_value,
        noise_cv: if noise_cv == f64::INFINITY {
            None
        } else {
            Some(py_round6(noise_cv))
        },
    })
}

/// `main` — objectives, guardrails, then watched observations (every
/// trial's readout channels; a name already gathered is skipped).
/// Returns `{status, metrics, metric_noise}` with the failure values
/// rendered through the null boundary.
pub fn main(
    context: &Value,
    _targets: &Value,
    _settings: Option<&Value>,
    transport: &dyn ReconHttp,
    sleep: &mut dyn FnMut(f64),
) -> Value {
    eprintln!("Starting HTTP reconnaissance");
    let global_url = context
        .get("reconnaissance")
        .and_then(|r| r.get("http"))
        .and_then(|h| h.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("http://localhost:8090");
    eprintln!("Default HTTP URL: {global_url}");

    let empty = Vec::new();
    let mut metric_data = Map::new();
    let mut metric_noise = Map::new();

    let sections = ["objectives", "guardrails"];
    for section in sections {
        let items = context
            .get(section)
            .and_then(|s| s.as_array())
            .unwrap_or(&empty);
        for item in items {
            let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                // python keyed the dict with None and died at
                // serialization; the rust component skips with the error
                // line (recorded divergence, crate doc)
                eprintln!("Unnamed {section} entry skipped (no name)");
                continue;
            };
            eprintln!("Gathering {section} metric: {name}");
            let recon_config = item.get("reconnaissance").cloned().unwrap_or(json!({}));
            let base_url = recon_config
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or(global_url);
            if recon_config.get("url").is_some() {
                eprintln!("Using per-entry HTTP URL: {base_url}");
            }
            let result = gather_single_metric(transport, sleep, base_url, name, &recon_config);
            metric_data.insert(name.to_string(), number_or_null(result.value));
            if let Some(cv) = result.noise_cv {
                metric_noise.insert(name.to_string(), number_or_null(cv));
            }
        }
    }

    // Watched observations: read every trial like objectives, but
    // never fed to the optimizer. These are the extra readout
    // channels — receiver rows and per-channel curves are built from
    // whatever metrics land here.
    let observations = context
        .get("observations")
        .and_then(|o| o.as_array())
        .unwrap_or(&empty);
    for observation in observations {
        let Some(obs_name) = observation.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if metric_data.contains_key(obs_name) {
            continue;
        }
        eprintln!("Gathering observation metric: {obs_name}");
        let recon_config = observation
            .get("reconnaissance")
            .cloned()
            .unwrap_or(json!({}));
        let base_url = recon_config
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or(global_url);
        let result = gather_single_metric(transport, sleep, base_url, obs_name, &recon_config);
        metric_data.insert(obs_name.to_string(), number_or_null(result.value));
        if let Some(cv) = result.noise_cv {
            metric_noise.insert(obs_name.to_string(), number_or_null(cv));
        }
    }

    eprintln!(
        "HTTP reconnaissance completed with {} metrics",
        metric_data.len()
    );

    json!({
        "status": "completed",
        "metrics": Value::Object(metric_data),
        "metric_noise": Value::Object(metric_noise),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeHttp {
        responses: Mutex<VecDeque<Result<Value, ReconHttpErr>>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeHttp {
        fn new(responses: Vec<Result<Value, ReconHttpErr>>) -> Self {
            FakeHttp {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl ReconHttp for FakeHttp {
        fn get_json(&self, url: &str, _timeout: f64) -> Result<Value, ReconHttpErr> {
            self.calls.lock().unwrap().push(url.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ReconHttpErr::Fatal("no more responses".into())))
        }
    }

    fn ok(body: Value) -> Result<Value, ReconHttpErr> {
        Ok(body)
    }
    fn conn_err() -> Result<Value, ReconHttpErr> {
        Err(ReconHttpErr::Retryable("refused".into()))
    }

    fn no_sleep(_: f64) {}

    fn cfg(entries: &[(&str, Value)]) -> Value {
        let mut m = Map::new();
        for (k, v) in entries {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    fn recon_cfg(service: &str) -> Value {
        cfg(&[
            ("service", json!(service)),
            ("path", json!("/metrics/json")),
            ("key", json!("growth_rate")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(1)),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
        ])
    }

    // ─── http_get_with_retry ─────────────────────────────────────

    #[test]
    fn get_with_retry_succeeds_first_attempt() {
        let f = FakeHttp::new(vec![ok(json!({"growth_rate": 0.85}))]);
        let out = http_get_with_retry(&f, &mut no_sleep, "http://x/metrics", 3, 2.0, 30.0).unwrap();
        assert_eq!(out["growth_rate"], 0.85);
        assert_eq!(f.call_count(), 1);
    }

    #[test]
    fn get_with_retry_retries_connection_error() {
        let f = FakeHttp::new(vec![conn_err(), ok(json!({"growth_rate": 0.85}))]);
        let mut slept = Vec::new();
        let out =
            http_get_with_retry(&f, &mut |s| slept.push(s), "http://x/m", 3, 1.0, 30.0).unwrap();
        assert_eq!(out["growth_rate"], 0.85);
        assert_eq!(f.call_count(), 2);
        assert_eq!(slept, vec![1.0]); // initial_delay * 2^0
    }

    #[test]
    fn get_with_retry_raises_after_exhausted() {
        let f = FakeHttp::new(vec![conn_err(), conn_err()]);
        let err = http_get_with_retry(&f, &mut no_sleep, "http://x/m", 2, 1.0, 30.0).unwrap_err();
        assert!(err.contains("failed after 2 retries"), "{err}");
    }

    #[test]
    fn get_with_retry_fatal_raises_without_retry() {
        let f = FakeHttp::new(vec![Err(ReconHttpErr::Fatal("bad json".into()))]);
        let err = http_get_with_retry(&f, &mut no_sleep, "http://x/m", 3, 1.0, 30.0).unwrap_err();
        assert_eq!(err, "bad json");
        assert_eq!(f.call_count(), 1);
    }

    // ─── aggregation / cv ────────────────────────────────────────

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
        assert_eq!(aggregate_samples(&[None, None], "median"), f64::INFINITY);
    }

    #[test]
    fn aggregate_mean_min_max() {
        assert_eq!(
            aggregate_samples(&[Some(1.0), Some(2.0), Some(3.0)], "mean"),
            2.0
        );
        assert_eq!(aggregate_samples(&[Some(1.0), Some(2.0)], "min"), 1.0);
        assert_eq!(aggregate_samples(&[Some(1.0), Some(2.0)], "max"), 2.0);
        // unknown method falls back to median
        assert_eq!(
            aggregate_samples(&[Some(1.0), Some(2.0), Some(8.0)], "bogus"),
            2.0
        );
    }

    #[test]
    fn compute_cv_low_is_stable_signal() {
        let cv = compute_cv(&[Some(10.0), Some(10.1), Some(9.9), Some(10.0), Some(10.05)]);
        assert!(cv < 0.02);
    }

    #[test]
    fn compute_cv_single_sample_returns_inf() {
        assert_eq!(compute_cv(&[Some(10.0)]), f64::INFINITY);
    }

    #[test]
    fn sprt_stable_with_three_samples() {
        assert!(sprt_sample_stable(
            &[Some(10.0), Some(10.01), Some(9.99)],
            0.05
        ));
    }

    #[test]
    fn sprt_too_few_samples() {
        assert!(!sprt_sample_stable(&[Some(10.0), Some(10.0)], 0.05));
    }

    // ─── gather_single_metric ────────────────────────────────────

    #[test]
    fn gather_single_sample_no_stabilization() {
        let f = FakeHttp::new(vec![ok(json!({"growth_rate": 0.85}))]);
        let mut slept = Vec::new();
        let out = gather_single_metric(
            &f,
            &mut |s| slept.push(s),
            "http://localhost:8090",
            "growth_rate",
            &recon_cfg("http"),
        );
        assert_eq!(out.value, 0.85);
        assert!(slept.is_empty());
    }

    #[test]
    fn gather_with_stabilization_wait() {
        let f = FakeHttp::new(vec![ok(json!({"growth_rate": 0.9}))]);
        let mut slept = Vec::new();
        let c = cfg(&[
            ("service", json!("http")),
            ("path", json!("/metrics/json")),
            ("key", json!("growth_rate")),
            ("stabilization_seconds", json!(5)),
            ("samples", json!(1)),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
        ]);
        let out = gather_single_metric(&f, &mut |s| slept.push(s), "http://x", "growth_rate", &c);
        assert_eq!(out.value, 0.9);
        assert!(slept.contains(&5.0));
    }

    #[test]
    fn gather_multiple_samples_with_interval() {
        let f = FakeHttp::new(vec![ok(json!({"energy": 12.5})); 3]);
        let mut slept = Vec::new();
        let c = cfg(&[
            ("service", json!("http")),
            ("path", json!("/m")),
            ("key", json!("energy")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(3)),
            ("interval", json!(2)),
            ("aggregation", json!("median")),
        ]);
        let out = gather_single_metric(&f, &mut |s| slept.push(s), "http://x", "energy", &c);
        assert_eq!(out.value, 12.5);
        assert_eq!(f.call_count(), 3);
        assert_eq!(slept, vec![2.0, 2.0]); // between the three samples
    }

    #[test]
    fn gather_missing_key_returns_inf() {
        let f = FakeHttp::new(vec![ok(json!({"other_key": 42.0}))]);
        let out = gather_single_metric(
            &f,
            &mut no_sleep,
            "http://x",
            "growth_rate",
            &recon_cfg("http"),
        );
        assert_eq!(out.value, f64::INFINITY);
        assert!(out.noise_cv.is_none());
    }

    #[test]
    fn gather_http_failure_returns_inf() {
        let f = FakeHttp::new(vec![
            conn_err(),
            conn_err(),
            conn_err(), // the inner ladder exhausts its three
        ]);
        let out = gather_single_metric(
            &f,
            &mut no_sleep,
            "http://x",
            "growth_rate",
            &recon_cfg("http"),
        );
        assert_eq!(out.value, f64::INFINITY);
    }

    #[test]
    fn gather_unsupported_service_returns_inf() {
        let f = FakeHttp::new(vec![]);
        let out = gather_single_metric(
            &f,
            &mut no_sleep,
            "http://x",
            "growth_rate",
            &recon_cfg("unsupported"),
        );
        assert_eq!(out.value, f64::INFINITY);
        assert_eq!(f.call_count(), 0);
    }

    // ─── adaptive sampling ───────────────────────────────────────

    fn adaptive_cfg(max: Value, cv_threshold: f64) -> Value {
        cfg(&[
            ("service", json!("http")),
            ("path", json!("/m")),
            ("key", json!("temp")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(3)),
            ("max_samples", max),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
            ("cv_threshold", json!(cv_threshold)),
        ])
    }

    #[test]
    fn adaptive_stops_early_when_stable() {
        let responses: Vec<_> = (0..3)
            .map(|_| ok(json!({"temp": 100.0})))
            .chain((0..20).map(|_| ok(json!({"temp": 200.0}))))
            .collect();
        let f = FakeHttp::new(responses);
        let out = gather_single_metric(
            &f,
            &mut no_sleep,
            "http://x",
            "temp",
            &adaptive_cfg(json!(20), 0.05),
        );
        assert_eq!(out.value, 100.0);
        assert_eq!(f.call_count(), 3);
    }

    #[test]
    fn adaptive_keeps_sampling_when_noisy() {
        let vals = [
            80.0, 120.0, 60.0, 140.0, 50.0, 130.0, 70.0, 110.0, 90.0, 100.0,
        ];
        let responses: Vec<_> = vals.iter().map(|v| ok(json!({"temp": *v}))).collect();
        let f = FakeHttp::new(responses);
        let _ = gather_single_metric(
            &f,
            &mut no_sleep,
            "http://x",
            "temp",
            &adaptive_cfg(json!(20), 0.01),
        );
        assert!(f.call_count() > 3);
        assert!(f.call_count() <= 20);
    }

    #[test]
    fn adaptive_respects_max_samples() {
        let responses: Vec<_> = (0..100).map(|i| ok(json!({"temp": i as f64}))).collect();
        let f = FakeHttp::new(responses);
        let _ = gather_single_metric(
            &f,
            &mut no_sleep,
            "http://x",
            "temp",
            &adaptive_cfg(json!(8), 0.001),
        );
        assert_eq!(f.call_count(), 8);
    }

    #[test]
    fn backward_compat_no_max_samples() {
        let f = FakeHttp::new(vec![ok(json!({"temp": 42.0})); 5]);
        let c = cfg(&[
            ("service", json!("http")),
            ("path", json!("/m")),
            ("key", json!("temp")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(5)),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
        ]);
        let out = gather_single_metric(&f, &mut no_sleep, "http://x", "temp", &c);
        assert_eq!(out.value, 42.0);
        assert_eq!(f.call_count(), 5);
    }

    #[test]
    fn backward_compat_single_sample() {
        let f = FakeHttp::new(vec![ok(json!({"temp": 42.0}))]);
        let c = cfg(&[
            ("service", json!("http")),
            ("path", json!("/m")),
            ("key", json!("temp")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(1)),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
        ]);
        let out = gather_single_metric(&f, &mut no_sleep, "http://x", "temp", &c);
        assert_eq!(out.value, 42.0);
        assert_eq!(f.call_count(), 1);
    }

    #[test]
    fn float_samples_fail_like_pythons_range_typeerror() {
        // samples: 3.0 is a float — python's range() raises TypeError
        // inside the try, the gather fails to inf
        let f = FakeHttp::new(vec![]);
        let c = cfg(&[
            ("service", json!("http")),
            ("path", json!("/m")),
            ("key", json!("temp")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(3.0)),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
        ]);
        let out = gather_single_metric(&f, &mut no_sleep, "http://x", "temp", &c);
        assert_eq!(out.value, f64::INFINITY);
        assert_eq!(f.call_count(), 0);
    }

    // ─── main ────────────────────────────────────────────────────

    #[test]
    fn main_gathers_objective_metrics() {
        let f = FakeHttp::new(vec![ok(json!({"growth_rate": 0.85}))]);
        let context = json!({
            "reconnaissance": {"http": {"url": "http://greenhouse:8090"}},
            "objectives": [{
                "name": "growth_rate",
                "reconnaissance": {
                    "service": "http", "path": "/metrics/json", "key": "growth_rate",
                }
            }],
            "guardrails": []
        });
        let out = main(&context, &json!([]), None, &f, &mut no_sleep);
        assert_eq!(out["status"], "completed");
        assert_eq!(out["metrics"]["growth_rate"], 0.85);
    }

    #[test]
    fn main_empty_objectives_and_guardrails() {
        let f = FakeHttp::new(vec![]);
        let context = json!({
            "reconnaissance": {"http": {"url": "http://localhost:8090"}},
            "objectives": [],
            "guardrails": []
        });
        let out = main(&context, &json!([]), None, &f, &mut no_sleep);
        assert_eq!(out["status"], "completed");
        assert_eq!(out["metrics"], json!({}));
    }

    #[test]
    fn main_gathers_watched_observations() {
        // Observations must land in the metrics dict — they are the
        // extra readout channels (python receipt: run 28 carried only
        // objective_0 because recon never read objective_1)
        let f = FakeHttp::new(vec![
            ok(json!({"objective_0": 0.5})),
            ok(json!({"objective_1": 0.3})),
        ]);
        let context = json!({
            "reconnaissance": {"type": "http", "http": {"url": "http://bench:8090"}},
            "objectives": [{"name": "objective_0", "reconnaissance": {
                "service": "http", "path": "/metrics/json", "key": "objective_0",
                "samples": 1, "stabilization_seconds": 0, "interval": 0, "aggregation": "median"}}],
            "observations": [{"name": "objective_1", "reconnaissance": {
                "service": "http", "path": "/metrics/json", "key": "objective_1",
                "samples": 1, "stabilization_seconds": 0, "interval": 0, "aggregation": "median"}}],
        });
        let out = main(&context, &json!([]), None, &f, &mut no_sleep);
        assert_eq!(out["metrics"]["objective_0"], 0.5);
        assert_eq!(out["metrics"]["objective_1"], 0.3);
    }

    #[test]
    fn main_failure_value_renders_null_at_the_boundary() {
        let f = FakeHttp::new(vec![conn_err(), conn_err(), conn_err()]);
        let context = json!({
            "objectives": [{"name": "x", "reconnaissance": {
                "service": "http", "path": "/m", "key": "x", "stabilization_seconds": 0,
                "samples": 1, "interval": 0, "aggregation": "median"}}],
            "guardrails": []
        });
        let out = main(&context, &json!([]), None, &f, &mut no_sleep);
        assert_eq!(out["metrics"]["x"], Value::Null);
        assert!(out
            .get("metric_noise")
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn main_noise_cv_rounds_and_rides_along() {
        let f = FakeHttp::new(vec![
            ok(json!({"temp": 10.0})),
            ok(json!({"temp": 10.002})),
            ok(json!({"temp": 9.998})),
        ]);
        let c = cfg(&[
            ("service", json!("http")),
            ("path", json!("/m")),
            ("key", json!("temp")),
            ("stabilization_seconds", json!(0)),
            ("samples", json!(3)),
            ("interval", json!(0)),
            ("aggregation", json!("median")),
        ]);
        let context = json!({
            "objectives": [{"name": "temp", "reconnaissance": c}],
            "guardrails": []
        });
        let out = main(&context, &json!([]), None, &f, &mut no_sleep);
        let noise = out["metric_noise"]["temp"].as_f64().unwrap();
        assert!(noise > 0.0 && noise < 0.001, "noise {noise}");
    }
}
