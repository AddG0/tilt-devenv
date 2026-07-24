use anyhow::Result;
use repos_core::registry::Registry;

use crate::cli::ListArgs;
use crate::output::{json, terminal};

pub fn run(args: &ListArgs) -> Result<()> {
    let reg = Registry::load()?;
    let resolved = reg.resolve();
    if args.json {
        json::print_list_json(&resolved)
    } else {
        terminal::print_list_table(&resolved);
        Ok(())
    }
}
