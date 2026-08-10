//! Keeping the dev environment *itself* current — the repo holding
//! `tilt-devenv.json`, as opposed to the service repos the rest of the domain
//! manages.
//!
//! Nothing else fetches it: `repos` only ever touches the repos in the
//! registry, so without this a developer runs an out-of-date Tiltfile until
//! they think to pull by hand. The daemon offers it as a button; `repos update`
//! is the same thing from a terminal.
//!
//! Applying an update is a fast-forward, plus a *restart* when the dev
//! environment has a shell to re-enter (see [`has_dev_shell`](DevEnv::has_dev_shell)
//! and [`crate::supervisor`]). Without one, the pull is the whole of it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{git, supervisor};

/// The dev-env repo: the git working tree holding `tilt-devenv.json`.
pub struct DevEnv {
    root: PathBuf,
}

/// What applying an update accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Pulled, and Tilt is coming back up on the new version.
    Restarting,
    /// Pulled, but nothing restarted it — a bare `tilt up` has no supervisor.
    PulledOnly,
}

impl DevEnv {
    /// The dev environment at `root`, or `None` when that isn't a git working
    /// tree — a dev-env checked in nowhere has no updates to offer.
    pub fn at(root: &Path) -> Option<DevEnv> {
        git::is_repo(root).then(|| DevEnv {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether this dev environment puts you in a shell — a nix flake or a
    /// direnv `.envrc` — that a pull can change.
    ///
    /// This is the only reason an update needs a *restart*: a moved `flake.nix`
    /// or `.envrc` reaches a process only if it started afterwards. Without one
    /// there is nothing a restart would pick up, so don't ask for one.
    pub fn has_dev_shell(&self) -> bool {
        ["flake.nix", ".envrc"]
            .iter()
            .any(|f| self.root.join(f).exists())
    }

    /// Commits the dev-env repo is behind its upstream, from local refs only —
    /// call [`fetch`](Self::fetch) first for a live answer. Zero when the
    /// branch has no upstream, or when git can't be read at all: an update
    /// button is an offer, and there's nothing to offer without a known remote.
    pub fn behind(&self) -> i32 {
        match git::get_status(&self.root) {
            Ok(s) => s.behind,
            Err(e) => {
                tracing::debug!(root = %self.root.display(), error = %e, "dev-env status unreadable");
                0
            }
        }
    }

    /// Updates the dev-env repo's remote-tracking refs. Best-effort: offline,
    /// the last known count stands rather than dropping to zero.
    pub fn fetch(&self) {
        if let Err(e) = git::fetch(&self.root) {
            tracing::debug!(root = %self.root.display(), error = %e, "dev-env fetch failed");
        }
    }

    /// Fast-forwards the dev-env repo to its upstream. Never merges or rebases,
    /// so local work is refused rather than silently reconciled.
    pub fn pull(&self) -> Result<()> {
        git::fetch(&self.root).context("fetching the dev environment")?;
        git::fast_forward(&self.root).with_context(|| {
            format!(
                "can't fast-forward the dev environment at {} — save or undo your own \
                 changes there, then click update again",
                self.root.display()
            )
        })
    }

    /// Pulls, then restarts Tilt when running under `repos up` so the new
    /// version takes effect. Reports [`Update::PulledOnly`] when there's no
    /// supervisor to restart — the pull still landed.
    pub fn update(&self) -> Result<Update> {
        self.pull()?;
        if supervisor::marker().is_none() {
            return Ok(Update::PulledOnly);
        }
        supervisor::request_restart().context("restarting Tilt on the new version")?;
        Ok(Update::Restarting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gittest;
    use tempfile::TempDir;

    /// A dev-env clone of a bare origin, plus that origin — so `behind` has a
    /// real upstream to count against. Keep both alive for the test.
    fn cloned_dev_env() -> (TempDir, TempDir, TempDir) {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let clone = gittest::clone(origin.path());
        (seed, origin, clone)
    }

    /// Adds a commit to `origin` (via `seed`, which already points at it), so
    /// the dev-env clone falls behind.
    fn push_upstream_commit(seed: &Path, origin: &Path, msg: &str) {
        gittest::commit(seed, "new.txt", msg, msg);
        gittest::git(seed, &["push", &origin.to_string_lossy(), "HEAD:main"]);
    }

    #[test]
    fn should_not_offer_updates_outside_a_git_repo() {
        let dir = TempDir::new().unwrap();
        assert!(DevEnv::at(dir.path()).is_none());
    }

    #[test]
    fn a_plain_dev_environment_has_no_shell_to_re_enter() {
        let (_seed, _origin, clone) = cloned_dev_env();
        assert!(!DevEnv::at(clone.path()).unwrap().has_dev_shell());
    }

    #[test]
    fn a_flake_or_envrc_marks_a_shell_a_pull_can_change() {
        for marker in ["flake.nix", ".envrc"] {
            let (_seed, _origin, clone) = cloned_dev_env();
            std::fs::write(clone.path().join(marker), "").unwrap();
            assert!(
                DevEnv::at(clone.path()).unwrap().has_dev_shell(),
                "{marker} should mark a dev shell"
            );
        }
    }

    #[test]
    fn should_report_zero_behind_when_level_with_upstream() {
        let (_seed, _origin, clone) = cloned_dev_env();
        let dev = DevEnv::at(clone.path()).unwrap();
        assert_eq!(dev.behind(), 0);
    }

    #[test]
    fn should_count_commits_behind_after_fetching() {
        let (seed, origin, clone) = cloned_dev_env();
        let dev = DevEnv::at(clone.path()).unwrap();

        push_upstream_commit(seed.path(), origin.path(), "upstream work");
        assert_eq!(dev.behind(), 0, "local refs know nothing until a fetch");

        dev.fetch();
        assert_eq!(dev.behind(), 1);
    }

    #[test]
    fn should_fast_forward_to_upstream_on_pull() {
        let (seed, origin, clone) = cloned_dev_env();
        let dev = DevEnv::at(clone.path()).unwrap();
        push_upstream_commit(seed.path(), origin.path(), "upstream work");

        dev.pull().unwrap();

        assert_eq!(dev.behind(), 0, "level with upstream after pulling");
        assert!(clone.path().join("new.txt").exists(), "the update landed");
    }

    #[test]
    fn should_refuse_to_pull_over_local_commits_rather_than_merge_them() {
        let (seed, origin, clone) = cloned_dev_env();
        let dev = DevEnv::at(clone.path()).unwrap();
        push_upstream_commit(seed.path(), origin.path(), "upstream work");
        gittest::commit(clone.path(), "mine.txt", "local\n", "my own change");

        let err = dev
            .pull()
            .expect_err("diverged history must not fast-forward");

        assert!(
            format!("{err:#}").contains("save or undo your own changes"),
            "error should say what to do, got: {err:#}"
        );
        assert!(
            clone.path().join("mine.txt").exists(),
            "the developer's own work must survive a refused update"
        );
    }

    #[test]
    fn should_report_pulled_only_when_nothing_can_restart_tilt() {
        // The daemon's own test process has no REPOS_RESTART_MARKER, which is
        // exactly the bare `tilt up` case.
        let (seed, origin, clone) = cloned_dev_env();
        let dev = DevEnv::at(clone.path()).unwrap();
        push_upstream_commit(seed.path(), origin.path(), "upstream work");

        assert_eq!(dev.update().unwrap(), Update::PulledOnly);
        assert_eq!(dev.behind(), 0, "the pull still landed");
    }
}
