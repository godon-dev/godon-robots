//! The microgrid create-time preflight — statement-by-statement port
//! of `strains/bench_microgrid/preflight.py` (the rung-9 create-time
//! check the controller's executor calls through windmill).
//!
//! Python's `main` returns the result dicts OR raises: the meta read
//! sits OUTSIDE the try, so a null `meta` raises straight out of
//! `main` — mirrored by `Err` carrying the raise text. Everything
//! inside the try maps to the `"Preflight validation error: {e}"`
//! envelope, python's except verbatim.

use serde::Serialize;
use serde_json::Value;

use crate::registry;

#[derive(Debug, Serialize, PartialEq)]
pub struct SuccessData {
    pub message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

/// The python dict pair, rendered exactly:
/// `{"result": "SUCCESS", "data": {...}}` /
/// `{"result": "FAILURE", "error": "..."}`.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "result", rename_all = "UPPERCASE")]
pub enum PreflightResult {
    Success { data: SuccessData },
    Failure { error: String },
}

// --- python semantics helpers (duplicated per family crate, the
// estate's own shape — python carries three copy-paste preflights) ---

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

fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn py_get<'a>(v: &'a Value, key: &str) -> Result<Option<&'a Value>, String> {
    match v {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            py_type(other)
        )),
    }
}

fn py_get_or<'a>(v: &'a Value, key: &str, default: &'a Value) -> Result<&'a Value, String> {
    Ok(py_get(v, key)?.unwrap_or(default))
}

enum Constraints<'a> {
    Items(Vec<&'a Value>),
}

fn normalize_constraints<'a>(
    raw: &'a Value,
    ctx: &str,
    errors: &mut Vec<String>,
) -> Option<Constraints<'a>> {
    if let Some(m) = raw.as_object() {
        if m.contains_key("values") {
            return Some(Constraints::Items(vec![raw]));
        }
        errors.push(format!("{ctx}: constraints dict must have 'values' key"));
        return None;
    }
    if let Some(a) = raw.as_array() {
        return Some(Constraints::Items(a.iter().collect()));
    }
    errors.push(format!("{ctx}: constraints must be a list"));
    None
}

fn any_constraint_with(items: &[&Value], has: fn(&serde_json::Map<String, Value>) -> bool) -> bool {
    items.iter().any(|c| c.as_object().map_or(false, has))
}

/// Port of `preflight.main(config=None, strict_mode=True)`.
pub fn main(config: Option<&Value>, strict_mode: bool) -> Result<PreflightResult, String> {
    // python: `if not config` — None OR an empty dict
    let Some(config) = config.filter(|c| py_truthy(c)) else {
        return Ok(PreflightResult::Failure {
            error: "Missing config parameter".to_string(),
        });
    };

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // meta read sits OUTSIDE python's try: a null meta raises
    // TypeError out of main (uncaught there too).
    let mut strict = strict_mode;
    match py_get(config, "meta")? {
        None => {}
        Some(Value::Object(m)) => {
            if let Some(v) = m.get("strict_validation") {
                strict = py_truthy(v);
            }
        }
        // `'strict_validation' in None` raises; python's `in` on a
        // LIST does a membership check and passes — only null raises
        Some(Value::Null) => {
            return Err("argument of type 'NoneType' is not iterable".to_string());
        }
        Some(_) => {}
    }

    match validate(config, strict, &mut errors, &mut warnings) {
        Ok(r) => Ok(r),
        Err(e) => Ok(PreflightResult::Failure {
            error: format!("Preflight validation error: {e}"),
        }),
    }
}

