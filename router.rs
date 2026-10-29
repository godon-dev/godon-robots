//! The router — the platform run's two halves (the holder's call:
//! the rust entrypoint IS the router, and it lives here, not in the
//! tender engine's lib).
//!
//! Source-read receipts (windmill v1.835.0): the sandbox child
//! receives the reserved job env — WM_JOB_PATH (the job's own
//! runnable path), WM_JOB_ID, WM_TOKEN, WM_WORKSPACE, WM_BASE_URL —
//! delivered via nsjail keep_env ("so wmill/API calls work"), and
//! `jobs_u/get_args/{id}` returns the job's args raw. Together: a
//! no-argv entrypoint routes itself and fetches its own payload.

use serde_json::Value;

use crate::invocation::ComponentDeps;

/// Route a platform job path to the door component. The windmill
/// scripts are one-line headers with EMPTY bodies, and this map
/// plus the rail's WM_JOB_PATH decides what runs — no per-part
/// wrapper anywhere.
pub fn component_for_path(job_path: &str) -> Result<&'static str, String> {
    match job_path {
        "f/systemtender/engine/systemtender_worker" => Ok("worker"),
        "f/reconnaissance/http" => Ok("recon-http"),
        "f/reconnaissance/prometheus" => Ok("recon-prometheus"),
        "f/effectuation/http" => Ok("effectuation-http"),
        "f/effectuation/ssh" => Ok("effectuation-ssh"),
        p if p.starts_with("f/systemtender/strains/") && p.ends_with("/preflight") => {
            Ok("preflight")
        }
        other => Err(format!(
            "unknown job path {other:?} (not a component the engine serves)"
        )),
    }
}

/// The platform run: NO argv — the rail's reserved env instead.
/// WM_JOB_PATH routes, WM_JOB_ID serves this job's own args via
/// jobs_u/get_args — the caller's keyed object rides the job row
/// verbatim, so callers change NOTHING. Manual and wrapper
/// invocations keep the argv door.
pub fn run_platform_job(deps: &ComponentDeps) -> Result<Value, String> {
    let job_path = std::env::var("WM_JOB_PATH")
        .map_err(|_| "platform run requires WM_JOB_PATH".to_string())?;
    let component = component_for_path(&job_path)?;
    let job_id =
        std::env::var("WM_JOB_ID").map_err(|_| "platform run requires WM_JOB_ID".to_string())?;
    let args = deps.wmill.get_job_args(&job_id)?;
    crate::invocation::run_component(component, &args, deps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invocation::ComponentDeps;
    use godon_engine::worker_loop::Wmill;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_router_maps_every_served_path() {
        assert_eq!(
            component_for_path("f/systemtender/engine/systemtender_worker").unwrap(),
            "worker"
        );
        assert_eq!(
            component_for_path("f/reconnaissance/http").unwrap(),
            "recon-http"
        );
        assert_eq!(
            component_for_path("f/reconnaissance/prometheus").unwrap(),
            "recon-prometheus"
        );
        assert_eq!(
            component_for_path("f/effectuation/http").unwrap(),
            "effectuation-http"
        );
        assert_eq!(
            component_for_path("f/effectuation/ssh").unwrap(),
            "effectuation-ssh"
        );
        for family in [
            "bench_generic",
            "bench_greenhouse",
            "bench_microgrid",
            "linux_performance",
        ] {
            assert_eq!(
                component_for_path(&format!("f/systemtender/strains/{family}/preflight")).unwrap(),
                "preflight"
            );
        }
        let err = component_for_path("f/other/thing").unwrap_err();
        assert!(err.contains("unknown job path"), "{err}");
    }

    struct FakeArgsWmill {
        args: Value,
        asked: Mutex<Vec<String>>,
    }
    impl Wmill for FakeArgsWmill {
        fn run_script_by_path(&self, _p: &str, _a: &Value) -> Result<Value, String> {
            Ok(json!({}))
        }
        fn set_state(&self, _s: &Value) -> Result<(), String> {
            Ok(())
        }
        fn get_job_args(&self, job_id: &str) -> Result<Value, String> {
            self.asked.lock().unwrap().push(job_id.to_string());
            Ok(self.args.clone())
        }
    }

    /// The minimal deps for a platform-run test: the preflight arm
    /// touches only the wmill edge (the args source).
    fn preflight_deps(wmill: Arc<FakeArgsWmill>) -> ComponentDeps {
        crate::invocation::ComponentDeps {
            wmill,
            recon_http: Arc::new(NoopRecon),
            prom: Arc::new(NoopProm),
            effectuation_http: Arc::new(NoopHttp),
            effectuation_vars: Arc::new(NoopVars),
            playbook: Arc::new(NoopPlaybook),
            sleep: Arc::new(|_s: f64| {}),
        }
    }

    struct NoopRecon;
    impl godon_recon::http::ReconHttp for NoopRecon {
        fn get_json(&self, _url: &str, _t: f64) -> Result<Value, godon_recon::http::ReconHttpErr> {
            Ok(json!({}))
        }
    }
    struct NoopProm;
    impl godon_recon::prometheus::PromConnect for NoopProm {
        fn custom_query(
            &self,
            _url: &str,
            _q: &str,
        ) -> Result<Value, godon_recon::prometheus::PromErr> {
            Ok(json!({}))
        }
    }
    struct NoopHttp;
    impl godon_effectuation::http::Http for NoopHttp {
        fn request(
            &self,
            _m: &str,
            _u: &str,
            _h: &[(String, String)],
            _b: &Value,
            _t: f64,
        ) -> Result<godon_effectuation::http::HttpResponse, godon_effectuation::http::HttpErr>
        {
            Ok(godon_effectuation::http::HttpResponse {
                ok: true,
                status_code: 200,
                body: Vec::new(),
            })
        }
    }
    struct NoopVars;
    impl godon_effectuation::http::Variables for NoopVars {
        fn get_variable(&self, _p: &str) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }
    struct NoopPlaybook;
    impl godon_effectuation::ssh::PlaybookRunner for NoopPlaybook {
        fn run_script_by_path(&self, _p: &str, _a: &Value) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    #[test]
    fn the_platform_run_routes_by_path_and_serves_its_own_args() {
        // the rail's reserved env: WM_JOB_PATH routes, WM_JOB_ID
        // names the args row; the caller's keyed object arrives
        // verbatim (one env-mutating test — no parallel twins)
        std::env::set_var(
            "WM_JOB_PATH",
            "f/systemtender/strains/bench_greenhouse/preflight",
        );
        std::env::set_var("WM_JOB_ID", "018f-deadbeef");
        let wmill = Arc::new(FakeArgsWmill {
            args: json!({
                "config": {
                    "systemtender": {"name": "t", "type": "bench_greenhouse"},
                    "settings": {"greenhouse": {"zones": 2}},
                }
            }),
            asked: Mutex::new(Vec::new()),
        });
        let d = preflight_deps(wmill.clone());
        let out = run_platform_job(&d).unwrap();
        assert_eq!(out["result"], "SUCCESS", "{out}");
        assert_eq!(
            *wmill.asked.lock().unwrap(),
            vec!["018f-deadbeef".to_string()]
        );
        std::env::remove_var("WM_JOB_PATH");
        std::env::remove_var("WM_JOB_ID");

        // no env, no argv: the router refuses
        let err = run_platform_job(&d).unwrap_err();
        assert_eq!(err, "platform run requires WM_JOB_PATH");
    }
}
