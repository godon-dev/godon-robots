//! The invocation surface — argv[1] names the component, the args
//! JSON arrives as argv[2] (the windmill bash-script `$1` shape, the
//! controller lane's receipted pattern), and the result prints to
//! stdout as JSON. One whole engine binary per the rung-10 ruling;
//! the header-script motion wires windmill to this binary later.
//!
//! Components:
//! - `worker` — the trial loop (args: `{config, systemtender_id?,
//!   run_id?, target_id?}`); returns the python main's summary.
//! - `preflight` — the strain preflights the controller's create
//!   executor calls (args: `{config}`; the strain type rides the
//!   config — one binary serves every family path).
//! - `recon-http` / `recon-prometheus` — the reconnaissance
//!   components (args: `{context, targets, settings}`).
//! - `effectuation-http` / `effectuation-ssh` — the effectuation
//!   components (args: `{context, targets, settings}`).
//!
//! The dispatch itself is testable with fakes; [`main.rs`] builds
//! the production dependencies from the environment (the platform's
//! `WM_*` env for windmill, `GODON_ARCHIVE_*` for the databases).

use std::sync::Arc;

use serde_json::{json, Value};

use godon_engine::ledger::TrialLedger;
use godon_engine::worker_loop::{Wmill, Worker, WorkerDeps};

/// The component dependencies — everything [`main.rs`] builds from
/// env, and everything the tests fake.
pub struct ComponentDeps {
    pub wmill: Arc<dyn Wmill>,
    pub recon_http: Arc<dyn godon_recon::http::ReconHttp>,
    pub prom: Arc<dyn godon_recon::prometheus::PromConnect>,
    pub effectuation_http: Arc<dyn godon_effectuation::http::Http>,
    pub effectuation_vars: Arc<dyn godon_effectuation::http::Variables>,
    pub playbook: Arc<dyn godon_effectuation::ssh::PlaybookRunner>,
    /// the components' sleeps (stabilization waits, retry ladders)
    pub sleep: Arc<dyn Fn(f64) + Send + Sync>,
}

impl ComponentDeps {
    /// The production dependencies: the platform env exactly as the python
    /// read it (`WM_*` for windmill, http transports otherwise).
    pub fn production() -> Result<Self, String> {
        let transport = Arc::new(godon_wmill::ReqwestWmillHttp::from_env()?);
        let client = Arc::new(godon_wmill::WmillClient::from_env(transport)?);
        Ok(ComponentDeps {
            wmill: client.clone(),
            recon_http: Arc::new(godon_recon::http::ReqwestReconHttp),
            prom: Arc::new(godon_recon::prometheus::ReqwestPromConnect),
            effectuation_http: Arc::new(godon_effectuation::http::ReqwestHttp),
            effectuation_vars: client.clone(),
            playbook: client,
            sleep: Arc::new(|secs| std::thread::sleep(std::time::Duration::from_secs_f64(secs))),
        })
    }
}

