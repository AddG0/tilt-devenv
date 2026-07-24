//! `repos logs` — tail Tilt service logs in lnav.
//!
//! One `tilt logs --json` follower for all resources, demuxed into a file per
//! resource, which lnav opens together. The per-resource files give lnav's Files
//! panel a toggle per server. Files live in a temp dir cleaned up when lnav exits.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use repos_core::tilt as client;

use crate::cli::LogsArgs;

pub fn run(args: &LogsArgs) -> Result<()> {
    let available = fetch_resources()?;
    let targets = resolve_targets(args, &available)?;

    let dir = tempfile::Builder::new()
        .prefix("repos-logs-")
        .tempdir()
        .context("creating a temp dir for per-resource logs")?;

    // Pre-create a file per resource so lnav lists them all from the start; the
    // demux thread fills them in as lines arrive.
    let mut paths = Vec::new();
    for res in &targets {
        let path = dir.path().join(res);
        File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        paths.push(path);
    }

    let mut tilt = Command::new("tilt")
        .args(tilt_logs_args(args, &targets))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning `tilt logs`")?;
    let stdout = tilt.stdout.take().expect("stdout was piped");

    let dir_path = dir.path().to_path_buf();
    let demux = std::thread::spawn(move || demux(stdout, &dir_path));

    let mut lnav = match Command::new("lnav").args(&paths).spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = tilt.kill();
            let _ = tilt.wait();
            let _ = demux.join();
            return Err(
                anyhow::Error::new(e).context("spawning `lnav` — is it installed and on PATH?")
            );
        }
    };

    let status = lnav.wait().context("waiting for `lnav`")?;

    // The user quit lnav; stop the follower and let the demux thread drain.
    let _ = tilt.kill();
    let _ = tilt.wait();
    let demux_result = demux
        .join()
        .unwrap_or_else(|_| Err(anyhow!("log demux thread panicked")));

    if !status.success() {
        return Err(anyhow!("lnav exited with {status}"));
    }
    demux_result
}

/// Writes each `tilt logs --json` line's raw message into its resource's file,
/// opening files on first sighting. Ends when the follower's stdout closes
/// (i.e. after the tilt child is killed).
fn demux(stdout: ChildStdout, dir: &Path) -> Result<()> {
    let mut files: HashMap<String, File> = HashMap::new();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Some(entry) = LogLine::parse(&line) else {
            continue;
        };
        if !files.contains_key(&entry.resource) {
            let path = dir.join(&entry.resource);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            files.insert(entry.resource.clone(), file);
        }
        let file = files.get_mut(&entry.resource).expect("just inserted");
        writeln!(file, "{}", entry.message)
            .with_context(|| format!("writing {} log", entry.resource))?;
    }
    Ok(())
}

/// One line of `tilt logs --json`. Only the fields we re-emit are kept.
#[derive(Deserialize)]
struct LogLine {
    #[serde(default)]
    resource: String,
    #[serde(default)]
    message: String,
}

impl LogLine {
    fn parse(line: &str) -> Option<LogLine> {
        let entry: LogLine = serde_json::from_str(line).ok()?;
        if entry.resource.is_empty() {
            return None;
        }
        Some(entry)
    }
}

/// Best-effort resource names for shell completion — empty when Tilt is
/// unreachable, so a missing daemon degrades to no suggestions rather than an error.
pub fn live_resource_names() -> Vec<String> {
    fetch_resources()
        .map(|rs| rs.into_iter().map(|r| r.name).collect())
        .unwrap_or_default()
}

pub fn complete_resource(current: &OsStr) -> Vec<clap_complete::engine::CompletionCandidate> {
    let prefix = current.to_string_lossy();
    live_resource_names()
        .into_iter()
        .filter(|n| n.starts_with(prefix.as_ref()))
        .map(clap_complete::engine::CompletionCandidate::new)
        .collect()
}

/// The current Tilt resources. Doubles as the "is Tilt running?" pre-flight —
/// a failure here means there's nothing to tail.
fn fetch_resources() -> Result<Vec<client::Resource>> {
    client::uiresources().context("couldn't reach Tilt — is `tilt up` running?")
}

