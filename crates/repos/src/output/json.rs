//! Machine-facing `--json` output. These structs are the public JSON contract
//! that the Tiltfile and scripts consume — field order and skip rules are part
//! of that contract, so keep them stable.

use std::collections::BTreeMap;

use anyhow::Result;
use repos_core::devenv::{OpResult, Snapshot};
use repos_core::registry::Resolved;
use serde::Serialize;

/// One item of `repos list --json`.
#[derive(Serialize)]
struct ListItem {
    name: String,
    url: String,
    group: String,
    path: String,
    present: bool,
}

/// One item of `repos status --json`.
#[derive(Serialize)]
struct StatusItem {
    name: String,
    group: String,
    present: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    branch: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    detached: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    upstream: String,
    ahead: i32,
    behind: i32,
    dirty: bool,
    #[serde(rename = "defaultBranch", skip_serializing_if = "String::is_empty")]
    default_branch: String,
    #[serde(rename = "onDefault")]
    on_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(rename = "fetchError", skip_serializing_if = "Option::is_none")]
    fetch_error: Option<String>,
}

/// Encodes `v` as indented JSON with a trailing newline.
fn print_json<T: Serialize>(v: &T) -> Result<()> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    print!("{s}");
    Ok(())
}

pub fn print_list_json(repos: &[Resolved]) -> Result<()> {
    let items: Vec<ListItem> = repos
        .iter()
        .map(|r| ListItem {
            name: r.repo.name.clone(),
            url: r.repo.url.clone(),
            group: r.repo.group.clone(),
            path: r.path.display().to_string(),
            present: r.present,
        })
        .collect();
    print_json(&items)
}

pub fn print_profiles_json(profiles: &BTreeMap<String, Vec<String>>) -> Result<()> {
    print_json(profiles)
}

/// One item of `repos clone --json`.
#[derive(Serialize)]
struct CloneItem {
    name: String,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn print_clone_json(results: &[OpResult]) -> Result<()> {
    let items: Vec<CloneItem> = results
        .iter()
        .map(|r| CloneItem {
            name: r.name.clone(),
            outcome: r.outcome.label().to_string(),
            error: r.err.clone(),
        })
        .collect();
    print_json(&items)
}

pub fn print_status_json(statuses: &[Snapshot]) -> Result<()> {
    let items: Vec<StatusItem> = statuses
        .iter()
        .map(|s| StatusItem {
            name: s.name.clone(),
            group: s.group.clone(),
            present: s.present,
            branch: s.branch.clone(),
            detached: s.detached,
            upstream: s.upstream.clone(),
            ahead: s.ahead,
            behind: s.behind,
            dirty: s.dirty,
            default_branch: s.default_branch.clone(),
            on_default: s.is_on_default_branch(),
            error: s.err.clone(),
            fetch_error: s.fetch_err.clone(),
        })
        .collect();
    print_json(&items)
}
