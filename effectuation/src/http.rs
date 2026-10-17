//! Port of `effectuation/http.py` — generic HTTP effectuation: apply
//! the settings JSON to every target's endpoint, with per-target
//! auth material fetched from windmill variables (bearer, basic,
//! api_key), the shared endpoint config (path/method/headers/
//! timeout), a stabilization wait, and the summary result.
//! Statement-by-statement against the python original; the tests
//! mirror `tests/unit/test_effectuation_logic.py`'s HTTP classes.

use serde_json::{json, Value};

use base64::Engine as _;

/// The python `requests.exceptions` trio the script distinguished:
/// Timeout, ConnectionError, and everything else.
#[derive(Clone)]
pub enum HttpErr {
    Timeout,
    Connection(String),
    Other(String),
}

impl std::fmt::Display for HttpErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpErr::Timeout => write!(f, "request timed out"),
            HttpErr::Connection(e) => write!(f, "connection failed: {e}"),
            HttpErr::Other(e) => write!(f, "{e}"),
        }
    }
}

/// One response — python's `response.ok` (status < 400),
/// `status_code`, raw `content` bytes, and `text` for error rows.
#[derive(Clone)]
pub struct HttpResponse {
    pub ok: bool,
    pub status_code: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    fn has_content(&self) -> bool {
        // python `if response.content` — empty bytes are falsy
        !self.body.is_empty()
    }
}

/// The HTTP edge — the python patched `requests.request`; the rust
/// tests fake this surface and inspect method/url/headers/body.
pub trait Http: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        json_body: &Value,
        timeout_secs: f64,
    ) -> Result<HttpResponse, HttpErr>;
}

/// The production edge: reqwest blocking, no TLS, per-call timeout.
pub struct ReqwestHttp;

impl Http for ReqwestHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        json_body: &Value,
        timeout_secs: f64,
    ) -> Result<HttpResponse, HttpErr> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs_f64(timeout_secs))
            .build()
            .map_err(|e| HttpErr::Other(e.to_string()))?;
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| HttpErr::Other(e.to_string()))?;
        let mut req = client.request(m, url).json(json_body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().map_err(|e| {
            if e.is_timeout() {
                HttpErr::Timeout
            } else if e.is_connect() {
                HttpErr::Connection(e.to_string())
            } else {
                HttpErr::Other(e.to_string())
            }
        })?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .map_err(|e| HttpErr::Other(e.to_string()))?
            .to_vec();
        Ok(HttpResponse {
            ok: status < 400,
            status_code: status,
            body,
        })
    }
}

/// The windmill variable edge (the auth material). The python script
/// called `wmill.get_variable`; the real client implements this.
pub trait Variables: Send + Sync {
    fn get_variable(&self, path: &str) -> Result<Value, String>;
}

impl Variables for godon_wmill::WmillClient {
    fn get_variable(&self, path: &str) -> Result<Value, String> {
        godon_wmill::WmillClient::get_variable(self, path)
    }
}

/// `main` — the per-target loop with auth resolution, the request,
/// the three error classes, the stabilization wait, and the summary.
pub fn main(
    context: &Value,
    targets: &Value,
    settings: &Value,
    http: &dyn Http,
    vars: &dyn Variables,
    sleep: &mut dyn FnMut(f64),
) -> Value {
    let effectuation_config = context.get("effectuation").cloned().unwrap_or(json!({}));
    let endpoint_config = effectuation_config
        .get("endpoint_config")
        .cloned()
        .unwrap_or(json!({}));
    let stabilization_seconds = effectuation_config
        .get("stabilization_seconds")
        .and_then(|s| s.as_f64())
        .unwrap_or(0.0);

    let targets = targets.as_array().cloned().unwrap_or_default();
    log::info!("HTTP effectuation for {} targets", targets.len());
    log::debug!(
        "Endpoint: {} {}",
        endpoint_config
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("POST"),
        endpoint_config
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
    );
    log::info!("Settings: {}", settings_keys(settings));

    let mut all_results: Vec<Value> = Vec::new();

    for target in &targets {
        let target_id = target.get("id").cloned().unwrap_or(Value::Null);
        let base_url = target.get("url").and_then(|u| u.as_str());
        let auth_type = target
            .get("auth_type")
            .and_then(|a| a.as_str())
            .unwrap_or("none");
        let auth_variable_path = target.get("auth_variable_path").and_then(|p| p.as_str());

        log::info!(
            "Processing target {}: {}",
            target_id,
            base_url.unwrap_or("")
        );

        let row = apply_to_target(
            http,
            vars,
            &endpoint_config,
            target_id,
            base_url,
            auth_type,
            auth_variable_path,
            settings,
        );
        all_results.push(row);
    }

    if stabilization_seconds > 0.0 {
        log::info!("Waiting {stabilization_seconds}s for system stabilization");
        sleep(stabilization_seconds);
    }

    let success_count = all_results
        .iter()
        .filter(|r| r.get("success").and_then(|s| s.as_bool()).unwrap_or(false))
        .count();
    let total_count = all_results.len();

    log::info!("HTTP effectuation completed: {success_count}/{total_count} successful");

    json!({
        "status": "completed",
        "targets_count": targets.len(),
        "successful_changes": success_count,
        "failed_changes": total_count - success_count,
        "results": all_results,
    })
}

