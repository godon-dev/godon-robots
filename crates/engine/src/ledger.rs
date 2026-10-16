//! The tender's trial ledger — the estate's own tables.
//!
//! Two tables in the tender's own database, one direct connection per
//! operation. The SQL side of the port lands in its own slice; carried
//! here from day one are the three contract pieces every later slice
//! must obey: the classification function (pinned against the rust
//! reader's `from_trials`), the schema DDL (byte-identical tables —
//! the readers in godon-images read them), and the transient-failure
//! patterns that take a retry, never a lost trial.

/// The one classification — pinned against the rust reader's
/// `from_trials`. push/pause/hold_calib come from the impulse phase
/// (probe_-prefix normalized away: a rename must never re-starve a
/// reader), receiver hold from detection mode + coordinator state.
///
/// A seek dart is its OWN kind: a joint random setting is
/// unattributable, so no reader may ever bank one as curve ground —
/// darts are priors, never map entries.
///
/// An ambience trial is its OWN kind: a fenced own-effect walk at the
/// resting spot — ambience entries at a named baseline, never chain
/// ground (the map keeps the two scopes apart).
///
/// Byte-compatible with the python original (see
/// `tests/unit/test_trial_ledger.py`, the golden mapping):
/// `None`/empty inputs behave exactly as the python `or`-chains do.
pub fn classify(
    mode: Option<&str>,
    impulse_phase: Option<&str>,
    coord_state: &str,
    dart: bool,
    ambience: bool,
) -> String {
    if dart {
        return "seek_prior".to_string();
    }
    if ambience {
        return "ambience".to_string();
    }
    let phase_raw = impulse_phase.unwrap_or("");
    let phase = phase_raw.strip_prefix("probe_").unwrap_or(phase_raw);
    if matches!(phase, "push" | "pause" | "hold_calib") {
        return phase.to_string();
    }
    if mode == Some("hold") && coord_state != "hold_calib" {
        return "hold".to_string();
    }
    match mode {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => "unclassified".to_string(),
    }
}

/// Schema version 1 — the two tables, byte-identical to the python
/// original's migrations. The readers in godon-images depend on these
/// shapes; any change is an estate motion, not a code decision.
pub const SCHEMA_VERSION: i64 = 1;

pub const MIGRATIONS_V1: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS trials (
            number      BIGSERIAL PRIMARY KEY,
            kind        TEXT NOT NULL DEFAULT 'unclassified',
            params      JSONB NOT NULL DEFAULT '{}',
            value       DOUBLE PRECISION,
            state       TEXT NOT NULL DEFAULT 'RUNNING',
            error       TEXT,
            attrs       JSONB NOT NULL DEFAULT '{}',
            created_ts  TIMESTAMPTZ NOT NULL DEFAULT now(),
            finished_ts TIMESTAMPTZ
        )",
    "CREATE TABLE IF NOT EXISTS ledger_state (
            key        TEXT PRIMARY KEY,
            value_json JSONB NOT NULL,
            updated_ts TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
];

/// Transient failures take a retry (per-verb short connections),
/// never a lost trial. Substrings, exactly as the python side matches
/// them.
pub const RETRYABLE_DB_ERROR_PATTERNS: [&str; 5] = [
    "SerializationFailure",
    "40001",
    "expired or aborted",
    "OperationalError",
    "server closed the connection",
];

#[cfg(test)]
mod tests {
    use super::classify;

    /// The pinned mapping — causal's from_trials, written once.
    /// Every row mirrors the python golden cases byte-for-byte.
    #[test]
    fn phase_trials() {
        for (phase, expected) in [
            ("push", "push"),
            // the rename that starved a reader
            ("probe_push", "push"),
            ("pause", "pause"),
            ("probe_pause", "pause"),
            ("hold_calib", "hold_calib"),
        ] {
            assert_eq!(
                classify(Some("impulse"), Some(phase), "probe_push", false, false),
                expected,
                "phase {phase}"
            );
        }
    }

    #[test]
    fn receiver_hold() {
        assert_eq!(
            classify(Some("hold"), Some("hold"), "idle", false, false),
            "hold"
        );
    }

    #[test]
    fn own_hold_calib_is_not_receiver_hold() {
        assert_eq!(
            classify(Some("hold"), Some("hold_calib"), "hold_calib", false, false),
            "hold_calib"
        );
    }

    #[test]
    fn mode_passthrough() {
        assert_eq!(classify(Some("park"), None, "idle", false, false), "park");
        assert_eq!(
            classify(Some("idle"), Some(""), "idle", false, false),
            "idle"
        );
    }

    #[test]
    fn unknown_falls_back() {
        assert_eq!(classify(Some(""), None, "", false, false), "unclassified");
        assert_eq!(classify(None, None, "", false, false), "unclassified");
    }

    #[test]
    fn dart_is_its_own_kind() {
        assert_eq!(
            classify(
                Some("impulse"),
                Some("probe_push"),
                "probe_push",
                true,
                false,
            ),
            "seek_prior"
        );
    }

    #[test]
    fn ambience_is_its_own_kind() {
        assert_eq!(
            classify(
                Some("impulse"),
                Some("probe_push"),
                "probe_push",
                false,
                true,
            ),
            "ambience"
        );
        // dart wins over ambience, per the python order
        assert_eq!(
            classify(Some("impulse"), None, "idle", true, true),
            "seek_prior"
        );
    }
}
