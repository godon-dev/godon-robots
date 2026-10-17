//! The generic bench strain — one family crate of the rung-10 rust
//! port (`strains/bench_generic` in the python tree).
//!
//! This family carries no registry and no preflight script — python's
//! `strain.py` is its own minimal check. Its contract differs from
//! the other three: `validate_config` RAISES `ValueError` on a bad
//! config and returns `True` on a good one — no result dict. The
//! port keeps that shape: `Err` is the raise, `Ok(())` is the `True`.

use serde_json::Value;

/// Port of `strain.py`'s `validate_config` — the minimal preflight
/// for generic bench, python's statements in order. `Err` carries
/// the ValueError text verbatim.
pub fn validate_config(config: &Value) -> Result<(), String> {
    let empty = Value::Object(Default::default());
    let settings = py_get_or(config, "settings", &empty)?;
    let gen = py_get_or(settings, "generic", settings)?;

    if !py_truthy(gen) {
        return Err("No 'generic' settings found in config".to_string());
    }

    let Some(m) = gen.as_object() else {
        // python: gen.values() on a non-dict raises AttributeError —
        // bench_generic has no try/except, the raise escapes
        return Err(format!(
            "'{}' object has no attribute 'values'",
            py_type(gen)
        ));
    };

    let has_constraints = m
        .values()
        .any(|v| v.as_object().map_or(false, |d| d.contains_key("constraints")));
    if !has_constraints {
        return Err("No parameter constraints found in generic settings".to_string());
    }

    Ok(())
}

// --- the same python semantics helpers the family crates carry (the
// estate duplicates; this crate needs only three of them) ---

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

/// `.get(key, default)` — the default applies on a MISSING key only;
/// a present null yields the null, a non-dict parent raises.
fn py_get_or<'a>(v: &'a Value, key: &str, default: &'a Value) -> Result<&'a Value, String> {
    match v {
        Value::Object(m) => Ok(m.get(key).unwrap_or(default)),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            py_type(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // python has no bench_generic test file — these pin the ported
    // statements directly (the loader suite mocked the module away).

    #[test]
    fn valid_config_passes() {
        let config = json!({
            "settings": {"generic": {"dial_a": {"constraints": [{"lower": 0, "upper": 10}]}}}
        });
        assert_eq!(validate_config(&config), Ok(()));
    }

    #[test]
    fn constraints_directly_under_settings_count() {
        // python: gen = settings.get('generic', settings) — without
        // a 'generic' key the whole settings dict is the dial set
        let config = json!({
            "settings": {"dial_a": {"constraints": [{"lower": 0, "upper": 10}]}}
        });
        assert_eq!(validate_config(&config), Ok(()));
    }

    #[test]
    fn empty_generic_raises() {
        let config = json!({"settings": {"generic": {}}});
        assert_eq!(
            validate_config(&config),
            Err("No 'generic' settings found in config".to_string())
        );
    }

    #[test]
    fn no_constraints_raises() {
        let config = json!({"settings": {"generic": {"dial_a": {"lower": 0}}}});
        assert_eq!(
            validate_config(&config),
            Err("No parameter constraints found in generic settings".to_string())
        );
    }

    #[test]
    fn non_dict_values_are_skipped_not_raised() {
        // python's isinstance(v, dict) guard in the any()
        let config = json!({"settings": {"generic": {"scalar": 5}}});
        assert_eq!(
            validate_config(&config),
            Err("No parameter constraints found in generic settings".to_string())
        );
    }
}
