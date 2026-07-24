use anyhow::Result;
use repos_core::devenv::Workspace;

use crate::cli::StatusArgs;
use crate::output::{json, terminal};

pub fn run(args: &StatusArgs) -> Result<()> {
    let ws = Workspace::load()?;
    let statuses = ws.filter(&[], &args.group).status_all(args.fetch);
    if args.json {
        json::print_status_json(&statuses)
    } else {
        terminal::print_status_table(&statuses);
        Ok(())
    }
}
