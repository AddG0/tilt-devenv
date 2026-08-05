//! The specific buttons the daemon shows in Tilt and what they mean: a branch
//! picker and a pull button per repo, plus a global checkout-all. Built on the
//! [`repos_core::tilt`] client seam, which does the actual apply/delete/watch.

use std::path::{Path, PathBuf};

use anyhow::Result;
use repos_core::devenv::{self, Snapshot};
use repos_core::git;
use repos_core::tilt::{self as client, UiButton};

const BRANCH_PREFIX: &str = "repos-branch-";
const PULL_PREFIX: &str = "repos-pull-";
const WORKTREE_PREFIX: &str = "repos-worktree-";
const CHECKOUT_ALL_BUTTON: &str = "repos-checkout-all";
const PROFILE_BUTTON: &str = "repos-profile";

fn branch_name(resource: &str) -> String {
    format!("{BRANCH_PREFIX}{resource}")
}
fn pull_name(resource: &str) -> String {
    format!("{PULL_PREFIX}{resource}")
}
fn worktree_name(resource: &str) -> String {
    format!("{WORKTREE_PREFIX}{resource}")
}

/// Renders one repo's branch + pull + worktree buttons. Satisfies
/// [`devenv::Presenter`]. Holds the repo's on-disk path so it can list the
/// repo's worktrees for the picker.
pub struct Presenter {
    resource: String,
    path: PathBuf,
}

impl Presenter {
    pub fn new(resource: &str, path: PathBuf) -> Presenter {
        Presenter {
            resource: resource.to_string(),
            path,
        }
    }

    /// A text box that checks out whatever branch you type.
    fn branch_button(&self, label: &str) -> UiButton {
        UiButton::new(branch_name(&self.resource), label.to_string())
            .icon("call_split")
            .at(&self.resource, "Resource")
            .text_input("branch", "checkout", "branch name")
    }

    /// A pull button, disabled when there's nothing to pull.
    fn pull_button(&self, behind: i32) -> UiButton {
        UiButton::new(pull_name(&self.resource), pull_caption(behind))
            .icon("cloud_download")
            .at(&self.resource, "Resource")
            .disabled(behind == 0)
    }

    /// A dropdown of the repo's worktree branches. Picking one records it as the
    /// active worktree (the daemon writes the selection; the Tiltfile reload
    /// restarts the resource at that path). The list includes the main checkout
    /// so you can switch back.
    fn worktree_button(&self, worktrees: &[git::Worktree], active_branch: &str) -> UiButton {
        let choices = worktrees
            .iter()
            .filter(|w| !w.branch.is_empty())
            .map(|w| w.branch.clone())
            .collect();
        UiButton::new(
            worktree_name(&self.resource),
            worktree_caption(active_branch, active_is_main(worktrees, &self.path)),
        )
        .icon("account_tree")
        .at(&self.resource, "Resource")
        .choice_input("worktree", "switch worktree", choices)
    }
}

/// Whether `active_path` is the repo's primary working tree — the "main git
/// thing" — as opposed to a linked worktree. Keyed on which *tree* is active,
/// not on its branch: the primary is flagged `(main)` whatever branch it sits
/// on, and a linked worktree never is (branch strings can coincide, e.g. when
/// both are detached).
fn active_is_main(worktrees: &[git::Worktree], active_path: &Path) -> bool {
    worktrees
        .iter()
        .any(|w| w.is_main && same_path(&w.path, active_path))
}

/// Path equality that sees through symlinks and `..` when both paths exist
/// (git reports canonical worktree paths; the resolved resource path may not
/// be), falling back to a literal compare when either can't be canonicalized.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl devenv::Presenter for Presenter {
    fn render(&self, s: &Snapshot) -> Result<()> {
        tracing::debug!(resource = %self.resource, branch = %s.branch, behind = s.behind, "rendering buttons");
        client::apply(&self.branch_button(&caption(s)))?;
        client::apply(&self.pull_button(s.behind))?;

        // Only show the picker once there's a worktree to switch to (a linked
        // worktree beyond the main checkout).
        let worktrees = git::worktrees(&self.path);
        if worktrees.iter().any(|w| !w.is_main && !w.branch.is_empty()) {
            client::apply(&self.worktree_button(&worktrees, &s.branch))
        } else {
            client::delete_button(&worktree_name(&self.resource))
        }
    }

    fn remove(&self) -> Result<()> {
        client::delete_button(&branch_name(&self.resource))?;
        client::delete_button(&pull_name(&self.resource))?;
        client::delete_button(&worktree_name(&self.resource))
    }
}

/// The worktree button's label: the branch checked out at the resource's active
/// worktree (its dir is named `repo--branch`, so the branch alone reads cleaner).
fn worktree_caption(branch: &str, on_main: bool) -> String {
    let branch = if branch.is_empty() {
        "(detached)"
    } else {
        branch
    };
    let suffix = if on_main { " (main)" } else { "" };
    format!("🌳 {branch}{suffix}")
}