/// Run one component with the given args JSON; the result is the
/// job's return value (printed to stdout by the binary).
pub fn run_component(component: &str, args: &Value, deps: &ComponentDeps) -> Result<Value, String> {
    let context = args.get("context").cloned().unwrap_or(json!({}));
    let targets = args.get("targets").cloned().unwrap_or(json!([]));
    let settings = args.get("settings").cloned().unwrap_or(json!({}));
    let sleep = deps.sleep.clone();
    let mut sleep_fn = move |secs: f64| sleep(secs);

    match component {
        "recon-http" => Ok(godon_recon::http::main(
            &context,
            &targets,
            args.get("settings"),
            deps.recon_http.as_ref(),
            &mut sleep_fn,
        )),
        "recon-prometheus" => Ok(godon_recon::prometheus::main(
            &context,
            &targets,
            args.get("settings"),
            deps.prom.as_ref(),
            &mut sleep_fn,
        )),
        "effectuation-http" => Ok(godon_effectuation::http::main(
            &context,
            &targets,
            &settings,
            deps.effectuation_http.as_ref(),
            deps.effectuation_vars.as_ref(),
            &mut sleep_fn,
        )),
        "effectuation-ssh" => Ok(godon_effectuation::ssh::main(
            &context,
            &targets,
            &settings,
            deps.playbook.as_ref(),
            &mut sleep_fn,
        )),
        "worker" => run_worker(args, deps),
        "preflight" => {
            // The controller's create executor calls the strain
            // preflights at f/systemtender/strains/<family>/preflight
            // with {"config": <the tender's config>}. The wrapper
            // headers point every family path at this ONE arm; the
            // strain type rides the config (the worker boot's own
            // read). A RAISED preflight (python's TypeError
            // fallthroughs, ported verbatim) is the door's Err — the
            // job fails and the executor's warn-and-continue arm
            // answers a failed run, python's shape.
            let config = args
                .get("config")
                .cloned()
                .ok_or_else(|| "preflight requires a config argument".to_string())?;
            let strain_type = config
                .get("systemtender")
                .and_then(|s| s.get("type"))
                .or_else(|| config.get("type"))
                .and_then(|t| t.as_str())
                .ok_or_else(|| "preflight requires the strain type in the config".to_string())?;
            match godon_engine::strains::load_strain(strain_type)? {
                godon_engine::strains::Strain::LinuxPerformance => {
                    preflight_value(strain_linux_performance::validate_config(&config))
                }
                godon_engine::strains::Strain::BenchGreenhouse => {
                    preflight_value(strain_bench_greenhouse::validate_config(&config))
                }
                godon_engine::strains::Strain::BenchMicrogrid => {
                    preflight_value(strain_bench_microgrid::validate_config(&config))
                }
                godon_engine::strains::Strain::BenchGeneric => {
                    preflight_value(strain_bench_generic::validate_config(&config))
                }
            }
        }
        other => Err(format!(
            "unknown component {other:?} (worker | recon-http | recon-prometheus | effectuation-http | effectuation-ssh | preflight)"
        )),
    }
}

/// Serialize a strain preflight's verdict into the door's result
/// value — the controller's executor reads `result: "SUCCESS"` off
/// it (each family crate carries its own PreflightResult type; the
/// envelope bytes are identical).
fn preflight_value<T: serde::Serialize>(r: Result<T, String>) -> Result<Value, String> {
    r.and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
}

/// The door's result emission: stdout for manual runs, and the
/// result.json file the windmill image rail mounts at the working
/// dir — on that rail THE FILE is the job result (stdout stays
/// logs). Best-effort beside the stdout answer: an unwritable
/// working dir (a debug exec) warns, and stdout still carries it.
pub fn emit_result(result: &Value) {
    println!("{result}");
    if let Err(e) = write_result_file(std::path::Path::new("."), result) {
        log::info!(
            "result.json write failed ({e}) - the rail reads the file; stdout still carries the answer"
        );
    }
}

/// Write the rail's result file into `dir`. Split out so the suite
/// can pin the bytes without touching the crate tree.
pub fn write_result_file(dir: &std::path::Path, result: &Value) -> std::io::Result<()> {
    std::fs::write(dir.join("result.json"), result.to_string())
}

