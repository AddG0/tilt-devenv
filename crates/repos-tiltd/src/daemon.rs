//! The daemon's core loop, run by the Tiltfile as a long-lived serve_cmd. It
//! maintains the live Tilt buttons — per-repo branch/pull plus the nav
//! checkout-all — handles their clicks in-process, and triggers the `git-status`
//! resource to reprint its status table whenever a repo's git state changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::MissedTickBehavior;

use repos_core::devenv::{CheckoutTarget, Config, Outcome, Project, Workspace, count_with_outcome};
use repos_core::tilt::{self as client, Click};
use repos_core::{git, registry, worktree};

use crate::buttons;
use crate::debounce::Debouncer;

/// The Tiltfile resource that runs `repos status`; the daemon triggers it to
/// refresh the consolidated status table. Its `.git`-watching can't live in Tilt
/// (Tilt ignores `.git`), so the daemon detects changes and triggers it here.
const STATUS_RESOURCE: &str = "git-status";

/// One entry of `REPOS_TILT_SPEC`: the Tilt resource, the repo it shows, and
/// that repo's on-disk path.
#[derive(Deserialize)]
struct WatchSpec {
    resource: String,
    repo: String,
    path: PathBuf,
}

/// Whether a `.git` filesystem event warrants a refresh: a real change to HEAD
/// or index (branch switch / commit / stage) or to worktree metadata under
/// `worktrees/` (a worktree added or removed).
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
    // Inside a worktree's own metadata dir, only its addition/removal matters —
    // not churn to its files (its `index`, which `git status` rewrites, would
    // otherwise spin the watch).
    if is_under_worktrees(path) {
        return is_worktree_entry(path);
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "HEAD" || n == "index")
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

pub fn run(poll: Duration) -> Result<()> {
    let spec = std::env::var("REPOS_TILT_SPEC")
        .map_err(|_| anyhow!("REPOS_TILT_SPEC is not set (the Tiltfile passes it)"))?;
    let specs: Vec<WatchSpec> = serde_json::from_str(&spec).context("parsing REPOS_TILT_SPEC")?;
    let cfgs = specs
        .into_iter()
        .map(|s| Config {
            name: s.repo,
            group: String::new(),
            resource: s.resource,
            path: s.path,
        })
        .collect();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_daemon(cfgs, poll))
}

