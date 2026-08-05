//! Command-line definitions (clap derive) plus the dynamic completion wiring.

use std::ffi::OsStr;

use clap::{Args, Parser, Subcommand};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use repos_core::devenv::DEFAULT_ALIAS;
use repos_core::git;
use repos_core::registry::Registry;

#[derive(Parser)]
#[command(
    name = "repos",
    about = "Cross-repo git branch management for a multi-repo dev environment",
    long_about = "repos manages git branches across every repo in the dev environment at once.\n\n\
                  Repos are listed in tilt-devenv.json at the root of the dev environment.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show current branch, dirty state, and ahead/behind for every repo
    Status(StatusArgs),
    /// Switch every repo to <branch>, falling back to each repo's default branch where it doesn't exist
    Checkout(CheckoutArgs),
    /// Fast-forward every repo to its upstream (never merges or rebases)
    Pull(PullArgs),
    /// List every repo and where it lives on disk
    List(ListArgs),
    /// List every named profile and the repos/groups it enables
    Profiles(ProfilesArgs),
    /// Inspect or set the developer's active profile selection
    Profile(ProfileArgs),
    /// Tail service logs in lnav, one toggleable source per resource
    Logs(LogsArgs),
    /// Inspect the active per-repo worktree selection
    Worktree(WorktreeArgs),
}

#[derive(Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub cmd: WorktreeCmd,
}

#[derive(Subcommand)]
pub enum WorktreeCmd {
    /// Print the worktree-selection state file's path (the Tiltfile watches it)
    StatePath,
}

#[derive(Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub cmd: ProfileCmd,
}

#[derive(Subcommand)]
pub enum ProfileCmd {
    /// Print the profile-selection state file's path (the Tiltfile watches it)
    StatePath,
    /// Print the active profile selection (empty means every profile enabled)
    Active {
        /// Emit a JSON array instead of one name per line
        #[arg(long)]
        json: bool,
    },
    /// Persist a new active profile selection (comma-separated; empty enables every profile)
    Set {
        /// Profile names to enable together (comma- or space-separated); omit to enable every profile
        #[arg(value_delimiter = ',', add = ArgValueCompleter::new(complete_profile_name))]
        profiles: Vec<String>,
    },
}

