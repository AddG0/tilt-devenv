use anyhow::{Context, Result};
use repos_core::devenv::{CheckoutTarget, Workspace};
use repos_core::registry::Registry;

use crate::cli::CheckoutArgs;
use crate::output::terminal;

pub fn run(args: &CheckoutArgs) -> Result<()> {
    let target = CheckoutTarget::parse(&args.branch)
        .with_context(|| format!("invalid branch argument {:?}", args.branch))?;
    let reg = Registry::load()?;
    let (names, groups) = reg.scoped(&args.only, &args.group, &args.profile, args.all)?;
    let ws = Workspace::from_registry(&reg);
    let w = ws.filter(&names, &groups);
    let results = if args.dry_run {
        w.plan_checkout_all(&target)
    } else {
        if !reg.is_unscoped_clone(&names, &groups, args.all) {
            crate::commands::clone_missing_and_report(&w);
        }
        if args.fetch {
            // Fetch first so a freshly-pushed branch is found.
            w.fetch_all();
        }
        w.checkout_all(&target)
    };
    terminal::print_checkout_results(&results, args.dry_run);
    Ok(())
}
