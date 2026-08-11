//! The restart contract between `repos up` (the supervisor, which runs `tilt
//! up` in a loop) and `repos-tiltd` (which asks for a restart after updating
//! the dev-env repo itself).
//!
//! Restarting — rather than reloading — is the point: an update can move
//! `flake.nix` or `.envrc`, and nothing already running picks that up. Only a
//! `tilt up` started *after* the pull re-enters the new dev shell.
//!
//! The whole contract is one environment variable naming a marker file. The
//! supervisor exports it and Tilt passes it down to every resource it spawns,
//! so the daemon can tell whether a restart is even possible: unset means a
//! bare `tilt up`, where the most an update can do is pull and say so.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow};

/// Names the restart-marker file. Set by [`repos up`](marker_path), read by
/// [`marker`]; absent when Tilt wasn't started by a supervisor.
pub const MARKER_ENV: &str = "REPOS_RESTART_MARKER";

/// How far up the process tree to look for `tilt`. Far past the real depth
/// (tilt -> shell -> daemon), but bounded so a cycle or an odd `ps` reply
/// can't spin.
const MAX_ANCESTRY_DEPTH: usize = 64;

/// The marker path the supervisor exported, or `None` when this process isn't
/// running under one.
pub fn marker() -> Option<PathBuf> {
    std::env::var_os(MARKER_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where this supervisor keeps its restart marker: the
/// [state dir](crate::state::dir), alongside the worktree and profile state.
///
/// Keyed by process id, so any number of supervisors can run at once — several
/// dev environments, or the same one twice — without reading each other's
/// requests. The daemon never derives this; it reads the path from its env.
///
/// Falls back to the temp dir rather than failing: `repos up` is still worth
/// running where a restart could never work.
pub fn marker_path() -> PathBuf {
    crate::state::dir()
        .unwrap_or_else(|| std::env::temp_dir().join("repos"))
        .join(format!("restart-{}", std::process::id()))
}

/// Asks the supervisor to restart Tilt: drops the marker it checks after each
/// run, then terminates the `tilt` process this one is a descendant of.
///
/// Errors (leaving no marker behind) when there's no supervisor or no `tilt`
/// ancestor to stop — the caller reports that as "restart it yourself", never
/// as a half-applied update.
pub fn request_restart() -> Result<()> {
    let marker =
        marker().ok_or_else(|| anyhow!("not running under `repos up` ({MARKER_ENV} unset)"))?;
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::File::create(&marker).with_context(|| format!("creating {}", marker.display()))?;

    // Find tilt before killing anything, and undo the marker if we can't: a
    // marker with no restart would fire on the next ordinary Ctrl-C instead.
    let pid = match tilt_ancestor(std::process::id(), ps_parent) {
        Some(pid) => pid,
        None => {
            let _ = std::fs::remove_file(&marker);
            return Err(anyhow!(
                "no `tilt` process found among this process's ancestors"
            ));
        }
    };
    terminate(pid)
}

/// Walks up from `pid` through `parent` until it finds a process named `tilt`,
/// returning that pid. Ancestry rather than a pidfile: the daemon is spawned by
/// Tilt itself, so the tree already records the relationship and there's no
/// stale file to clean up.
fn tilt_ancestor(pid: u32, parent: impl Fn(u32) -> Option<(u32, String)>) -> Option<u32> {
    let mut pid = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        let (ppid, comm) = parent(pid)?;
        if comm == "tilt" {
            return Some(pid);
        }
        if ppid == 0 || ppid == pid {
            return None;
        }
        pid = ppid;
    }
    None
}

/// `pid`'s parent pid and command name, via `ps` — portable across Linux and
/// macOS, unlike reading `/proc`.
fn ps_parent(pid: u32) -> Option<(u32, String)> {
    let out = Command::new("ps")
        .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps(&String::from_utf8_lossy(&out.stdout))
}

/// Parses one `ps -o ppid=,comm=` line into (parent pid, command name). The
/// command can itself be a path (`/nix/store/…/bin/tilt` on macOS), so it's
/// reduced to its basename.
fn parse_ps(out: &str) -> Option<(u32, String)> {
    let line = out.lines().next()?.trim();
    let (ppid, comm) = line.split_once(char::is_whitespace)?;
    let comm = comm.trim();
    let comm = comm.rsplit('/').next().unwrap_or(comm);
    Some((ppid.trim().parse().ok()?, comm.to_string()))
}

/// Sends SIGTERM to `pid` by shelling out to `kill`, so no libc dependency is
/// needed for the one signal this tool sends.
fn terminate(pid: u32) -> Result<()> {
    let out = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .with_context(|| format!("running `kill -TERM {pid}`"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "kill -TERM {pid}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake process tree: pid -> (parent pid, command name).
    fn tree(entries: &[(u32, u32, &str)]) -> impl Fn(u32) -> Option<(u32, String)> + use<> {
        let map: HashMap<u32, (u32, String)> = entries
            .iter()
            .map(|(pid, ppid, comm)| (*pid, (*ppid, comm.to_string())))
            .collect();
        move |pid| map.get(&pid).cloned()
    }

    #[test]
    fn should_find_the_tilt_process_the_daemon_runs_under() {
        // The real shape: tilt spawns a shell for the serve_cmd, which runs us.
        let procs = tree(&[
            (300, 200, "repos-tiltd"),
            (200, 100, "sh"),
            (100, 1, "tilt"),
            (1, 0, "init"),
        ]);
        assert_eq!(tilt_ancestor(300, procs), Some(100));
    }

    #[test]
    fn should_return_none_when_no_ancestor_is_tilt() {
        let procs = tree(&[(300, 200, "repos-tiltd"), (200, 1, "zsh"), (1, 0, "init")]);
        assert_eq!(tilt_ancestor(300, procs), None);
    }

    #[test]
    fn should_stop_rather_than_loop_on_a_self_parenting_process() {
        let procs = tree(&[(300, 300, "weird")]);
        assert_eq!(tilt_ancestor(300, procs), None);
    }

    #[test]
    fn should_return_none_when_a_pid_vanishes_mid_walk() {
        // Processes exit while we're walking; a gap must end the walk, not panic.
        let procs = tree(&[(300, 200, "repos-tiltd")]);
        assert_eq!(tilt_ancestor(300, procs), None);
    }

    #[test]
    fn should_parse_a_ps_line_into_parent_pid_and_command() {
        assert_eq!(parse_ps("  1234 tilt\n"), Some((1234, "tilt".to_string())));
    }

    #[test]
    fn should_reduce_a_ps_command_path_to_its_basename() {
        // macOS `ps -o comm=` prints the full executable path.
        assert_eq!(
            parse_ps(" 42 /nix/store/abc-tilt-0.35/bin/tilt\n"),
            Some((42, "tilt".to_string()))
        );
    }

    #[test]
    fn should_treat_unparseable_ps_output_as_no_parent() {
        assert_eq!(parse_ps(""), None);
        assert_eq!(parse_ps("nonsense\n"), None);
    }

    #[test]
    fn marker_path_is_this_process_alone() {
        let path = marker_path();
        assert!(path.ends_with(format!("restart-{}", std::process::id())));
    }
}
