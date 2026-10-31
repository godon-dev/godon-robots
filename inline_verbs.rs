//! The inline verb rail — MAKESHIFT, by holder ruling 2026-10-31.
//!
//! # Status: MAKESHIFT — up for revisal
//!
//! The two HTTP verbs (effectuation-http, recon-http,
//! recon-prometheus) run IN-PROCESS in the tender's own body, behind
//! the engine's [`Wmill`] seam. This is a progress ruling, not a
//! settled shape. The separation points that STAND (deliberation
//! 2026-10-31, mind-chime):
//!
//! 1. **Effectuation failures will be many.** Ordinary failures are
//!    Err-shaped and identical in every shape; the ones that ESCAPE
//!    (panic, unbounded hang) cost a worker restart + a room blink
//!    here, vs one failed trial in a separated shape. WATCH:
//!    failure→restart pairs in loki (godon-robots stream).
//! 2. **Verb count will grow.** Internalised verbs are welded into
//!    the tender image — the per-type-image door (each declared
//!    script can name its own image) is closed while this stands.
//! 3. **The tender must stay the small deciding actor.** The seam
//!    discipline below is the guard; verb logic never enters the
//!    walk's files.
//!
//! The revisal shape, settled in the same deliberation: ONE permanent
//! verb job per tender (windmill vessel), config at spawn, one
//! registration row in the tender's DB, ONE blocking HTTP call per
//! trial, sequence in the handler body (effectuate → settle →
//! measure → respond), hard timeouts everywhere. NOT BUILT — revisit
//! when (1) or (2) bites.
//!
//! What rides windmill while this stands: effectuation-ssh
//! (playbook-backed — it cannot leave the rail without new
//! machinery), strain preflights, `set_state`, manual verb runs.
//!
//! Escape hatch: `GODON_VERB_RAIL=wmill` (read once, at
//! construction) forces every verb back onto the windmill rail —
//! revisal by env flip, not re-release.

use std::sync::Arc;

use godon_engine::worker_loop::Wmill;
use serde_json::Value;

use crate::invocation::{run_component, ComponentDeps};
use crate::router::component_for_path;

/// The verbs internalised into the tender's body. Deliberately a
/// named set — anything not listed rides the rail.
const INLINE_COMPONENTS: [&str; 3] = ["effectuation-http", "recon-http", "recon-prometheus"];

pub struct InlineVerbs {
    /// the windmill client — every non-inline path, plus set_state
    /// and job args, ride it unchanged
    inner: Arc<dyn Wmill>,
    deps: ComponentDeps,
    /// GODON_VERB_RAIL=wmill at construction — the revisal flip
    force_rail: bool,
}

impl InlineVerbs {
    pub fn new(inner: Arc<dyn Wmill>, deps: ComponentDeps) -> Self {
        let force_rail = std::env::var("GODON_VERB_RAIL")
            .map(|v| v.trim().eq_ignore_ascii_case("wmill"))
            .unwrap_or(false);
        Self {
            inner,
            deps,
            force_rail,
        }
    }

    #[cfg(test)]
    fn with_rail_flag(inner: Arc<dyn Wmill>, deps: ComponentDeps, force_rail: bool) -> Self {
        Self {
            inner,
            deps,
            force_rail,
        }
    }
}

impl Wmill for InlineVerbs {
    fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String> {
        if !self.force_rail {
            if let Ok(component) = component_for_path(path) {
                if INLINE_COMPONENTS.contains(&component) {
                    // the SAME dispatch the windmill shell uses —
                    // byte-identical args in, byte-identical result out
                    return run_component(component, args, &self.deps);
                }
            }
        }
        self.inner.run_script_by_path(path, args)
    }

    fn set_state(&self, state: &Value) -> Result<(), String> {
        self.inner.set_state(state)
    }

