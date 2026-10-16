//! The greenhouse dial registry, compiled in from its golden fixture
//! (`tests/unit/fixtures/strain_registry_bench_greenhouse.json`,
//! generated from the python `PARAMETER_REGISTRY`). The fixture is
//! the data: the python suite asserts its live dict against the same
//! file, so both bodies read one source until the python deletion.
//! Entries keep python insertion order — the preflight's
//! supported-list strings ride it.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One dial definition — the python registry's value dict, typed.
/// `typical_range` stays untyped numbers so integer and float
/// rendering survive exactly as python wrote them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub scope: Option<String>,
    pub causes_downtime: bool,
    pub description: String,
    pub typical_range: Option<Vec<Value>>,
}

#[derive(Debug)]
pub struct ParamEntry {
    pub name: String,
    pub def: ParamDef,
}

const FIXTURE: &str =
    include_str!("../../../tests/unit/fixtures/strain_registry_bench_greenhouse.json");

#[derive(Deserialize)]
struct RegistryFile {
    entries: Vec<(String, ParamDef)>,
}

fn entries() -> &'static Vec<ParamEntry> {
    static REG: OnceLock<Vec<ParamEntry>> = OnceLock::new();
    REG.get_or_init(|| {
        let file: RegistryFile =
            serde_json::from_str(FIXTURE).expect("greenhouse registry fixture is well-formed");
        file.entries
            .into_iter()
            .map(|(name, def)| ParamEntry { name, def })
            .collect()
    })
}

/// The registry in python insertion order.
pub fn parameter_registry() -> &'static [ParamEntry] {
    entries()
}

pub fn lookup(name: &str) -> Option<&'static ParamEntry> {
    entries().iter().find(|e| e.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parity spots against the python dict (order and fields) — the
    /// fixture bytes themselves are pinned by the python suite.
    #[test]
    fn registry_order_and_count() {
        let names: Vec<&str> = parameter_registry().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "heating_setpoints",
                "vent_openings",
                "shading",
                "co2_injection",
                "light_intensity",
                "irrigation",
                "sim_steps",
            ]
        );
    }

    #[test]
    fn first_and_last_definitions() {
        let first = lookup("heating_setpoints").unwrap();
        assert_eq!(first.def.kind, "float");
        assert_eq!(first.def.scope.as_deref(), Some("per_zone"));
        assert!(!first.def.causes_downtime);
        assert_eq!(
            first.def.typical_range,
            Some(vec![Value::from(5.0), Value::from(40.0)])
        );

        let last = lookup("sim_steps").unwrap();
        assert_eq!(last.def.kind, "int");
        // ints stay ints through the fixture — the is_int contract
        assert_eq!(
            last.def.typical_range,
            Some(vec![Value::from(10), Value::from(200)])
        );
    }
}
