use anyhow::{Context, Result};

use crate::cli::{WorktreeArgs, WorktreeCmd};

pub fn run(args: &WorktreeArgs) -> Result<()> {
    match args.cmd {
        WorktreeCmd::StatePath => {
            let path = repos_core::worktree::state_path()
                .context("no XDG state or data directory available")?;
            // Ensure it exists so the Tiltfile can watch_file it before any pick.
            repos_core::worktree::ensure_exists(&path)?;
            println!("{}", path.display());
            Ok(())
        }
    }
}
