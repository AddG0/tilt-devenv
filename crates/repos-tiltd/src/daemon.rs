//! The daemon's core loop, run by the Tiltfile as a long-lived serve_cmd. It
//! maintains the live Tilt buttons — per-repo branch/pull plus the nav
//! checkout-all — handles their clicks in-process, and periodically fetches
//! remotes so ahead/behind counts stay current. It's the *only* thing that
//! fetches: the `git-status` resource is a self-refreshing `repos status
//! --watch` pane that only re-reads local state, so the two never race each
//! other's fetches.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::MissedTickBehavior;

use repos_core::devenv::{
    CheckoutTarget, Config, Outcome, Project, Workspace, count_with_outcome, unreachable_profiles,
};
use repos_core::registry::Registry;
use repos_core::selfupdate::DevEnv;
use repos_core::tilt::{self as client, Click};
use repos_core::{git, worktree};

use crate::buttons::{self, Action};
use crate::debounce::Debouncer;
use crate::updater;

/// One entry of `REPOS_TILT_SPEC`: the Tilt resource, the repo it shows, that
/// repo's on-disk path, and its registry group (checkout-all's group filter).
#[derive(Deserialize)]
struct WatchSpec {
    resource: String,
    repo: String,
    path: PathBuf,
    #[serde(default)]
    group: String,
}

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

pub fn run(poll: Duration, self_update: bool) -> Result<()> {
    let spec = std::env::var("REPOS_TILT_SPEC")
        .map_err(|_| anyhow!("REPOS_TILT_SPEC is not set (the Tiltfile passes it)"))?;
    let specs: Vec<WatchSpec> = serde_json::from_str(&spec).context("parsing REPOS_TILT_SPEC")?;
    // The spec carries per-resource data only; anything registry-wide (which
    // branches mirror their remote, the profiles) comes from here.
    let reg = Registry::load()
        .inspect_err(|e| tracing::warn!(error = %e, "couldn't read tilt-devenv.json — the worktree and profile buttons won't work"))
        .ok();
    let mirror_branches = reg
        .as_ref()
        .map(|r| r.mirror_branches.clone())
        .unwrap_or_default();
    let cfgs = specs
        .into_iter()
        .map(|s| Config {
            name: s.repo,
            group: s.group,
            resource: s.resource,
            path: s.path,
            // The Tiltfile clones before the daemon starts; it never clones.
            url: String::new(),
            mirror_branches: mirror_branches.clone(),
        })
        .collect();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_daemon(cfgs, reg, poll, self_update))
}

async fn run_daemon(
    cfgs: Vec<Config>,
    reg: Option<Registry>,
    poll: Duration,
    self_update: bool,
) -> Result<()> {
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
    let root = reg.as_ref().map(|r| r.root.clone());
    let profile_names: Vec<String> = reg
        .as_ref()
        .map(|r| r.profiles.keys().cloned().collect())
        .unwrap_or_default();
    // The developer's persisted selection (XDG state, survives a `tilt up`
    // restart) — the profile button's checkbox defaults, and (via
    // `profile_state`) where a click saves a new selection.
    let profile_state = repos_core::profile::state_path();
    let active_profiles = reg
        .as_ref()
        .map(|r| r.active_profile_names())
        .unwrap_or_default();
    let active_profile_repos: Vec<String> = reg
        .as_ref()
        .map(|r| r.active_profiles())
        .unwrap_or_default();

    let mut groups: Vec<String> = ws
        .projects()
        .iter()
        .map(|p| p.group().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    groups.sort();
    groups.dedup();

    // One live access check up front — an ls-remote per uncloned repo — so the
    // picker can leave profiles it can't clone out entirely.
    let unreachable = reg.as_ref().map(unreachable_profiles).unwrap_or_default();
    let selectable = reg
        .as_ref()
        .map(|r| r.selectable_profiles(&unreachable))
        .unwrap_or_default();
    render_global(&groups, &selectable, &active_profiles, profile_names.len());

    // Nothing else fetches the dev-env repo, so it rides the same tick as the
    // service remotes.
    let dev_env = self_update
        .then(|| root.as_deref().and_then(DevEnv::at))
        .flatten()
        .map(Arc::new);
    if let Some(dev) = dev_env.clone() {
        tracing::info!(root = %dev.root().display(), "watching the dev environment for updates");
        // Local refs first, so a dev-env already known to be behind shows its
        // button now rather than a poll interval later.
        tokio::task::block_in_place(|| updater::refresh_button(&dev, false));
        tokio::task::spawn_blocking(move || updater::refresh_button(&dev, true));
    }

    let ctx = ClickContext {
        root,
        profile_state,
        active_profile_repos,
        reg,
        dev_env: dev_env.clone(),
    };

    // Button clicks. If watching fails, keep serving status: the sender is held
    // alive so its receiver simply never fires.
    let (mut click_rx, _click_watcher, _keep_alive) = match client::watch_clicks() {
        Ok((rx, w)) => (rx, Some(w), None),
        Err(e) => {
            tracing::warn!(error = %e, "buttons won't respond — couldn't watch for clicks; the status table still updates");
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
    // A guard of its own, so a slow repo poll can't delay the dev-env check;
    // the two share only the tick.
    let polling_dev_env = Arc::new(AtomicBool::new(false));

    tracing::info!(
        repos = ws.projects().len(),
        groups = ?groups,
        profiles = ?profile_names,
        active = ?active_profiles,
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
                        && let Some(root) = ctx.root.clone()
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
                    }
                }
            }

            Some(click) = click_rx.recv() => handle_click(&ws, click, &ctx),

            _ = ticker.tick() => {
                poll_remotes(ws.clone(), polling.clone());
                updater::poll(dev_env.clone(), polling_dev_env.clone());
            }
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
        let _ = buttons::remove_global();
    })
    .await;
    Ok(())
}

