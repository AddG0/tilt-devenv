//! What the filesystem tells the daemon: which of its events mean a repo's git
//! state changed, and which directories have to be watched to hear them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{EventKind, RecursiveMode, Watcher};
use repos_core::devenv::{Project, Workspace};

/// Whether a `.git` filesystem event warrants a refresh: a change to HEAD or
/// index at the main checkout (branch switch / commit / stage), a worktree's
/// HEAD (a branch switch inside it), or a worktree added/removed under
/// `worktrees/`.
///
/// notify's inotify mask includes `IN_OPEN`, so the `git status` each refresh
/// runs re-arrives as an `Access(Open)` event on those same files — treating
/// that as a change makes refresh retrigger itself forever, pegging the CPU.
/// Real writes go through lockfile+rename or in-place modify, so restricting to
/// `Create`/`Modify`/`Remove` catches every genuine change while the read-only
/// opens/closes are ignored.
fn is_relevant_change(kind: &EventKind, path: &Path) -> bool {
    if !matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    // Ignore the worktree's `index` (git status churns it → refresh spin), but
    // react to its HEAD — a branch switch, which git status never writes.
    if is_under_worktrees(path) {
        return is_worktree_entry(path)
            || path.file_name().and_then(|n| n.to_str()) == Some("HEAD");
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "HEAD" || n == "index")
}

/// What a filesystem event means to the daemon. The watcher module decides what
/// paths mean; the daemon only performs the side effects.
#[derive(Default)]
pub(crate) struct WatchEffects {
    pub(crate) refresh: Vec<Arc<Project>>,
    pub(crate) removed_worktree_ids: Vec<String>,
}

impl WatchEffects {
    pub(crate) fn refresh(refresh: Vec<Arc<Project>>) -> WatchEffects {
        WatchEffects {
            refresh,
            removed_worktree_ids: Vec::new(),
        }
    }
}

/// Whether `path` lies within a repo's `worktrees/` metadata tree.
fn is_under_worktrees(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "worktrees")
}

/// Whether `path` is the `worktrees/` dir itself (first/last worktree) or a
/// `<id>` entry directly under it (a worktree added/removed) — the dir-level
/// events, not files inside a worktree's dir.
fn is_worktree_entry(path: &Path) -> bool {
    let name_is_worktrees = path.file_name().is_some_and(|n| n == "worktrees");
    let parent_is_worktrees = path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "worktrees");
    name_is_worktrees || parent_is_worktrees
}

/// Which projects the filesystem watcher is following, and where it waits for
/// the ones that aren't cloned. What to watch is each [`Project`]'s answer; this
/// only tracks what has been asked of notify.
pub(crate) struct Watches {
    /// (common git dir, project). One dir can serve several projects, since
    /// resources may share a repo. The recursive `worktrees/` watch goes
    /// unrecorded: it sits inside the common dir, so its events map here anyway.
    git: Vec<(PathBuf, Arc<Project>)>,
    /// Temporary watches for repos that are arriving but are not git-ready yet.
    arrivals: Vec<PathBuf>,
}

impl Watches {
    pub(crate) fn new() -> Watches {
        Watches {
            git: Vec::new(),
            arrivals: Vec::new(),
        }
    }

    /// Run on each arrival, and on each poll as a backstop: a repo cloned
    /// later — by a profile switch, or the first checkout of a missing one —
    /// would otherwise never be watched, since a Tiltfile whose resource list
    /// doesn't change never restarts the daemon. `startup` reports the repos that
    /// aren't there; later passes leave them be, an uncloned repo being nothing
    /// new.
    pub(crate) fn sync(
        &mut self,
        watcher: &mut notify::RecommendedWatcher,
        ws: &Workspace,
        startup: bool,
    ) -> Vec<Arc<Project>> {
        let mut newly_watched = Vec::new();
        let mut desired_arrivals = Vec::new();
        for p in ws.projects() {
            if self.git.iter().any(|(_, w)| w.resource() == p.resource()) {
                continue;
            }
            if self.watch_git(watcher, p, startup) {
                if !startup {
                    newly_watched.push(p.clone());
                }
                continue;
            }
            if startup {
                tracing::warn!(resource = p.resource(), "not watching repo (no git dir)");
            }
            if let Some(dir) = arrival_watch_dir(p.path()) {
                desired_arrivals.push(dir);
            }
        }
        self.sync_arrivals(watcher, desired_arrivals);
        for p in ws.projects() {
            if self.git.iter().any(|(_, w)| w.resource() == p.resource()) {
                continue;
            }
            if self.watch_git(watcher, p, startup) && !startup {
                newly_watched.push(p.clone());
            }
        }
        let desired_arrivals = ws
            .projects()
            .iter()
            .filter(|p| !self.git.iter().any(|(_, w)| w.resource() == p.resource()))
            .filter_map(|p| arrival_watch_dir(p.path()))
            .collect();
        self.sync_arrivals(watcher, desired_arrivals);
        newly_watched
    }

