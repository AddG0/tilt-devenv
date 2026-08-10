//! `repos` manages git branches across a whole set of dev-environment repos as a
//! single unit. See `repos --help`.
//!
//! This binary is a thin frontend over the `repos-core` library: it parses the
//! CLI and renders results for the terminal. The domain and git/registry infra
//! live in `repos-core`; the live Tilt daemon is the separate `repos-tiltd`
//! binary.

mod cli;
mod commands;
mod output;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

fn main() -> ExitCode {
    // Dynamic shell completion: when COMPLETE=<shell> is set, this prints
    // completions and exits; otherwise it returns and we parse normally.
    CompleteEnv::with_factory(cli::Cli::command).complete();

    init_tracing();

    let cli = cli::Cli::parse();
    let result = match &cli.command {
        cli::Command::Up(a) => commands::up::run(a),
        cli::Command::Update => commands::update::run(),
        cli::Command::Clone(a) => commands::clone::run(a),
        cli::Command::Status(a) => commands::status::run(a),
        cli::Command::Checkout(a) => commands::checkout::run(a),
        cli::Command::Pull(a) => commands::pull::run(a),
        cli::Command::List(a) => commands::list::run(a),
        cli::Command::Profiles(a) => commands::profiles::run(a),
        cli::Command::Profile(a) => commands::profile::run(a),
        cli::Command::Logs(a) => commands::logs::run(a),
        cli::Command::Worktree(a) => commands::worktree::run(a),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The fatal error is user-facing, not a diagnostic — print it
            // unconditionally rather than routing it through the log filter.
            eprintln!("repos: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Sends diagnostics to stderr (stdout is reserved for command output like
/// `--json`), at `info` by default and overridable via `RUST_LOG`.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