/// python `list(settings.keys())` rendering for the log line.
fn settings_keys(settings: &Value) -> Value {
    match settings.as_object() {
        Some(o) => Value::Array(o.keys().map(|k| Value::String(k.clone())).collect()),
        None => Value::Array(vec![]),
    }
}

/// One target's try-block — the python caught per-target exceptions
/// and carried them as result rows; `Err` maps to the same rows.
#[allow(clippy::too_many_arguments)]
fn apply_to_target(
    http: &dyn Http,
    vars: &dyn Variables,
    endpoint_config: &Value,
    target_id: Value,
    base_url: Option<&str>,
    auth_type: &str,
    auth_variable_path: Option<&str>,
    settings: &Value,
) -> Value {
    match apply_inner(
        http,
        vars,
        endpoint_config,
        target_id.clone(),
        base_url,
        auth_type,
        auth_variable_path,
        settings,
    ) {
        Ok(row) => row,
        Err(e) => {
            log::info!("Failed to process target {target_id}: {e}");
            error_row(
                target_id,
                Value::String(base_url.unwrap_or("").to_string()),
                match e {
                    HttpErr::Timeout => "Request timed out".to_string(),
                    HttpErr::Connection(msg) => {
                        format!("Connection failed: {}", char_truncate(&msg, 200))
                    }
                    HttpErr::Other(msg) => format!("Target processing failed: {msg}"),
                },
            )
        }
    }
}

fn apply_inner(
    http: &dyn Http,
    vars: &dyn Variables,
    endpoint_config: &Value,
    target_id: Value,
    base_url: Option<&str>,
    auth_type: &str,
    auth_variable_path: Option<&str>,
    settings: &Value,
) -> Result<Value, HttpErr> {
    let mut headers: Vec<(String, String)> = Vec::new();
    let base_headers = endpoint_config.get("headers").cloned().unwrap_or(json!({}));
    let base_headers = base_headers
        .as_object()
        .ok_or_else(|| HttpErr::Other("`headers` config is not an object".to_string()))?;
    for (k, v) in base_headers {
        headers.push((
            k.clone(),
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string()),
        ));
    }
    headers.push(("Content-Type".to_string(), "application/json".to_string()));

    if let Some(path) = auth_variable_path {
        let auth_creds = vars.get_variable(path).map_err(|e| HttpErr::Other(e))?;
        match auth_type {
            "bearer" => {
                let token = match &auth_creds {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                headers.push(("Authorization".to_string(), format!("Bearer {token}")));
            }
            "basic" => {
                let username = auth_creds
                    .get("username")
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                let password = auth_creds
                    .get("password")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let creds = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                headers.push(("Authorization".to_string(), format!("Basic {creds}")));
            }
            "api_key" => {
                let key_name = auth_creds
                    .get("header_name")
                    .and_then(|h| h.as_str())
                    .unwrap_or("X-API-Key");
                let api_key = auth_creds
                    .get("api_key")
                    .and_then(|a| a.as_str())
                    .unwrap_or("");
                headers.push((key_name.to_string(), api_key.to_string()));
            }
            _ => {}
        }
    }

    let path = endpoint_config
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let full_url = match base_url {
        Some(b) if !b.is_empty() => format!("{}{}", b.trim_end_matches('/'), path),
        _ => path.to_string(),
    };
    let method = endpoint_config
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("POST")
        .to_uppercase();
    let timeout = endpoint_config
        .get("timeout_seconds")
        .and_then(|t| t.as_f64())
        .unwrap_or(30.0);

    let response = http.request(&method, &full_url, &headers, settings, timeout)?;

    if response.ok {
        let parsed = if response.has_content() {
            let text = response.text();
            Some(
                serde_json::from_str::<Value>(&text)
                    .map_err(|e| HttpErr::Other(format!("error decoding response body: {e}")))?,
            )
        } else {
            None
        };
        Ok(json!({
            "target_id": target_id,
            "url": full_url,
            "success": true,
            "status_code": response.status_code,
            "response": parsed.unwrap_or(Value::Null),
        }))
    } else {
        Ok(json!({
            "target_id": target_id,
            "url": full_url,
            "success": false,
            "status_code": response.status_code,
            "error": format!("HTTP {}: {}", response.status_code, char_truncate(&response.text(), 500)),
        }))
    }
}

