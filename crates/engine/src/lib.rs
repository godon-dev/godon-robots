//! The tender body — the vessel walk engine (vessel rung 10 rust port).
//!
//! Port discipline, from the python original's own contract
//! (`engine/trial_ledger.py`, robots repo): the encoding rules are
//! canonical and BOTH sides obey them —
//!
//! - attrs values are stored as given: strings as JSON strings, floats
//!   as JSON numbers, pre-serialized JSON (observations, guardrails,
//!   effectuation_params) as strings containing JSON — byte-identical
//!   to what the readers decoded from the study era.
//! - trial states are the uppercase COMPLETE / FAIL / RUNNING.
//! - `kind` is the trial's classification, written once by the worker
//!   via [`ledger::classify`]; readers bucket by kind, never by
//!   string-matching attr literals.
//! - the `params` column is reserved: written empty until the read
//!   surface deliberately widens.
//!
//! The contract between this body and the rust readers in godon-images
//! is the golden fixture — python writes it
//! (`tests/unit/test_trial_ledger.py`), rust writes it back through
//! the same verbs (`ledger.rs`, the LEDGER_TEST_DSN golden test),
//! both against the one committed artifact
//! (`tests/unit/fixtures/ledger_golden.json`) — not shared code.
//! Fixture parity is byte-exact, never approximate.

pub mod ledger;
