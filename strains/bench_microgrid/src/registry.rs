//! The microgrid dial registry, compiled in from its golden fixture
//! (`tests/unit/fixtures/strain_registry_bench_microgrid.json`,
//! generated from the python `PARAMETER_REGISTRY`). One file, both
//! suites — entries keep python insertion order.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    include_str!("../../../tests/unit/fixtures/strain_registry_bench_microgrid.json");

#[derive(Deserialize)]
struct RegistryFile {
    entries: Vec<(String, ParamDef)>,
}

fn entries() -> &'static Vec<ParamEntry> {
    static REG: OnceLock<Vec<ParamEntry>> = OnceLock::new();
    REG.get_or_init(|| {
        let file: RegistryFile =
            serde_json::from_str(FIXTURE).expect("microgrid registry fixture is well-formed");
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

    #[test]
    fn registry_order_and_count() {
        let names: Vec<&str> = parameter_registry()
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["power_draw", "storage_dispatch", "local_generation", "sim_steps"]
        );
    }

    #[test]
    fn first_definition_fields() {
        let first = lookup("power_draw").unwrap();
        assert_eq!(first.def.kind, "float");
        assert_eq!(first.def.scope.as_deref(), Some("global"));
        assert!(!first.def.causes_downtime);
        assert_eq!(first.def.description, "Power draw from grid (kW)");
    }
}