#[derive(Args)]
pub struct StatusArgs {
    /// Fetch each repo first so ahead/behind reflects the remote (slower)
    #[arg(long)]
    pub fetch: bool,
    /// Restrict to repos in these logical groups (comma-separated), e.g. `backend`
    #[arg(long, short, value_delimiter = ',', add = ArgValueCompleter::new(complete_group_name))]
    pub group: Vec<String>,
    /// Restrict to repos in these named profiles (comma-separated), e.g. `frontend`
    #[arg(long, short, value_delimiter = ',', add = ArgValueCompleter::new(complete_profile_name))]
    pub profile: Vec<String>,
    /// Emit JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CheckoutArgs {
    /// The branch to switch every repo to (or `default` for each repo's own default branch)
    #[arg(add = ArgValueCompleter::new(complete_branch_name))]
    pub branch: String,
    /// Fetch each repo first so a freshly-pushed branch is found (slower)
    #[arg(long)]
    pub fetch: bool,
    /// Restrict to these repo names (comma-separated)
    #[arg(long, value_delimiter = ',', add = ArgValueCompleter::new(complete_repo_name))]
    pub only: Vec<String>,
    /// Restrict to repos in these logical groups (comma-separated), e.g. `backend`
    #[arg(long, short, value_delimiter = ',', add = ArgValueCompleter::new(complete_group_name))]
    pub group: Vec<String>,
    /// Restrict to repos in these named profiles (comma-separated), e.g. `frontend`
    #[arg(long, short, value_delimiter = ',', add = ArgValueCompleter::new(complete_profile_name))]
    pub profile: Vec<String>,
    /// Show what would happen without switching any repo
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct PullArgs {
    /// Restrict to these repo names (comma-separated)
    #[arg(long, value_delimiter = ',', add = ArgValueCompleter::new(complete_repo_name))]
    pub only: Vec<String>,
    /// Restrict to repos in these logical groups (comma-separated), e.g. `backend`
    #[arg(long, short, value_delimiter = ',', add = ArgValueCompleter::new(complete_group_name))]
    pub group: Vec<String>,
    /// Restrict to repos in these named profiles (comma-separated), e.g. `frontend`
    #[arg(long, short, value_delimiter = ',', add = ArgValueCompleter::new(complete_profile_name))]
    pub profile: Vec<String>,
}

#[derive(Args)]
pub struct ListArgs {
    /// Emit JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ProfilesArgs {
    /// Emit JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct LogsArgs {
    /// Tilt resources to tail (default: all). e.g. `redis notifications`
    #[arg(add = ArgValueCompleter::new(crate::commands::logs::complete_resource))]
    pub resources: Vec<String>,
    /// Show current logs and exit instead of following the live stream
    #[arg(long)]
    pub no_follow: bool,
    /// Start by showing only the last N lines
    #[arg(long, value_name = "N")]
    pub tail: Option<i64>,
}

/// clap dynamic value-completer for a comma-separated flag (`--only`,
/// `--group`, `--profile`): `current` is the whole token typed so far (e.g.
/// `web,api,no`), so this completes only the segment after the last comma and
/// re-attaches the committed prefix, dropping names already listed.
fn complete_from(names: &[String], current: &OsStr) -> Vec<CompletionCandidate> {
    complete_segment(names, &current.to_string_lossy())
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

/// clap dynamic value-completer for the `--only` flag, over every repo name.
fn complete_repo_name(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(reg) = Registry::load() else {
        return Vec::new();
    };
    let names: Vec<String> = reg.repos.into_iter().map(|r| r.name).collect();
    complete_from(&names, current)
}

/// clap dynamic value-completer for the `--group` flag, over the distinct group names.
fn complete_group_name(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(reg) = Registry::load() else {
        return Vec::new();
    };
    let mut groups: Vec<String> = reg.repos.into_iter().map(|r| r.group).collect();
    groups.sort();
    groups.dedup();
    complete_from(&groups, current)
}

/// clap dynamic value-completer for the `--profile` flag, over the registry's profile names.
fn complete_profile_name(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(reg) = Registry::load() else {
        return Vec::new();
    };
    let names: Vec<String> = reg.profiles.into_keys().collect();
    complete_from(&names, current)
}

/// clap dynamic value-completer for the `checkout <branch>` positional: the
/// branch names that exist across the cloned dev-env repos (local branches plus
/// origin remote-tracking branches), so tab-completing an existing branch is
/// easy. `branch` is a single value (not comma-separated), so `current` is
/// matched as a plain prefix. Best-effort — an unreachable registry or a repo
/// git failure just contributes no suggestions.
fn complete_branch_name(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(reg) = Registry::load() else {
        return Vec::new();
    };
    let mut names: Vec<String> = reg
        .resolve()
        .iter()
        .filter(|r| r.present)
        .flat_map(|r| git::branch_names(&r.path))
        .collect();
    names.sort();
    names.dedup();
    // The `default` alias isn't a real ref, so it won't come from git.
    names.insert(0, DEFAULT_ALIAS.to_string());
    let partial = current.to_string_lossy();
    names
        .into_iter()
        .filter(|n| n.starts_with(partial.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

/// Pure completion logic: given all repo `names` and the token typed so far,
/// return the suggestions (each carrying the committed comma-prefix).
fn complete_segment(names: &[String], to_complete: &str) -> Vec<String> {
    let (prefix, partial) = match to_complete.rfind(',') {
        Some(i) => (&to_complete[..=i], &to_complete[i + 1..]),
        None => ("", to_complete),
    };
    let chosen: Vec<&str> = prefix.split(',').filter(|s| !s.is_empty()).collect();
    names
        .iter()
        .filter(|n| !chosen.contains(&n.as_str()) && n.starts_with(partial))
        .map(|n| format!("{prefix}{n}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three() -> Vec<String> {
        ["web", "api", "worker"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn returns_all_names_for_empty_input() {
        assert_eq!(complete_segment(&three(), ""), ["web", "api", "worker"]);
    }

    #[test]
    fn filters_by_prefix() {
        assert_eq!(complete_segment(&three(), "w"), ["web", "worker"]);
    }

    #[test]
    fn completes_segment_after_comma() {
        assert_eq!(complete_segment(&three(), "web,w"), ["web,worker"]);
    }

    #[test]
    fn excludes_already_chosen_names() {
        assert_eq!(
            complete_segment(&three(), "web,"),
            ["web,api", "web,worker"]
        );
    }
}
