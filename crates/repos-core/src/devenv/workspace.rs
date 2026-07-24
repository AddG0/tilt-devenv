use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use super::{CheckoutTarget, Config, OpResult, Presenter, Project, Snapshot};
use crate::registry::Registry;

/// Bounds parallel git work across projects, so a batch op over ~13 repos
/// doesn't spawn a process storm.
const MAX_CONCURRENCY: usize = 8;

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
        let reg = Registry::load()?;
        let cfgs = reg
            .resolve()
            .into_iter()
            .map(|r| Config {
                name: r.repo.name.clone(),
                group: r.repo.group,
                resource: r.repo.name,
                path: r.path,
            })
            .collect();
        Ok(Workspace::plain(cfgs))
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
        map_concurrent(&self.projects, |p| {
            let fetch_err = if fetch {
                p.fetch().err().map(|e| e.to_string())
            } else {
                None
            };
            let mut s = p.refresh();
            s.fetch_err = fetch_err;
            s
        })
    }

    /// Switches every project to `target` (default-branch fallback where absent).
    pub fn checkout_all(&self, target: &CheckoutTarget) -> Vec<OpResult> {
        map_concurrent(&self.projects, |p| p.checkout(target))
    }

    /// Reports what [`checkout_all`](Self::checkout_all) would do, changing nothing.
    pub fn plan_checkout_all(&self, target: &CheckoutTarget) -> Vec<OpResult> {
        map_concurrent(&self.projects, |p| p.plan_checkout(target))
    }

    /// Fast-forwards every project to its upstream.
    pub fn pull_all(&self) -> Vec<OpResult> {
        map_concurrent(&self.projects, |p| p.pull())
    }

    /// Updates every project's remote-tracking refs, concurrently. Best-effort:
    /// errors are ignored — the operation that follows reports the real state.
    pub fn fetch_all(&self) {
        map_concurrent(&self.projects, |p| {
            let _ = p.fetch();
        });
    }
}

/// Maps `f` over the projects concurrently, bounded to [`MAX_CONCURRENCY`], and
/// returns results in the original order. Uses scoped threads (git ops are
/// blocking subprocess calls), so the library needs no async runtime.
fn map_concurrent<T, F>(projects: &[Arc<Project>], f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&Arc<Project>) -> T + Sync,
{
    let sem = Semaphore::new(MAX_CONCURRENCY);
    let mut out: Vec<Option<T>> = Vec::new();
    out.resize_with(projects.len(), || None);
    thread::scope(|scope| {
        for (slot, p) in out.iter_mut().zip(projects.iter()) {
            let sem = &sem;
            let f = &f;
            scope.spawn(move || {
                sem.acquire();
                let r = f(p);
                sem.release();
                *slot = Some(r);
            });
        }
    });
    out.into_iter()
        .map(|o| o.expect("scoped task set its slot"))
        .collect()
}

/// A minimal counting semaphore (std has none) for bounding the fan-out.
struct Semaphore {
    permits: Mutex<usize>,
    available: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Semaphore {
        Semaphore {
            permits: Mutex::new(n),
            available: Condvar::new(),
        }
    }
    fn acquire(&self) {
        let mut n = self.permits.lock().unwrap();
        while *n == 0 {
            n = self.available.wait(n).unwrap();
        }
        *n -= 1;
    }
    fn release(&self) {
        *self.permits.lock().unwrap() += 1;
        self.available.notify_one();
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