/// Context a button click may need beyond the click itself and the workspace.
struct ClickContext {
    root: Option<PathBuf>,
    profile_state: Option<PathBuf>,
    /// The active profiles' repo names, for checkout-all's "active profile
    /// only" checkbox.
    active_profile_repos: Vec<String>,
    /// For checking access to a newly-picked profile's repos before saving
    /// it (best-effort — skipped, so the switch is never blocked, when the
    /// registry couldn't be loaded).
    reg: Option<Registry>,
    /// The dev-env repo the update button acts on; `None` when self-update is
    /// off or the dev-env isn't checked into git.
    dev_env: Option<Arc<DevEnv>>,
}

/// Dispatches a button press to the matching in-process operation, off the
/// event loop (on the blocking pool) so a slow git op doesn't stall watching.
fn handle_click(ws: &Arc<Workspace>, c: Click, ctx: &ClickContext) {
    let Some(action) = buttons::action(&c) else {
        tracing::debug!(button = %c.button, "click on a button we don't own");
        return;
    };
    tracing::debug!(button = %c.button, ?action, "click");

    match action {
        Action::CheckoutAll { branch, group } => {
            let group: Vec<String> = group.into_iter().collect();
            let names = ctx.active_profile_repos.clone();
            // Profiles defined but none picked means nothing is in scope — the
            // same rule the CLI applies, rather than quietly acting on the
            // whole registry.
            if ctx
                .reg
                .as_ref()
                .is_some_and(|r| r.is_unscoped(&names, &group, false))
            {
                tracing::warn!("no active profile selected; nothing to check out");
                return;
            }
            let ws = ws.clone();
            tokio::task::spawn_blocking(move || checkout_all(&ws, &branch, &names, &group));
        }

        Action::UpdateDevEnv => match ctx.dev_env.clone() {
            Some(dev) => {
                tokio::task::spawn_blocking(move || updater::apply(&dev));
            }
            None => tracing::error!("no dev environment to update"),
        },

        Action::SetProfiles(checked) => match (ctx.root.clone(), ctx.profile_state.clone()) {
            (Some(root), Some(state)) => {
                let reg = ctx.reg.clone();
                tokio::task::spawn_blocking(move || {
                    switch_profile(&state, &root, reg.as_ref(), &checked)
                });
            }
            _ => tracing::error!(
                "couldn't find tilt-devenv.json or a state directory — selection not saved"
            ),
        },

        Action::Checkout { resource, branch } => {
            if let Some(p) = ws.by_resource(&resource) {
                let p = p.clone();
                tokio::task::spawn_blocking(move || checkout(&p, &branch));
            }
        }

        Action::Pull { resource } => {
            if let Some(p) = ws.by_resource(&resource) {
                let p = p.clone();
                tokio::task::spawn_blocking(move || pull(&p));
            }
        }

        Action::SelectWorktree { resource, branch } => {
            let Some(p) = ws.by_resource(&resource) else {
                return;
            };
            let Some(root) = ctx.root.clone() else {
                tracing::error!("couldn't find tilt-devenv.json — nothing switched");
                return;
            };
            let p = p.clone();
            tokio::task::spawn_blocking(move || select_worktree(&p, &branch, &root));
        }
    }
}

