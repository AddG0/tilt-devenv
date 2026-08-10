//! `repos-tiltd` — the live Tilt daemon for the `repos` multi-repo dev
//! environment. The Tiltfile runs it as a long-lived `serve_cmd`; it puts a
//! branch picker, pull button, and worktree picker on each mapped resource plus
//! a global checkout-all, handles their clicks in-process, and reprints the
//! `git-status` table when a repo's git state changes. It also watches the
//! dev-env repo itself and offers a nav update button when it falls behind.
//!
//! It reads its resource↔repo mapping from `REPOS_TILT_SPEC` (set by the
//! Tiltfile). The user-facing commands live in the sibling `repos` binary; the
//! shared domain + Tilt client live in `repos-core`.

mod buttons;
mod daemon;
mod debounce;
mod updater;

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "repos-tiltd",
    about = "Live Tilt daemon for the repos dev environment",
    version
)]
struct Cli {
    /// How often to fetch remotes and refresh ahead/behind counts
    #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
    poll: Duration,

    /// Don't fetch the dev-env repo itself or offer the nav update button
    #[arg(long)]
    no_self_update: bool,
}

fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();
    match daemon::run(cli.poll, !cli.no_self_update) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("repos-tiltd: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Sends diagnostics to stderr at `info` by default, overridable via `RUST_LOG`.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_watch_the_dev_env_for_updates_by_default() {
        assert!(!Cli::parse_from(["repos-tiltd"]).no_self_update);
    }

    #[test]
    fn should_accept_the_opt_out_the_tiltfile_passes() {
        // tilt/Tiltfile appends this flag for `repos_status_ui(self_update=False)`.
        assert!(Cli::parse_from(["repos-tiltd", "--no-self-update"]).no_self_update);
    }
}
