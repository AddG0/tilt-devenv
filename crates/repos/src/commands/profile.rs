use anyhow::{Context, Result};
use repos_core::registry;

use crate::cli::{ProfileArgs, ProfileCmd};

pub fn run(args: &ProfileArgs) -> Result<()> {
    let path =
        repos_core::profile::state_path().context("no XDG state or data directory available")?;
    match &args.cmd {
        ProfileCmd::StatePath => {
            // Ensure it exists so the Tiltfile can watch_file it before any pick.
            repos_core::profile::ensure_exists(&path)?;
            println!("{}", path.display());
            Ok(())
        }
        ProfileCmd::Active { json } => {
            let active = repos_core::profile::active(&path, &registry::find_root()?);
            if *json {
                println!("{}", serde_json::to_string(&active)?);
            } else {
                for name in &active {
                    println!("{name}");
                }
            }
            Ok(())
        }
        ProfileCmd::Set { profiles } => {
            repos_core::profile::set_active(&path, &registry::find_root()?, profiles)
        }
    }
}