fn checkout(p: &Project, branch: &str) {
    let target = match CheckoutTarget::parse(branch) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(repo = p.name(), %e, "nothing checked out");
            return;
        }
    };
    let r = p.checkout(&target);
    match r.outcome {
        Outcome::FellBack => tracing::info!(
            repo = p.name(),
            %branch,
            "no such branch here — switched to this repo's default instead"
        ),
        Outcome::SkippedDirty => tracing::warn!(
            repo = p.name(),
            "uncommitted changes — left on its current branch"
        ),
        _ => tracing::info!(repo = p.name(), %branch, outcome = r.outcome.label(), "checkout"),
    }
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
fn select_worktree(p: &Project, branch: &str, root: &Path) {
    if branch.is_empty() {
        return;
    }
    let worktrees = git::worktrees(p.path());
    let Some(wt) = worktrees.iter().find(|w| w.branch == branch) else {
        tracing::warn!(repo = p.name(), %branch, "that worktree is gone — nothing switched");
        return;
    };
    let Some(state) = worktree::state_path() else {
        tracing::error!("no state directory available — can't remember which worktree you picked");
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
    match worktree::set_selection(&state, root, p.name(), selection.as_deref()) {
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

/// Draws the nav checkout-all button (offering `groups` in its dropdown), plus
/// the profile-switcher button when `profile_names` is non-empty — its
/// checkboxes default to `active_profiles`, the persisted selection.
fn render_global(
    groups: &[String],
    selectable: &[String],
    active_profiles: &[String],
    defined: usize,
) {
    if let Err(e) = buttons::render_checkout_all(groups) {
        tracing::error!(error = %e, "failed to render checkout-all button");
    }
    render_profile_picker(selectable, active_profiles, defined);
}

/// Shows the profile picker, or hides it when there's nothing to pick — see
/// [`buttons::selectable_profiles`].
fn render_profile_picker(selectable: &[String], active: &[String], defined: usize) {
    match buttons::render_profile_button(selectable, active) {
        Ok(true) => {}
        Ok(false) if defined == 0 => {}
        Ok(false) => tracing::warn!(
            defined,
            "no profile picker: every profile reaches a repo this machine can't clone"
        ),
        Err(e) => tracing::error!(error = %e, "failed to render profile button"),
    }
}

/// Persists `checked` as the active profile selection (empty enables every
/// profile). The Tiltfile watches this file, so saving it triggers the reload
/// that redefines resources for the new selection — no `tilt args` involved, so
/// it survives a `tilt up` restart.
///
/// Refuses to save (best-effort, via `reg`) when `checked` includes a profile
/// in [`no_access_profiles`] — better to fail the switch than persist a
/// selection the next Tiltfile reload can't actually clone.
fn switch_profile(state: &Path, root: &Path, reg: Option<&Registry>, checked: &[String]) {
    if let Some(reg) = reg {
        let unreachable = unreachable_profiles(reg);
        let culprits: Vec<&str> = checked
            .iter()
            .filter(|p| unreachable.contains(p))
            .map(String::as_str)
            .collect();
        if !culprits.is_empty() {
            tracing::error!(
                profiles = ?culprits,
                "profile selection not saved — no access to a repo it needs"
            );
            return;
        }
    }
    match repos_core::profile::set_active(state, root, checked) {
        Ok(()) => {
            if checked.is_empty() {
                tracing::info!("profile selection cleared — nothing runs until you pick a profile");
            } else {
                tracing::info!(profiles = ?checked, "profile selection saved");
            }
            if let Some(reg) = reg {
                let unreachable = unreachable_profiles(reg);
                let selectable = reg.selectable_profiles(&unreachable);
                render_profile_picker(&selectable, checked, reg.profiles.len());
            }
        }
        Err(e) => tracing::error!(error = %e, "saving profile selection failed"),
    }
}

/// Switches every repo matching `names`/`groups` (either empty matches
/// everything — the default) to `branch` (repos without it fall back to their
/// default branch), reporting a tally.
fn checkout_all(ws: &Workspace, branch: &str, names: &[String], groups: &[String]) {
    let target = match CheckoutTarget::parse(branch) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%e, "nothing checked out");
            return;
        }
    };
    let results = ws.filter(names, groups).checkout_all(&target);
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

/// Fetches + refreshes every project so ahead/behind stays current. Refreshing
/// re-renders each project's own branch/pull buttons in place via its
/// `Presenter` — it does *not* trigger the `git-status` pane, so a poll never
/// interrupts what the developer is looking at there. Guarded so a slow poll
/// can't overlap the next tick.
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

    fn broken_repo_registry(root: &std::path::Path) -> Registry {
        std::fs::write(
            root.join("tilt-devenv.json"),
            r#"{"repos":[{"name":"broken","url":"/no/such/remote","group":"g"}],
                "profiles":{"bad":["broken"]}}"#,
        )
        .unwrap();
        Registry::load_from(root).unwrap()
    }

    #[test]
    fn switch_profile_saves_when_a_repo_merely_failed_to_answer() {
        // An unreachable remote isn't a refusal — treating it as one leaves a
        // developer unable to pick any profile offline.
        let root = tempfile::TempDir::new().unwrap();
        let reg = broken_repo_registry(root.path());
        let state = root.path().join("profiles.json");

        switch_profile(&state, root.path(), Some(&reg), &["bad".to_string()]);

        assert_eq!(
            repos_core::profile::active(&state, root.path()),
            vec!["bad".to_string()],
            "a remote that never answered must not block the selection"
        );
    }

    #[test]
    fn switch_profile_can_clear_the_selection_despite_an_unreachable_repo_elsewhere() {
        let root = tempfile::TempDir::new().unwrap();
        let reg = broken_repo_registry(root.path());
        let state = root.path().join("profiles.json");

        // Seed a real selection directly, bypassing the access check, so
        // there's something to clear.
        repos_core::profile::set_active(&state, root.path(), &["bad".to_string()]).unwrap();

        // Regression: clearing the selection must never be blocked by some
        // other, unrelated repo being unreachable — or nothing could ever
        // un-stick a selection once any repo in the registry broke.
        switch_profile(&state, root.path(), Some(&reg), &[]);
        assert!(
            repos_core::profile::active(&state, root.path()).is_empty(),
            "clearing the selection must always be possible"
        );
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
}
