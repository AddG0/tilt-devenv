use anyhow::Result;
use repos_core::devenv::Workspace;
use repos_core::registry::Registry;

use crate::cli::PullArgs;
use crate::output::terminal;

pub fn run(args: &PullArgs) -> Result<()> {
    let reg = Registry::load()?;
    let (names, groups) = reg.scoped(&args.only, &args.group, &args.profile, args.all)?;
    if reg.is_unscoped(&names, &groups, args.all) {
        eprintln!(
            "repos: no active profile selected; nothing to pull. Run `repos profile set <name>` first, or pass --all for the whole registry."
        );
        return Ok(());
    }
    let ws = Workspace::from_registry(&reg);
    let w = ws.filter(&names, &groups);
    crate::commands::clone_missing_and_report(&w);
    terminal::print_pull_results(&w.pull_all());
    Ok(())
}
