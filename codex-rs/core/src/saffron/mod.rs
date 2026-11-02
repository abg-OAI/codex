//! Saffrodex-owned extensions to the upstream Codex core.
//!
//! This module is the production-code boundary for behavior maintained by the
//! Saffrodex project. Upstream modules expose narrow, reusable primitives;
//! Saffron modules compose those primitives into complete features and export
//! only the integration points that upstream registration needs.

pub(crate) mod await_exec;