async fn run_daemon(cfgs: Vec<Config>, poll: Duration) -> Result<()> {
    let ws = Arc::new(Workspace::with_presenter(cfgs, |c| {
        Box::new(buttons::Presenter::new(&c.resource, c.path.clone()))
    }));

    // notify's callback runs on its own thread, so bridge its events into the
    // async select loop through a channel.
    let (fs_tx, mut fs_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = fs_tx.send(ev);
        }
    })?;

    // Draw every repo's initial buttons concurrently (each refresh shells out to
    // git + tilt), then set up the .git watches. A missing/uncloned repo can't be
    // watched; its button still shows "(missing)" from the refresh.
    tokio::task::block_in_place(|| ws.status_all(false));
    // (watched dir, project). We watch each repo's common `.git` (HEAD/index for
    // a main checkout, plus the appearance of `worktrees/`) and, when present,
    // its `worktrees/` recursively (each linked worktree's HEAD + add/remove).
    // A fs event maps to a project when a watched dir is an ancestor of it, so
    // several resources sharing one repo all refresh.
    let mut watched: Vec<(PathBuf, Arc<Project>)> = Vec::new();
    for p in ws.projects() {
        let Some(common) = git::common_dir(p.path()) else {
            tracing::warn!(resource = p.resource(), "not watching repo (no git dir)");
            continue;
        };
        if let Err(e) = watcher.watch(&common, RecursiveMode::NonRecursive) {
            tracing::warn!(resource = p.resource(), error = %e, "not watching repo");
            continue;
        }
        let worktrees = common.join("worktrees");
        if worktrees.is_dir() {
            let _ = watcher.watch(&worktrees, RecursiveMode::Recursive);
        }
        watched.push((common, p.clone()));
    }
    render_global();
    trigger_status();

    // Button clicks. If watching fails, keep serving status: the sender is held
    // alive so its receiver simply never fires.
    let (mut click_rx, _click_watcher, _keep_alive) = match client::watch_clicks() {
        Ok((rx, w)) => (rx, Some(w), None),
        Err(e) => {
            tracing::warn!(error = %e, "button clicks disabled; still serving status");
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Click>();
            (rx, None, Some(tx))
        }
    };

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await; // consume the immediate first tick (poll after `poll`, not now)

    let debouncer = Debouncer::new(Duration::from_millis(300));
    let polling = Arc::new(AtomicBool::new(false));
    // Dev-env root, for reverting a repo to its main checkout when its selected
    // worktree is removed (best-effort — no reconciliation if it can't be found).
    let root = registry::find_root().ok();

    tracing::info!(
        repos = ws.projects().len(),
        ?poll,
        "watching for branch changes"
    );

    loop {
        tokio::select! {
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,

            Some(ev) = fs_rx.recv() => {
                for path in &ev.paths {
                    if !is_relevant_change(&ev.kind, path) {
                        continue;
                    }
                    // A repo's `worktrees/` just appeared (its first worktree) —
                    // watch it so later add/removes fire too. Its events still map
                    // via the common-dir entry already in `watched`.
                    if matches!(&ev.kind, EventKind::Create(_))
                        && path.file_name().and_then(|n| n.to_str()) == Some("worktrees")
                    {
                        let _ = watcher.watch(path, RecursiveMode::Recursive);
                    }
                    // A worktree's metadata dir (`worktrees/<id>`) was removed —
                    // clear any selection on that id so the resource reverts to
                    // its main checkout (the write trips the Tiltfile reload).
                    if matches!(&ev.kind, EventKind::Remove(_))
                        && path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "worktrees")
                        && let Some(id) = path.file_name().and_then(|n| n.to_str())
                        && let Some(root) = root.clone()
                    {
                        let id = id.to_string();
                        debouncer.schedule(format!("__wt_revert_{id}"), move || {
                            revert_removed_worktree(&root, &id);
                        });
                    }
                    for (dir, p) in &watched {
                        if !path.starts_with(dir) {
                            continue;
                        }
                        tracing::debug!(resource = p.resource(), "change detected; scheduling refresh");
                        let p = p.clone();
                        debouncer.schedule(p.resource().to_string(), move || {
                            p.refresh();
                        });
                        // Coalesce any repos changing at once into one table refresh.
                        debouncer.schedule(STATUS_RESOURCE.to_string(), trigger_status);
                    }
                }
            }

            Some(click) = click_rx.recv() => handle_click(&ws, click),

            _ = ticker.tick() => poll_remotes(ws.clone(), polling.clone()),
        }
    }

    // Stop everything that could re-render, then retire (which makes each
    // project render-inert) before returning. Dropping `_click_watcher` kills
    // the click stream.
    debouncer.stop();
    let ws = ws.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for p in ws.projects() {
            let _ = p.retire();
        }
        let _ = buttons::remove_checkout_all();
    })
    .await;
    Ok(())
}

/// Dispatches a button press to the matching project's in-process operation, off
/// the event loop (on the blocking pool) so a slow git op doesn't stall watching.
fn handle_click(ws: &Arc<Workspace>, c: Click) {
    tracing::debug!(button = %c.button, "click");

    let branch = c.inputs.get("branch").cloned().unwrap_or_default();
    if buttons::is_checkout_all_click(&c.button) {
        let ws = ws.clone();
        tokio::task::spawn_blocking(move || checkout_all(&ws, &branch));
        return;
    }
    if let Some(res) = buttons::branch_click_resource(&c.button) {
        if let Some(p) = ws.by_resource(res) {
            let p = p.clone();
            tokio::task::spawn_blocking(move || checkout(&p, &branch));
        }
        return;
    }
    if let Some(res) = buttons::pull_click_resource(&c.button)
        && let Some(p) = ws.by_resource(res)
    {
        let p = p.clone();
        tokio::task::spawn_blocking(move || pull(&p));
        return;
    }
    if let Some(res) = buttons::worktree_click_resource(&c.button)
        && let Some(p) = ws.by_resource(res)
    {
        let p = p.clone();
        let selected = c.inputs.get("worktree").cloned().unwrap_or_default();
        tokio::task::spawn_blocking(move || select_worktree(&p, &selected));
    }
}

fn checkout(p: &Project, branch: &str) {
    let target = match CheckoutTarget::parse(branch) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(repo = p.name(), %e, "ignoring checkout click");
            return;
        }
    };
    let r = p.checkout(&target);
    tracing::info!(repo = p.name(), %branch, outcome = r.outcome.label(), "checkout");
    if let Some(e) = r.err {
        tracing::error!(repo = p.name(), error = %e, "checkout failed");
    }
}

