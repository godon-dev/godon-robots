//! Port of `effectuation/ssh.py` — the ansible-playbook effectuation:
//! per target, run the windmill playbook script by path with the
//! target's connection vars and the settings as `params`; collect
//! the rows; the stabilization wait; the summary result.
//! Statement-by-statement against the python original; the tests
//! mirror `tests/unit/test_effectuation_logic.py`'s SSH classes.

use serde_json::{json, Value};

/// The windmill script-runner edge — the python script called
/// `wmill.run_script_by_path`; the real client implements this.
pub trait PlaybookRunner: Send + Sync {
    fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String>;
}

impl PlaybookRunner for godon_wmill::WmillClient {
    fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String> {
        godon_wmill::WmillClient::run_script_by_path(self, path, args)
    }
}

/// python truthiness for the playbook result's `success` read:
/// bools are themselves; null is false; numbers/strings/arrays/
/// objects are true when non-empty/non-zero.
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `main` — the per-target loop, the stabilization wait, the summary.
pub fn main(
    context: &Value,
    targets: &Value,
    settings: &Value,
    gate: &dyn PlaybookRunner,
    sleep: &mut dyn FnMut(f64),
) -> Value {
    let effectuation_config = context.get("effectuation").cloned().unwrap_or(json!({}));
    let playbook_path = effectuation_config
        .get("playbook_path")
        .and_then(|p| p.as_str())
        .unwrap_or("f/systemtender/strains/linux_performance/effectuate_settings");
    let stabilization_seconds = effectuation_config
        .get("stabilization_seconds")
        .and_then(|s| s.as_f64())
        .unwrap_or(0.0);

    let targets = targets.as_array().cloned().unwrap_or_default();
    eprintln!(
        "SSH effectuation for {} targets via playbook {playbook_path}",
        targets.len()
    );
    eprintln!("Settings: {}", settings_keys(settings));

    let mut all_results: Vec<Value> = Vec::new();

    for target in &targets {
        let target_id = target.get("id").cloned().unwrap_or(Value::Null);
        let address = target.get("address").cloned().unwrap_or(Value::Null);
        let username = target
            .get("username")
            .and_then(|u| u.as_str())
            .unwrap_or("root");
        let ssh_key_path = target
            .get("ssh_key_variable_path")
            .cloned()
            .unwrap_or(Value::Null);

        eprintln!("Processing target {target_id}: {address}");

        let target_vars = json!({
            "target_hostname": address,
            "username": username,
            "ssh_key_variable": ssh_key_path,
            "params": settings,
        });

        let row = match gate.run_script_by_path(playbook_path, &target_vars) {
            Ok(result) => {
                if py_truthy(result.get("success").unwrap_or(&Value::Null)) {
                    json!({
                        "target_id": target_id,
                        "address": address,
                        "success": true,
                        "result": result,
                    })
                } else {
                    json!({
                        "target_id": target_id,
                        "address": address,
                        "success": false,
                        "error": error_of(&result),
                    })
                }
            }
            Err(e) => {
                eprintln!("Failed to process target {target_id}: {e}");
                json!({
                    "target_id": target_id,
                    "address": address,
                    "success": false,
                    "error": format!("Target processing failed: {e}"),
                })
            }
        };
        all_results.push(row);
    }

    if stabilization_seconds > 0.0 {
        eprintln!("Waiting {stabilization_seconds}s for system stabilization");
        sleep(stabilization_seconds);
    }

    let success_count = all_results
        .iter()
        .filter(|r| r.get("success").and_then(|s| s.as_bool()).unwrap_or(false))
        .count();
    let total_count = all_results.len();

    eprintln!("Effectuation completed: {success_count}/{total_count} successful");

    json!({
        "status": "completed",
        "targets_count": targets.len(),
        "successful_changes": success_count,
        "failed_changes": total_count - success_count,
        "results": all_results,
    })
}

/// python `result.get('error', 'Unknown error')` — a string error
/// reads as itself; anything else renders as its JSON form.
fn error_of(result: &Value) -> Value {
    match result.get("error") {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(other) => Value::String(other.to_string()),
        None => Value::String("Unknown error".to_string()),
    }
}

