//! `repos up` — start Tilt under a supervisor that can restart it.
//!
//! Each pass launches Tilt afresh — through `nix develop` when the dev-env is a
//! flake — and the daemon's update button asks for the next pass by dropping
//! the marker this watches. [`repos_core::supervisor`] has the why.
//!
//! Without a restart request the loop ends when Tilt does, so this behaves like
//! `tilt up` for anyone who never clicks update.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use repos_core::registry;
use repos_core::supervisor;

use crate::cli::UpArgs;

/// How a pass launches Tilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    /// Through `nix develop <root>`, so a pulled flake takes effect next pass.
    NixDevelop,
    /// `tilt` straight off the current PATH.
    Direct,
}

pub fn run(args: &UpArgs) -> Result<()> {
    let root = registry::find_root()?;
    let marker = supervisor::marker_path();
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let launch = plan(&root, args.no_nix, nix_available);
    if launch == Launch::Direct && !args.no_nix && root.join("flake.nix").exists() {
        eprintln!(
            "repos: {}/flake.nix exists but `nix` isn't runnable — starting Tilt directly, so an \
             update that changes the flake won't take effect until you re-enter the dev shell.",
            root.display()
        );
    }

    loop {
        // Clear first: pids get reused, so a marker left by a killed supervisor
        // that once held this pid would restart us the moment Tilt exits.
        let _ = std::fs::remove_file(&marker);

        let mut cmd = tilt_command(launch, &root, &args.tilt_args);
        cmd.env(supervisor::MARKER_ENV, &marker);
        // Ctrl-C reaches Tilt directly and it exits non-zero — an ordinary
        // quit, so the marker rather than the exit code decides on a rerun.
        cmd.status()
            .with_context(|| format!("running `{}`", describe(launch)))?;

        if !marker.exists() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&marker);
        println!("repos: starting back up on the new version — this takes a moment.");
    }
}

/// Whether each pass re-enters the dev shell. `nix_available` is taken as a
/// function so the decision is testable without a `nix` on PATH.
fn plan(root: &Path, no_nix: bool, nix_available: impl Fn() -> bool) -> Launch {
    if no_nix || !root.join("flake.nix").exists() || !nix_available() {
        Launch::Direct
    } else {
        Launch::NixDevelop
    }
}

fn nix_available() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Builds one pass's command. The working directory is inherited, so `tilt`
/// finds the same Tiltfile it would have on its own — `root` only names the
/// flake to enter.
fn tilt_command(launch: Launch, root: &Path, tilt_args: &[String]) -> Command {
    match launch {
        Launch::NixDevelop => {
            let mut cmd = Command::new("nix");
            cmd.arg("develop")
                .arg(root)
                .args(["--command", "tilt", "up"])
                .args(tilt_args);
            cmd
        }
        Launch::Direct => {
            let mut cmd = Command::new("tilt");
            cmd.arg("up").args(tilt_args);
            cmd
        }
    }
}

/// The command line to name in the error when spawning fails.
fn describe(launch: Launch) -> &'static str {
    match launch {
        Launch::NixDevelop => "nix develop --command tilt up",
        Launch::Direct => "tilt up",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn flake_root() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("flake.nix"), "{}").unwrap();
        dir
    }

    #[test]
    fn should_re_enter_the_dev_shell_when_the_dev_env_is_a_flake() {
        let root = flake_root();
        assert_eq!(plan(root.path(), false, || true), Launch::NixDevelop);
    }

    #[test]
    fn should_run_tilt_directly_when_there_is_no_flake() {
        let root = TempDir::new().unwrap();
        assert_eq!(plan(root.path(), false, || true), Launch::Direct);
    }

    #[test]
    fn should_run_tilt_directly_when_no_nix_is_requested() {
        let root = flake_root();
        assert_eq!(plan(root.path(), true, || true), Launch::Direct);
    }

    #[test]
    fn should_fall_back_to_running_tilt_directly_when_nix_is_missing() {
        // A flake in the repo doesn't mean this machine has nix; still start
        // Tilt rather than failing outright.
        let root = flake_root();
        assert_eq!(plan(root.path(), false, || false), Launch::Direct);
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn should_pass_extra_arguments_through_to_tilt_up() {
        let cmd = tilt_command(Launch::Direct, Path::new("/env"), &["--stream".to_string()]);
        assert_eq!(cmd.get_program(), "tilt");
        assert_eq!(args_of(&cmd), ["up", "--stream"]);
    }

    #[test]
    fn should_wrap_tilt_in_nix_develop_at_the_dev_env_root() {
        let cmd = tilt_command(
            Launch::NixDevelop,
            Path::new("/env"),
            &["--stream".to_string()],
        );
        assert_eq!(cmd.get_program(), "nix");
        assert_eq!(
            args_of(&cmd),
            ["develop", "/env", "--command", "tilt", "up", "--stream"]
        );
    }
}
