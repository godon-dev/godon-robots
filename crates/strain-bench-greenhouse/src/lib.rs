//! The greenhouse bench strain — one family crate of the rung-10 rust
//! port (`strains/bench_greenhouse` in the python tree).
//!
//! What a strain owns, per the python contract: its parameter
//! registry, its create-time preflight, and `validate_config` as the
//! single required attribute (`engine/strain_loader.py`'s
//! REQUIRED_ATTRS). The suggestion half died with the optimizer era
//! (rung 2 deleted the only caller; rung 3 the interface); python's
//! `strain.py` keeps a vestigial `PARAMETER_REGISTRY` import and a
//! `PER_ZONE_PARAMS` constant nothing reads — both stay python-side
//! residue, not ported.
//!
//! The registry is COMPILED IN as its golden fixture
//! ([`registry`]): one file, both suites — the python test holds its
//! live dict to the checked-in bytes, this crate parses the same
//! bytes as its data. Result strings are verbatim python.

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
    use crate::preflight::PreflightResult;

    /// Port of tests/unit/test_bench_greenhouse_strain.py — the
    /// delegation contract: validate_config IS the preflight.
    /// (python mocked the preflight module; here the equivalence is
    /// structural, so the test pins it on a real call.)
    #[test]
    fn validate_config_delegates_to_preflight() {
        let config = serde_json::json!({
            "settings": {"greenhouse": {"zones": 2}}
        });
        assert_eq!(
            validate_config(&config).unwrap(),
            preflight::main(Some(&config), true).unwrap()
        );
        assert_eq!(
            validate_config(&config).unwrap(),
            PreflightResult::Success {
                data: preflight::SuccessData {
                    message: "Preflight validation passed",
                    warnings: None
                }
            }
        );
    }
}
