//! The specific buttons the daemon shows in Tilt and what they mean: a branch
//! picker and a pull button per repo, plus a global checkout-all, profile
//! switcher, and dev-env update. Built on the [`repos_core::tilt`] client seam,
//! which does the actual apply/delete/watch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use repos_core::devenv::{self, Snapshot};
use repos_core::git;
use repos_core::tilt::{self as client, BRANCH_BUTTON_PREFIX, Click, UiButton};

const PULL_PREFIX: &str = "repos-pull-";
const WORKTREE_PREFIX: &str = "repos-worktree-";
const CHECKOUT_ALL_BUTTON: &str = "repos-checkout-all";
const PROFILE_BUTTON: &str = "repos-profile";
const UPDATE_BUTTON: &str = "repos-dev-env-update";
/// Checkout-all's group dropdown sentinel meaning "every group" (no restriction).
const ALL_GROUPS: &str = "(all groups)";

fn branch_name(resource: &str) -> String {
    format!("{BRANCH_BUTTON_PREFIX}{resource}")
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
    // See `Snapshot::mirror`: ahead is the rewritten remote's leftovers here.
    if s.ahead > 0 && !s.mirror {
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

/// The global checkout-all button. "All" means the active profile's repos: the
/// profile decides what you're working on, so this follows it, as the CLI does.
fn checkout_all_button(groups: &[String]) -> UiButton {
    let mut group_choices = vec![ALL_GROUPS.to_string()];
    group_choices.extend(groups.iter().cloned());
    UiButton::new(
        CHECKOUT_ALL_BUTTON.to_string(),
        "⎇ checkout all".to_string(),
    )
    .icon("call_split")
    .at("nav", "Global")
    .text_input(
        "branch",
        "branch for every active repo (missing → default)",
        "branch name",
    )
    .choice_input("group", "restrict to group", group_choices)
}

/// (Re)applies the nav checkout-all button, offering `groups` in its dropdown.
pub fn render_checkout_all(groups: &[String]) -> Result<()> {
    client::apply(&checkout_all_button(groups))
}

/// Deletes the nav checkout-all button.
pub fn remove_checkout_all() -> Result<()> {
    client::delete_button(CHECKOUT_ALL_BUTTON)
}

/// The global profile-switcher button: one checkbox per named profile (from
/// `tilt-devenv.json`'s `profiles` key) that isn't in `no_access` (a profile
/// resolving to a currently-unreachable repo, checked across the whole
/// registry — see the daemon's `no_access_profiles`) — a profile you can't
/// actually activate is left off the picker entirely, not just marked.
/// Checkboxes default to `active` (the persisted selection); check any
/// number and click to save, which triggers the Tiltfile reload that
/// redefines resources to match (see the daemon's click handler).
fn profile_button(names: &[String], active: &[String]) -> UiButton {
    let mut button = UiButton::new(PROFILE_BUTTON.to_string(), "profiles".to_string())
        .icon("checklist")
        .at("nav", "Global");
    for name in names {
        button = button.bool_input(name, name, active.contains(name));
    }
    button
}

/// (Re)applies the nav profile-switcher button: a checkbox per name in
/// `names`, defaulted to those in `active`. Which profiles are worth offering
/// is the caller's business.
///
/// No names means no button — a picker with no checkboxes isn't a picker, since
/// its only possible click silently clears the selection. Returns whether the
/// button is now showing.
pub fn render_profile_button(names: &[String], active: &[String]) -> Result<bool> {
    if names.is_empty() {
        remove_profile_button()?;
        return Ok(false);
    }
    client::apply(&profile_button(names, active))?;
    Ok(true)
}

/// Deletes the nav profile-switcher button.
pub fn remove_profile_button() -> Result<()> {
    client::delete_button(PROFILE_BUTTON)
}

/// The update button's icon. The count is drawn here rather than put in the
/// label because a nav button always shows its icon, and its text only where
/// there's room for it.
///
/// The viewBox must stay square — Tilt's nav slot is, and it crops a wider icon
/// rather than scaling it down.
///
/// `style` as well as `fill` throughout: Tilt's own CSS rule beats the
/// presentation attribute alone and would repaint the icon.
fn update_icon(behind: i32) -> String {
    // Exact to 999: capping lower would make 10 and 200 look alike.
    let count = if behind > 999 {
        "999+".to_string()
    } else {
        behind.to_string()
    };
    // Grows leftward from a fixed right edge, one step per digit: one digit
    // keeps the round badge, three still fit. `just icons` draws all three.
    let (width, font) = match count.chars().count() {
        1 => (12.4, "8.5"),
        2 => (16.0, "8"),
        3 => (21.0, "7"),
        _ => (24.0, "6"),
    };
    let x = BADGE_RIGHT - width;
    let centre = BADGE_RIGHT - width / 2.0;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 28 28" width="24" height="24">
  <circle cx="12" cy="12" r="11" fill="#FCC000" style="fill:#FCC000"/>
  <path d="M12 6l5.5 6.5h-3.25V18h-4.5v-5.5H6.5z" fill="#1D1D1D" style="fill:#1D1D1D"/>
  <rect x="{x}" y="14.8" width="{width}" height="12.4" rx="6.2"
        fill="#1D1D1D" stroke="#FCC000" stroke-width="1.6"
        style="fill:#1D1D1D;stroke:#FCC000"/>
  <text x="{centre}" y="21" fill="#FCC000" style="fill:#FCC000" font-size="{font}"
        font-family="sans-serif" font-weight="bold"
        text-anchor="middle" dominant-baseline="central">{count}</text>
</svg>"##
    )
}

/// The badge's right edge, just inside the 28-unit viewBox.
const BADGE_RIGHT: f32 = 27.2;

/// The global dev-env update button.
///
/// `restarts` says whether Tilt is supervised (`repos up`) and can relaunch
/// itself: the label promises only what will actually happen, and confirms
/// first when the click will tear down every running service.
fn update_button(behind: i32, restarts: bool) -> UiButton {
    UiButton::new(UPDATE_BUTTON.to_string(), update_caption(restarts))
        .icon_svg(&update_icon(behind))
        .at("nav", "Global")
        .requires_confirmation(restarts)
}

/// Short enough to survive the nav bar's width — the icon's badge carries the
/// count, so the words only have to warn about the disruptive case.
fn update_caption(restarts: bool) -> String {
    if restarts {
        "Update dev env — restarts Tilt".to_string()
    } else {
        "Update dev env".to_string()
    }
}

/// (Re)applies the nav dev-env update button. See [`update_button`].
pub fn render_update_button(behind: i32, restarts: bool) -> Result<()> {
    client::apply(&update_button(behind, restarts))
}

/// Deletes the nav dev-env update button.
pub fn remove_update_button() -> Result<()> {
    client::delete_button(UPDATE_BUTTON)
}

/// What a button press means. The handler matches on this, so a new button
/// nobody wired up fails to compile rather than silently doing nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Check every in-scope repo out onto `branch`; empty means each repo's
    /// own default.
    CheckoutAll {
        branch: String,
        group: Option<String>,
    },
    /// Fast-forward the dev environment itself.
    UpdateDevEnv,
    /// Save this profile selection; empty clears it.
    SetProfiles(Vec<String>),
    Checkout {
        resource: String,
        branch: String,
    },
    Pull {
        resource: String,
    },
    /// Switch `resource` to the worktree holding `branch`.
    SelectWorktree {
        resource: String,
        branch: String,
    },
}

