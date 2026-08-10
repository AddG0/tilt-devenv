use anyhow::{Context, Result, anyhow};
use repos_core::registry::Registry;
use repos_core::worktree::{self, Selected};

use crate::cli::{WorktreeArgs, WorktreeCmd};

pub fn run(args: &WorktreeArgs) -> Result<()> {
    let path = worktree::state_path().context("no XDG state or data directory available")?;
    match &args.cmd {
        WorktreeCmd::StatePath => {
            // Ensure it exists so the Tiltfile can watch_file it before any pick.
            worktree::ensure_exists(&path)?;
            println!("{}", path.display());
            Ok(())
        }
        WorktreeCmd::Use { repo, branch } => {
            let reg = Registry::load()?;
            let resolved = reg
                .resolve()
                .into_iter()
                .find(|r| r.repo.name == *repo)
                .ok_or_else(|| anyhow!("no repo named {repo} in tilt-devenv.json"))?;

            match worktree::select(&path, &reg.root, repo, &resolved.path, branch)? {
                Selected::Worktree => println!("{repo} now follows the {branch} worktree."),
                Selected::MainCheckout => println!("{repo} is back on its main checkout."),
            }
            println!("Reload Tilt, if it's running, to restart the resource there.");
            Ok(())
        }
    }
}
