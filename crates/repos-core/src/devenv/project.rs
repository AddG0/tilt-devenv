use std::path::Path;
use std::sync::Mutex;

use super::{
    BranchName, CheckoutTarget, Config, NopPresenter, OpResult, Outcome, Presenter, Snapshot,
};
use crate::git;

/// Branch that must always mirror the remote: on checkout, a stale local copy is
/// discarded in favour of `origin/nightly`.
const NIGHTLY: &str = "nightly";

/// The aggregate root: one repo checkout. It owns its current state, the
/// operations on it, and its own concurrency — every state read/write goes
/// through its mutex, so the daemon's tasks (fs-watch, clicks) need no
/// external locking.
pub struct Project {
    cfg: Config,
    view: Box<dyn Presenter>,
    inner: Mutex<Inner>,
}

struct Inner {
    snap: Snapshot,
    retired: bool,
}

impl Project {
    pub(super) fn new(cfg: Config, view: Option<Box<dyn Presenter>>) -> Project {
        let snap = base_snapshot(&cfg);
        Project {
            cfg,
            view: view.unwrap_or_else(|| Box::new(NopPresenter)),
            inner: Mutex::new(Inner {
                snap,
                retired: false,
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }
    pub fn resource(&self) -> &str {
        &self.cfg.resource
    }
    pub fn group(&self) -> &str {
        &self.cfg.group
    }
    pub fn path(&self) -> &Path {
        &self.cfg.path
    }

    /// Returns a copy of the project's current cached state.
    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().snap.clone()
    }

    /// Recomputes state from git, caches it, renders it, and returns it. After
    /// [`retire`](Self::retire) it still updates cached state but no longer
    /// renders, so a late refresh (a queued fs-watch) can't recreate the
    /// presentation post-teardown.
    pub fn refresh(&self) -> Snapshot {
        let snap = self.compute();
        let retired = {
            let mut inner = self.inner.lock().unwrap();
            inner.snap = snap.clone();
            inner.retired
        };
        if !retired && let Err(e) = self.view.render(&snap) {
            tracing::warn!("rendering {} failed: {e}", self.cfg.name);
        }
        snap
    }

    fn compute(&self) -> Snapshot {
        let mut s = base_snapshot(&self.cfg);
        if !git::is_repo(&self.cfg.path) {
            return s;
        }
        let st = match git::get_status(&self.cfg.path) {
            Ok(st) => st,
            Err(e) => {
                s.err = Some(e.to_string());
                return s;
            }
        };
        s.present = true;
        s.branch = st.branch;
        s.detached = st.detached;
        s.upstream = st.upstream;
        s.ahead = st.ahead;
        s.behind = st.behind;
        s.dirty = st.dirty;
        if let Ok(def) = git::default_branch(&self.cfg.path) {
            s.default_branch = def;
        }
        s
    }

    /// Switches the project to `target`, falling back to its default branch when
    /// a named branch exists nowhere. A dirty tree is left untouched. Refreshes
    /// after.
    pub fn checkout(&self, target: &CheckoutTarget) -> OpResult {
        self.do_checkout(target, true)
    }

    /// Reports what [`checkout`](Self::checkout) would do without switching
    /// anything — powers `--dry-run`.
    pub fn plan_checkout(&self, target: &CheckoutTarget) -> OpResult {
        self.do_checkout(target, false)
    }

    /// Gathers the repo's state (I/O), asks the pure [`decide`] what to do, then
    /// carries it out — switching (and refreshing) only when `apply` is set, so
    /// the plan and the real checkout run the same decision.
    fn do_checkout(&self, target: &CheckoutTarget, apply: bool) -> OpResult {
        let name = &self.cfg.name;
        if !git::is_repo(&self.cfg.path) {
            return OpResult::missing(name);
        }
        let st = match git::get_status(&self.cfg.path) {
            Ok(st) => st,
            Err(e) => return OpResult::errored(name, e),
        };

        match decide(target, &self.gather_facts(target, &st)) {
            Plan::SkippedDirty => OpResult::skipped_dirty(name, st.branch),
            Plan::Fail(e) => OpResult::errored(name, e),
            Plan::Switch {
                branch,
                outcome,
                mirror,
            } => {
                let on_branch = OpResult {
                    branch: branch.as_str().to_string(),
                    outcome,
                    ..OpResult::new(name)
                };
                if !apply {
                    return on_branch;
                }
                let switched = if mirror {
                    git::checkout_reset_to_remote(&self.cfg.path, branch.as_str())
                } else {
                    git::checkout(&self.cfg.path, branch.as_str())
                };
                self.refresh();
                match switched {
                    Ok(()) => on_branch,
                    Err(e) => OpResult::errored(name, e),
                }
            }
        }
    }

    /// Collects the facts [`decide`] needs: dirty state, the resolved default
    /// branch, and (for a named target) whether it exists locally / on origin.
    fn gather_facts(&self, target: &CheckoutTarget, st: &git::Status) -> Facts {
        let default_branch = git::default_branch(&self.cfg.path)
            .map(BranchName::from_trusted)
            .map_err(|e| e.to_string());
        let (local, remote) = match target {
            CheckoutTarget::Named(name) => git::branch_exists(&self.cfg.path, name.as_str()),
            CheckoutTarget::Default => (false, false),
        };
        Facts {
            dirty: st.dirty,
            default_branch,
            local,
            remote,
        }
    }

    /// Fast-forwards the project to its upstream. Fetches first, skips a dirty
    /// tree, and reports (rather than merges) a diverged branch. On `nightly`,
    /// force-resets to `origin/nightly` instead — same as checkout's nightly
    /// handling — since a fast-forward would fail once local and remote have
    /// diverged after a rewrite there. Refreshes after.
    pub fn pull(&self) -> OpResult {
        let name = &self.cfg.name;
        if !git::is_repo(&self.cfg.path) {
            return OpResult::missing(name);
        }
        if let Err(e) = git::fetch(&self.cfg.path) {
            return OpResult::errored(name, e);
        }
        let st = match git::get_status(&self.cfg.path) {
            Ok(st) => st,
            Err(e) => return OpResult::errored(name, e),
        };

        let mut res = OpResult::new(name);
        res.branch = st.branch.clone();
        if st.dirty {
            res.outcome = Outcome::SkippedDirty;
        } else if st.branch == NIGHTLY {
            if st.ahead == 0 && st.behind == 0 {
                res.outcome = Outcome::UpToDate;
            } else if let Err(e) = git::checkout_reset_to_remote(&self.cfg.path, NIGHTLY) {
                res.outcome = Outcome::Errored;
                res.err = Some(e.to_string());
            } else {
                res.outcome = Outcome::Pulled;
            }
        } else if st.behind == 0 {
            res.outcome = Outcome::UpToDate;
        } else if let Err(e) = git::fast_forward(&self.cfg.path) {
            res.outcome = Outcome::Errored;
            res.err = Some(e.to_string());
        } else {
            res.outcome = Outcome::Pulled;
        }
        self.refresh();
        res
    }

    /// Clones the project from `self.cfg.url` if it isn't already a working
    /// tree on disk. Refreshes after a real clone; an already-present repo is
    /// reported as such, untouched. Named to avoid colliding with `Clone`
    /// (`Arc<Project>::clone()` must keep cloning the pointer).
    pub fn clone_if_missing(&self) -> OpResult {
        let name = &self.cfg.name;
        if git::is_repo(&self.cfg.path) {
            return OpResult {
                outcome: Outcome::AlreadyPresent,
                ..OpResult::new(name)
            };
        }
        match git::clone(&self.cfg.url, &self.cfg.path) {
            Ok(()) => {
                self.refresh();
                OpResult {
                    outcome: Outcome::Cloned,
                    ..OpResult::new(name)
                }
            }
            Err(e @ git::Error::AccessDenied(_)) => OpResult {
                outcome: Outcome::AccessDenied,
                err: Some(e.to_string()),
                ..OpResult::new(name)
            },
            Err(e) => OpResult::errored(name, e),
        }
    }

    /// Whether the project's remote is reachable and permitted, checked via a
    /// lightweight `git ls-remote` rather than a clone. Skipped (`Ok`) when
    /// already on disk — already having it is already-proven access.
    pub fn check_access(&self) -> anyhow::Result<()> {
        if git::is_repo(&self.cfg.path) {
            return Ok(());
        }
        git::can_access(&self.cfg.url).map_err(Into::into)
    }

    /// Updates remote-tracking refs, returning any error (local status stays
    /// valid regardless). Callers refresh separately.
    pub fn fetch(&self) -> anyhow::Result<()> {
        if !git::is_repo(&self.cfg.path) {
            return Ok(());
        }
        git::fetch(&self.cfg.path).map_err(Into::into)
    }

    /// Makes the project render-inert and tears down its presentation. After
    /// this, [`refresh`](Self::refresh) updates state but never renders again.
    pub fn retire(&self) -> anyhow::Result<()> {
        self.inner.lock().unwrap().retired = true;
        self.view.remove()
    }
}

/// The observed repo state [`decide`] operates on. [`Project`] gathers it (the
/// I/O), so the decision itself stays pure and testable without git.
struct Facts {
    dirty: bool,
    /// The repo's default branch, or why it couldn't be determined.
    default_branch: Result<BranchName, String>,
    /// For a named target: whether it exists as a local / origin branch.
    local: bool,
    remote: bool,
}

/// What to do about a checkout, decided from [`Facts`] alone.
#[derive(Debug)]
enum Plan {
    /// Uncommitted changes; leave the tree untouched.
    SkippedDirty,
    /// Switch to `branch`; `mirror` force-resets it to the remote (nightly).
    Switch {
        branch: BranchName,
        outcome: Outcome,
        mirror: bool,
    },
    /// The target couldn't be resolved (e.g. no default branch to fall back to).
    Fail(String),
}

/// Pure checkout decision: the target plus observed facts in, the plan out — no
/// git, so every branch is exhaustively testable in isolation.
fn decide(target: &CheckoutTarget, f: &Facts) -> Plan {
    if f.dirty {
        return Plan::SkippedDirty;
    }
    match target {
        CheckoutTarget::Default => switch_to_default(f, Outcome::OnBranch),
        CheckoutTarget::Named(name) if f.local || f.remote => Plan::Switch {
            branch: name.clone(),
            // nightly always mirrors the remote, so force-reset to origin/nightly
            // rather than checking out a possibly-stale local copy.
            mirror: name.as_str() == NIGHTLY && f.remote,
            outcome: Outcome::OnBranch,
        },
        // A named branch that exists nowhere falls back to the default.
        CheckoutTarget::Named(_) => switch_to_default(f, Outcome::FellBack),
    }
}

/// Plan a switch onto the resolved default branch, or fail if there is none.
fn switch_to_default(f: &Facts, outcome: Outcome) -> Plan {
    match &f.default_branch {
        Ok(def) => Plan::Switch {
            branch: def.clone(),
            outcome,
            mirror: false,
        },
        Err(e) => Plan::Fail(e.clone()),
    }
}

/// A snapshot with only the project's identity filled in, before git state.
fn base_snapshot(cfg: &Config) -> Snapshot {
    Snapshot {
        name: cfg.name.clone(),
        group: cfg.group.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gittest;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake Presenter that captures the last rendered snapshot.
    #[derive(Default)]
    struct Recorder {
        last: StdMutex<Option<Snapshot>>,
        renders: AtomicUsize,
        removed: std::sync::atomic::AtomicBool,
    }
    impl Presenter for Recorder {
        fn render(&self, snap: &Snapshot) -> anyhow::Result<()> {
            *self.last.lock().unwrap() = Some(snap.clone());
            self.renders.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn remove(&self) -> anyhow::Result<()> {
            self.removed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn config(name: &str, path: &Path) -> Config {
        Config {
            name: name.to_string(),
            group: String::new(),
            resource: name.to_string(),
            path: path.to_path_buf(),
            url: String::new(),
        }
    }

    fn named(branch: &str) -> CheckoutTarget {
        CheckoutTarget::parse(branch).unwrap()
    }

    fn current_branch(dir: &Path) -> String {
        gittest::git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    }

    #[test]
    fn refresh_reflects_state_and_renders() {
        gittest::isolate();
        let dir = gittest::init_repo();
        gittest::write(dir.path(), "wip.txt", "x\n");

        // Recorder must outlive the Project's Box<dyn Presenter>; assert via a
        // second Arc handle.
        let rec = std::sync::Arc::new(Recorder::default());
        let p = Project::new(
            config("r", dir.path()),
            Some(Box::new(ArcPresenter(rec.clone()))),
        );
        let s = p.refresh();

        assert!(
            s.present && s.branch == "main" && s.dirty,
            "snapshot = {s:?}"
        );
        assert_eq!(rec.renders.load(Ordering::SeqCst), 1);
        assert_eq!(rec.last.lock().unwrap().as_ref().unwrap().branch, "main");
    }

    #[test]
    fn checkout_switches_falls_back_and_skips_dirty() {
        gittest::isolate();

        let with_feature = gittest::init_repo();
        gittest::git(with_feature.path(), &["branch", "feature"]);
        let r = Project::new(config("a", with_feature.path()), None).checkout(&named("feature"));
        assert_eq!(r.outcome, Outcome::OnBranch);
        assert_eq!(r.branch, "feature");
        assert_eq!(current_branch(with_feature.path()), "feature");

        let default_only = gittest::init_repo();
        let r = Project::new(config("b", default_only.path()), None).checkout(&named("feature"));
        assert_eq!(r.outcome, Outcome::FellBack);
        assert_eq!(r.branch, "main");

        let dirty = gittest::init_repo();
        gittest::write(dirty.path(), "wip.txt", "x\n");
        let r = Project::new(config("c", dirty.path()), None).checkout(&named("feature"));
        assert_eq!(r.outcome, Outcome::SkippedDirty);
    }

    #[test]
    fn checkout_default_alias_selects_the_repos_default_branch() {
        gittest::isolate();
        let dir = gittest::init_repo(); // default branch is main
        gittest::git(dir.path(), &["switch", "-c", "feature"]);

        let r = Project::new(config("r", dir.path()), None).checkout(&CheckoutTarget::Default);

        // Landing on the default is the requested outcome, not a fallback.
        assert_eq!(r.outcome, Outcome::OnBranch);
        assert_eq!(r.branch, "main");
        assert_eq!(current_branch(dir.path()), "main");
    }

    #[test]
    fn checkout_nightly_discards_local_and_takes_remote() {
        gittest::isolate();
        let seed = gittest::init_repo();
        gittest::git(seed.path(), &["branch", "nightly"]);
        let origin = gittest::clone_bare(seed.path());
        let work = gittest::clone(origin.path());

        // A stale local nightly, then start off it to prove the checkout resets it.
        gittest::git(work.path(), &["switch", "nightly"]);
        gittest::commit(work.path(), "stale.txt", "x\n", "stale local nightly");
        gittest::git(work.path(), &["switch", "main"]);

        let r = Project::new(config("n", work.path()), None).checkout(&named("nightly"));
        assert_eq!(r.outcome, Outcome::OnBranch);
        assert_eq!(current_branch(work.path()), "nightly");
        let head = gittest::git(work.path(), &["rev-parse", "HEAD"]);
        let origin_nightly = gittest::git(work.path(), &["rev-parse", "origin/nightly"]);
        assert_eq!(
            head.trim(),
            origin_nightly.trim(),
            "nightly reset to origin"
        );
    }

    #[test]
    fn checkout_nightly_falls_back_when_no_remote_nightly() {
        gittest::isolate();
        let dir = gittest::init_repo(); // main only; no nightly anywhere
        let r = Project::new(config("n", dir.path()), None).checkout(&named("nightly"));
        assert_eq!(r.outcome, Outcome::FellBack);
        assert_eq!(r.branch, "main");
    }

    #[test]
    fn plan_checkout_does_not_switch() {
        gittest::isolate();
        let dir = gittest::init_repo();
        gittest::git(dir.path(), &["branch", "feature"]);

        let r = Project::new(config("r", dir.path()), None).plan_checkout(&named("feature"));
        assert_eq!(r.outcome, Outcome::OnBranch);
        assert_eq!(current_branch(dir.path()), "main", "plan must not switch");
    }

    #[test]
    fn pull_fast_forwards_up_to_date_and_skips_dirty() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());

        let behind = gittest::clone(origin.path());
        let other = gittest::clone(origin.path());
        gittest::commit(other.path(), "b.txt", "b\n", "remote commit");
        gittest::git(other.path(), &["push", "origin", "main"]);

        let r = Project::new(config("behind", behind.path()), None).pull();
        assert_eq!(r.outcome, Outcome::Pulled);
        gittest::git(behind.path(), &["cat-file", "-e", "HEAD:b.txt"]); // commit arrived

        let up_to_date = gittest::clone(origin.path());
        let r = Project::new(config("up", up_to_date.path()), None).pull();
        assert_eq!(r.outcome, Outcome::UpToDate);

        let dirty = gittest::clone(origin.path());
        gittest::write(dirty.path(), "wip.txt", "x\n");
        let r = Project::new(config("dirty", dirty.path()), None).pull();
        assert_eq!(r.outcome, Outcome::SkippedDirty);
    }

    #[test]
    fn pull_on_nightly_mirrors_the_remote_when_diverged() {
        gittest::isolate();
        let seed = gittest::init_repo();
        gittest::git(seed.path(), &["branch", "nightly"]);
        let origin = gittest::clone_bare(seed.path());
        let work = gittest::clone(origin.path());

        // A stale local commit on nightly that a plain fast-forward can't reconcile.
        gittest::git(work.path(), &["switch", "nightly"]);
        gittest::commit(work.path(), "stale.txt", "x\n", "stale local nightly");

        // Meanwhile origin/nightly moved on independently (e.g. a nightly rebuild).
        let other = gittest::clone(origin.path());
        gittest::git(other.path(), &["switch", "nightly"]);
        gittest::commit(other.path(), "new.txt", "y\n", "new remote nightly");
        gittest::git(other.path(), &["push", "origin", "nightly"]);

        let r = Project::new(config("n", work.path()), None).pull();
        assert_eq!(r.outcome, Outcome::Pulled);
        assert_eq!(current_branch(work.path()), "nightly");
        let head = gittest::git(work.path(), &["rev-parse", "HEAD"]);
        let origin_nightly = gittest::git(work.path(), &["rev-parse", "origin/nightly"]);
        assert_eq!(
            head.trim(),
            origin_nightly.trim(),
            "nightly mirrored; stale local commit discarded"
        );
    }

    #[test]
    fn pull_on_nightly_reports_up_to_date_without_resetting() {
        gittest::isolate();
        let seed = gittest::init_repo();
        gittest::git(seed.path(), &["branch", "nightly"]);
        let origin = gittest::clone_bare(seed.path());
        let work = gittest::clone(origin.path());
        gittest::git(work.path(), &["switch", "nightly"]);

        let r = Project::new(config("n", work.path()), None).pull();
        assert_eq!(r.outcome, Outcome::UpToDate);
    }

    #[test]
    fn clone_if_missing_errors_for_an_unreachable_url() {
        gittest::isolate();
        let dest = tempfile::TempDir::new().unwrap();
        let mut cfg = config("bad", &dest.path().join("bad"));
        cfg.url = "/no/such/remote".to_string();

        let r = Project::new(cfg, None).clone_if_missing();
        assert_eq!(r.outcome, Outcome::Errored);
        assert!(r.err.is_some());
    }

    #[test]
    fn check_access_succeeds_for_a_reachable_remote_without_cloning_it() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let dest = tempfile::TempDir::new().unwrap();
        let target = dest.path().join("not-yet-cloned");
        let mut cfg = config("r", &target);
        cfg.url = origin.path().to_string_lossy().into_owned();

        Project::new(cfg, None).check_access().unwrap();
        assert!(
            !git::is_repo(&target),
            "check_access must not clone anything"
        );
    }

    #[test]
    fn check_access_skips_the_network_check_when_already_present() {
        gittest::isolate();
        let dir = gittest::init_repo();
        let mut cfg = config("r", dir.path());
        cfg.url = "/no/such/remote".to_string();

        Project::new(cfg, None)
            .check_access()
            .expect("already on disk, so the (bad) url is never consulted");
    }

    #[test]
    fn check_access_errors_for_an_unreachable_remote() {
        gittest::isolate();
        let dest = tempfile::TempDir::new().unwrap();
        let mut cfg = config("r", &dest.path().join("missing"));
        cfg.url = "/no/such/remote".to_string();

        assert!(Project::new(cfg, None).check_access().is_err());
    }

    #[test]
    fn retire_stops_rendering_and_removes() {
        gittest::isolate();
        let dir = gittest::init_repo();
        let rec = std::sync::Arc::new(Recorder::default());
        let p = Project::new(
            config("r", dir.path()),
            Some(Box::new(ArcPresenter(rec.clone()))),
        );

        p.retire().unwrap();
        assert!(rec.removed.load(Ordering::SeqCst), "Retire did not Remove");
        let before = rec.renders.load(Ordering::SeqCst);
        p.refresh(); // must not render after retirement
        assert_eq!(
            rec.renders.load(Ordering::SeqCst),
            before,
            "Refresh rendered after Retire"
        );
    }

    fn facts(dirty: bool, default: Result<&str, &str>, local: bool, remote: bool) -> Facts {
        Facts {
            dirty,
            default_branch: default
                .map(|s| BranchName::from_trusted(s.to_string()))
                .map_err(str::to_string),
            local,
            remote,
        }
    }

    #[test]
    fn decide_skips_a_dirty_tree_before_resolving_anything() {
        let p = decide(
            &CheckoutTarget::Default,
            &facts(true, Ok("main"), true, true),
        );
        assert!(matches!(p, Plan::SkippedDirty));
    }

    #[test]
    fn decide_default_target_lands_on_the_default_branch() {
        match decide(
            &CheckoutTarget::Default,
            &facts(false, Ok("develop"), false, false),
        ) {
            Plan::Switch {
                branch,
                outcome,
                mirror,
            } => {
                assert_eq!(branch.as_str(), "develop");
                assert_eq!(outcome, Outcome::OnBranch);
                assert!(!mirror);
            }
            other => panic!("want Switch, got {other:?}"),
        }
    }

    #[test]
    fn decide_named_branch_that_exists_is_on_branch() {
        let p = decide(&named("feature"), &facts(false, Ok("main"), true, false));
        assert!(matches!(
            p,
            Plan::Switch { branch, outcome: Outcome::OnBranch, mirror: false } if branch.as_str() == "feature"
        ));
    }

    #[test]
    fn decide_named_branch_missing_everywhere_falls_back_to_default() {
        let p = decide(&named("feature"), &facts(false, Ok("main"), false, false));
        assert!(matches!(
            p,
            Plan::Switch { branch, outcome: Outcome::FellBack, .. } if branch.as_str() == "main"
        ));
    }

    #[test]
    fn decide_fails_when_a_fallback_has_no_default_branch() {
        assert!(matches!(
            decide(
                &named("feature"),
                &facts(false, Err("no default"), false, false)
            ),
            Plan::Fail(_)
        ));
        assert!(matches!(
            decide(
                &CheckoutTarget::Default,
                &facts(false, Err("no default"), false, false)
            ),
            Plan::Fail(_)
        ));
    }

    #[test]
    fn decide_mirrors_nightly_only_when_it_exists_on_the_remote() {
        // On the remote: mirror (force-reset to origin/nightly).
        assert!(matches!(
            decide(&named("nightly"), &facts(false, Ok("main"), false, true)),
            Plan::Switch { mirror: true, .. }
        ));
        // Local-only nightly: a plain checkout, no mirror.
        assert!(matches!(
            decide(&named("nightly"), &facts(false, Ok("main"), true, false)),
            Plan::Switch { mirror: false, .. }
        ));
    }

    /// Adapts a shared `Arc<Recorder>` to the `Presenter` trait so a test can
    /// hold its own handle while the Project owns a boxed one.
    struct ArcPresenter(std::sync::Arc<Recorder>);
    impl Presenter for ArcPresenter {
        fn render(&self, snap: &Snapshot) -> anyhow::Result<()> {
            self.0.render(snap)
        }
        fn remove(&self) -> anyhow::Result<()> {
            self.0.remove()
        }
    }
}
