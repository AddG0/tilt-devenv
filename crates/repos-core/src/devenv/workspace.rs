use std::sync::Arc;

use rayon::prelude::*;

use super::{CheckoutTarget, Config, OpResult, Presenter, Project, Snapshot};
use crate::registry::Registry;

/// The set of projects plus the cross-repo operations over them.
pub struct Workspace {
    projects: Vec<Arc<Project>>,
}

/// Every profile that reaches a repo this machine can't clone, checked live
/// across the *whole* registry rather than just what's currently scoped in — so
/// a caller can leave those profiles out of a picker entirely instead of
/// failing the selection after the fact.
///
/// Only counts remotes that answered and refused, so offline this is empty and
/// every profile stays pickable. Does network I/O: one `git ls-remote` per
/// not-yet-cloned repo, so cache the result rather than calling it per profile.
pub fn unreachable_profiles(reg: &Registry) -> Vec<String> {
    let denied = Workspace::from_registry(reg).access_denied();
    let denied: Vec<&str> = denied.iter().map(String::as_str).collect();
    reg.profiles_reaching(&denied)
}

impl Workspace {
    /// Builds a Workspace from configs, giving each project a presenter from
    /// `view`. Used by the daemon (Tilt adapter).
    pub fn with_presenter<F>(cfgs: Vec<Config>, view: F) -> Workspace
    where
        F: Fn(&Config) -> Box<dyn Presenter>,
    {
        let projects = cfgs
            .into_iter()
            .map(|c| {
                let v = view(&c);
                Arc::new(Project::new(c, Some(v)))
            })
            .collect();
        Workspace { projects }
    }

    /// Builds a presenter-less Workspace (e.g. the CLI).
    pub fn plain(cfgs: Vec<Config>) -> Workspace {
        Workspace {
            projects: cfgs
                .into_iter()
                .map(|c| Arc::new(Project::new(c, None)))
                .collect(),
        }
    }

    /// Builds a presenter-less Workspace from the shared registry (CLI use). The
    /// Tilt resource defaults to the repo name.
    pub fn load() -> anyhow::Result<Workspace> {
        Ok(Workspace::from_registry(&Registry::load()?))
    }

    /// Like [`load`](Self::load), from an already-loaded registry — for callers
    /// that also need the registry itself (e.g. to expand `--profile`).
    pub fn from_registry(reg: &Registry) -> Workspace {
        let cfgs = reg
            .resolve()
            .into_iter()
            .map(|r| Config {
                name: r.repo.name.clone(),
                group: r.repo.group,
                resource: r.repo.name,
                path: r.path,
                url: r.repo.url,
                mirror_branches: reg.mirror_branches.clone(),
            })
            .collect();
        Workspace::plain(cfgs)
    }

    pub fn projects(&self) -> &[Arc<Project>] {
        &self.projects
    }

    /// Finds the project a Tilt resource represents.
    pub fn by_resource(&self, resource: &str) -> Option<&Arc<Project>> {
        self.projects.iter().find(|p| p.resource() == resource)
    }

    /// Returns a sub-workspace of the projects matching the filters (sharing the
    /// same Project values). Each filter is ignored when empty; a project is kept
    /// when it satisfies every active filter — by name and by group.
    pub fn filter(&self, names: &[String], groups: &[String]) -> Workspace {
        let projects = self
            .projects
            .iter()
            .filter(|p| names.is_empty() || names.iter().any(|n| n == p.name()))
            .filter(|p| groups.is_empty() || groups.iter().any(|g| g == p.group()))
            .cloned()
            .collect();
        Workspace { projects }
    }

