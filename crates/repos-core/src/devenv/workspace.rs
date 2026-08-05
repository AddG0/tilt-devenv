use std::sync::Arc;

use rayon::prelude::*;

use super::{CheckoutTarget, Config, OpResult, Presenter, Project, Snapshot};
use crate::registry::Registry;

/// The set of projects plus the cross-repo operations over them.
pub struct Workspace {
    projects: Vec<Arc<Project>>,
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

    /// Clones every project not yet on disk, concurrently. Already-present
    /// projects are reported as-is, untouched.
    pub fn clone_missing(&self) -> Vec<OpResult> {
        self.projects
            .par_iter()
            .map(|p| p.clone_if_missing())
            .collect()
    }

    /// The name and error message of every project whose remote isn't
    /// accessible, checked concurrently via [`Project::check_access`] —
    /// verifying access to a profile's repos before persisting it.
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
            group: String::new(),
            resource: name.to_string(),
            path: path.to_path_buf(),
            url: String::new(),
        }
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
