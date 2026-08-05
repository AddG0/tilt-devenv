use anyhow::{Result, bail};
use repos_core::devenv::{Snapshot, Workspace};
use repos_core::registry::Registry;

use crate::cli::StatusArgs;
use crate::output::{json, terminal};

pub fn run(args: &StatusArgs) -> Result<()> {
    if args.watch {
        if args.json {
            bail!("--watch doesn't support --json (it reprints a table, not a JSON stream)");
        }
        return watch(args);
    }
    let statuses = statuses_for(args, args.fetch, !args.json)?;
    if args.json {
        json::print_status_json(&statuses)
    } else {
        terminal::print_status_table(&statuses);
        Ok(())
    }
}

/// Enough blank lines to scroll a previous table out of view in a normal
/// terminal or Tilt's log pane — a real clear-screen escape doesn't survive
/// Tilt's log capture, but plain newlines work everywhere.
const BLANK_LINES: usize = 60;

/// Reprints the table only when it changes, checking every `args.interval`
/// until killed — printing on every tick regardless of change would flood
/// the log with noise. Never fetches; pair with something else that does
/// (e.g. `repos-tiltd`'s poll) rather than having two things fetch the same
/// repos on independent schedules.
fn watch(args: &StatusArgs) -> Result<()> {
    let mut last: Option<Vec<Snapshot>> = None;
    let mut warned = false;
    loop {
        let statuses = statuses_for(args, false, !warned)?;
        warned = true;
        if last.as_ref() != Some(&statuses) {
            print!("{}", "\n".repeat(BLANK_LINES));
            terminal::print_status_table(&statuses);
            last = Some(statuses);
        }
        std::thread::sleep(args.interval);
    }
}

/// `warn` prints the no-active-profile note to stderr when unscoped — off for
/// JSON output and after `watch`'s first tick, so a long-lived poll doesn't
/// repeat it forever.
fn statuses_for(args: &StatusArgs, fetch: bool, warn: bool) -> Result<Vec<Snapshot>> {
    let reg = Registry::load()?;
    let (names, groups) = reg.scoped(&[], &args.group, &args.profile, args.all)?;
    if reg.is_unscoped(&names, &groups, args.all) {
        if warn {
            eprintln!(
                "repos: no active profile selected; nothing to show. Run `repos profile set <name>` first, or pass --all for the whole registry."
            );
        }
        return Ok(Vec::new());
    }
    let ws = Workspace::from_registry(&reg);
    Ok(ws.filter(&names, &groups).status_all(fetch))
}