/// Resolves a click into the [`Action`] it means, or `None` for a button this
/// daemon doesn't own — Tilt streams every button's clicks, including its own
/// stop/disable ones.
pub fn action(click: &Click) -> Option<Action> {
    let input = |name: &str| click.inputs.get(name).cloned().unwrap_or_default();
    let of_resource = |prefix: &str| {
        click
            .button
            .strip_prefix(prefix)
            .filter(|r| !r.is_empty())
            .map(str::to_string)
    };

    match click.button.as_str() {
        CHECKOUT_ALL_BUTTON => {
            return Some(Action::CheckoutAll {
                branch: input("branch"),
                group: chosen_group(&click.inputs),
            });
        }
        UPDATE_BUTTON => return Some(Action::UpdateDevEnv),
        PROFILE_BUTTON => return Some(Action::SetProfiles(checked(&click.inputs))),
        _ => {}
    }
    if let Some(resource) = of_resource(BRANCH_BUTTON_PREFIX) {
        return Some(Action::Checkout {
            resource,
            branch: input("branch"),
        });
    }
    if let Some(resource) = of_resource(PULL_PREFIX) {
        return Some(Action::Pull { resource });
    }
    of_resource(WORKTREE_PREFIX).map(|resource| Action::SelectWorktree {
        resource,
        branch: input("worktree"),
    })
}