    /// The distinct groups these projects belong to, sorted; an ungrouped
    /// project contributes none. Over a [`filter`](Self::filter)ed workspace:
    /// the groups that scope reaches.
    pub fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = self
            .projects
            .iter()
            .map(|p| p.group().to_string())
            .filter(|g| !g.is_empty())
            .collect();
        groups.sort();
        groups.dedup();
        groups
    }

    /// Refreshes every project (fetching first when `fetch` is true) and returns
    /// their snapshots, in workspace order. A failed fetch is recorded on the
    /// snapshot's `fetch_err` without blanking the (still-valid) local status.
    pub fn status_all(&self, fetch: bool) -> Vec<Snapshot> {
        self.projects
            .par_iter()
            .map(|p| {
                let fetch_err = if fetch {
                    p.fetch().err().map(|e| e.to_string())
                } else {
                    None
                };
                let mut s = p.refresh();
                s.fetch_err = fetch_err;
                s
            })
            .collect()
    }

    /// The projects whose remote *answered and refused*. Empty when offline: an
    /// unreachable network says nothing about whether you have access, and
    /// treating it as a refusal would strip a developer's profiles on a plane.
    pub fn access_denied(&self) -> Vec<String> {
        self.projects
            .par_iter()
            .filter(|p| matches!(p.check_access(), Err(crate::git::Error::AccessDenied(_))))
            .map(|p| p.name().to_string())
            .collect()
    }

    /// Switches every project to `target` (default-branch fallback where absent).
    pub fn checkout_all(&self, target: &CheckoutTarget) -> Vec<OpResult> {
        self.projects
            .par_iter()
            .map(|p| p.checkout(target))
            .collect()
    }

    /// Reports what [`checkout_all`](Self::checkout_all) would do, changing nothing.
    pub fn plan_checkout_all(&self, target: &CheckoutTarget) -> Vec<OpResult> {
        self.projects
            .par_iter()
            .map(|p| p.plan_checkout(target))
            .collect()
    }

    /// Fast-forwards every project to its upstream.
    pub fn pull_all(&self) -> Vec<OpResult> {
        self.projects.par_iter().map(|p| p.pull()).collect()
    }

    /// Updates every project's remote-tracking refs, concurrently. Best-effort:
    /// errors are ignored — the operation that follows reports the real state.
    pub fn fetch_all(&self) {
        self.projects.par_iter().for_each(|p| {
            let _ = p.fetch();
        });
    }

    /// Makes every project render-inert and tears down its presentation,
    /// returning the name and error of each that couldn't be torn down —
    /// retiring the rest regardless, since this runs on the way out.
    ///
    /// Sequential, unlike the git ops: it drives the presentation seam, where
    /// there's nothing to overlap.
    pub fn retire_all(&self) -> Vec<(String, anyhow::Error)> {
        self.projects
            .iter()
            .filter_map(|p| p.retire().err().map(|e| (p.name().to_string(), e)))
            .collect()
    }

    /// Clones every project not yet on disk, concurrently. Already-present
    /// projects are reported as-is, untouched.
    pub fn clone_missing(&self) -> Vec<OpResult> {
        self.projects
            .par_iter()
            .map(|p| p.clone_if_missing())
            .collect()
    }

    /// The name and error of every project whose remote didn't answer cleanly.
    ///
    /// Diagnostic only: it includes failures that prove nothing about access —
    /// offline, a bad URL — so don't gate a decision on it. Use
    /// [`access_denied`](Self::access_denied) for that.
    pub fn inaccessible(&self) -> Vec<(String, String)> {
        self.projects
            .par_iter()
            .filter_map(|p| match p.check_access() {
                Ok(()) => None,
                Err(e) => Some((p.name().to_string(), e.to_string())),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devenv::Outcome;
    use crate::gittest;
    use std::collections::HashMap;
    use std::path::Path;

    fn config(name: &str, path: &Path) -> Config {
        Config {
            name: name.to_string(),
            resource: name.to_string(),
            path: path.to_path_buf(),
            ..Default::default()
        }
    }

    fn grouped(repos: &[(&str, &str)]) -> Workspace {
        Workspace::plain(
            repos
                .iter()
                .map(|(name, group)| Config {
                    group: group.to_string(),
                    ..config(name, Path::new(""))
                })
                .collect(),
        )
    }

    struct FailingRemove;
    impl Presenter for FailingRemove {
        fn render(&self, _: &Snapshot) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove(&self) -> anyhow::Result<()> {
            anyhow::bail!("tilt refused the delete")
        }
    }

    struct RecordRemove(Arc<std::sync::atomic::AtomicBool>);
    impl Presenter for RecordRemove {
        fn render(&self, _: &Snapshot) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove(&self) -> anyhow::Result<()> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn should_retire_the_remaining_projects_after_one_teardown_fails() {
        let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = torn_down.clone();
        let ws = Workspace::with_presenter(
            vec![
                config("first", Path::new("")),
                config("second", Path::new("")),
            ],
            move |c| match c.name.as_str() {
                "first" => Box::new(FailingRemove),
                _ => Box::new(RecordRemove(flag.clone())),
            },
        );

        let failed = ws.retire_all();

        assert!(
            torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "a failure on the way out must not strand the other projects' buttons"
        );
        assert_eq!(
            failed
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["first"],
        );
    }

    #[test]
    fn should_list_each_group_once_in_order() {
        let ws = grouped(&[("web", "frontend"), ("auth", "backend"), ("ui", "frontend")]);

        assert_eq!(
            ws.groups(),
            vec!["backend".to_string(), "frontend".to_string()]
        );
    }

    #[test]
    fn should_leave_an_ungrouped_project_out_of_the_groups() {
        let ws = grouped(&[("web", "frontend"), ("loose", "")]);

        assert_eq!(ws.groups(), vec!["frontend".to_string()]);
    }

    #[test]
    fn should_list_only_the_groups_a_filtered_scope_reaches() {
        let ws = grouped(&[("web", "frontend"), ("auth", "backend")]);

        assert_eq!(
            ws.filter(&["web".to_string()], &[]).groups(),
            vec!["frontend".to_string()],
            "the name filter left backend empty, so offering it would select nothing"
        );
    }

    #[test]
    fn a_remote_that_never_answered_is_not_a_refusal() {
        // git appends "Could not read from remote repository" to every failure,
        // so a bogus URL and an offline SSH remote look alike at the surface.
        gittest::isolate();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::plain(vec![Config {
            url: "/no/such/remote".to_string(),
            ..config("gone", &dir.path().join("not-cloned"))
        }]);

        assert!(ws.access_denied().is_empty(), "not a refusal");
        assert_eq!(ws.inaccessible().len(), 1, "still worth reporting though");
    }

    #[test]
    fn filter_and_batch_ops() {
        gittest::isolate();
        let a = gittest::init_repo();
        gittest::git(a.path(), &["branch", "feature"]);
        let b = gittest::init_repo();

        let ws = Workspace::plain(vec![config("a", a.path()), config("b", b.path())]);

        assert!(ws.by_resource("a").is_some(), "by_resource(a) not found");
        let filtered = ws.filter(&["a".to_string()], &[]);
        assert_eq!(filtered.projects().len(), 1);
        assert_eq!(filtered.projects()[0].name(), "a");

        let results = ws.checkout_all(&CheckoutTarget::parse("feature").unwrap());
        let got: HashMap<&str, Outcome> = results
            .iter()
            .map(|r| (r.name.as_str(), r.outcome))
            .collect();
        assert_eq!(got["a"], Outcome::OnBranch);
        assert_eq!(got["b"], Outcome::FellBack);
    }

    #[test]
    fn clone_missing_clones_absent_and_leaves_present_projects_alone() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let dest = tempfile::TempDir::new().unwrap();
        let absent_path = dest.path().join("absent");
        let already = gittest::init_repo();

        let mut absent_cfg = config("absent", &absent_path);
        absent_cfg.url = origin.path().to_string_lossy().into_owned();
        let ws = Workspace::plain(vec![absent_cfg, config("already", already.path())]);

        let results = ws.clone_missing();
        let got: HashMap<&str, Outcome> = results
            .iter()
            .map(|r| (r.name.as_str(), r.outcome))
            .collect();
        assert_eq!(got["absent"], Outcome::Cloned);
        assert_eq!(got["already"], Outcome::AlreadyPresent);
        assert!(
            crate::git::is_repo(&absent_path),
            "clone should have left a working tree on disk"
        );
    }

    #[test]
    fn inaccessible_reports_only_unreachable_projects_not_already_present() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let dest = tempfile::TempDir::new().unwrap();
        let already = gittest::init_repo();

        let mut reachable = config("reachable", &dest.path().join("reachable"));
        reachable.url = origin.path().to_string_lossy().into_owned();
        let mut unreachable = config("unreachable", &dest.path().join("unreachable"));
        unreachable.url = "/no/such/remote".to_string();
        let mut already_cfg = config("already", already.path());
        already_cfg.url = "/no/such/remote".to_string();

        let ws = Workspace::plain(vec![reachable, unreachable, already_cfg]);
        let inaccessible = ws.inaccessible();
        let names: Vec<&str> = inaccessible.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["unreachable"]);
    }

    #[test]
    fn filter_by_group_keeps_only_matching_groups() {
        gittest::isolate();
        let a = gittest::init_repo();
        let b = gittest::init_repo();
        let mut ca = config("a", a.path());
        ca.group = "backend".into();
        let mut cb = config("b", b.path());
        cb.group = "analytics".into();
        let ws = Workspace::plain(vec![ca, cb]);

        let backend = ws.filter(&[], &["backend".to_string()]);
        assert_eq!(backend.projects().len(), 1);
        assert_eq!(backend.projects()[0].name(), "a");

        // Empty filters keep the whole workspace.
        assert_eq!(ws.filter(&[], &[]).projects().len(), 2);
    }
}
