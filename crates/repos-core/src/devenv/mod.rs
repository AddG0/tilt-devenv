//! The domain for managing the dev environment's git repos.
//!
//! The aggregate root is [`Project`] — one checkout with its live branch state
//! and the operations you can perform on it. [`Workspace`] is the collection of
//! projects and the cross-repo operations over them. A [`Project`] renders
//! itself through the [`Presenter`] port; the `repos` bin's Tilt adapter
//! implements that, so the domain stays free of Tilt/git-I/O coupling and is
//! unit-testable with a fake presenter.

mod branch;
mod project;
mod workspace;

pub use branch::{BranchName, CheckoutTarget, DEFAULT_ALIAS, DomainError};
pub use project::Project;
pub use workspace::Workspace;

use std::path::PathBuf;

/// Identifies a project: its registry name and group, the Tilt resource that
/// represents it (defaults to the name outside Tilt), and its on-disk path.
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub group: String,
    pub resource: String,
    pub path: PathBuf,
}

/// The current information about a project — a value object copied out from
/// under the aggregate's lock.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub name: String,
    pub group: String,
    pub present: bool,
    pub branch: String,
    pub detached: bool,
    pub upstream: String,
    pub ahead: i32,
    pub behind: i32,
    pub dirty: bool,
    pub default_branch: String,
    /// Status couldn't be read.
    pub err: Option<String>,
    /// A `--fetch` failed; local status is still valid, sync counts may be stale.
    pub fetch_err: Option<String>,
}

impl Snapshot {
    /// False when the default branch is unknown (couldn't be determined).
    pub fn is_on_default_branch(&self) -> bool {
        !self.default_branch.is_empty() && self.branch == self.default_branch
    }
}

/// What an operation did to a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Switched to (or already on) the requested branch.
    OnBranch,
    /// Branch absent; switched to the default branch instead.
    FellBack,
    /// Uncommitted changes; left untouched.
    SkippedDirty,
    /// Repo not cloned on disk.
    Missing,
    /// A git command failed.
    Errored,
    /// Fast-forwarded to upstream.
    Pulled,
    /// Already level with upstream.
    UpToDate,
}

impl Outcome {
    /// A short kebab-case label for log lines.
    pub fn label(self) -> &'static str {
        match self {
            Outcome::OnBranch => "on-branch",
            Outcome::FellBack => "fell-back",
            Outcome::SkippedDirty => "skipped-dirty",
            Outcome::Missing => "missing",
            Outcome::Errored => "error",
            Outcome::Pulled => "pulled",
            Outcome::UpToDate => "up-to-date",
        }
    }
}

pub fn count_with_outcome(results: &[OpResult], outcome: Outcome) -> usize {
    results.iter().filter(|r| r.outcome == outcome).count()
}

/// The outcome of an operation on a single project.
#[derive(Debug, Clone)]
pub struct OpResult {
    pub name: String,
    pub outcome: Outcome,
    pub branch: String,
    pub err: Option<String>,
}

impl OpResult {
    fn new(name: &str) -> OpResult {
        OpResult {
            name: name.to_string(),
            outcome: Outcome::OnBranch,
            branch: String::new(),
            err: None,
        }
    }

    fn missing(name: &str) -> OpResult {
        OpResult {
            outcome: Outcome::Missing,
            ..OpResult::new(name)
        }
    }

    fn errored(name: &str, err: impl std::fmt::Display) -> OpResult {
        OpResult {
            outcome: Outcome::Errored,
            err: Some(err.to_string()),
            ..OpResult::new(name)
        }
    }

    fn skipped_dirty(name: &str, branch: String) -> OpResult {
        OpResult {
            outcome: Outcome::SkippedDirty,
            branch,
            ..OpResult::new(name)
        }
    }
}

/// The port a [`Project`] renders itself through. Adapters (e.g. the Tilt
/// adapter in the `repos` bin) implement it. Implementations own their own I/O
/// error handling. `Send + Sync` because the daemon shares projects across tasks.
pub trait Presenter: Send + Sync {
    fn render(&self, snap: &Snapshot) -> anyhow::Result<()>;
    fn remove(&self) -> anyhow::Result<()>;
}

/// The default presenter when there's no UI (e.g. the CLI).
pub struct NopPresenter;

impl Presenter for NopPresenter {
    fn render(&self, _snap: &Snapshot) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
