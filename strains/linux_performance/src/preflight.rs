//! The linux create-time preflight — statement-by-statement port of
//! `strains/linux_performance/preflight.py` (the rung-9 create-time
//! check the controller's executor calls through windmill).
//!
//! Python quirks kept on purpose, with their receipts in the tests:
//! - in PERMISSIVE mode an unknown non-ethtool parameter warns, then
//!   python's bare `PARAMETER_REGISTRY[param_name]['type']` raises
//!   KeyError — the outer except turns it into the
//!   `"Preflight validation error: '{name}'"` failure and the whole
//!   validation stops there. Mirrored by an early `Err`.
//! - python re-checks `isinstance(param_config, dict)` inside the
//!   ethtool branch after already erroring on non-dicts one line
//!   above — dead code, not ported.
//! - a null `settings` makes `category not in settings` raise
//!   TypeError INSIDE the try — the except result carries it.
//! - NAMED DIVERGENCE, non-object non-null `settings` (a string):
//!   python's `in` does substring matching there, so most such
//!   configs skip every category and pass; the port fails closed
//!   with the AttributeError result. No python test pins the
//!   substring path, and a preflight refusing absurd shapes is the
//!   safer read of the contract.
//! - the meta read sits OUTSIDE the try (a null meta raises straight
//!   out of main, uncaught there too).

use serde::Serialize;
use serde_json::Value;

use crate::registry;

pub const CATEGORIES: [&str; 4] = ["sysctl", "sysfs", "cpufreq", "ethtool"];

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

/// python's pragmatic accept: a dict WITH 'values' rebinds to a
/// one-element list; the failure tail differs per call site, passed
/// as `list_error`.
fn normalize_constraints<'a>(
    raw: &'a Value,
    ctx: &str,
    list_error: &str,
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
    errors.push(format!("{ctx}: {list_error}"));
    None
}

fn any_constraint_with(items: &[&Value], has: fn(&serde_json::Map<String, Value>) -> bool) -> bool {
    items.iter().any(|c| c.as_object().map_or(false, has))
}

fn has_values(c: &serde_json::Map<String, Value>) -> bool {
    c.contains_key("values")
}

