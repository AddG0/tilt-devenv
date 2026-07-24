//! Rendering `repos_core` values for the outside world, in two modes:
//!
//! - [`terminal`] — coloured tables and lines for a human at a terminal.
//! - [`json`] — the `--json` data contract that the Tiltfile and scripts consume.

pub mod json;
pub mod terminal;