/// The branch button's label: branch, ahead/behind, and a dirty dot.
fn caption(s: &Snapshot) -> String {
    if !s.present {
        return "⎇ (missing)".to_string();
    }
    if s.detached {
        return "⎇ (detached)".to_string();
    }
    let mut c = format!("⎇ {}", s.branch);
    if s.ahead > 0 {
        c += &format!(" ↑{}", s.ahead);
    }
    if s.behind > 0 {
        c += &format!(" ↓{}", s.behind);
    }
    if s.dirty {
        c += " ●";
    }
    c
}

fn pull_caption(behind: i32) -> String {
    if behind > 0 {
        format!("⬇ pull ↓{behind}")
    } else {
        "⬇ up to date".to_string()
    }
}

/// The global checkout-all button: type a branch and it's checked out across
/// every repo (repos without it fall back to their default branch). A text box,
/// not a dropdown, since the union of branches across all repos is far too long
/// to pick from.
fn checkout_all_button() -> UiButton {
    UiButton::new(
        CHECKOUT_ALL_BUTTON.to_string(),
        "⎇ checkout all repos".to_string(),
    )
    .icon("call_split")
    .at("nav", "Global")
    .text_input(
        "branch",
        "branch for all repos (missing → default)",
        "branch name",
    )
}

/// (Re)applies the nav checkout-all button.
pub fn render_checkout_all() -> Result<()> {
    client::apply(&checkout_all_button())
}

/// Deletes the nav checkout-all button.
pub fn remove_checkout_all() -> Result<()> {
    client::delete_button(CHECKOUT_ALL_BUTTON)
}

/// Reports whether a click came from the nav checkout-all button.
pub fn is_checkout_all_click(button: &str) -> bool {
    button == CHECKOUT_ALL_BUTTON
}

/// The global profile-switcher button: one checkbox per named profile (from
/// `tilt-devenv.json`'s `profiles` key), defaulted to `active` (the persisted
/// selection) — check any number, then click to save the selection, which
/// triggers the Tiltfile reload that redefines resources to match (see the
/// daemon's click handler).
fn profile_button(names: &[String], active: &[String]) -> UiButton {
    let mut button = UiButton::new(PROFILE_BUTTON.to_string(), "apply profiles".to_string())
        .icon("checklist")
        .at("nav", "Global");
    for name in names {
        button = button.bool_input(name, name, active.contains(name));
    }
    button
}

/// (Re)applies the nav profile-switcher button, offering `names` defaulted to
/// `active`.
pub fn render_profile_button(names: &[String], active: &[String]) -> Result<()> {
    client::apply(&profile_button(names, active))
}

/// Deletes the nav profile-switcher button.
pub fn remove_profile_button() -> Result<()> {
    client::delete_button(PROFILE_BUTTON)
}

/// Reports whether a click came from the nav profile-switcher button.
pub fn is_profile_click(button: &str) -> bool {
    button == PROFILE_BUTTON
}

/// Returns the resource a branch-button click targets.
pub fn branch_click_resource(button: &str) -> Option<&str> {
    button.strip_prefix(BRANCH_PREFIX).filter(|r| !r.is_empty())
}

/// Returns the resource a pull-button click targets.
pub fn pull_click_resource(button: &str) -> Option<&str> {
    button.strip_prefix(PULL_PREFIX).filter(|r| !r.is_empty())
}

