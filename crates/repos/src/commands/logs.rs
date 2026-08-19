//! `repos logs` — tail Tilt service logs in lnav.
//!
//! One `tilt logs --json` follower for all resources, demuxed into a file per
//! resource, which lnav opens together. The per-resource files give lnav's Files
//! panel a toggle per server. Files live in a temp dir cleaned up when lnav exits.
//!
//! Each line goes in as a JSON record: the service's line verbatim, plus the
//! timestamp [`repos_core::logstamp`] chose for it in a field lnav orders by but
//! never renders.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use repos_core::logstamp::{Sink, Stamper, install_lnav_format, physical_lines};
use repos_core::tilt as client;

use crate::cli::LogsArgs;

/// Well under [`repos_core::logstamp::HEAD_GRACE`], so the grace period is what
/// decides how long a line waits, not this.
const TICK: Duration = Duration::from_millis(50);

pub fn run(args: &LogsArgs) -> Result<()> {
    let available = fetch_resources()?;
    let branch_managed = client::branch_managed_resources().with_context(tilt_unreachable)?;
    let targets = resolve_targets(args, &available, &branch_managed)?;

    let dir = tempfile::Builder::new()
        .prefix("repos-logs-")
        .tempdir()
        .context("creating a temp dir for per-resource logs")?;

    // Their own subdirectory, so the lnav config beside them can't be taken for a
    // log and the format's file-pattern has something specific to match.
    let logs_dir = dir.path().join("logs");
    fs::create_dir(&logs_dir).with_context(|| format!("creating {}", logs_dir.display()))?;
    let lnav_config = install_lnav_format(dir.path())?;

    // Pre-create a file per resource so lnav lists them all from the start; the
    // demux thread fills them in as lines arrive.
    let mut paths = Vec::new();
    for res in &targets {
        let path = logs_dir.join(res);
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

    let demux = std::thread::spawn(move || demux(stdout, &logs_dir));

    let mut lnav = match Command::new("lnav")
        .arg("-I")
        .arg(&lnav_config)
        .args(&paths)
        .spawn()
    {
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

/// Writes each `tilt logs --json` line into its resource's file as a stamped JSON
/// record, opening files on first sighting. Ends when the follower's stdout closes
/// (i.e. after the tilt child is killed).
///
/// Reading and the timestamp grace period run on separate threads so a resource
/// whose first lines carry no timestamp still reaches lnav promptly, even while
/// the stream is quiet.
fn demux(stdout: ChildStdout, dir: &Path) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let reader = spawn_reader(stdout, tx.clone());
    let ticker = spawn_ticker(tx);

    let pumped = pump(rx, dir);
    let read = reader
        .join()
        .unwrap_or_else(|_| Err(anyhow!("log reader thread panicked")));
    // The ticker stops on its next send, once `pump` has dropped the receiver.
    let _ = ticker.join();
    pumped.and(read)
}

/// `Tick` carries nothing: it exists so the grace period can expire while the
/// stream is quiet.
enum Event {
    Line(String),
    Tick,
    Eof,
}

/// Reads the follower's stdout until it closes. A line that isn't valid UTF-8 is
/// dropped and reading continues — losing one line beats ending the tail — but any
/// other read error stops it and is reported, rather than looking like the stream
/// simply ended.
fn spawn_reader(stdout: ChildStdout, tx: Sender<Event>) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || {
        let mut mangled = 0u64;
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(Event::Line(line)).is_err() {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => mangled += 1,
                Err(e) => {
                    let _ = tx.send(Event::Eof);
                    return Err(anyhow::Error::new(e).context("reading `tilt logs` output"));
                }
            }
        }
        let _ = tx.send(Event::Eof);
        if mangled > 0 {
            eprintln!("repos logs: dropped {mangled} log line(s) that weren't valid UTF-8");
        }
        Ok(())
    })
}

fn spawn_ticker(tx: Sender<Event>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while tx.send(Event::Tick).is_ok() {
            std::thread::sleep(TICK);
        }
    })
}

fn pump(rx: Receiver<Event>, dir: &Path) -> Result<()> {
    let mut sink = Sink::new(dir.to_path_buf());
    let mut stamper = Stamper::new();
    let mut unusable = 0u64;

    for event in &rx {
        match event {
            Event::Line(line) => {
                let entry = match LogLine::parse(&line) {
                    Ok(entry) => entry,
                    Err(Dropped::NotAResource) => continue,
                    Err(Dropped::Unusable) => {
                        unusable += 1;
                        continue;
                    }
                };
                let now = Instant::now();
                for physical in physical_lines(&entry.message) {
                    let ready =
                        stamper.push(&entry.resource, physical.to_string(), entry.time, now);
                    sink.write(&entry.resource, &ready)?;
                }
            }
            Event::Tick => {
                for (resource, ready) in stamper.tick(Instant::now()) {
                    sink.write(&resource, &ready)?;
                }
            }
            Event::Eof => break,
        }
    }

    for (resource, ready) in stamper.drain() {
        sink.write(&resource, &ready)?;
    }

    // Not worth failing the tail over, but not worth hiding either.
    if unusable > 0 {
        eprintln!("repos logs: skipped {unusable} log line(s) Tilt gave no usable time for");
    }
    Ok(())
}

