//! The windmill client — a port of the `wmill` python client's three
//! verbs (v1.623.1, the estate's pin, source-read from the PyPI sdist
//! this session): `run_script_by_path`, `set_state`, `get_variable`.
//! These are the only parts of the python client the tender body and
//! its leaves speak.
//!
//! Endpoint truth (client source, verbatim shapes):
//! - `run_script_by_path` = POST `/w/{ws}/jobs/run/p/{path}` with the
//!   args as the JSON body, `parent_job` / `root_job` query params
//!   when the env carries them — the response TEXT is the job id —
//!   then a poll loop: GET
//!   `/w/{ws}/jobs_u/completed/get_result_maybe/{job_id}?get_started=true`
//!   every 0.5s until `completed`; `success` returns `result`, a
//!   failed job raises `Job {id} was not successful: {error}`. The
//!   python body passes no timeout — the wait is unbounded; each
//!   individual request rides the httpx default (5s).
//! - `set_state` = the state resource create-or-update at
//!   `WM_STATE_PATH_NEW` || `WM_STATE_PATH`: GET
//!   `/w/{ws}/resources/get/{path}`; 404 → POST `/resources/create`
//!   `{path, value, resource_type: "state"}`; anything else → POST
//!   `/resources/update_value/{path}` `{value}`.
//! - `get_variable` = strip a `var://` prefix if present, GET
//!   `/w/{ws}/variables/get_value/{path}`, parse the body as JSON
//!   (string variables arrive as JSON strings, credential objects as
//!   JSON objects).
//!
//! NOT ported (named, not silent):
//! - the python client's `atexit` cancel guard (`cleanup=True`
//!   registers a cancel-on-exit for the spawned job) — it fires only
//!   on clean interpreter exits; SIGKILL, the estate's actual death
//!   mode, skips it. The rust body has no equivalent guard.
//! - `verify` / TLS toggles — no TLS feature is compiled in at all
//!   (see the Cargo.toml note).
//!
//! The transport is a trait ([`WmillHttp`]) so tests fake the HTTP
//! edge exactly where the python tests patched the `wmill` module.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// One HTTP exchange at the windmill API edge. `status` is carried
/// raw — the client methods decide which statuses raise, mirroring
/// the python helper's `raise_for_status` flag.
pub struct WmillResp {
    pub status: u16,
    pub text: String,
}

impl WmillResp {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.text).ok()
    }
}

/// The transport trait. Paths arrive exactly as the python client
/// built them: `/w/{workspace}/...` (no `/api` prefix — that joins at
/// the real transport, like the python `base_url = f"{base}/api"`).
pub trait WmillHttp: Send + Sync {
    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<WmillResp, String>;
    fn post(&self, path: &str, query: &[(&str, String)], body: &Value)
        -> Result<WmillResp, String>;
}

/// The client. Constructed from the platform env the python client
/// read (`WM_WORKSPACE`, `WM_TOKEN`, `BASE_INTERNAL_URL` ||
/// `WM_BASE_URL`, `WM_STATE_PATH_NEW` || `WM_STATE_PATH`,
/// `WM_JOB_ID`, `WM_ROOT_FLOW_JOB_ID`).
pub struct WmillClient {
    http: Arc<dyn WmillHttp>,
    workspace: String,
    state_path: Option<String>,
    parent_job: Option<String>,
    root_job: Option<String>,
}

impl WmillClient {
    pub fn new(
        http: Arc<dyn WmillHttp>,
        workspace: String,
        state_path: Option<String>,
        parent_job: Option<String>,
        root_job: Option<String>,
    ) -> Self {
        WmillClient {
            http,
            workspace,
            state_path,
            parent_job,
            root_job,
        }
    }

    /// The env exactly as the python client's constructor read it.
    /// A missing workspace is a construction error (the python
    /// asserted the same); a missing state path defers to the
    /// `set_state` call, exactly as the python property raised there.
    pub fn from_env(http: Arc<dyn WmillHttp>) -> Result<Self, String> {
        let workspace = std::env::var("WM_WORKSPACE").map_err(|_| {
            "workspace required as an argument or as WM_WORKSPACE environment variable".to_string()
        })?;
        let state_path = std::env::var("WM_STATE_PATH_NEW")
            .or_else(|_| std::env::var("WM_STATE_PATH"))
            .ok();
        let parent_job = std::env::var("WM_JOB_ID").ok();
        let root_job = std::env::var("WM_ROOT_FLOW_JOB_ID").ok();
        Ok(WmillClient::new(
            http, workspace, state_path, parent_job, root_job,
        ))
    }