fn has_range(c: &serde_json::Map<String, Value>) -> bool {
    c.contains_key("step") && c.contains_key("lower") && c.contains_key("upper")
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
    if settings.is_null() {
        // python: `category not in settings` on None raises TypeError
        // inside the try — the except result carries it
        return Err("argument of type 'NoneType' is not iterable".to_string());
    }

    for category in CATEGORIES {
        let Some(category_settings) = py_get(settings, category)? else {
            continue;
        };

        if !category_settings.is_object() {
            errors.push(format!("settings.{category}: must be a dict"));
            continue;
        }

        for (param_name, param_config) in category_settings.as_object().expect("checked above") {
            if !param_config.is_object() {
                errors.push(format!("settings.{category}.{param_name}: must be a dict"));
                continue;
            }

            if category == "ethtool" {
                // param_name is the interface (e.g. "eth0")
                for (ethtool_param, ethtool_config) in
                    param_config.as_object().expect("checked above")
                {
                    let Some(entry) = registry::ethtool_lookup(ethtool_param) else {
                        errors.push(format!(
                            "settings.ethtool.{param_name}.{ethtool_param}: unsupported ethtool parameter. Supported: {}",
                            registry::ethtool_params()
                                .iter()
                                .map(|e| e.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        continue;
                    };

                    let ctx = format!("settings.ethtool.{param_name}.{ethtool_param}");

                    let Some(raw_constraints) = ethtool_config.get("constraints") else {
                        errors.push(format!("{ctx}: missing 'constraints'"));
                        continue;
                    };

                    let Some(Constraints::Items(items)) = normalize_constraints(
                        raw_constraints,
                        &ctx,
                        "constraints must be a list or dict with 'values'",
                        errors,
                    ) else {
                        continue;
                    };

                    let registry_type = &entry.def.kind;
                    if registry_type == "categorical" {
                        if !any_constraint_with(&items, has_values) {
                            errors.push(format!(
                                "{ctx}: parameter is categorical but constraints don't have 'values'"
                            ));
                        }
                    } else if registry_type == "int" || registry_type == "float" {
                        if !any_constraint_with(&items, has_range) {
                            errors.push(format!(
                                "{ctx}: parameter is {registry_type} but constraints don't have step/lower/upper"
                            ));
                        }
                    }
                }
                continue;
            }

            // non-ethtool parameters
            let ctx = format!("settings.{category}.{param_name}");
            let entry = registry::lookup(param_name);
            if entry.is_none() {
                if strict {
                    errors.push(format!(
                        "{ctx}: unsupported parameter. Supported {category} parameters: {}",
                        registry::parameter_registry()
                            .iter()
                            .filter(|e| e.def.category.as_deref() == Some(category))
                            .map(|e| e.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    continue;
                } else {
                    warnings.push(format!(
                        "{ctx}: parameter not in registry. Metadata unavailable (reboot requirement, description, safety info). Proceeding with structural validation only."
                    ));
                }
            }

            // python's bare lookup: in permissive mode an unknown
            // param falls through to KeyError here — the except
            // catches it and the whole validation stops.
            let Some(entry) = entry else {
                return Err(format!("'{param_name}'"));
            };

            let Some(raw_constraints) = param_config.get("constraints") else {
                errors.push(format!("{ctx}: missing 'constraints'"));
                continue;
            };

            let Some(Constraints::Items(items)) = normalize_constraints(
                raw_constraints,
                &ctx,
                "constraints must be a list or dict with 'values'",
                errors,
            ) else {
                continue;
            };

            let registry_type = &entry.def.kind;
            if registry_type == "categorical" {
                if !any_constraint_with(&items, has_values) {
                    errors.push(format!(
                        "{ctx}: parameter is categorical but constraints don't have 'values'"
                    ));
                }
            } else if registry_type == "int" || registry_type == "float" {
                if !any_constraint_with(&items, has_range) {
                    errors.push(format!(
                        "{ctx}: parameter is {registry_type} but constraints don't have step/lower/upper"
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

    /// Port of tests/unit/test_preflight_validation.py — the python
    /// suite's 14 tests, one-to-one.
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
    fn valid_sysctl_config_passes() {
        let config = json!({
            "settings": {"sysctl": {
                "net.ipv4.tcp_rmem": {
                    "constraints": [{"step": 100, "lower": 4096, "upper": 131072}]
                }
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Success { data } => {
                assert!(data.message.contains("Preflight validation passed"));
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn valid_sysfs_config_passes() {
        let config = json!({
            "settings": {"sysfs": {
                "cpu_governor": {
                    "constraints": [{"values": ["performance", "powersave", "ondemand"]}]
                }
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn valid_cpufreq_config_passes() {
        let config = json!({
            "settings": {"cpufreq": {
                "min_freq_ghz": {
                    "constraints": [{"step": 0.1, "lower": 1.0, "upper": 3.0}]
                }
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn valid_ethtool_config_passes() {
        let config = json!({
            "settings": {"ethtool": {
                "eth0": {
                    "tso": {"constraints": [{"values": ["on", "off"]}]},
                    "rx_ring": {"constraints": [{"step": 64, "lower": 256, "upper": 4096}]}
                }
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn unsupported_sysctl_parameter_fails() {
        let config = json!({
            "settings": {"sysctl": {
                "unsupported.param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
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
    fn unsupported_sysfs_parameter_fails() {
        let config = json!({
            "settings": {"sysfs": {
                "unknown_sysfs_param": {"constraints": [{"values": ["a", "b"]}]}
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
    fn unsupported_ethtool_parameter_fails() {
        let config = json!({
            "settings": {"ethtool": {
                "eth0": {
                    "unsupported_ethtool_param": {"constraints": [{"values": ["on", "off"]}]}
                }
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error
                    .to_lowercase()
                    .contains("unsupported ethtool parameter"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn categorical_parameter_with_wrong_constraint_type_fails() {
        let config = json!({
            "settings": {"sysfs": {
                "cpu_governor": {
                    // wrong: should be 'values'
                    "constraints": [{"step": 1, "lower": 0, "upper": 100}]
                }
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("constraints don't have 'values'"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn integer_parameter_with_wrong_constraint_type_fails() {
        let config = json!({
            "settings": {"sysctl": {
                "net.core.netdev_budget": {
                    // wrong: should be range
                    "constraints": [{"values": ["100", "200", "300"]}]
                }
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("constraints don't have step/lower/upper"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn missing_constraints_fails() {
        let config = json!({
            "settings": {"sysctl": {
                "net.ipv4.tcp_rmem": {
                    // missing 'constraints'
                }
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("missing 'constraints'"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn constraints_dict_without_values_fails() {
        let config = json!({
            "settings": {"sysctl": {
                "net.ipv4.tcp_rmem": {
                    // dict without 'values'
                    "constraints": {"lower": 4096, "upper": 131072}
                }
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("constraints dict must have"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn constraints_dict_with_values_succeeds() {
        let config = json!({
            "settings": {"sysctl": {
                "net.ipv4.tcp_congestion_control": {
                    // dict with 'values' — categorical
                    "constraints": {"values": ["cubic", "bbr"]}
                }
            }}
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn multiple_categories_all_valid() {
        let config = json!({
            "settings": {
                "sysctl": {
                    "net.ipv4.tcp_rmem": {
                        "constraints": [{"step": 100, "lower": 4096, "upper": 131072}]
                    }
                },
                "sysfs": {
                    "cpu_governor": {
                        "constraints": [{"values": ["performance", "powersave"]}]
                    }
                },
                "cpufreq": {
                    "min_freq_ghz": {
                        "constraints": [{"step": 0.1, "lower": 1.0, "upper": 2.0}]
                    }
                },
                "ethtool": {
                    "eth0": {
                        "tso": {"constraints": [{"values": ["on", "off"]}]}
                    }
                }
            }
        });
        assert!(matches!(
            main(Some(&config), true).unwrap(),
            PreflightResult::Success { .. }
        ));
    }

    #[test]
    fn multiple_errors_aggregated() {
        let config = json!({
            "settings": {"sysctl": {
                "unsupported_param_1": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]},
                "unsupported_param_2": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                let error_lines = error.split('\n').count();
                assert!(error_lines > 2); // header + at least 2 errors
            }
            _ => panic!("expected failure"),
        }
    }

    // --- python's own quirks, pinned here because the port keeps
    // them (receipts in the module doc) ---

    #[test]
    fn permissive_unknown_param_stops_with_keyerror_result() {
        // python: warning appended, then the bare registry lookup
        // raises KeyError — the except result carries str(KeyError)
        let config = json!({
            "meta": {"strict_validation": false},
            "settings": {"sysctl": {
                "unsupported_param": {"constraints": [{"step": 1, "lower": 0, "upper": 100}]}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert_eq!(error, "Preflight validation error: 'unsupported_param'");
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn permissive_ethtool_unknown_param_only_errors() {
        // ethtool has no strict branch — unknown names error in BOTH
        // modes, no KeyError fallthrough
        let config = json!({
            "meta": {"strict_validation": false},
            "settings": {"ethtool": {
                "eth0": {"unsupported_ethtool_param": {"constraints": [{"values": ["on"]}]}}
            }}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("unsupported ethtool parameter"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn unsupported_list_filters_by_category() {
        let config = json!({
            "settings": {"sysctl": {"unsupported.param": {
                "constraints": [{"step": 1, "lower": 0, "upper": 100}]
            }}}
        });
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert!(error.contains("Supported sysctl parameters: "));
                // registry insertion order, sysctl-only names
                assert!(error.contains("net.ipv4.tcp_rmem"));
                assert!(!error.contains("cpu_governor"));
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn null_settings_is_the_except_result() {
        // python: `'sysctl' not in None` raises TypeError inside try
        let config = json!({"settings": null});
        match main(Some(&config), true).unwrap() {
            PreflightResult::Failure { error } => {
                assert_eq!(
                    error,
                    "Preflight validation error: argument of type 'NoneType' is not iterable"
                );
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