/// One line of `tilt logs --json`. Only the fields we use are kept.
#[derive(Deserialize, Debug)]
struct LogLine {
    #[serde(default)]
    resource: String,
    #[serde(default)]
    message: String,
    /// When Tilt handed us the line — second resolution, and attach time for
    /// replayed history — so it is a fallback, but it does carry the machine's offset.
    time: DateTime<FixedOffset>,
}

/// Why a line from the follower produced nothing to write.
#[derive(Debug, PartialEq, Eq)]
enum Dropped {
    /// Tilt talking about itself — banner, version — so there is no file to put it
    /// in. Every attach emits a few, so this must stay quiet.
    NotAResource,
    /// Neither placeable nor datable, so worth mentioning to the user.
    Unusable,
}

impl LogLine {
    fn parse(line: &str) -> Result<LogLine, Dropped> {
        let entry: LogLine = serde_json::from_str(line).map_err(|_| Dropped::Unusable)?;
        if entry.resource.is_empty() {
            return Err(Dropped::NotAResource);
        }
        Ok(entry)
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
    client::uiresources().with_context(tilt_unreachable)
}

/// Reaching the wrong Tilt looks exactly like reaching none — the port tells them apart.
fn tilt_unreachable() -> String {
    match client::apiserver_port() {
        Some(port) => format!("couldn't reach Tilt on port {port} — is it running?"),
        None => "couldn't reach Tilt — is `tilt up` running? Pass `--port` if it \
                 serves on one other than 10350."
            .to_string(),
    }
}

/// The resources to tail: those named, or every repo-backed resource when
/// none are. Infra and setup tasks (redis, gradle-wrappers, ...) are off by
/// default, but still tailable when named explicitly.
fn resolve_targets(
    args: &LogsArgs,
    available: &[client::Resource],
    branch_managed: &[String],
) -> Result<Vec<String>> {
    if args.resources.is_empty() {
        let targets = default_targets(available, branch_managed);
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

/// The default tail set: currently-existing resources the daemon manages a
/// branch-picker button for — i.e. repos, not infra or setup tasks.
fn default_targets(available: &[client::Resource], branch_managed: &[String]) -> Vec<String> {
    available
        .iter()
        .filter(|r| branch_managed.contains(&r.name))
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

    fn resources(names: &[&str]) -> Vec<client::Resource> {
        names
            .iter()
            .map(|name| client::Resource {
                name: name.to_string(),
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
    fn resolve_targets_defaults_to_branch_managed_resources_only() {
        let available = resources(&["redis", "gradle-wrappers", "auth-service"]);
        let branch_managed = owned(&["auth-service"]);
        let targets =
            resolve_targets(&logs_args(false, None, &[]), &available, &branch_managed).unwrap();
        assert_eq!(targets, ["auth-service"]);
    }

    #[test]
    fn resolve_targets_allows_naming_an_infra_resource() {
        let available = resources(&["redis", "auth-service"]);
        let branch_managed = owned(&["auth-service"]);
        let targets = resolve_targets(
            &logs_args(false, None, &["redis"]),
            &available,
            &branch_managed,
        )
        .unwrap();
        assert_eq!(targets, ["redis"]);
    }

    #[test]
    fn resolve_targets_errors_on_unknown_resource() {
        let available = resources(&["auth-service"]);
        let branch_managed = owned(&["auth-service"]);
        assert!(
            resolve_targets(
                &logs_args(false, None, &["typo"]),
                &available,
                &branch_managed
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_targets_errors_when_nothing_is_branch_managed() {
        let available = resources(&["gradle-wrappers"]);
        assert!(resolve_targets(&logs_args(false, None, &[]), &available, &[]).is_err());
    }

    #[test]
    fn parses_resource_and_message_from_a_json_line() {
        let line = r#"{"time":"2026-07-20T16:44:04-05:00","resource":"redis","level":"info","message":"hi","source":"runtime"}"#;
        let entry = LogLine::parse(line).unwrap();
        assert_eq!(entry.resource, "redis");
        assert_eq!(entry.message, "hi");
    }

    #[test]
    fn treats_tilts_own_output_as_belonging_to_no_resource() {
        // Counting these as unplaceable ended every run with a spurious warning.
        let banner = r#"{"time":"2026-07-20T16:44:04-05:00","resource":"","message":"Tilt started on http://localhost:10351/"}"#;
        assert_eq!(LogLine::parse(banner).unwrap_err(), Dropped::NotAResource);
    }

    #[test]
    fn reports_a_line_it_can_place_no_other_way() {
        assert_eq!(LogLine::parse("not json").unwrap_err(), Dropped::Unusable);
        assert_eq!(
            LogLine::parse(r#"{"resource":"redis","message":"hi"}"#).unwrap_err(),
            Dropped::Unusable,
            "with no time from Tilt there is nothing left to order the line by"
        );
    }
}