fn error_row(target_id: Value, url: Value, error: String) -> Value {
    json!({
        "target_id": target_id,
        "url": url,
        "success": false,
        "error": error,
    })
}

/// python `s[:n]` — a CHARACTER slice, not bytes.
fn char_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHttp {
        response: Option<Result<HttpResponse, HttpErr>>,
        calls: Mutex<Vec<(String, String, Vec<(String, String)>, Value, f64)>>,
    }

    impl FakeHttp {
        fn ok(response: HttpResponse) -> Self {
            FakeHttp {
                response: Some(Ok(response)),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn err(e: HttpErr) -> Self {
            FakeHttp {
                response: Some(Err(e)),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn last_call(&self) -> (String, String, Vec<(String, String)>, Value, f64) {
            self.calls.lock().unwrap().last().unwrap().clone()
        }
    }

    impl Http for FakeHttp {
        fn request(
            &self,
            method: &str,
            url: &str,
            headers: &[(String, String)],
            json_body: &Value,
            timeout_secs: f64,
        ) -> Result<HttpResponse, HttpErr> {
            self.calls.lock().unwrap().push((
                method.to_string(),
                url.to_string(),
                headers.to_vec(),
                json_body.clone(),
                timeout_secs,
            ));
            self.response
                .clone()
                .unwrap_or(Err(HttpErr::Other("no response".into())))
        }
    }

    struct FakeVars {
        value: Value,
        requested: Mutex<Vec<String>>,
    }

    impl FakeVars {
        fn never() -> Self {
            FakeVars {
                value: Value::Null,
                requested: Mutex::new(Vec::new()),
            }
        }
        fn with(value: Value) -> Self {
            FakeVars {
                value,
                requested: Mutex::new(Vec::new()),
            }
        }
    }

    impl Variables for FakeVars {
        fn get_variable(&self, path: &str) -> Result<Value, String> {
            self.requested.lock().unwrap().push(path.to_string());
            Ok(self.value.clone())
        }
    }

    fn no_sleep(_: f64) {}

    fn ctx(path: &str, method: &str) -> Value {
        json!({
            "effectuation": {
                "type": "http",
                "endpoint_config": {"path": path, "method": method},
            }
        })
    }

    #[test]
    fn single_target_success_with_bearer() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: b"{\"applied\": true}".to_vec(),
        });
        let vars = FakeVars::with(json!("test-token"));
        let targets = json!([{
            "id": "t1", "url": "https://api.example.com",
            "auth_type": "bearer", "auth_variable_path": "u/auth/token"
        }]);
        let settings = json!({"tcp_rmem": "4096 131072 174760"});

        let out = main(
            &ctx("/api/v1/config", "POST"),
            &targets,
            &settings,
            &http,
            &vars,
            &mut no_sleep,
        );

        assert_eq!(out["status"], "completed");
        assert_eq!(out["successful_changes"], 1);
        assert_eq!(out["failed_changes"], 0);
        assert_eq!(out["results"][0]["success"], true);
        assert_eq!(out["results"][0]["status_code"], 200);
        assert_eq!(out["results"][0]["response"]["applied"], true);

        let (method, url, headers, body, _) = http.last_call();
        assert_eq!(url, "https://api.example.com/api/v1/config");
        assert_eq!(method, "POST");
        assert_eq!(body, settings);
        assert!(headers.contains(&("Authorization".to_string(), "Bearer test-token".to_string())));
        assert!(headers.contains(&("Content-Type".to_string(), "application/json".to_string())));
    }

    #[test]
    fn target_http_failure_row() {
        let http = FakeHttp::ok(HttpResponse {
            ok: false,
            status_code: 503,
            body: b"Service Unavailable".to_vec(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);

        let out = main(
            &ctx("/api/config", "PUT"),
            &targets,
            &json!({"cpu_governor": "performance"}),
            &http,
            &vars,
            &mut no_sleep,
        );

        assert_eq!(out["successful_changes"], 0);
        assert_eq!(out["results"][0]["success"], false);
        assert_eq!(out["results"][0]["status_code"], 503);
        let err = out["results"][0]["error"].as_str().unwrap();
        assert!(err.starts_with("HTTP 503"), "{err}");
    }

    #[test]
    fn empty_body_response_field_is_null() {
        // python: response.json() if response.content else None
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let out = main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({"param": "value"}),
            &http,
            &vars,
            &mut no_sleep,
        );
        assert_eq!(out["results"][0]["response"], Value::Null);
        assert_eq!(out["successful_changes"], 1);
    }

    #[test]
    fn basic_auth_header() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::with(json!({"username": "admin", "password": "secret"}));
        let targets = json!([{
            "id": "t1", "url": "https://api.example.com",
            "auth_type": "basic", "auth_variable_path": "u/auth/creds"
        }]);
        main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({"param": "value"}),
            &http,
            &vars,
            &mut no_sleep,
        );
        let (_, _, headers, _, _) = http.last_call();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(auth.starts_with("Basic "), "{auth}");
        // base64("admin:secret")
        assert_eq!(auth, "Basic YWRtaW46c2VjcmV0");
    }

    #[test]
    fn api_key_auth_header() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::with(json!({"header_name": "X-Custom-Key", "api_key": "my-key-123"}));
        let targets = json!([{
            "id": "t1", "url": "https://api.example.com",
            "auth_type": "api_key", "auth_variable_path": "u/auth/key"
        }]);
        main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({"param": "value"}),
            &http,
            &vars,
            &mut no_sleep,
        );
        let (_, _, headers, _, _) = http.last_call();
        assert!(headers.contains(&("X-Custom-Key".to_string(), "my-key-123".to_string())));
    }

    #[test]
    fn no_auth_never_touches_variables() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let out = main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({"param": "value"}),
            &http,
            &vars,
            &mut no_sleep,
        );
        assert_eq!(out["successful_changes"], 1);
        assert!(vars.requested.lock().unwrap().is_empty());
    }

    #[test]
    fn timeout_row() {
        let http = FakeHttp::err(HttpErr::Timeout);
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let out = main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({}),
            &http,
            &vars,
            &mut no_sleep,
        );
        assert_eq!(out["failed_changes"], 1);
        let err = out["results"][0]["error"].as_str().unwrap();
        assert!(err.to_lowercase().contains("timed out"), "{err}");
    }

    #[test]
    fn connection_error_row() {
        let http = FakeHttp::err(HttpErr::Connection("refused".into()));
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let out = main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({}),
            &http,
            &vars,
            &mut no_sleep,
        );
        assert_eq!(out["failed_changes"], 1);
        let err = out["results"][0]["error"].as_str().unwrap();
        assert!(err.starts_with("Connection failed"), "{err}");
    }

    #[test]
    fn stabilization_wait() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::never();
        let context = json!({
            "effectuation": {
                "type": "http",
                "endpoint_config": {"path": "/api/config", "method": "POST"},
                "stabilization_seconds": 15,
            }
        });
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let mut slept = Vec::new();
        main(&context, &targets, &json!({}), &http, &vars, &mut |s| {
            slept.push(s)
        });
        assert_eq!(slept, vec![15.0]);
    }

    #[test]
    fn no_stabilization_by_default() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let mut slept = Vec::new();
        main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({}),
            &http,
            &vars,
            &mut |s| slept.push(s),
        );
        assert!(slept.is_empty());
    }

    #[test]
    fn custom_method_put() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        main(
            &ctx("/api/config", "PUT"),
            &targets,
            &json!({"param": "value"}),
            &http,
            &vars,
            &mut no_sleep,
        );
        let (method, _, _, _, _) = http.last_call();
        assert_eq!(method, "PUT");
    }

    #[test]
    fn contract_compliance_keys() {
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: Vec::new(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let out = main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({"param": "value"}),
            &http,
            &vars,
            &mut no_sleep,
        );
        for key in ["status", "successful_changes", "failed_changes", "results"] {
            assert!(out.get(key).is_some(), "missing {key}");
        }
        assert_eq!(out["targets_count"], 1);
    }

    #[test]
    fn unparseable_response_body_is_target_failure() {
        // python: response.json() raised inside the try → the generic
        // except → 'Target processing failed'
        let http = FakeHttp::ok(HttpResponse {
            ok: true,
            status_code: 200,
            body: b"not json".to_vec(),
        });
        let vars = FakeVars::never();
        let targets = json!([{"id": "t1", "url": "https://api.example.com"}]);
        let out = main(
            &ctx("/api/config", "POST"),
            &targets,
            &json!({}),
            &http,
            &vars,
            &mut no_sleep,
        );
        assert_eq!(out["failed_changes"], 1);
        let err = out["results"][0]["error"].as_str().unwrap();
        assert!(err.starts_with("Target processing failed"), "{err}");
    }
}