    pub(crate) fn handle_event(
        &mut self,
        watcher: &mut notify::RecommendedWatcher,
        ws: &Workspace,
        kind: &EventKind,
        path: &Path,
    ) -> WatchEffects {
        let mut effects = WatchEffects::default();
        if self.expects_arrival(kind, path) {
            effects.refresh.extend(self.sync(watcher, ws, false));
        }
        if !is_relevant_change(kind, path) {
            return effects;
        }
        if is_worktrees_dir_created(kind, path) {
            match watcher.watch(path, RecursiveMode::Recursive) {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(dir = %path.display(), error = %e, "not watching worktrees")
                }
            }
        }
        if let Some(id) = removed_worktree_id(kind, path) {
            effects.removed_worktree_ids.push(id.to_string());
        }
        for p in self.projects_at(path) {
            tracing::debug!(
                resource = p.resource(),
                "change detected; scheduling refresh"
            );
            effects.refresh.push(p.clone());
        }
        effects
    }

    fn watch_git(
        &mut self,
        watcher: &mut notify::RecommendedWatcher,
        p: &Arc<Project>,
        startup: bool,
    ) -> bool {
        let mut dirs = p.git_dirs().into_iter();
        let Some(common) = dirs.next() else {
            return false;
        };
        if let Err(e) = watcher.watch(&common, RecursiveMode::NonRecursive) {
            tracing::warn!(resource = p.resource(), error = %e, "not watching repo");
            return false;
        }
        // Each linked worktree's HEAD sits a level down, so this one is
        // recursive; a repo whose first worktree appears while we run is covered
        // by the `worktrees/` create event instead.
        for nested in dirs {
            if let Err(e) = watcher.watch(&nested, RecursiveMode::Recursive) {
                tracing::warn!(resource = p.resource(), dir = %nested.display(), error = %e, "not watching worktrees");
            }
        }
        if !startup {
            tracing::info!(
                resource = p.resource(),
                "watching repo, now that it's cloned"
            );
        }
        self.git.push((common, p.clone()));
        true
    }

    fn sync_arrivals(
        &mut self,
        watcher: &mut notify::RecommendedWatcher,
        mut desired: Vec<PathBuf>,
    ) {
        desired.sort();
        desired.dedup();
        for stale in self.arrivals.iter().filter(|p| !desired.contains(p)) {
            if let Err(e) = watcher.unwatch(stale) {
                tracing::debug!(dir = %stale.display(), error = %e, "failed to remove arrival watch");
            }
        }
        let mut active: Vec<PathBuf> = desired
            .iter()
            .filter(|p| self.arrivals.contains(p))
            .cloned()
            .collect();
        for dir in desired.iter().filter(|p| !self.arrivals.contains(p)) {
            match watcher.watch(dir, arrival_watch_mode(dir)) {
                Ok(()) => active.push(dir.clone()),
                Err(e) => {
                    tracing::debug!(dir = %dir.display(), error = %e, "not watching for an arrival")
                }
            }
        }
        self.arrivals = active;
    }

    fn expects_arrival(&self, kind: &EventKind, path: &Path) -> bool {
        matches!(kind, EventKind::Create(_) | EventKind::Modify(_))
            && self.arrivals.iter().any(|dir| path.starts_with(dir))
    }

    fn projects_at(&self, path: &Path) -> impl Iterator<Item = &Arc<Project>> {
        self.git
            .iter()
            .filter(move |(dir, _)| path.starts_with(dir))
            .map(|(_, p)| p)
    }
}

fn arrival_watch_dir(path: &Path) -> Option<PathBuf> {
    let git = path.join(".git");
    if git.is_dir() {
        return Some(git);
    }
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .map(Path::to_path_buf)
}

fn arrival_watch_mode(path: &Path) -> RecursiveMode {
    match path.file_name().is_some_and(|name| name == ".git") {
        true => RecursiveMode::Recursive,
        false => RecursiveMode::NonRecursive,
    }
}

fn is_worktrees_dir_created(kind: &EventKind, path: &Path) -> bool {
    matches!(kind, EventKind::Create(_))
        && path.file_name().and_then(|n| n.to_str()) == Some("worktrees")
}

