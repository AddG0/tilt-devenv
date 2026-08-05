use anyhow::Result;
use repos_core::devenv::Workspace;
use repos_core::registry::Registry;

use crate::cli::CloneArgs;
use crate::output::{json, terminal};

pub fn run(args: &CloneArgs) -> Result<()> {
    let reg = Registry::load()?;
    let (names, groups) = reg.scoped(&args.only, &args.group, &args.profile, args.all)?;
    if reg.is_unscoped_clone(&names, &groups, args.all) {
        if !args.json {
            eprintln!(
                "repos: no active profile selected; nothing cloned. Run `repos profile set <name>` first, or pass --all to clone every repo."
            );
        }
        return if args.json {
            json::print_clone_json(&[])
        } else {
            Ok(())
        };
    }
    let ws = Workspace::from_registry(&reg);
    let results = ws.filter(&names, &groups).clone_missing();
    if args.json {
        json::print_clone_json(&results)
    } else {
        terminal::print_clone_results(&results);
        Ok(())
    }
}
