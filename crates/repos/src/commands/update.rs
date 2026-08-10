//! `repos update` — fast-forward the dev environment itself, from a terminal.
//!
//! The same operation the daemon's nav button performs, minus the restart:
//! restarting is Tilt's business, so this reports what's left to do instead.

use anyhow::{Result, anyhow};
use repos_core::registry;
use repos_core::selfupdate::DevEnv;

pub fn run() -> Result<()> {
    let root = registry::find_root()?;
    let dev = DevEnv::at(&root).ok_or_else(|| {
        anyhow!(
            "the dev environment at {} isn't a git repo, so there's nothing to update",
            root.display()
        )
    })?;

    dev.fetch();
    let behind = dev.behind();
    if behind == 0 {
        println!("Dev environment is up to date.");
        return Ok(());
    }

    dev.pull()?;
    let plural = if behind == 1 { "" } else { "s" };
    println!("Updated the dev environment ({behind} commit{plural}).");
    if dev.has_dev_shell() {
        println!("Restart Tilt to pick up the new dev shell.");
    }
    Ok(())
}
