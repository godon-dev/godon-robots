//! The linux dial registry, compiled in from its golden fixture
//! (`tests/unit/fixtures/strain_registry_linux_performance.json`,
//! generated from the python `PARAMETER_REGISTRY` and
//! `ETHTOOL_PARAMS`). One file, both suites — the python test holds
//! its live dicts to the checked-in bytes, this crate parses the
//! same bytes as its data. Entries keep python insertion order —
//! the preflight's supported-list strings ride it.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One dial definition — the python registry's value dict, typed.
/// `typical_range` and `available_values` stay untyped numbers and
/// strings so their rendering survives exactly as python wrote them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub category: Option<String>,
    pub causes_downtime: bool,
    pub description: String,
    pub typical_range: Option<Vec<Value>>,
    pub available_values: Option<Vec<Value>>,
}

#[derive(Debug)]
pub struct ParamEntry {
    pub name: String,
    pub def: ParamDef,
}

const FIXTURE: &str =
    include_str!("../../../tests/unit/fixtures/strain_registry_linux_performance.json");

#[derive(Deserialize)]
struct RegistryFile {
    entries: Vec<(String, ParamDef)>,
    ethtool_entries: Vec<(String, ParamDef)>,
}

struct Registries {
    entries: Vec<ParamEntry>,
    ethtool: Vec<ParamEntry>,
}

fn registries() -> &'static Registries {
    static REG: OnceLock<Registries> = OnceLock::new();
    REG.get_or_init(|| {
        let file: RegistryFile =
            serde_json::from_str(FIXTURE).expect("linux registry fixture is well-formed");
        let map = |v: Vec<(String, ParamDef)>| {
            v.into_iter()
                .map(|(name, def)| ParamEntry { name, def })
                .collect()
        };
        Registries {
            entries: map(file.entries),
            ethtool: map(file.ethtool_entries),
        }
    })
}

/// The parameter registry in python insertion order.
pub fn parameter_registry() -> &'static [ParamEntry] {
    &registries().entries
}

/// The per-interface ethtool parameters in python insertion order.
pub fn ethtool_params() -> &'static [ParamEntry] {
    &registries().ethtool
}

pub fn lookup(name: &str) -> Option<&'static ParamEntry> {
    registries().entries.iter().find(|e| e.name == name)
}

pub fn ethtool_lookup(name: &str) -> Option<&'static ParamEntry> {
    registries().ethtool.iter().find(|e| e.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parity spots against the python dicts — the fixture bytes are
    /// pinned by the python suite; these pin the parse and the
    /// order the preflight's supported-lists ride.
    #[test]
    fn registry_counts() {
        assert_eq!(parameter_registry().len(), 193);
        assert_eq!(ethtool_params().len(), 21);
    }

    #[test]
    fn first_entries_and_fields() {
        let first = lookup("net.ipv4.tcp_rmem").unwrap();
        assert_eq!(first.def.kind, "int");
        assert_eq!(first.def.category.as_deref(), Some("sysctl"));
        assert!(!first.def.causes_downtime);
        assert_eq!(
            first.def.description,
            "TCP read buffer (min, default, max)"
        );
        assert_eq!(first.def.typical_range, None);

        // first entry WITH a typical range keeps int rendering
        let fin = lookup("net.ipv4.tcp_fin_timeout").unwrap();
        assert_eq!(
            fin.def.typical_range,
            Some(vec![Value::from(5), Value::from(120)])
        );
    }

    #[test]
    fn ethtool_shape() {
        let tso = ethtool_lookup("tso").unwrap();
        assert_eq!(tso.def.kind, "categorical");
        assert!(tso.def.causes_downtime);
        assert_eq!(tso.def.category, None);
        assert_eq!(
            tso.def.available_values,
            Some(vec![
                Value::from("on".to_string()),
                Value::from("off".to_string())
            ])
        );
    }

    #[test]
    fn categories_cover_the_preflight_loop() {
        for category in ["sysctl", "sysfs", "cpufreq"] {
            assert!(
                parameter_registry()
                    .iter()
                    .any(|e| e.def.category.as_deref() == Some(category)),
                "registry carries {category} params"
            );
        }
        // ethtool lives in its own table, never in the main registry
        assert!(parameter_registry()
            .iter()
            .all(|e| e.def.category.as_deref() != Some("ethtool")));
    }
}