/// The worker arm — the python `main(config, systemtender_id,
/// run_id, target_id)`: build the production dependencies and the
/// ledger from the env, run the loop, answer the summary.
fn run_worker(args: &Value, deps: &ComponentDeps) -> Result<Value, String> {
    let config = args
        .get("config")
        .cloned()
        .ok_or_else(|| "worker requires a config argument".to_string())?;
    let systemtender_id = args.get("systemtender_id").cloned().unwrap_or(Value::Null);
    let run_id = args.get("run_id").cloned().unwrap_or(Value::Null);
    let target_id = args.get("target_id").cloned().unwrap_or(Value::Null);

    // the ledger's database name — the python `__init__` derivation
    let uuid = config
        .get("systemtender")
        .and_then(|s| s.get("uuid"))
        .or_else(|| config.get("systemtender").and_then(|s| s.get("name")))
        .and_then(|u| u.as_str())
        .unwrap_or("unknown");
    let tender_db = format!("systemtender_{}", uuid.replace('-', "_"));

    let pw = std::env::var("GODON_ARCHIVE_DB_PASSWORD").unwrap_or_else(|_| "postgres".into());
    let user = std::env::var("GODON_ARCHIVE_DB_USER").unwrap_or_else(|_| "postgres".into());
    let host =
        std::env::var("GODON_ARCHIVE_DB_SERVICE_HOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("GODON_ARCHIVE_DB_SERVICE_PORT").unwrap_or_else(|_| "5432".into());
    let dsn = format!("postgresql://{user}:{pw}@{host}:{port}/{tender_db}");

    let ledger = Arc::new(TrialLedger::new(&dsn).map_err(|e| e.to_string())?);
    let worker_deps = WorkerDeps::production(deps.wmill.clone(), &tender_db);
    type ProductionCoordinator =
        godon_engine::coordinator::ProbeCoordinator<Arc<dyn godon_engine::ledger::Connect + Send + Sync>>;
    let mut worker =
        Worker::<ProductionCoordinator>::connect(config, worker_deps, ledger).map_err(|e| e.to_string())?;
    worker.run()?;

    // the python main's summary
    let minimize = worker
        .config
        .get("objectives")
        .and_then(|o| o.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("direction"))
        .and_then(|d| d.as_str())
        .map(|d| d.to_lowercase() == "minimize")
        .unwrap_or(false);
    Ok(json!({
        "worker_id": worker.cfg.worker_id(),
        "systemtender_type": worker.cfg.systemtender_type,
        "systemtender_id": worker.cfg.systemtender_uuid,
        "run_id": run_id,
        "target_id": target_id,
        "total_trials": worker.ledger.count().map_err(|e| e.to_string())?,
        "best_trial": if worker.ledger.best_trial(minimize).map_err(|e| e.to_string())?.is_some() { 1 } else { 0 },
        "status": "completed",
        // python logged the start line with these when present; the
        // result carries what the caller passed through
        "_systemtender_id_arg": systemtender_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use godon_effectuation::http::HttpErr;
    use godon_recon::http::ReconHttpErr;
    use std::sync::Mutex;

    struct FakeRecon {
        body: Value,
    }

    impl godon_recon::http::ReconHttp for FakeRecon {
        fn get_json(&self, _url: &str, _t: f64) -> Result<Value, ReconHttpErr> {
            Ok(self.body.clone())
        }
    }

    struct FakeProm;
    impl godon_recon::prometheus::PromConnect for FakeProm {
        fn custom_query(
            &self,
            _url: &str,
            _q: &str,
        ) -> Result<Value, godon_recon::prometheus::PromErr> {
            Ok(json!({"resultType": "scalar", "result": [1, "7.5"]}))
        }
    }

    struct FakeEff;
    impl godon_effectuation::http::Http for FakeEff {
        fn request(
            &self,
            _m: &str,
            _u: &str,
            _h: &[(String, String)],
            _b: &Value,
            _t: f64,
        ) -> Result<godon_effectuation::http::HttpResponse, HttpErr> {
            Ok(godon_effectuation::http::HttpResponse {
                ok: true,
                status_code: 200,
                body: Vec::new(),
            })
        }
    }

    struct FakeVars;
    impl godon_effectuation::http::Variables for FakeVars {
        fn get_variable(&self, _p: &str) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }

    struct FakePlaybook;
    impl godon_effectuation::ssh::PlaybookRunner for FakePlaybook {
        fn run_script_by_path(&self, _p: &str, _a: &Value) -> Result<Value, String> {
            Ok(json!({"success": true}))
        }
    }

    struct FakeWmill;
    impl Wmill for FakeWmill {
        fn run_script_by_path(&self, _p: &str, _a: &Value) -> Result<Value, String> {
            Ok(json!({}))
        }
        fn set_state(&self, _s: &Value) -> Result<(), String> {
            Ok(())
        }
        fn get_job_args(&self, _job_id: &str) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    fn deps() -> ComponentDeps {
        ComponentDeps {
            wmill: Arc::new(FakeWmill),
            recon_http: Arc::new(FakeRecon {
                body: json!({"growth_rate": 0.85}),
            }),
            prom: Arc::new(FakeProm),
            effectuation_http: Arc::new(FakeEff),
            effectuation_vars: Arc::new(FakeVars),
            playbook: Arc::new(FakePlaybook),
            sleep: Arc::new(|_| {}),
        }
    }

    #[test]
    fn component_dispatches_recon_http() {
        let args = json!({
            "context": {"objectives": [{"name": "growth_rate", "reconnaissance": {
                "service": "http", "path": "/m", "key": "growth_rate",
                "stabilization_seconds": 0, "samples": 1, "interval": 0, "aggregation": "median"}}]},
            "targets": [],
            "settings": {},
        });
        let out = run_component("recon-http", &args, &deps()).unwrap();
        assert_eq!(out["status"], "completed");
        assert_eq!(out["metrics"]["growth_rate"], 0.85);
    }

    #[test]
    fn component_dispatches_recon_prometheus() {
        let args = json!({
            "context": {"objectives": [{"name": "t", "reconnaissance": {"service": "prometheus", "query": "up"}}]},
            "targets": [],
        });
        let out = run_component("recon-prometheus", &args, &deps()).unwrap();
        assert_eq!(out["metrics"]["t"], 7.5);
    }

    #[test]
    fn component_dispatches_effectuation_ssh() {
        let args = json!({
            "context": {"effectuation": {"type": "ssh"}},
            "targets": [{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}],
            "settings": {"vm.swappiness": 10},
        });
        let out = run_component("effectuation-ssh", &args, &deps()).unwrap();
        assert_eq!(out["successful_changes"], 1);
    }

    #[test]
    fn component_dispatches_effectuation_http() {
        let args = json!({
            "context": {"effectuation": {"type": "http", "endpoint_config": {"path": "/c", "method": "POST"}}},
            "targets": [{"id": "t1", "url": "http://t:80"}],
            "settings": {"a": 1},
        });
        let out = run_component("effectuation-http", &args, &deps()).unwrap();
        assert_eq!(out["successful_changes"], 1);
    }

    #[test]
    fn component_rejects_unknown_name() {
        let err = run_component("nope", &json!({}), &deps()).unwrap_err();
        assert!(err.contains("unknown component"), "{err}");
    }

    #[test]
    fn component_worker_requires_config() {
        let err = run_component("worker", &json!({}), &deps()).unwrap_err();
        assert!(err.contains("config"), "{err}");
    }

    #[test]
    fn component_dispatches_preflight_to_the_strain_crate() {
        // the controller's preflight call shape: {"config": <tender
        // config>} — the strain type rides config.systemtender.type
        let args = json!({
            "config": {
                "systemtender": {"name": "t", "type": "bench_greenhouse"},
                "settings": {"greenhouse": {"zones": 2}},
            }
        });
        let out = run_component("preflight", &args, &deps()).unwrap();
        assert_eq!(out["result"], "SUCCESS", "{out}");
    }

    #[test]
    fn component_preflight_refuses_unknown_strain() {
        let args = json!({"config": {"systemtender": {"type": "nope"}, "settings": {}}});
        let err = run_component("preflight", &args, &deps()).unwrap_err();
        assert!(err.contains("Unknown strain type: 'nope'"), "{err}");
    }

    #[test]
    fn component_preflight_requires_config() {
        let err = run_component("preflight", &json!({}), &deps()).unwrap_err();
        assert_eq!(err, "preflight requires a config argument");
    }

    #[test]
    fn result_file_carries_the_envelope_bytes() {
        let dir = std::env::temp_dir().join(format!("engine-result-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = json!({"result": "SUCCESS", "data": {"message": "ok"}});
        write_result_file(&dir, &result).unwrap();
        let bytes = std::fs::read_to_string(dir.join("result.json")).unwrap();
        assert_eq!(bytes, result.to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

}
