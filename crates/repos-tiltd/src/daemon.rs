//! The daemon's core loop, run by the Tiltfile as a long-lived serve_cmd. It
//! maintains the live Tilt buttons — per-repo branch/pull plus the nav
//! checkout-all — handles their clicks in-process, and periodically fetches
//! remotes so ahead/behind counts stay current. It's the *only* thing that
//! fetches: the `git-status` resource is a self-refreshing `repos status
//! --watch` pane that only re-reads local state, so the two never race each
//! other's fetches.
//!
//! What a press does lives in [`crate::actions`], and what the filesystem says
//! in [`crate::watches`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::MissedTickBehavior;

use repos_core::devenv::{Config, Project, Workspace, unreachable_profiles};
use repos_core::registry::Registry;
use repos_core::selfupdate::DevEnv;
use repos_core::tilt::{self as client, Click};

use crate::actions::{
    active_repos, checkout, checkout_all, pull, render_global, revert_removed_worktree,
    select_worktree, switch_profile,
};
use crate::buttons::{self, Action};
use crate::debounce::Debouncer;
use crate::updater;
use crate::watches::{WatchEffects, Watches};

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

pub fn run(poll: Duration, self_update: bool) -> Result<()> {
    let spec = std::env::var("REPOS_TILT_SPEC")
        .map_err(|_| anyhow!("REPOS_TILT_SPEC is not set (the Tiltfile passes it)"))?;
    let specs: Vec<WatchSpec> = serde_json::from_str(&spec).context("parsing REPOS_TILT_SPEC")?;
    // The spec carries per-resource data only; anything registry-wide (which
    // branches mirror their remote, the profiles) comes from here.
    let reg = Registry::load()
        .inspect_err(|e| tracing::warn!(error = %format!("{e:#}"), "couldn't read tilt-devenv.json — the worktree and profile buttons won't work"))
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
    let mut watches = Watches::new();
    watches.sync(&mut watcher, &ws, true);
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
    // Checkout-all intersects the picked group with the selection, so offering a
    // group outside it would check out nothing — and the Tiltfile may well
    // declare a resource per registry repo, whatever the selection.
    let groups = ws.filter(&active_repos(reg.as_ref()), &[]).groups();

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
        reg,
        dev_env: dev_env.clone(),
    };

    // Button clicks. If watching fails, keep serving status: the sender is held
    // alive so its receiver simply never fires.
    let (mut click_rx, _click_watcher, _keep_alive) = match client::watch_clicks() {
        Ok((rx, w)) => (rx, Some(w), None),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "buttons won't respond — couldn't watch for clicks; the status table still updates");
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

    match client::apiserver_port() {
        Some(port) => tracing::info!(port, "talking to Tilt's apiserver"),
        // Every `tilt` then resolves through TILT_PORT, which Tilt doesn't set
        // and a shell may have left pointing at an apiserver that is gone.
        None => tracing::warn!(
            "couldn't tell which Tilt started this daemon; its buttons may reach the wrong one"
        ),
    }
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
                    apply_watch_effects(
                        watches.handle_event(&mut watcher, &ws, &ev.kind, path),
                        &debouncer,
                        ctx.root.as_deref(),
                    );
                }
            }

            Some(click) = click_rx.recv() => handle_click(&ws, click, &ctx),

            _ = ticker.tick() => {
                // Backstop only: arrivals are events. Kept for what inotify can
                // miss — a network filesystem, a bind mount, a watch limit.
                apply_watch_effects(
                    WatchEffects::refresh(watches.sync(&mut watcher, &ws, false)),
                    &debouncer,
                    ctx.root.as_deref(),
                );
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
        for (repo, e) in ws.retire_all() {
            tracing::warn!(repo, error = %format!("{e:#}"), "couldn't remove a repo's buttons on shutdown");
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
    /// For resolving the active profiles' repo names (checkout-all's scope) and
    /// for checking access to a newly-picked profile's repos before saving
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
            let names = active_repos(ctx.reg.as_ref());
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
                let ws = ws.clone();
                tokio::task::spawn_blocking(move || {
                    switch_profile(&ws, &state, &root, reg.as_ref(), &checked)
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

fn apply_watch_effects(
    effects: WatchEffects,
    debouncer: &Debouncer,
    root: Option<&std::path::Path>,
) {
    for p in effects.refresh {
        schedule_refresh(debouncer, p);
    }
    let Some(root) = root else {
        return;
    };
    for id in effects.removed_worktree_ids {
        let root = root.to_path_buf();
        debouncer.schedule(format!("__wt_revert_{id}"), move || {
            revert_removed_worktree(&root, &id);
        });
    }
}

fn schedule_refresh(debouncer: &Debouncer, p: Arc<Project>) {
    debouncer.schedule(p.resource().to_string(), move || {
        p.refresh();
    });
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