    /// The platform-run self-serve edge: this job's own args as the
    /// caller passed them (GET jobs_u/get_args — the raw args JSON
    /// rides the job row verbatim, keyed exactly as the caller sent).
    pub fn get_job_args(&self, job_id: &str) -> Result<Value, String> {
        let resp = self.check(self.http.get(
            &format!("/w/{}/jobs_u/get_args/{}", self.workspace, job_id),
            &[],
        )?)?;
        resp.json()
            .ok_or_else(|| format!("invalid args body for job {job_id}"))
    }

    /// Run a script by path and WAIT for its result — the python
    /// `run_script_by_path`: async run (job id from the response
    /// text) + the 0.5s result poll, unbounded, returning the job's
    /// result value or the failure the python raised.
    pub fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(pj) = &self.parent_job {
            query.push(("parent_job", pj.clone()));
        }
        if let Some(rj) = &self.root_job {
            query.push(("root_job", rj.clone()));
        }
        let resp = self.check(self.http.post(
            &format!("/w/{}/jobs/run/p/{}", self.workspace, path),
            &query,
            args,
        )?)?;
        let job_id = resp.text.trim().to_string();

        loop {
            let resp = self.check(self.http.get(
                &format!(
                    "/w/{}/jobs_u/completed/get_result_maybe/{}",
                    self.workspace, job_id
                ),
                &[("get_started", "true".to_string())],
            )?)?;
            let body = resp
                .json()
                .ok_or_else(|| format!("invalid result poll body for job {job_id}"))?;
            let completed = body
                .get("completed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if completed {
                let success = body
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let result = body.get("result").cloned().unwrap_or(Value::Null);
                if success {
                    return Ok(result);
                }
                // python: error = result["error"]; str(error)
                let error = result
                    .get("error")
                    .map(render_value)
                    .unwrap_or_else(|| "None".to_string());
                return Err(format!("Job {job_id} was not successful: {error}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Set the job state — the python `set_state`: create-or-update
    /// the state resource at the env's state path.
    pub fn set_state(&self, value: &Value) -> Result<(), String> {
        let path = self
            .state_path
            .as_ref()
            .ok_or_else(|| "State path not found".to_string())?;
        // python read this with raise_for_status=False: only a 404
        // decides "create"; every other status falls through to the
        // update branch (whose own POST raises if it fails).
        let resp = self.http.get(
            &format!("/w/{}/resources/get/{}", self.workspace, path),
            &[],
        )?;
        if resp.status == 404 {
            self.check(self.http.post(
                &format!("/w/{}/resources/create", self.workspace),
                &[],
                &serde_json::json!({
                    "path": path,
                    "value": value,
                    "resource_type": "state",
                }),
            )?)?;
            Ok(())
        } else {
            self.check(self.http.post(
                &format!("/w/{}/resources/update_value/{}", self.workspace, path),
                &[],
                &serde_json::json!({ "value": value }),
            )?)?;
            Ok(())
        }
    }

    /// Get a variable by path — the python `get_variable`: the
    /// `var://` prefix strips, the value arrives as parsed JSON.
    pub fn get_variable(&self, path: &str) -> Result<Value, String> {
        let p = path.strip_prefix("var://").unwrap_or(path);
        let resp = self.check(self.http.get(
            &format!("/w/{}/variables/get_value/{}", self.workspace, p),
            &[],
        )?)?;
        resp.json()
            .ok_or_else(|| format!("invalid variable body at {p}"))
    }

    /// The python post/get helpers raised on 4xx/5xx (the one
    /// deliberate exception being set_state's existence probe).
    fn check(&self, resp: WmillResp) -> Result<WmillResp, String> {
        if (400..600).contains(&resp.status) {
            Err(format!("HTTP {}: {}", resp.status, resp.text))
        } else {
            Ok(resp)
        }
    }
}

/// python `str(x)` for the shapes the job error carries: a string is
/// itself, everything else renders as its repr/json form, missing is
/// `None`.
fn render_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The production transport: reqwest blocking, no TLS, the Bearer
/// token and Content-Type the python client set on every request,
/// the httpx default 5s per-request timeout.
pub struct ReqwestWmillHttp {
    client: reqwest::blocking::Client,
    base: String,
    token: String,
}

impl ReqwestWmillHttp {
    /// `BASE_INTERNAL_URL` (the platform's internal url) wins over
    /// `WM_BASE_URL`, exactly as the python constructor ordered it.
    pub fn from_env() -> Result<Self, String> {
        let base = std::env::var("BASE_INTERNAL_URL")
            .or_else(|_| std::env::var("WM_BASE_URL"))
            .map_err(|_| {
                "windmill base url required (BASE_INTERNAL_URL or WM_BASE_URL)".to_string()
            })?;
        let token = std::env::var("WM_TOKEN")
            .map_err(|_| "windmill token required (WM_TOKEN)".to_string())?;
        Ok(ReqwestWmillHttp {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?,
            base: format!("{base}/api"),
            token,
        })
    }
}

impl WmillHttp for ReqwestWmillHttp {
    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<WmillResp, String> {
        let mut req = self
            .client
            .get(format!("{}{}", self.base, path))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json");
        if !query.is_empty() {
            let pairs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
            req = req.query(&pairs);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        Ok(WmillResp {
            status: resp.status().as_u16(),
            text: resp.text().map_err(|e| e.to_string())?,
        })
    }

    fn post(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &Value,
    ) -> Result<WmillResp, String> {
        let mut req = self
            .client
            .post(format!("{}{}", self.base, path))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(body);
        if !query.is_empty() {
            let pairs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
            req = req.query(&pairs);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        Ok(WmillResp {
            status: resp.status().as_u16(),
            text: resp.text().map_err(|e| e.to_string())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeHttp {
        gets: std::sync::Mutex<Vec<(String, Vec<(String, String)>)>>,
        posts: std::sync::Mutex<Vec<(String, Value)>>,
        script: std::sync::Mutex<VecDeque<Result<WmillResp, String>>>,
    }

    impl FakeHttp {
        fn push(&self, r: WmillResp) -> &Self {
            self.script.lock().unwrap().push_back(Ok(r));
            self
        }
        fn next(&self) -> Result<WmillResp, String> {
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err("no more scripted responses".into()))
        }
    }

    impl WmillHttp for FakeHttp {
        fn get(&self, path: &str, query: &[(&str, String)]) -> Result<WmillResp, String> {
            self.gets.lock().unwrap().push((
                path.to_string(),
                query
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ));
            self.next()
        }
        fn post(
            &self,
            path: &str,
            _query: &[(&str, String)],
            body: &Value,
        ) -> Result<WmillResp, String> {
            self.posts
                .lock()
                .unwrap()
                .push((path.to_string(), body.clone()));
            self.next()
        }
    }

    fn resp(status: u16, text: &str) -> WmillResp {
        WmillResp {
            status,
            text: text.to_string(),
        }
    }

    fn client(http: Arc<FakeHttp>) -> WmillClient {
        WmillClient::new(
            http,
            "godon".into(),
            Some("u/admin/states/job".into()),
            Some("parent-1".into()),
            None,
        )
    }

    #[test]
    fn run_script_waits_and_returns_result() {
        let http = Arc::new(FakeHttp::default());
        http.push(resp(200, "job-17"));
        http.push(resp(
            200,
            r#"{"started": true, "completed": true, "success": true, "result": {"metrics": {"a": 1.0}}}"#,
        ));
        let out = client(http.clone())
            .run_script_by_path("f/reconnaissance/http", &serde_json::json!({"context": {}}))
            .unwrap();
        assert_eq!(out["metrics"]["a"], 1.0);
        // the run POST carried parent_job from the env-shaped field
        let posts = http.posts.lock().unwrap();
        assert_eq!(posts[0].0, "/w/godon/jobs/run/p/f/reconnaissance/http");
        let gets = http.gets.lock().unwrap();
        assert_eq!(
            gets[0].0,
            "/w/godon/jobs_u/completed/get_result_maybe/job-17"
        );
        assert_eq!(
            gets[0].1[0],
            ("get_started".to_string(), "true".to_string())
        );
    }

    #[test]
    fn run_script_failed_job_raises_python_message() {
        let http = Arc::new(FakeHttp::default());
        http.push(resp(200, "job-18"));
        http.push(resp(
            200,
            r#"{"started": true, "completed": true, "success": false, "result": {"error": "boom"}}"#,
        ));
        let err = client(http)
            .run_script_by_path("f/x", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(err, "Job job-18 was not successful: boom");
    }

    #[test]
    fn set_state_creates_on_404_updates_otherwise() {
        let http = Arc::new(FakeHttp::default());
        http.push(resp(404, ""));
        http.push(resp(200, ""));
        client(http.clone())
            .set_state(&serde_json::json!({"step": 1}))
            .unwrap();
        {
            let posts = http.posts.lock().unwrap();
            assert_eq!(posts[0].0, "/w/godon/resources/create");
            assert_eq!(posts[0].1["resource_type"], "state");
            assert_eq!(posts[0].1["path"], "u/admin/states/job");
        }

        let http2 = Arc::new(FakeHttp::default());
        http2.push(resp(200, "existing"));
        http2.push(resp(200, ""));
        client(http2.clone())
            .set_state(&serde_json::json!({"step": 2}))
            .unwrap();
        let posts = http2.posts.lock().unwrap();
        assert_eq!(
            posts[0].0,
            "/w/godon/resources/update_value/u/admin/states/job"
        );
    }

    #[test]
    fn set_state_without_path_errors_like_python() {
        let http = Arc::new(FakeHttp::default());
        let c = WmillClient::new(http, "godon".into(), None, None, None);
        let err = c.set_state(&serde_json::json!({})).unwrap_err();
        assert_eq!(err, "State path not found");
    }

    #[test]
    fn get_variable_strips_var_scheme() {
        let http = Arc::new(FakeHttp::default());
        http.push(resp(200, r#""test-token""#));
        let out = client(http.clone())
            .get_variable("var://u/auth/token")
            .unwrap();
        assert_eq!(out, serde_json::json!("test-token"));
        assert_eq!(
            http.gets.lock().unwrap()[0].0,
            "/w/godon/variables/get_value/u/auth/token"
        );
    }

    #[test]
    fn error_statuses_raise() {
        let http = Arc::new(FakeHttp::default());
        http.push(resp(500, "internal"));
        let err = client(http).get_variable("u/x").unwrap_err();
        assert!(err.starts_with("HTTP 500"));
    }

    /// The wire regression the estate caught: run_script_by_path sends
    /// parent_job as a query pair, and the real transport must carry it
    /// to the wire. Against the old per-item `.query(&(*k, v))` shape
    /// reqwest fails to build the request ("builder error", source
    /// "unsupported pair") and the POST never leaves the process — the
    /// estate's symptom: zero recon/effectuation children while every
    /// trial logged "Trial execution failed: builder error". This test
    /// runs the REAL reqwest transport against a loopback listener, so
    /// the serialization shape is exercised, not faked.
    #[test]
    fn dispatch_query_pairs_reach_the_wire() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_thread = seen.clone();
        std::thread::spawn(move || {
            let replies = [
                ("text/plain", "job-17".to_string()),
                (
                    "application/json",
                    r#"{"started": true, "completed": true, "success": true, "result": {"metrics": {"a": 1.0}}}"#.to_string(),
                ),
            ];
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut chunk) {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break; // headers (and the tiny body) arrived
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                seen_thread
                    .lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or_default().to_string());
                let (ctype, body) = &replies[i];
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    ctype,
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).unwrap();
                if i + 1 == replies.len() {
                    break;
                }
            }
        });

        std::env::set_var("BASE_INTERNAL_URL", format!("http://{addr}"));
        std::env::set_var("WM_TOKEN", "test-token");
        std::env::set_var("WM_WORKSPACE", "godon");
        std::env::set_var("WM_JOB_ID", "parent-1");
        std::env::remove_var("WM_ROOT_FLOW_JOB_ID");

        let transport = Arc::new(ReqwestWmillHttp::from_env().unwrap());
        let wired = WmillClient::from_env(transport).unwrap();
        let out = wired
            .run_script_by_path("f/x", &serde_json::json!({}))
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out["metrics"]["a"], 1.0);

        let lines = seen.lock().unwrap();
        assert_eq!(
            lines.len(),
            2,
            "POST and result poll both reached the listener"
        );
        assert_eq!(
            lines[0], "POST /api/w/godon/jobs/run/p/f/x?parent_job=parent-1 HTTP/1.1",
            "the parent_job pair must ride the request line"
        );
        assert_eq!(
            lines[1],
            "GET /api/w/godon/jobs_u/completed/get_result_maybe/job-17?get_started=true HTTP/1.1",
            "the poll pair must ride the request line too"
        );
        std::env::remove_var("BASE_INTERNAL_URL");
        std::env::remove_var("WM_TOKEN");
        std::env::remove_var("WM_WORKSPACE");
        std::env::remove_var("WM_JOB_ID");
    }
}