/// The resources to tail: those named, or all *labeled* resources when none are.
/// Unlabeled resources are Tilt's setup tasks (gradle-wrappers, pnpm-installs,
/// repos-branches) — off by default, but still tailable when named explicitly.
fn resolve_targets(args: &LogsArgs, available: &[client::Resource]) -> Result<Vec<String>> {
    if args.resources.is_empty() {
        let targets = default_targets(available);
        if targets.is_empty() {
            return Err(anyhow!("no resources to tail"));
        }
        return Ok(targets);
    }
    let mut names: Vec<String> = available.iter().map(|r| r.name.clone()).collect();
    let unknown = unknown_resources(&args.resources, &names);
    if !unknown.is_empty() {
        names.sort();
        return Err(anyhow!(
            "unknown resource(s): {}\navailable: {}",
            unknown.join(", "),
            names.join(", ")
        ));
    }
    Ok(args.resources.clone())
}

fn tilt_logs_args(args: &LogsArgs, targets: &[String]) -> Vec<String> {
    let mut v = vec!["logs".to_string()];
    if !args.no_follow {
        v.push("-f".to_string());
    }
    if let Some(n) = args.tail {
        v.push("--tail".to_string());
        v.push(n.to_string());
    }
    v.push("--json".to_string());
    v.extend(targets.iter().cloned());
    v
}

/// The default tail set: resources carrying a Tilt label. Unlabeled ones are
/// setup/meta tasks (gradle-wrappers, pnpm-installs, `(Tiltfile)`), not services.
fn default_targets(available: &[client::Resource]) -> Vec<String> {
    available
        .iter()
        .filter(|r| r.labeled)
        .map(|r| r.name.clone())
        .collect()
}

fn unknown_resources(requested: &[String], available: &[String]) -> Vec<String> {
    requested
        .iter()
        .filter(|r| !available.contains(r))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs_args(no_follow: bool, tail: Option<i64>, resources: &[&str]) -> LogsArgs {
        LogsArgs {
            resources: owned(resources),
            no_follow,
            tail,
        }
    }

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn resources(items: &[(&str, bool)]) -> Vec<client::Resource> {
        items
            .iter()
            .map(|(name, labeled)| client::Resource {
                name: name.to_string(),
                labeled: *labeled,
            })
            .collect()
    }

    #[test]
    fn requests_json_and_follows_targets_by_default() {
        assert_eq!(
            tilt_logs_args(&logs_args(false, None, &[]), &owned(&["redis"])),
            ["logs", "-f", "--json", "redis"]
        );
    }

    #[test]
    fn omits_follow_flag_when_no_follow_set() {
        assert_eq!(
            tilt_logs_args(&logs_args(true, None, &[]), &owned(&["redis"])),
            ["logs", "--json", "redis"]
        );
    }

    #[test]
    fn adds_tail_count_before_json_and_targets() {
        assert_eq!(
            tilt_logs_args(&logs_args(false, Some(50), &[]), &owned(&["redis"])),
            ["logs", "-f", "--tail", "50", "--json", "redis"]
        );
    }

    #[test]
    fn resolve_targets_defaults_to_labeled_resources_only() {
        let available = resources(&[
            ("redis", true),
            ("gradle-wrappers", false),
            ("postgres", true),
        ]);
        let targets = resolve_targets(&logs_args(false, None, &[]), &available).unwrap();
        assert_eq!(targets, ["redis", "postgres"]);
    }

    #[test]
    fn resolve_targets_allows_naming_an_unlabeled_resource() {
        let available = resources(&[("redis", true), ("gradle-wrappers", false)]);
        let targets =
            resolve_targets(&logs_args(false, None, &["gradle-wrappers"]), &available).unwrap();
        assert_eq!(targets, ["gradle-wrappers"]);
    }

    #[test]
    fn resolve_targets_errors_on_unknown_resource() {
        let available = resources(&[("redis", true)]);
        assert!(resolve_targets(&logs_args(false, None, &["typo"]), &available).is_err());
    }

    #[test]
    fn resolve_targets_errors_when_nothing_is_labeled() {
        let available = resources(&[("gradle-wrappers", false)]);
        assert!(resolve_targets(&logs_args(false, None, &[]), &available).is_err());
    }

    #[test]
    fn parses_resource_and_message_from_a_json_line() {
        let line = r#"{"time":"2026-07-20T16:44:04-05:00","resource":"redis","level":"info","message":"hi","source":"runtime"}"#;
        let entry = LogLine::parse(line).unwrap();
        assert_eq!(entry.resource, "redis");
        assert_eq!(entry.message, "hi");
    }

    #[test]
    fn skips_lines_without_a_resource() {
        assert!(LogLine::parse(r#"{"message":"no resource"}"#).is_none());
        assert!(LogLine::parse("not json").is_none());
    }
}