/// The try-body; `Err` is the raised exception the except catches.
fn validate(
    config: &Value,
    strict: bool,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<PreflightResult, String> {
    let empty = Value::Object(Default::default());
    let settings = py_get_or(config, "settings", &empty)?;
    let mg_settings = py_get_or(settings, "microgrid", settings)?;

    if !mg_settings.is_object() {
        return Ok(PreflightResult::Failure {
            error: "settings.microgrid must be a dict".to_string(),
        });
    }

    for (param_name, param_config) in mg_settings.as_object().expect("checked above") {
        if !param_config.is_object() {
            errors.push(format!("settings.microgrid.{param_name}: must be a dict"));
            continue;
        }

        let ctx = format!("settings.microgrid.{param_name}");
        let entry = registry::lookup(param_name);
        if entry.is_none() {
            if strict {
                errors.push(format!(
                    "{ctx}: unsupported parameter. Supported: {}",
                    registry::parameter_registry()
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            } else {
                warnings.push(format!(
                    "{ctx}: unknown parameter, skipping registry validation"
                ));
            }
        }

        let Some(raw_constraints) = param_config.get("constraints") else {
            errors.push(format!("{ctx}: missing 'constraints'"));
            continue;
        };

        let Some(Constraints::Items(items)) = normalize_constraints(raw_constraints, &ctx, errors)
        else {
            continue;
        };

        if let Some(e) = entry {
            let registry_type = &e.def.kind;
            if registry_type == "categorical" {
                if !any_constraint_with(&items, |c| c.contains_key("values")) {
                    errors.push(format!(
                        "{ctx}: categorical param needs 'values' in constraints"
                    ));
                }
            } else if registry_type == "int" || registry_type == "float" {
                if !any_constraint_with(&items, |c| {
                    c.contains_key("step") && c.contains_key("lower") && c.contains_key("upper")
                }) {
                    errors.push(format!(
                        "{ctx}: {registry_type} param needs step/lower/upper in constraints"
                    ));
                }
            }
        }
    }

    if !errors.is_empty() {
        let error_msg = format!(
            "Preflight validation failed:\n{}",
            errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        return Ok(PreflightResult::Failure { error: error_msg });
    }

    Ok(PreflightResult::Success {
        data: SuccessData {
            message: "Preflight validation passed",
            warnings: if warnings.is_empty() {
                None
            } else {
                Some(std::mem::take(warnings))
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // python has no microgrid preflight test file — this set mirrors
    // the greenhouse suite (the code shape is the same copy-paste in
    // python); asserts use the greenhouse suite's own forms.

    #[test]
    fn missing_config_returns_failure() {
        match main(None, true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("Missing config parameter"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn valid_config_passes() {
        let config = json!({
            "settings": {"microgrid": {
                "power_draw": {"constraints": [{"step": 10.0, "lower": 0.0, "upper": 1000.0}]},
                "storage_dispatch": {"constraints": [{"step": 25.0, "lower": -500.0, "upper": 500.0}]},
                "local_generation": {"constraints": [{"step": 10.0, "lower": 0.0, "upper": 500.0}]},
                "sim_steps": {"constraints": [{"step": 10, "lower": 10, "upper": 200}]}
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn unsupported_parameter_fails_strict() {
        let config = json!({
            "settings": {"microgrid": {
                "nonexistent_param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.to_lowercase().contains("unsupported parameter"));
                assert!(error.contains(
                    "Supported: power_draw, storage_dispatch, local_generation, sim_steps"
                ));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn unsupported_parameter_warns_non_strict() {
        let config = json!({
            "meta": {"strict_validation": false},
            "settings": {"microgrid": {
                "nonexistent_param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Success { data } => assert!(data.warnings.is_some()),
            _ => panic!("expected success with warnings"),
        }
    }

    #[test]
    fn int_param_wrong_constraints_fails() {
        let config = json!({
            "settings": {"microgrid": {
                "sim_steps": {"constraints": [{"values": [10, 50, 100]}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("step/lower/upper"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn null_meta_raises_out_of_main() {
        let config = json!({"meta": null, "settings": {}});
        let err = main(Some(&config), true).unwrap_err();
        assert_eq!(err, "argument of type 'NoneType' is not iterable");
    }
}
