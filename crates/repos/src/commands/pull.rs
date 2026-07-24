use anyhow::Result;
use repos_core::devenv::Workspace;

use crate::cli::PullArgs;
use crate::output::terminal;

pub fn run(args: &PullArgs) -> Result<()> {
    let ws = Workspace::load()?;
    terminal::print_pull_results(&ws.filter(&args.only, &args.group).pull_all());
    Ok(())
}