    fn get_job_args(&self, job_id: &str) -> Result<Value, String> {
        self.inner.get_job_args(job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invocation::ComponentDeps;
    use serde_json::json;
    use std::sync::Mutex;

    struct RailRecorder {
        calls: Mutex<Vec<String>>,
    }
    impl Wmill for RailRecorder {
        fn run_script_by_path(&self, path: &str, _a: &Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push(path.to_string());
            Ok(json!({"rail": true}))
        }
        fn set_state(&self, _s: &Value) -> Result<(), String> {
            Ok(())
        }
        fn get_job_args(&self, _id: &str) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    struct OkProm;
    impl godon_recon::prometheus::PromConnect for OkProm {
        fn custom_query(
            &self,
            _url: &str,
            _q: &str,
        ) -> Result<Value, godon_recon::prometheus::PromErr> {
            Ok(json!({"resultType": "scalar", "result": [0, "1"]}))
        }
    }
    struct OkHttp;
    impl godon_recon::http::ReconHttp for OkHttp {
        fn get_json(&self, _url: &str, _t: f64) -> Result<Value, godon_recon::http::ReconHttpErr> {
            Ok(json!({"v": 1}))
        }
    }
    struct OkEffect;
    impl godon_effectuation::http::Http for OkEffect {
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
    struct OkVars;
    impl godon_effectuation::http::Variables for OkVars {
        fn get_variable(&self, _p: &str) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }
    struct OkPlaybook;
    impl godon_effectuation::ssh::PlaybookRunner for OkPlaybook {
        fn run_script_by_path(&self, _p: &str, _a: &Value) -> Result<Value, String> {
            Ok(json!({}))
        }
    }

    fn deps() -> ComponentDeps {
        ComponentDeps {
            wmill: Arc::new(RailRecorder {
                calls: Mutex::new(Vec::new()),
            }),
            recon_http: Arc::new(OkHttp),
            prom: Arc::new(OkProm),
            effectuation_http: Arc::new(OkEffect),
            effectuation_vars: Arc::new(OkVars),
            playbook: Arc::new(OkPlaybook),
            sleep: Arc::new(|_s: f64| {}),
        }
    }

    fn inline_args() -> Value {
        json!({
            "context": {
                "objectives": [{"name": "objective_0",
                    "reconnaissance": {"service": "prometheus", "query": "up",
                        "samples": 1, "interval": 0.0, "stabilization_seconds": 0.0}}],
            },
            "targets": [],
            "settings": {},
        })
    }

    #[test]
    fn the_inline_verbs_run_in_process() {
        let inline = InlineVerbs::new(
            Arc::new(RailRecorder {
                calls: Mutex::new(Vec::new()),
            }),
            deps(),
        );
        let out = inline
            .run_script_by_path("f/reconnaissance/prometheus", &inline_args())
            .unwrap();
        // the component ran: prometheus main's own result envelope
        assert_eq!(out["status"], "completed", "{out}");
        assert!(out.get("metrics").is_some(), "{out}");
    }

    #[test]
    fn ssh_preflight_and_unknown_paths_ride_the_rail() {
        let rail = Arc::new(RailRecorder {
            calls: Mutex::new(Vec::new()),
        });
        let inline = InlineVerbs::new(rail.clone(), deps());
        for path in [
            "f/effectuation/ssh",
            "f/systemtender/strains/bench_generic/preflight",
            "f/some/other/script",
        ] {
            let out = inline.run_script_by_path(path, &json!({})).unwrap();
            assert_eq!(out["rail"], true, "{path} should ride the rail: {out}");
        }
        let calls = rail.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls.contains(&"f/effectuation/ssh".to_string()));
    }

    #[test]
    fn the_escape_hatch_forces_every_verb_onto_the_rail() {
        let rail = Arc::new(RailRecorder {
            calls: Mutex::new(Vec::new()),
        });
        let forced = InlineVerbs::with_rail_flag(rail.clone(), deps(), true);
        let out = forced
            .run_script_by_path("f/reconnaissance/prometheus", &inline_args())
            .unwrap();
        assert_eq!(out["rail"], true, "{out}");
        assert_eq!(rail.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn state_and_args_always_ride_the_rail() {
        let rail = Arc::new(RailRecorder {
            calls: Mutex::new(Vec::new()),
        });
        let inline = InlineVerbs::new(rail.clone(), deps());
        inline.set_state(&json!({"a": 1})).unwrap();
        inline.get_job_args("job-1").unwrap();
        assert!(rail.calls.lock().unwrap().is_empty());
    }
}
