//! Reusable core for the dev-environment tools.
//!
//! - [`git`] — thin git-CLI wrappers for the operations we need (status,
//!   checkout, and listing a repo's worktrees).
//! - [`registry`] — parses `tilt-devenv.json` and resolves each repo to its on-disk
//!   path, mirroring the Tiltfile's resolution rules.
//! - [`worktree`] — the developer's active per-repo worktree selection (XDG
//!   state), the top-priority input to path resolution.
//! - [`profile`] — the developer's active profile selection (XDG state),
//!   persisted the same way as the worktree selection.
//! - [`logstamp`] — decides the timestamp lnav orders each demuxed Tilt log line
//!   by, because lnav's own per-file format detection can't be trusted with a mix
//!   of services' log shapes.
//! - [`devenv`] — the domain: a [`devenv::Project`] aggregate (one checkout with
//!   its live state + operations), a [`devenv::Workspace`] collection, and a
//!   [`devenv::Presenter`] port that adapters (e.g. the daemon's Tilt button
//!   adapter) render through.
//! - [`tilt`] — the low-level Tilt UIButton/UIResource client (the seam both
//!   frontends drive Tilt through), independent of which buttons a caller shows.
//! - [`state`] — where that per-developer state is kept on disk.
//! - [`selfupdate`] — keeping the dev-env repo itself current (the one holding
//!   `tilt-devenv.json`), which nothing else fetches.
//! - [`supervisor`] — the restart contract between `repos up` and the daemon,
//!   so updating the dev-env repo itself can relaunch Tilt in the new dev shell.

pub mod devenv;
pub mod git;
pub mod logstamp;
pub mod profile;
pub mod registry;
pub mod selfupdate;
pub mod state;
pub mod supervisor;
pub mod tilt;
pub mod worktree;

#[cfg(any(test, feature = "testing"))]
pub mod gittest;
