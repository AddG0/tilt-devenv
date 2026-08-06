use std::collections::BTreeMap;

use anyhow::Result;
use repos_core::devenv::Workspace;
use repos_core::registry::Registry;

use crate::cli::ListArgs;
use crate::output::{json, terminal};

pub fn run(args: &ListArgs) -> Result<()> {
    let reg = Registry::load()?;
    let resolved = reg.resolve();
    let no_access = if args.check_access {
        Some(
            Workspace::from_registry(&reg)
                .inaccessible()
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
        )
    } else {
        None
    };
    if args.json {
        json::print_list_json(&resolved, no_access.as_ref())
    } else {
        terminal::print_list_table(&resolved, no_access.as_ref());
        Ok(())
    }
}
