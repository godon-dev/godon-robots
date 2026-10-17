//! The linux performance strain — one family crate of the rung-10
//! rust port (`strains/linux_performance` in the python tree), the
//! family the tender body defaults to (`systemtender_config.get(
//! 'type', 'linux_performance')`).
//!
//! Python's `strain.py` carries a vestigial `PARAMETER_REGISTRY`
//! import and a `CATEGORIES` constant nothing else reads — the
//! category list lives in the preflight's own loop, and both stay
//! python-side residue, not ported. The registry is COMPILED IN from
//! its golden fixture ([`registry`]): 193 dial definitions plus the
//! 21 per-interface ethtool parameters, one file, both suites.

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

    /// python's strain-contract test (the greenhouse family's) — the
    /// delegation is structural here, pinned on a real call.
    #[test]
    fn validate_config_delegates_to_preflight() {
        let config = serde_json::json!({
            "settings": {"sysctl": {}}
        });
        assert_eq!(
            validate_config(&config).unwrap(),
            preflight::main(Some(&config), true).unwrap()
        );
    }
}
