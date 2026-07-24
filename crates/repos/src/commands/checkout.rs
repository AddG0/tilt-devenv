use anyhow::{Context, Result};
use repos_core::devenv::{CheckoutTarget, Workspace};

use crate::cli::CheckoutArgs;
use crate::output::terminal;

pub fn run(args: &CheckoutArgs) -> Result<()> {
    let target = CheckoutTarget::parse(&args.branch)
        .with_context(|| format!("invalid branch argument {:?}", args.branch))?;
    let ws = Workspace::load()?;
    let w = ws.filter(&args.only, &args.group);
    let results = if args.dry_run {
        w.plan_checkout_all(&target)
    } else {
        if args.fetch {
            // Fetch first so a freshly-pushed branch is found.
            w.fetch_all();
        }
        w.checkout_all(&target)
    };
    terminal::print_checkout_results(&results, args.dry_run);
    Ok(())
}
