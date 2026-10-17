//! The reconnaissance components — ports of `reconnaissance/http.py`
//! and `reconnaissance/prometheus.py`, the two windmill scripts the
//! worker runs by path (`f/reconnaissance/{type}`) to gather metric
//! values per trial.
//!
//! Standing shape (mirrors the python files):
//! - the components take `(context, targets, settings)`; the recon
//!   ones never read `settings` or `targets` (the python signatures
//!   carried them unused — kept for the windmill contract).
//! - `logger.debug` lines are not carried (house logging port);
//!   info/warning/error lines are eprintlns in order.
//! - failure values are `f64::INFINITY` in memory and render as JSON
//!   `null` at the result boundary (serde_json cannot carry
//!   Infinity; the worker's reader maps null to NaN with the same
//!   recovery — slice 6's landed convention). Python's own json
//!   writes a bare `Infinity` token its reader round-trips; the
//!   rust pair never crosses languages (the estate motion installs
//!   the whole body at once). Recorded divergence.
//! - python's json module also ACCEPTS the `Infinity` token in
//!   responses; serde_json rejects it and the whole body parse
//!   fails → the gather fails → inf. Same terminal value, different
//!   path; recorded here rather than faked.

pub mod http;
pub mod prometheus;
