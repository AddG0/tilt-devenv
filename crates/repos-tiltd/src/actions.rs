//! What a button press does. Each reports its outcome to the log, which is where
//! a developer sees it: Tilt shows a resource's log beside its buttons.

use std::path::Path;

use repos_core::devenv::unreachable_profiles;
use repos_core::devenv::{CheckoutTarget, Outcome, Project, Workspace, count_with_outcome};
use repos_core::registry::Registry;
use repos_core::worktree;

use crate::buttons;

pub(crate) fn checkout(p: &Project, branch: &str) {
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

pub(crate) fn pull(p: &Project) {
    let r = p.pull();
    tracing::info!(repo = p.name(), outcome = r.outcome.label(), "pull");
    if let Some(e) = r.err {
        tracing::error!(repo = p.name(), error = %e, "pull failed");
    }
}

/// Records the picked worktree, which trips the Tiltfile reload that restarts
/// the resource at the new path. An empty `branch` is Tilt re-sending the
/// dropdown's initial state, not a choice.
pub(crate) fn select_worktree(p: &Project, branch: &str, root: &Path) {
    if branch.is_empty() {
        return;
    }
    let Some(state) = worktree::state_path() else {
        tracing::error!("no state directory available — can't remember which worktree you picked");
        return;
    };
    match worktree::select(&state, root, p.name(), p.path(), branch) {
        Ok(sel) => tracing::info!(repo = p.name(), %branch, ?sel, "worktree selected"),
        Err(e) => {
            tracing::warn!(repo = p.name(), %branch, error = %format!("{e:#}"), "nothing switched")
        }
    }
}

/// A worktree with git id `id` was removed — clear any selection on it so the
/// repo reverts to its main checkout. Writing the selection file triggers the
/// Tiltfile reload that restarts the affected resource.
pub(crate) fn revert_removed_worktree(root: &Path, id: &str) {
    let Some(state) = worktree::state_path() else {
        return;
    };
    for (repo, selected) in worktree::selections(&state, root) {
        if selected == id {
            match worktree::set_selection(&state, root, &repo, None) {
                Ok(()) => {
                    tracing::info!(repo, worktree_id = id, "worktree removed; reverted to main")
                }
                Err(e) => {
                    tracing::error!(repo, error = %format!("{e:#}"), "reverting removed worktree failed")
                }
            }
        }
    }
}

/// The repo names the persisted profile selection reaches (empty restricts
/// nothing — see [`Registry::active_profiles`]). Never cached: a profile switch
/// restarts the daemon only when it changes the Tiltfile's resource list, which
/// a Tiltfile listing every registry repo never does.
pub(crate) fn active_repos(reg: Option<&Registry>) -> Vec<String> {
    reg.map(|r| r.active_profiles()).unwrap_or_default()
}

/// Draws the nav checkout-all button (offering `groups` in its dropdown), plus
/// the profile-switcher button when `profile_names` is non-empty — its
/// checkboxes default to `active_profiles`, the persisted selection.
pub(crate) fn render_global(
    groups: &[String],
    selectable: &[String],
    active_profiles: &[String],
    defined: usize,
) {
    if let Err(e) = buttons::render_checkout_all(groups) {
        tracing::error!(error = %format!("{e:#}"), "failed to render checkout-all button");
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
        Err(e) => tracing::error!(error = %format!("{e:#}"), "failed to render profile button"),
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
///
/// Redraws both nav buttons itself: the reload restarts the daemon only when the
/// resource list changed (see [`active_repos`]).
pub(crate) fn switch_profile(
    ws: &Workspace,
    state: &Path,
    root: &Path,
    reg: Option<&Registry>,
    checked: &[String],
) {
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
            // Expanded from `checked` rather than re-read through
            // `active_repos`, which would ignore the `state` file we were given.
            let enabled = reg
                .map(|r| r.resolve_only(&[], checked))
                .unwrap_or_default();
            if let Err(e) = buttons::render_checkout_all(&ws.filter(&enabled, &[]).groups()) {
                tracing::error!(error = %format!("{e:#}"), "failed to redraw checkout-all button");
            }
            if let Some(reg) = reg {
                let unreachable = unreachable_profiles(reg);
                let selectable = reg.selectable_profiles(&unreachable);
                render_profile_picker(&selectable, checked, reg.profiles.len());
            }
        }
        Err(e) => tracing::error!(error = %format!("{e:#}"), "saving profile selection failed"),
    }
}

/// Switches every repo matching `names`/`groups` (either empty matches
/// everything — the default) to `branch` (repos without it fall back to their
/// default branch), reporting a tally.
pub(crate) fn checkout_all(ws: &Workspace, branch: &str, names: &[String], groups: &[String]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use repos_core::devenv::Config;

    fn broken_repo_ws() -> Workspace {
        Workspace::plain(vec![Config {
            name: "broken".to_string(),
            group: "g".to_string(),
            resource: "broken".to_string(),
            ..Default::default()
        }])
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

        switch_profile(
            &broken_repo_ws(),
            &state,
            root.path(),
            Some(&reg),
            &["bad".to_string()],
        );

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
        switch_profile(&broken_repo_ws(), &state, root.path(), Some(&reg), &[]);
        assert!(
            repos_core::profile::active(&state, root.path()).is_empty(),
            "clearing the selection must always be possible"
        );
    }
}