fn removed_worktree_id<'a>(kind: &EventKind, path: &'a Path) -> Option<&'a str> {
    if !matches!(kind, EventKind::Remove(_)) {
        return None;
    }
    if !path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "worktrees")
    {
        return None;
    }
    path.file_name().and_then(|n| n.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
    };

    fn git_path(name: &str) -> PathBuf {
        PathBuf::from("/repo/.git").join(name)
    }

    /// The workspace matching [`broken_repo_registry`]. No path: switching a
    /// profile only reads these projects' groups.

    #[test]
    fn should_ignore_read_open_of_head_or_index() {
        // The regression: notify's inotify mask includes IN_OPEN, so every `git
        // status` the daemon runs to refresh re-arrives as an Access(Open) event
        // on these files. Reacting to it makes refresh retrigger itself forever.
        let open = EventKind::Access(AccessKind::Open(AccessMode::Any));
        assert!(!is_relevant_change(&open, &git_path("HEAD")));
        assert!(!is_relevant_change(&open, &git_path("index")));
    }

    #[test]
    fn should_ignore_read_close_of_head_or_index() {
        let close_read = EventKind::Access(AccessKind::Close(AccessMode::Read));
        assert!(!is_relevant_change(&close_read, &git_path("HEAD")));
        assert!(!is_relevant_change(&close_read, &git_path("index")));
    }

    #[test]
    fn should_refresh_when_head_or_index_is_modified() {
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(is_relevant_change(&modify, &git_path("HEAD")));
        assert!(is_relevant_change(&modify, &git_path("index")));
    }

    #[test]
    fn should_refresh_when_head_or_index_is_created() {
        // Git writes HEAD/index via a lockfile that it renames into place, which
        // surfaces as a create/rename rather than an in-place modify.
        let create = EventKind::Create(CreateKind::File);
        assert!(is_relevant_change(&create, &git_path("index")));
    }

    #[test]
    fn should_refresh_when_a_worktree_is_added_or_removed() {
        // `git worktree add/remove` creates/removes `.git/worktrees/<id>/`.
        let create = EventKind::Create(CreateKind::Folder);
        assert!(is_relevant_change(&create, &git_path("worktrees")));
        assert!(is_relevant_change(
            &create,
            &git_path("worktrees").join("feat-login")
        ));
    }

    #[test]
    fn should_ignore_index_churn_inside_a_worktree() {
        // `git status` in a linked worktree rewrites its own
        // `.git/worktrees/<id>/index`. Treating that as a change spins the watch
        // (refresh -> git status -> index write -> refresh ...), which pegs the
        // git-status resource.
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(!is_relevant_change(
            &modify,
            &git_path("worktrees").join("feat-login").join("index")
        ));
    }

    #[test]
    fn should_refresh_on_branch_switch_inside_a_worktree() {
        // A branch switch in a linked worktree rewrites its HEAD — the buttons
        // must reflect it (its `index`, churned by `git status`, stays ignored).
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(is_relevant_change(
            &modify,
            &git_path("worktrees").join("feat-login").join("HEAD")
        ));
    }

    #[test]
    fn should_ignore_read_open_under_worktrees() {
        // The same IN_OPEN spin guard must hold for worktree metadata.
        let open = EventKind::Access(AccessKind::Open(AccessMode::Any));
        assert!(!is_relevant_change(
            &open,
            &git_path("worktrees").join("feat-login").join("HEAD")
        ));
    }

    #[test]
    fn should_ignore_changes_to_other_git_files() {
        // FETCH_HEAD (written by every fetch) and config must not trigger a
        // refresh even when genuinely modified.
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(!is_relevant_change(&modify, &git_path("FETCH_HEAD")));
        assert!(!is_relevant_change(&modify, &git_path("config")));
    }

    #[test]
    fn should_resync_arrivals_reported_as_parent_directory_modifies() {
        let missing_parent = PathBuf::from("/repos");
        let watches = Watches {
            git: Vec::new(),
            arrivals: vec![missing_parent.clone()],
        };
        let modify = EventKind::Modify(ModifyKind::Name(RenameMode::Any));

        assert!(watches.expects_arrival(&modify, &missing_parent));
        assert!(watches.expects_arrival(&modify, &missing_parent.join("late-arrival")));
    }

    #[test]
    fn should_watch_the_parent_before_the_repo_directory_arrives() {
        let root = tempfile::TempDir::new().unwrap();
        let repo = root.path().join("late-arrival");

        assert_eq!(arrival_watch_dir(&repo).as_deref(), Some(root.path()));
    }

    #[test]
    fn should_watch_the_repo_directory_until_git_metadata_exists() {
        let root = tempfile::TempDir::new().unwrap();
        let repo = root.path().join("late-arrival");
        std::fs::create_dir(&repo).unwrap();

        assert_eq!(arrival_watch_dir(&repo).as_deref(), Some(repo.as_path()));
        assert_eq!(arrival_watch_mode(&repo), RecursiveMode::NonRecursive);
    }

    #[test]
    fn should_watch_git_metadata_recursively_until_git_accepts_the_repo() {
        let root = tempfile::TempDir::new().unwrap();
        let repo = root.path().join("late-arrival");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();

        assert_eq!(arrival_watch_dir(&repo).as_deref(), Some(git.as_path()));
        assert_eq!(arrival_watch_mode(&git), RecursiveMode::Recursive);
    }

    #[test]
    fn should_report_the_removed_worktree_id() {
        let remove = EventKind::Remove(RemoveKind::Folder);

        assert_eq!(
            removed_worktree_id(&remove, &git_path("worktrees").join("feat-login")),
            Some("feat-login")
        );
        assert_eq!(
            removed_worktree_id(&remove, &git_path("worktrees").join("feat-login/HEAD")),
            None
        );
    }
}
