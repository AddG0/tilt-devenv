use anyhow::Result;
use repos_core::devenv::Workspace;
use repos_core::registry::Registry;

use crate::cli::StatusArgs;
use crate::output::{json, terminal};

pub fn run(args: &StatusArgs) -> Result<()> {
    let reg = Registry::load()?;
    let names = reg.resolve_only(&[], &args.profile);
    let ws = Workspace::from_registry(&reg);
    let statuses = ws.filter(&names, &args.group).status_all(args.fetch);
    if args.json {
        json::print_status_json(&statuses)
    } else {
        terminal::print_status_table(&statuses);
        Ok(())
    }
}