fn settings_keys(settings: &Value) -> Value {
    match settings.as_object() {
        Some(o) => Value::Array(o.keys().map(|k| Value::String(k.clone())).collect()),
        None => Value::Array(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakePlaybookRunner {
        responses: Mutex<VecDeque<Result<Value, String>>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakePlaybookRunner {
        fn with(responses: Vec<Result<Value, String>>) -> Self {
            FakePlaybookRunner {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl PlaybookRunner for FakePlaybookRunner {
        fn run_script_by_path(&self, path: &str, args: &Value) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push((path.to_string(), args.clone()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err("no more responses".into()))
        }
    }

    fn no_sleep(_: f64) {}

    fn ssh_ctx(extra: Value) -> Value {
        let mut eff = json!({"type": "ssh"});
        if let (Some(dst), Some(src)) = (eff.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        json!({"effectuation": eff})
    }

    #[test]
    fn single_target_success() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": true}))]);
        let context = ssh_ctx(json!({"playbook_path": "f/test/playbook"}));
        let targets = json!([{
            "id": "t1", "address": "10.0.0.1", "username": "admin",
            "ssh_key_variable_path": "u/user/key"
        }]);
        let settings = json!({"net.ipv4.tcp_rmem": "4096 131072 174760"});

        let out = main(&context, &targets, &settings, &gate, &mut no_sleep);

        assert_eq!(out["status"], "completed");
        assert_eq!(out["successful_changes"], 1);
        assert_eq!(out["failed_changes"], 0);
        assert_eq!(out["results"][0]["success"], true);

        let calls = gate.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "f/test/playbook");
        assert_eq!(calls[0].1["params"], settings);
        assert_eq!(calls[0].1["username"], "admin");
        assert_eq!(calls[0].1["target_hostname"], "10.0.0.1");
        assert_eq!(calls[0].1["ssh_key_variable"], "u/user/key");
    }

    #[test]
    fn multiple_targets_mixed_results() {
        let gate = FakePlaybookRunner::with(vec![
            Ok(json!({"success": true})),
            Ok(json!({"success": false, "error": "Connection refused"})),
            Ok(json!({"success": true})),
        ]);
        let targets = json!([
            {"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"},
            {"id": "t2", "address": "10.0.0.2", "ssh_key_variable_path": "u/key"},
            {"id": "t3", "address": "10.0.0.3", "ssh_key_variable_path": "u/key"},
        ]);

        let out = main(
            &ssh_ctx(json!({})),
            &targets,
            &json!({"vm.swappiness": 10}),
            &gate,
            &mut no_sleep,
        );

        assert_eq!(out["successful_changes"], 2);
        assert_eq!(out["failed_changes"], 1);
        assert_eq!(out["results"][1]["success"], false);
        let err = out["results"][1]["error"].as_str().unwrap();
        assert!(err.contains("Connection refused"), "{err}");
    }

    #[test]
    fn target_exception_handled() {
        let gate = FakePlaybookRunner::with(vec![Err("SSH timeout".to_string())]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);

        let out = main(
            &ssh_ctx(json!({})),
            &targets,
            &json!({"net.core.somaxconn": 4096}),
            &gate,
            &mut no_sleep,
        );

        assert_eq!(out["successful_changes"], 0);
        assert_eq!(out["failed_changes"], 1);
        let err = out["results"][0]["error"].as_str().unwrap();
        assert!(err.contains("SSH timeout"), "{err}");
    }

    #[test]
    fn default_username_is_root() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": true}))]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);

        main(
            &ssh_ctx(json!({})),
            &targets,
            &json!({}),
            &gate,
            &mut no_sleep,
        );

        let calls = gate.calls.lock().unwrap();
        assert_eq!(calls[0].1["username"], "root");
    }

    #[test]
    fn default_playbook_path() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": true}))]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);

        main(
            &ssh_ctx(json!({})),
            &targets,
            &json!({}),
            &gate,
            &mut no_sleep,
        );

        let calls = gate.calls.lock().unwrap();
        assert_eq!(
            calls[0].0,
            "f/systemtender/strains/linux_performance/effectuate_settings"
        );
    }

    #[test]
    fn missing_error_reads_unknown() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": false}))]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);

        let out = main(
            &ssh_ctx(json!({})),
            &targets,
            &json!({}),
            &gate,
            &mut no_sleep,
        );
        assert_eq!(out["results"][0]["error"], "Unknown error");
    }

    #[test]
    fn stabilization_wait() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": true}))]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);
        let mut slept = Vec::new();
        main(
            &ssh_ctx(json!({"stabilization_seconds": 30})),
            &targets,
            &json!({}),
            &gate,
            &mut |s| slept.push(s),
        );
        assert_eq!(slept, vec![30.0]);
    }

    #[test]
    fn no_stabilization_by_default() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": true}))]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);
        let mut slept = Vec::new();
        main(&ssh_ctx(json!({})), &targets, &json!({}), &gate, &mut |s| {
            slept.push(s)
        });
        assert!(slept.is_empty());
    }

    #[test]
    fn contract_compliance_keys() {
        let gate = FakePlaybookRunner::with(vec![Ok(json!({"success": true}))]);
        let targets =
            json!([{"id": "t1", "address": "10.0.0.1", "ssh_key_variable_path": "u/key"}]);
        let out = main(
            &ssh_ctx(json!({})),
            &targets,
            &json!({"param": "value"}),
            &gate,
            &mut no_sleep,
        );
        for key in ["status", "successful_changes", "failed_changes", "results"] {
            assert!(out.get(key).is_some(), "missing {key}");
        }
    }
}
