//! The effectuation components — ports of `effectuation/http.py`
//! (apply settings over HTTP with per-target auth) and
//! `effectuation/ssh.py` (apply settings by running the windmill
//! ansible playbook per target), the two windmill scripts the
//! worker runs by path (`f/effectuation/{type}`).
//!
//! Standing shape (mirrors the python files): the components take
//! `(context, targets, settings)`; info/error lines are eprintlns in
//! order; every per-target failure becomes a result row, never a
//! raised job (the python caught per-target exceptions and carried
//! them as rows).

pub mod http;
pub mod ssh;
