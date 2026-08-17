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
    let pid = match crate::tilt::ancestor_pid() {
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

    #[test]
    fn marker_path_is_this_process_alone() {
        let path = marker_path();
        assert!(path.ends_with(format!("restart-{}", std::process::id())));
    }
}
