//! The microgrid bench strain — one family crate of the rung-10 rust
//! port (`strains/bench_microgrid` in the python tree).
//!
//! Same contract as the greenhouse family: the parameter registry
//! compiled in from its golden fixture, the create-time preflight,
//! and `validate_config` as the loader's single required attribute.
//! Python keeps a vestigial `PARAMETER_REGISTRY` import in
//! `strain.py` that nothing reads — python-side residue, not ported.
//! The registry is COMPILED IN as its golden fixture ([`registry`]):
//! one file, both suites.

pub mod preflight;
pub mod registry;

use serde_json::Value;

/// Port of `strain.py`'s `validate_config` — delegation to the
/// preflight with python's default strict mode, python's own shape.
pub fn validate_config(config: &Value) -> Result<preflight::PreflightResult, String> {
    preflight::main(Some(config), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// python has no microgrid preflight test file — this mirrors
    /// the greenhouse delegation pin (the code shape is the same).
    #[test]
    fn validate_config_delegates_to_preflight() {
        let config = serde_json::json!({
            "settings": {"microgrid": {
                "power_draw": {"constraints": [{"step": 10.0, "lower": 0.0, "upper": 1000.0}]}
            }}
        });
        assert_eq!(
            validate_config(&config).unwrap(),
            preflight::main(Some(&config), true).unwrap()
        );
    }
}
