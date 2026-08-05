use anyhow::Result;
use repos_core::devenv::Workspace;
use repos_core::registry::Registry;

use crate::cli::PullArgs;
use crate::output::terminal;

pub fn run(args: &PullArgs) -> Result<()> {
    let reg = Registry::load()?;
    let (names, groups) = reg.scoped(&args.only, &args.group, &args.profile, args.all)?;
    let ws = Workspace::from_registry(&reg);
    let w = ws.filter(&names, &groups);
    if !reg.is_unscoped_clone(&names, &groups, args.all) {
        crate::commands::clone_missing_and_report(&w);
    }
    terminal::print_pull_results(&w.pull_all());
    Ok(())
}