/// Returns the resource a worktree-button click targets.
pub fn worktree_click_resource(button: &str) -> Option<&str> {
    button
        .strip_prefix(WORKTREE_PREFIX)
        .filter(|r| !r.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(button: &UiButton) -> serde_json::Value {
        serde_json::to_value(button).unwrap()
    }

    #[test]
    fn caption_shows_branch_ahead_behind_dirty() {
        let got = caption(&Snapshot {
            present: true,
            branch: "develop".into(),
            ahead: 2,
            behind: 1,
            dirty: true,
            ..Default::default()
        });
        for want in ["develop", "↑2", "↓1", "●"] {
            assert!(got.contains(want), "caption = {got:?}, want {want:?}");
        }
    }

    #[test]
    fn caption_missing_and_detached() {
        assert!(caption(&Snapshot::default()).contains("missing"));
        assert!(
            caption(&Snapshot {
                present: true,
                detached: true,
                ..Default::default()
            })
            .contains("detached")
        );
    }

    #[test]
    fn pull_caption_reflects_behind() {
        assert!(pull_caption(3).contains("↓3"));
        assert!(pull_caption(0).contains("up to date"));
    }

    #[test]
    fn branch_button_has_label_and_text_input() {
        let v = json(&Presenter::new("web-app", "/x".into()).branch_button("⎇ develop"));
        assert_eq!(v["kind"], "UIButton");
        assert_eq!(v["metadata"]["name"], "repos-branch-web-app");
        assert_eq!(v["spec"]["location"]["componentID"], "web-app");
        assert_eq!(v["spec"]["location"]["componentType"], "Resource");
        assert_eq!(v["spec"]["text"], "⎇ develop");
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["name"], "branch");
        assert!(inputs[0]["text"].is_object());
    }

    #[test]
    fn pull_button_disabled_when_not_behind() {
        let behind = json(&Presenter::new("svc", "/x".into()).pull_button(2));
        assert_eq!(behind["metadata"]["name"], "repos-pull-svc");
        assert!(
            behind["spec"].get("disabled").is_none(),
            "should be enabled"
        );
        assert!(behind["spec"]["text"].as_str().unwrap().contains("↓2"));

        let level = json(&Presenter::new("svc", "/x".into()).pull_button(0));
        assert_eq!(
            level["spec"]["disabled"], true,
            "want disabled when behind == 0"
        );
    }

    #[test]
    fn checkout_all_is_a_nav_text_button() {
        assert!(is_checkout_all_click("repos-checkout-all"));
        assert!(!is_checkout_all_click("repos-branch-x"));

        let v = json(&checkout_all_button());
        assert_eq!(v["metadata"]["name"], "repos-checkout-all");
        assert_eq!(v["spec"]["location"]["componentType"], "Global");
        assert_eq!(v["spec"]["location"]["componentID"], "nav");
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["name"], "branch");
        assert!(inputs[0]["text"].is_object());
    }

    #[test]
    fn profile_button_is_a_nav_button_with_one_checkbox_per_profile() {
        assert!(is_profile_click("repos-profile"));
        assert!(!is_profile_click("repos-checkout-all"));

        let names = ["frontend".to_string(), "backend".to_string()];
        let v = json(&profile_button(&names, &["backend".to_string()]));
        assert_eq!(v["metadata"]["name"], "repos-profile");
        assert_eq!(v["spec"]["location"]["componentType"], "Global");
        assert_eq!(v["spec"]["location"]["componentID"], "nav");
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["name"], "frontend");
        assert_eq!(inputs[0]["bool"]["defaultValue"], false, "not in active");
        assert_eq!(inputs[1]["name"], "backend");
        assert_eq!(inputs[1]["bool"]["defaultValue"], true, "in active");
    }

    #[test]
    fn worktree_button_is_a_dropdown_of_branches() {
        let worktrees = vec![
            git::Worktree {
                path: "/repos/app".into(),
                branch: "develop".into(),
                is_main: true,
            },
            git::Worktree {
                path: "/wt/app/feat-login".into(),
                branch: "feat/login".into(),
                is_main: false,
            },
        ];
        let p = Presenter::new("app", "/wt/app/feat-login".into());
        let v = json(&p.worktree_button(&worktrees, "feat/login"));
        assert_eq!(v["metadata"]["name"], "repos-worktree-app");
        assert_eq!(v["spec"]["location"]["componentType"], "Resource");
        // On a worktree: just the branch (not the dir path).
        assert_eq!(v["spec"]["text"], "🌳 feat/login");
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs[0]["name"], "worktree");
        assert_eq!(
            inputs[0]["choice"]["choices"],
            serde_json::json!(["develop", "feat/login"])
        );
        // On the main checkout (a Presenter rooted at the primary tree's path):
        // its branch is flagged (main).
        let on_main = Presenter::new("app", "/repos/app".into());
        let v = json(&on_main.worktree_button(&worktrees, "develop"));
        assert_eq!(v["spec"]["text"], "🌳 develop (main)");
    }

    #[test]
    fn flags_main_by_active_checkout_not_branch() {
        // Primary tree on feat/login, a linked worktree on fix/auth. Which one
        // is "(main)" depends on which *tree* is active — never on the branch.
        let worktrees = vec![
            git::Worktree {
                path: "/repos/app".into(),
                branch: "feat/login".into(),
                is_main: true,
            },
            git::Worktree {
                path: "/wt/app/fix-auth".into(),
                branch: "fix/auth".into(),
                is_main: false,
            },
        ];
        let text = |v: serde_json::Value| v["spec"]["text"].as_str().unwrap().to_string();

        // On the primary tree: flagged (main), whatever branch it sits on.
        let primary = Presenter::new("app", "/repos/app".into());
        assert!(
            text(json(&primary.worktree_button(&worktrees, "feat/login"))).ends_with("(main)"),
            "the primary tree must be flagged (main)"
        );

        // On the linked worktree: never flagged (main) — even when its active
        // branch string coincides with the primary's (e.g. both detached → "").
        let linked = Presenter::new("app", "/wt/app/fix-auth".into());
        assert!(
            !text(json(&linked.worktree_button(&worktrees, "feat/login"))).contains("(main)"),
            "a linked worktree must never be flagged (main)"
        );
    }

    #[test]
    fn click_resource_classification() {
        assert_eq!(
            branch_click_resource("repos-branch-web-app"),
            Some("web-app")
        );
        assert_eq!(pull_click_resource("repos-pull-worker"), Some("worker"));
        assert_eq!(
            worktree_click_resource("repos-worktree-auth-service"),
            Some("auth-service")
        );
        assert_eq!(branch_click_resource("repos-pull-x"), None);
        assert_eq!(pull_click_resource("toggle-redis-disable"), None);
        assert_eq!(worktree_click_resource("repos-branch-x"), None);
    }
}
