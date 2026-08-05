use anyhow::Result;
use repos_core::registry::Registry;

use crate::cli::ProfilesArgs;
use crate::output::{json, terminal};

pub fn run(args: &ProfilesArgs) -> Result<()> {
    let reg = Registry::load()?;
    if args.json {
        json::print_profiles_json(&reg.profiles)
    } else {
        terminal::print_profiles_table(&reg.profiles);
        Ok(())
    }
}
