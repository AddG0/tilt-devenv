use anyhow::Result;
use repos_core::devenv::Workspace;
use repos_core::registry::Registry;

use crate::cli::PullArgs;
use crate::output::terminal;

pub fn run(args: &PullArgs) -> Result<()> {
    let reg = Registry::load()?;
    let names = reg.resolve_only(&args.only, &args.profile);
    let ws = Workspace::from_registry(&reg);
    terminal::print_pull_results(&ws.filter(&names, &args.group).pull_all());
    Ok(())
}
