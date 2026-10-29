//! The greenhouse create-time preflight — statement-by-statement port
//! of `strains/bench_greenhouse/preflight.py` (the rung-9 create-time
//! check the controller's executor calls through windmill).
//!
//! Python's `main` returns the result dicts OR raises: the meta read
//! sits OUTSIDE the try, so a null `meta` raises straight out of
//! `main` — mirrored by `Err` carrying the raise text. Everything
//! inside the try maps to `Ok(PreflightResult::Failure { .. })` with
//! the `"Preflight validation error: {e}"` result, python's except
//! verbatim. Result strings are byte-exact.

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

/// `.get(key)` on a dict — a non-dict parent is python's
/// AttributeError, the raise text the caller sees.
fn py_get<'a>(v: &'a Value, key: &str) -> Result<Option<&'a Value>, String> {
    match v {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            py_type(other)
        )),
    }
}

/// `.get(key, default)` — the default applies on a MISSING key only;
/// a present null yields the null (python's own semantics).
fn py_get_or<'a>(v: &'a Value, key: &str, default: &'a Value) -> Result<&'a Value, String> {
    Ok(py_get(v, key)?.unwrap_or(default))
}

/// python `isinstance(x, int)` — bool IS int there, so zones=true
/// reads as the int 1.
fn py_is_int(v: &Value) -> bool {
    v.is_i64() || v.is_u64() || v.is_boolean()
}

fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// python str() of a value, for the message tails.
fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        // python floats always render with a decimal point
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.is_finite() && f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A constraints value after python's normalization: a dict WITH
/// 'values' rebinds to a one-element list; anything else stays as is
/// for the list check.
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
    items.iter().any(|c| c.as_object().is_some_and(has))
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
    let gh_settings = py_get_or(settings, "greenhouse", settings)?;

    if !gh_settings.is_object() {
        return Ok(PreflightResult::Failure {
            error: "settings.greenhouse must be a dict".to_string(),
        });
    }

    let gh = gh_settings.as_object().expect("checked above");
    let two = Value::from(2);
    let zones = gh.get("zones").unwrap_or(&two);
    if !py_is_int(zones) || py_int(zones).is_none_or(|z| z < 1) {
        errors.push(format!(
            "settings.greenhouse.zones must be a positive integer, got: {}",
            py_repr(zones)
        ));
    }

    for (param_name, param_config) in gh {
        if param_name == "zones" {
            continue;
        }

        if !param_config.is_object() {
            errors.push(format!("settings.greenhouse.{param_name}: must be a dict"));
            continue;
        }

        let ctx = format!("settings.greenhouse.{param_name}");
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
            } else if (registry_type == "int" || registry_type == "float")
                && !any_constraint_with(&items, |c| {
                    c.contains_key("step") && c.contains_key("lower") && c.contains_key("upper")
                })
            {
                errors.push(format!(
                    "{ctx}: {registry_type} param needs step/lower/upper in constraints"
                ));
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

    /// Port of tests/unit/test_bench_greenhouse_preflight.py — the
    /// python suite's asserts, one-to-one.
    #[test]
    fn missing_config_returns_failure() {
        let result = main(None, true).unwrap();
        match result {
            PreflightResult::Failure { error } => {
                assert!(error.contains("Missing config parameter"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn empty_config_is_missing_too() {
        // python's `if not config` — {} is falsy
        let result = main(Some(&json!({})), true).unwrap();
        assert!(matches!(result, PreflightResult::Failure { .. }));
    }

    #[test]
    fn valid_greenhouse_config_passes() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 2,
                "heating_setpoints": {"constraints": [{"step": 0.5, "lower": 5.0, "upper": 40.0}]},
                "shading": {"constraints": [{"step": 0.05, "lower": 0.0, "upper": 1.0}]}
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn valid_all_params() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 4,
                "heating_setpoints": {"constraints": [{"step": 0.5, "lower": 5.0, "upper": 40.0}]},
                "vent_openings": {"constraints": [{"step": 0.05, "lower": 0.0, "upper": 1.0}]},
                "shading": {"constraints": [{"step": 0.05, "lower": 0.0, "upper": 1.0}]},
                "co2_injection": {"constraints": [{"step": 0.5, "lower": 0.0, "upper": 20.0}]},
                "light_intensity": {"constraints": [{"step": 10.0, "lower": 0.0, "upper": 1000.0}]},
                "irrigation": {"constraints": [{"step": 0.1, "lower": 0.0, "upper": 3.0}]},
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
            "settings": {"greenhouse": {
                "zones": 2,
                "nonexistent_param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.to_lowercase().contains("unsupported parameter"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn unsupported_parameter_warns_non_strict() {
        let config = json!({
            "meta": {"strict_validation": false},
            "settings": {"greenhouse": {
                "zones": 2,
                "nonexistent_param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Success { data } => assert!(data.warnings.is_some()),
            _ => panic!("expected success with warnings"),
        }
    }

    #[test]
    fn missing_constraints_fails() {
        let config = json!({
            "settings": {"greenhouse": {"zones": 2, "shading": {}}}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("missing 'constraints'"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn invalid_zones_value_fails() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 0,
                "shading": {"constraints": [{"step": 0.05, "lower": 0.0, "upper": 1.0}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.to_lowercase().contains("zones"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn float_param_wrong_constraints_fails() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 2,
                "shading": {"constraints": [{"values": ["a", "b"]}]}
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
    fn per_zone_param_wrong_constraints_fails() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 2,
                "heating_setpoints": {"constraints": [{"values": ["a", "b"]}]}
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
    fn non_dict_settings_fails() {
        let config = json!({"settings": {"greenhouse": "not_a_dict"}});
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.to_lowercase().contains("must be a dict"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn non_dict_param_config_fails() {
        let config = json!({
            "settings": {"greenhouse": {"zones": 2, "shading": "not_a_dict"}}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Failure { .. }
        ));
    }

    /// python's test name says "succeeds" but pins FAILURE — a dict
    /// with 'values' normalizes, then a FLOAT param still demands
    /// step/lower/upper. Ported as pinned, not as named.
    #[test]
    fn constraints_dict_with_values_on_float_param_fails() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 2,
                "heating_setpoints": {
                    "constraints": {"step": 0.5, "lower": 5.0, "upper": 40.0}
                }
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Failure { .. }
        ));
    }

    #[test]
    fn multiple_errors_aggregated() {
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 2,
                "bad_param_1": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]},
                "bad_param_2": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                let error_lines = error.split('\n').count();
                assert!(error_lines > 2);
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn error_result_is_verbatim_python() {
        // the aggregated result, byte-exact against the python shape
        let config = json!({
            "settings": {"greenhouse": {"zones": 2, "shading": {}}}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert_eq!(
                    error,
                    "Preflight validation failed:\n  - settings.greenhouse.shading: missing 'constraints'"
                );
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn unsupported_list_rides_registry_order() {
        // python joins PARAMETER_REGISTRY keys in insertion order
        let config = json!({
            "settings": {"greenhouse": {
                "zones": 2,
                "nonexistent_param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains(
                    "Supported: heating_setpoints, vent_openings, shading, co2_injection, light_intensity, irrigation, sim_steps"
                ));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn null_meta_raises_out_of_main() {
        // the meta read is outside python's try: TypeError escapes
        let config = json!({"meta": null, "settings": {}});
        let err = main(Some(&config), true).unwrap_err();
        assert_eq!(err, "argument of type 'NoneType' is not iterable");
    }
}