/// Checkout-all's `group` input, or `None` for [`ALL_GROUPS`] / an absent value.
fn chosen_group(inputs: &HashMap<String, String>) -> Option<String> {
    match inputs.get("group") {
        Some(g) if !g.is_empty() && g != ALL_GROUPS => Some(g.clone()),
        _ => None,
    }
}

/// The names of the ticked checkboxes, from a click's raw input values
/// (`"true"`/`"false"`, per [`repos_core::tilt`]'s bool encoding).
fn checked(inputs: &HashMap<String, String>) -> Vec<String> {
    inputs
        .iter()
        .filter(|(_, v)| v.as_str() == "true")
        .map(|(name, _)| name.clone())
        .collect()
}

/// Deletes every nav button this daemon owns, so shutdown leaves none behind.
pub fn remove_global() -> Result<()> {
    remove_checkout_all()?;
    remove_profile_button()?;
    remove_update_button()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(button: &str, inputs: &[(&str, &str)]) -> Click {
        Click {
            button: button.to_string(),
            inputs: inputs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_button_this_daemon_does_not_own_decodes_to_nothing() {
        // A bare prefix names no resource, so it must not decode to one.
        for name in [
            "toggle-redis-disable",
            "git-status-stopbuild",
            "repos-branch-",
            "repos-pull-",
        ] {
            assert_eq!(action(&click(name, &[])), None, "{name}");
        }
    }

    #[test]
    fn checkout_all_carries_its_branch_and_group() {
        assert_eq!(
            action(&click(
                CHECKOUT_ALL_BUTTON,
                &[("branch", "feat/x"), ("group", "backend")]
            )),
            Some(Action::CheckoutAll {
                branch: "feat/x".to_string(),
                group: Some("backend".to_string()),
            })
        );
    }

    #[test]
    fn checkout_all_treats_the_all_groups_sentinel_as_no_restriction() {
        let group = |g| match action(&click(CHECKOUT_ALL_BUTTON, &[("group", g)])) {
            Some(Action::CheckoutAll { group, .. }) => group,
            other => panic!("{other:?}"),
        };
        assert_eq!(group(ALL_GROUPS), None);
        assert_eq!(group(""), None);
        assert_eq!(group("backend"), Some("backend".to_string()));
    }

    #[test]
    fn setting_profiles_keeps_only_the_ticked_boxes() {
        assert_eq!(
            action(&click(
                PROFILE_BUTTON,
                &[("frontend", "true"), ("backend", "false")]
            )),
            Some(Action::SetProfiles(vec!["frontend".to_string()]))
        );
    }

    #[test]
    fn setting_no_profiles_is_a_selection_of_none_not_a_missing_action() {
        assert_eq!(
            action(&click(PROFILE_BUTTON, &[("frontend", "false")])),
            Some(Action::SetProfiles(vec![]))
        );
    }

    #[test]
    fn the_update_button_needs_no_inputs() {
        assert_eq!(
            action(&click(UPDATE_BUTTON, &[])),
            Some(Action::UpdateDevEnv)
        );
    }

    #[test]
    fn per_resource_buttons_carry_the_resource_they_belong_to() {
        assert_eq!(
            action(&click("repos-branch-web-app", &[("branch", "main")])),
            Some(Action::Checkout {
                resource: "web-app".to_string(),
                branch: "main".to_string(),
            })
        );
        assert_eq!(
            action(&click("repos-pull-worker", &[])),
            Some(Action::Pull {
                resource: "worker".to_string()
            })
        );
        assert_eq!(
            action(&click(
                "repos-worktree-auth-service",
                &[("worktree", "feat/login")]
            )),
            Some(Action::SelectWorktree {
                resource: "auth-service".to_string(),
                branch: "feat/login".to_string(),
            })
        );
    }

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
    fn caption_hides_ahead_on_a_mirror_branch() {
        let got = caption(&Snapshot {
            present: true,
            branch: "nightly".into(),
            mirror: true,
            ahead: 1,
            behind: 1,
            ..Default::default()
        });

        assert_eq!(got, "⎇ nightly ↓1");
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
    fn checkout_all_is_a_nav_button_with_branch_and_group_inputs() {
        let v = json(&checkout_all_button(&[
            "backend".to_string(),
            "frontend".to_string(),
        ]));
        assert_eq!(v["metadata"]["name"], "repos-checkout-all");
        assert_eq!(v["spec"]["text"], "⎇ checkout all");
        assert_eq!(v["spec"]["location"]["componentType"], "Global");
        assert_eq!(v["spec"]["location"]["componentID"], "nav");
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["name"], "branch");
        assert!(inputs[0]["text"].is_object());
        assert_eq!(inputs[1]["name"], "group");
        assert_eq!(
            inputs[1]["choice"]["choices"],
            serde_json::json!(["(all groups)", "backend", "frontend"])
        );
    }

    #[test]
    fn profile_button_is_a_nav_button_with_one_checkbox_per_profile() {
        let names = ["frontend".to_string(), "backend".to_string()];
        let v = json(&profile_button(&names, &["backend".to_string()]));
        assert_eq!(v["metadata"]["name"], "repos-profile");
        assert_eq!(v["spec"]["location"]["componentType"], "Global");
        assert_eq!(v["spec"]["location"]["componentID"], "nav");
        assert_eq!(v["spec"]["iconName"], "checklist");
        let inputs = v["spec"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["name"], "frontend");
        assert_eq!(inputs[0]["label"], "frontend");
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
    fn update_is_a_nav_button_carrying_its_own_colour() {
        let v = json(&update_button(3, true));
        assert_eq!(v["metadata"]["name"], "repos-dev-env-update");
        assert_eq!(v["spec"]["location"]["componentType"], "Global");
        assert_eq!(v["spec"]["location"]["componentID"], "nav");
        assert!(
            v["spec"]["iconSVG"].as_str().unwrap().contains("<svg"),
            "an inline SVG is the only way to colour a Tilt button"
        );
        assert!(v["spec"]["inputs"].is_null(), "nothing to fill in");
    }

    #[test]
    fn update_icon_badges_the_pending_commit_count() {
        assert!(update_icon(3).contains(">3<"), "icon = {}", update_icon(3));
        assert!(update_icon(42).contains(">42<"));
    }

    #[test]
    fn update_icon_caps_the_badge_at_999() {
        assert!(update_icon(1000).contains(">999+<"));
        assert!(update_icon(999).contains(">999<"), "999 still fits exactly");
    }

    /// The badge `<rect>`'s (x, width), read out of a rendered icon.
    fn badge_box(behind: i32) -> (f32, f32) {
        let svg = update_icon(behind);
        let rect = svg
            .lines()
            .find(|l| l.trim_start().starts_with("<rect"))
            .expect("icon has a badge rect");
        let attr = |name: &str| {
            rect.split(&format!("{name}=\""))
                .nth(1)
                .expect(name)
                .split('"')
                .next()
                .unwrap()
                .parse::<f32>()
                .unwrap()
        };
        (attr("x"), attr("width"))
    }

    #[test]
    fn update_icon_widens_the_badge_one_step_per_digit() {
        assert!(
            badge_box(1).1 < badge_box(42).1,
            "two digits need more room"
        );
        assert!(
            badge_box(42).1 < badge_box(999).1,
            "three digits, more still"
        );
    }

    #[test]
    fn update_icon_keeps_the_badge_inside_the_viewbox() {
        // The badge grows leftward precisely so it can't run off the right edge
        // and be cropped — which is what happened when the whole icon widened.
        for n in [1, 42, 999, 1000] {
            let (x, width) = badge_box(n);
            assert!(x >= 0.0, "badge starts off-canvas at {n}");
            assert!(x + width <= 28.0, "badge overflows at {n}: {}", x + width);
        }
    }

    #[test]
    fn update_icon_stays_square_whatever_the_count() {
        // Regression: Tilt's nav slot is square and crops a wider icon instead
        // of scaling it, so a 34x28 viewBox lost the right-hand digits.
        let box_of = |n| {
            update_icon(n)
                .lines()
                .next()
                .unwrap()
                .split("viewBox=")
                .nth(1)
                .unwrap()
                .split('"')
                .nth(1)
                .unwrap()
                .to_string()
        };
        for n in [1, 42, 100] {
            let view_box = box_of(n);
            let dims: Vec<&str> = view_box.split_whitespace().collect();
            assert_eq!(dims[2], dims[3], "viewBox must be square for behind={n}");
        }
        assert_eq!(box_of(1), box_of(100), "and identical, so nothing reflows");
    }

    /// Writes one icon per badge width to `target/icons/`, for eyeballing an
    /// icon change. Ignored by default — it asserts nothing, it just draws.
    /// `just icons` runs it and rasterises the result.
    #[test]
    #[ignore = "preview generator; run it via `just icons`"]
    fn dump_update_icons() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/icons")
            .canonicalize()
            .unwrap_or_else(|_| "target/icons".into());
        std::fs::create_dir_all(&dir).unwrap();
        for n in [1, 42, 999] {
            std::fs::write(dir.join(format!("update-{n}.svg")), update_icon(n)).unwrap();
        }
        println!("wrote update-{{1,42,999}}.svg to {}", dir.display());
    }

    #[test]
    fn update_caption_stays_short_enough_to_read_in_the_nav() {
        // The nav truncates: the earlier "dev environment update available ↓1 —
        // restarts Tilt" was unreadable, which is why the count moved to the icon.
        for restarts in [true, false] {
            let got = update_caption(restarts);
            assert!(got.chars().count() <= 32, "too long to read: {got:?}");
        }
    }

    #[test]
    fn update_confirms_only_when_clicking_it_restarts_tilt() {
        // Supervised, the click tears down every running service — worth a
        // confirm. Unsupervised it only pulls, so a confirm is just friction.
        assert_eq!(
            json(&update_button(1, true))["spec"]["requiresConfirmation"],
            true
        );
        assert!(
            json(&update_button(1, false))["spec"]
                .get("requiresConfirmation")
                .is_none()
        );
    }

    #[test]
    fn update_caption_promises_a_restart_only_when_one_will_happen() {
        assert!(update_caption(true).contains("restarts Tilt"));

        let unsupervised = update_caption(false);
        assert!(
            !unsupervised.contains("restarts Tilt"),
            "a bare `tilt up` can't restart itself; don't claim it will: {unsupervised}"
        );
    }
}
