//! The compiled-in strain registry — the port of
//! `engine/strain_loader.py`'s contract (the purpose-profile ruling:
//! strains compile in, the importlib loader dies with the python
//! era).
//!
//! What ports of the loader:
//! - the name → family mapping, with python's `STRAIN_MODULES` dict
//!   order riding [`STRAIN_TYPES`] (the Available list quotes it);
//! - `load_strain`'s unknown-type refusal, message verbatim;
//! - the `REQUIRED_ATTRS = ("validate_config",)` check — compiled in
//!   by construction, and pinned by a test that calls every family's
//!   `validate_config` once (the attr exists and runs).
//!
//! What dies with the loader: importlib (no modules to import), the
//! ImportError branch (no imports to fail), and the attr-introspection
//! ValueError (the type system carries it now). Python's worker kept
//! `self.strain` assigned but never read it — the boot check is the
//! whole observable behavior, so no handle is stored.

use serde_json::Value;

/// The strain families, python `STRAIN_MODULES` dict order.
pub const STRAIN_TYPES: [&str; 4] = [
    "linux_performance",
    "bench_greenhouse",
    "bench_microgrid",
    "bench_generic",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strain {
    LinuxPerformance,
    BenchGreenhouse,
    BenchMicrogrid,
    BenchGeneric,
}

/// Port of `load_strain` — the boot-time refusal on unknown names,
/// python's message verbatim (dict order in the Available list).
pub fn load_strain(strain_type: &str) -> Result<Strain, String> {
    match strain_type {
        "linux_performance" => Ok(Strain::LinuxPerformance),
        "bench_greenhouse" => Ok(Strain::BenchGreenhouse),
        "bench_microgrid" => Ok(Strain::BenchMicrogrid),
        "bench_generic" => Ok(Strain::BenchGeneric),
        other => Err(format!(
            "Unknown strain type: '{}'. Available: {}",
            other,
            STRAIN_TYPES.join(", ")
        )),
    }
}

/// The worker's strain-type read — python's second `get` with its own
/// default (`systemtender_config.get('type', 'linux_performance')`),
/// distinct from the identity read's 'unknown_systemtender'.
pub fn strain_type_of(systemtender: &Value) -> String {
    systemtender
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("linux_performance")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader contract that survives compilation: every family
    /// resolves, and every family's `validate_config` exists and runs
    /// — REQUIRED_ATTRS, proven by call. Each family gets its own
    /// minimal config (the families' dial grammars differ by design).
    #[test]
    fn every_family_resolves_and_validates() {
        let cases = [
            (
                "linux_performance",
                serde_json::json!({"settings": {"sysctl": {}}}),
            ),
            (
                "bench_greenhouse",
                serde_json::json!({"settings": {"greenhouse": {"zones": 2}}}),
            ),
            (
                "bench_microgrid",
                serde_json::json!({"settings": {"microgrid": {}}}),
            ),
            (
                "bench_generic",
                serde_json::json!({"settings": {"generic": {
                    "dial": {"constraints": [{"lower": 0, "upper": 1}]}
                }}}),
            ),
        ];
        for (name, config) in cases {
            let strain = load_strain(name).unwrap();
            // unwrap is the assertion: a missing or failing
            // validate_config fails the test right here
            match strain {
                Strain::LinuxPerformance => {
                    let _ = strain_linux_performance::validate_config(&config).unwrap();
                }
                Strain::BenchGreenhouse => {
                    let _ = strain_bench_greenhouse::validate_config(&config).unwrap();
                }
                Strain::BenchMicrogrid => {
                    let _ = strain_bench_microgrid::validate_config(&config).unwrap();
                }
                Strain::BenchGeneric => {
                    let _ = strain_bench_generic::validate_config(&config).unwrap();
                }
            }
        }
    }

    /// Port of test_strain_loader.py's unknown-type refusal — the
    /// message verbatim, dict order included.
    #[test]
    fn unknown_strain_type_refused_verbatim() {
        let err = load_strain("nonexistent_strain").unwrap_err();
        assert_eq!(
            err,
            "Unknown strain type: 'nonexistent_strain'. Available: \
             linux_performance, bench_greenhouse, bench_microgrid, bench_generic"
        );
    }

    #[test]
    fn strain_type_defaults_to_linux_performance() {
        assert_eq!(strain_type_of(&serde_json::json!({})), "linux_performance");
        assert_eq!(
            strain_type_of(&serde_json::json!({"type": "bench_greenhouse"})),
            "bench_greenhouse"
        );
    }
}