fn pull(p: &Project) {
    let r = p.pull();
    tracing::info!(repo = p.name(), outcome = r.outcome.label(), "pull");
    if let Some(e) = r.err {
        tracing::error!(repo = p.name(), error = %e, "pull failed");
    }
}

/// Records the picked worktree as the active selection for the repo. `branch` is
/// the dropdown value; selecting the main checkout clears the selection (back to
/// the repo's default path). The Tiltfile watches the selection file, so writing
/// it triggers the reload that restarts the resource at the new path.
fn select_worktree(p: &Project, branch: &str) {
    if branch.is_empty() {
        return;
    }
    let worktrees = git::worktrees(p.path());
    let Some(wt) = worktrees.iter().find(|w| w.branch == branch) else {
        tracing::warn!(repo = p.name(), %branch, "no worktree with that branch; ignoring");
        return;
    };
    let root = match registry::find_root() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "can't locate dev-env root to record worktree");
            return;
        }
    };
    let Some(state) = worktree::state_path() else {
        tracing::error!("no XDG state dir; can't record worktree selection");
        return;
    };
    // Store the worktree's git id (stable across move/branch-switch), or clear
    // for the main checkout. The registry resolves the id back to a path.
    let selection = if wt.is_main {
        None
    } else {
        match git::worktree_id(&wt.path) {
            Some(id) => Some(id),
            None => {
                tracing::warn!(repo = p.name(), %branch, "could not determine worktree id; ignoring");
                return;
            }
        }
    };
    match worktree::set_selection(&state, &root, p.name(), selection.as_deref()) {
        Ok(()) => tracing::info!(repo = p.name(), %branch, main = wt.is_main, "worktree selected"),
        Err(e) => tracing::error!(repo = p.name(), error = %e, "recording worktree failed"),
    }
}

/// A worktree with git id `id` was removed — clear any selection on it so the
/// repo reverts to its main checkout. Writing the selection file triggers the
/// Tiltfile reload that restarts the affected resource.
fn revert_removed_worktree(root: &Path, id: &str) {
    let Some(state) = worktree::state_path() else {
        return;
    };
    for (repo, selected) in worktree::selections(&state, root) {
        if selected == id {
            match worktree::set_selection(&state, root, &repo, None) {
                Ok(()) => {
                    tracing::info!(repo, worktree_id = id, "worktree removed; reverted to main")
                }
                Err(e) => tracing::error!(repo, error = %e, "reverting removed worktree failed"),
            }
        }
    }
}

/// Draws the nav checkout-all button (a static text box).
fn render_global() {
    if let Err(e) = buttons::render_checkout_all() {
        tracing::error!(error = %e, "failed to render checkout-all button");
    }
}

/// (Re)runs the `git-status` resource so its log pane reflects current state.
fn trigger_status() {
    if let Err(e) = client::trigger(STATUS_RESOURCE) {
        tracing::warn!(error = %e, "failed to trigger git-status resource");
    }
}

/// Switches every repo to `branch` (repos without it fall back to their default
/// branch), reporting a tally.
fn checkout_all(ws: &Workspace, branch: &str) {
    let target = match CheckoutTarget::parse(branch) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%e, "ignoring checkout-all click");
            return;
        }
    };
    let results = ws.checkout_all(&target);
    for r in &results {
        if let Some(e) = &r.err {
            tracing::error!(repo = %r.name, error = %e, "checkout failed");
        }
    }
    let n = |o| count_with_outcome(&results, o);
    tracing::info!(
        %branch,
        on_branch = n(Outcome::OnBranch),
        on_default = n(Outcome::FellBack),
        skipped = n(Outcome::SkippedDirty),
        errored = n(Outcome::Errored),
        "checkout-all",
    );
}

/// Fetches + refreshes every project so ahead/behind stays current, then
/// refreshes the status table. Guarded so a slow poll can't overlap the next
/// tick.
fn poll_remotes(ws: Arc<Workspace>, polling: Arc<AtomicBool>) {
    if polling
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    tracing::debug!("polling remotes");
    tokio::task::spawn_blocking(move || {
        ws.status_all(true);
        trigger_status();
        polling.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind};
    use std::path::PathBuf;

    fn git_path(name: &str) -> PathBuf {
        PathBuf::from("/repo/.git").join(name)
    }

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
        // git-status resource. Only worktree add/remove (dir-level) is relevant.
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(!is_relevant_change(
            &modify,
            &git_path("worktrees").join("feat-login").join("index")
        ));
        assert!(!is_relevant_change(
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
}
