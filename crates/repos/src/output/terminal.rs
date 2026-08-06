//! Human-facing terminal output: the coloured per-repo status cells, the
//! ahead/behind sync indicator, the checkout/pull result lines, and the aligned
//! tables. Formats `repos_core` value objects for a person reading a terminal.

use std::collections::BTreeMap;
use std::io::IsTerminal;

use comfy_table::presets::{ASCII_FULL_CONDENSED, UTF8_FULL_CONDENSED};
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use owo_colors::{OwoColorize, Stream::Stdout};
use repos_core::devenv::{OpResult, Outcome, Snapshot, count_with_outcome};
use repos_core::registry::Resolved;

fn dim(s: &str) -> String {
    format!("{}", s.if_supports_color(Stdout, |t| t.dimmed()))
}
fn green(s: &str) -> String {
    format!("{}", s.if_supports_color(Stdout, |t| t.green()))
}
fn yellow(s: &str) -> String {
    format!("{}", s.if_supports_color(Stdout, |t| t.yellow()))
}
fn red(s: &str) -> String {
    format!("{}", s.if_supports_color(Stdout, |t| t.red()))
}
fn cyan(s: &str) -> String {
    format!("{}", s.if_supports_color(Stdout, |t| t.cyan()))
}
fn bold(s: &str) -> String {
    format!("{}", s.if_supports_color(Stdout, |t| t.bold()))
}

/// Whether stdout should carry colour, via owo-colors' own detection (honours
/// NO_COLOR, FORCE_COLOR/CLICOLOR_FORCE, and TTY). comfy-table otherwise only
/// checks for a TTY, so we drive its styling from this — matching the line
/// output and keeping colour in Tilt's (non-TTY) log pane.
fn color_enabled() -> bool {
    // owo emits escape codes only when colour is supported; sniff for one.
    format!("{}", "x".if_supports_color(Stdout, |t| t.red())).contains('\x1b')
}

/// A subtle background for alternating table rows (tuned for dark terminals and
/// Tilt's log pane) so the eye can track a row across the columns.
const ZEBRA_BG: Color = Color::AnsiValue(236);

/// The colour of a status cell. Applied by comfy-table (not owo) so a cell's
/// whole background can be striped — owo-coloured text can't be, as its resets
/// would break the stripe partway across the cell.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Style {
    Plain,
    Green,
    Yellow,
    Red,
    Dim,
}

/// Builds a comfy-table cell for a `(text, style)` value.
fn styled(text: impl Into<String>, style: Style) -> Cell {
    let cell = Cell::new(text.into());
    match style {
        Style::Plain => cell,
        Style::Green => cell.fg(Color::Green),
        Style::Yellow => cell.fg(Color::Yellow),
        Style::Red => cell.fg(Color::Red),
        Style::Dim => cell.add_attribute(Attribute::Dim),
    }
}

/// The bold group-label cell shown once per block (blank on a group's later
/// rows); ungrouped repos fall under "other".
fn group_label_cell(group: &str, first: bool) -> Cell {
    if !first {
        return Cell::new("");
    }
    Cell::new(if group.is_empty() { "other" } else { group }).add_attribute(Attribute::Bold)
}

/// Backgrounds `cell` when it falls on a striped row.
fn zebra(cell: Cell, striped: bool) -> Cell {
    if striped { cell.bg(ZEBRA_BG) } else { cell }
}

/// Column headers for the status table: a GROUP label column, then the columns
/// [`status_cells`] returns.
const STATUS_HEADERS: [&str; 5] = ["GROUP", "REPO", "BRANCH", "SYNC", "STATE"];

/// Groups items by key, preserving the order the groups first appear so the
/// output sections follow the registry's declaration order.
fn group_by<'a, T>(items: &'a [T], key: impl Fn(&'a T) -> &'a str) -> Vec<(&'a str, Vec<&'a T>)> {
    let mut groups: Vec<(&str, Vec<&T>)> = Vec::new();
    for item in items {
        let k = key(item);
        match groups.iter_mut().find(|(name, _)| *name == k) {
            Some((_, members)) => members.push(item),
            None => groups.push((k, vec![item])),
        }
    }
    groups
}

/// The REPO, BRANCH, SYNC, STATE values for one project as `(text, colour)`
/// pairs (the colour is applied by comfy-table so rows can be striped).
/// `unicode` picks marker glyphs — off for piped output (e.g. Tilt's log pane),
/// whose font renders symbols like ●/✓ at widths that break table alignment.
fn status_cells(s: &Snapshot, unicode: bool) -> Vec<(String, Style)> {
    if !s.present {
        return vec![
            (s.name.clone(), Style::Plain),
            ((if unicode { "—" } else { "-" }).to_string(), Style::Dim),
            (String::new(), Style::Plain),
            ("not cloned".to_string(), Style::Red),
        ];
    }
    if s.err.is_some() {
        return vec![
            (s.name.clone(), Style::Plain),
            ("?".to_string(), Style::Dim),
            (String::new(), Style::Plain),
            ("error".to_string(), Style::Red),
        ];
    }

    let branch = if s.detached {
        ("(detached)".to_string(), Style::Yellow)
    } else {
        (s.branch.clone(), Style::Plain)
    };
    let state = if s.dirty {
        (
            (if unicode { "● dirty" } else { "dirty" }).to_string(),
            Style::Yellow,
        )
    } else {
        ("clean".to_string(), Style::Dim)
    };

    let mut sync = sync_text(s, unicode);
    let mut sync_style = if s.upstream.is_empty() {
        Style::Dim
    } else if s.behind > 0 {
        Style::Yellow
    } else {
        Style::Green
    };
    if s.fetch_err.is_some() {
        // Fetch failed, so ahead/behind reflect the last known remote state.
        if !sync.is_empty() {
            sync.push(' ');
        }
        sync += "(stale)";
        sync_style = Style::Dim;
    }

    vec![
        (s.name.clone(), Style::Plain),
        branch,
        (sync, sync_style),
        state,
    ]
}

/// The plain-text SYNC value: ahead/behind vs upstream, in-sync, or a note when
/// there's no upstream. `unicode` picks gitsigns-style arrows (↑/↓/✓) or ASCII
/// (+/-/ok) for piped output.
fn sync_text(s: &Snapshot, unicode: bool) -> String {
    if s.detached {
        return String::new();
    }
    if s.upstream.is_empty() {
        return "no upstream".to_string();
    }
    let mut out = String::new();
    if s.ahead > 0 {
        out += &format!("{}{}", if unicode { "↑" } else { "+" }, s.ahead);
    }
    if s.behind > 0 {
        if !out.is_empty() {
            out.push(' ');
        }
        out += &format!("{}{}", if unicode { "↓" } else { "-" }, s.behind);
    }
    if out.is_empty() {
        return if unicode { "✓" } else { "ok" }.to_string();
    }
    out
}

/// Renders a single checkout result.
fn checkout_line(r: &OpResult) -> String {
    let err = r.err.as_deref().unwrap_or_default();
    match r.outcome {
        Outcome::OnBranch => green(&format!("→ {} → {}", r.name, r.branch)),
        Outcome::FellBack => cyan(&format!("↩ {} → default (branch not found)", r.name)),
        Outcome::SkippedDirty => yellow(&format!("● skip {} (dirty, on {})", r.name, r.branch)),
        Outcome::Missing => dim(&format!("· skip {} (not cloned)", r.name)),
        Outcome::Errored => red(&format!("! {}: {}", r.name, err)),
        _ => String::new(),
    }
}

/// Renders the tallied one-line summary of a checkout run.
fn checkout_summary(results: &[OpResult]) -> String {
    let n = |o| count_with_outcome(results, o);
    dim(&format!(
        "— {} on branch, {} on default, {} skipped, {} missing, {} errored",
        n(Outcome::OnBranch),
        n(Outcome::FellBack),
        n(Outcome::SkippedDirty),
        n(Outcome::Missing),
        n(Outcome::Errored),
    ))
}

/// Renders a single pull result.
fn pull_line(r: &OpResult) -> String {
    let err = r.err.as_deref().unwrap_or_default();
    match r.outcome {
        Outcome::Pulled => green(&format!("⬇ {} → {} (fast-forwarded)", r.name, r.branch)),
        Outcome::UpToDate => dim(&format!("✓ {} up to date", r.name)),
        Outcome::SkippedDirty => yellow(&format!("● skip {} (dirty, on {})", r.name, r.branch)),
        Outcome::Missing => dim(&format!("· skip {} (not cloned)", r.name)),
        Outcome::Errored => red(&format!("! {}: {}", r.name, err)),
        _ => String::new(),
    }
}

/// Renders the tallied one-line summary of a pull run.
fn pull_summary(results: &[OpResult]) -> String {
    let n = |o| count_with_outcome(results, o);
    dim(&format!(
        "— {} pulled, {} up to date, {} skipped, {} missing, {} errored",
        n(Outcome::Pulled),
        n(Outcome::UpToDate),
        n(Outcome::SkippedDirty),
        n(Outcome::Missing),
        n(Outcome::Errored),
    ))
}

/// A table with the shared repos-CLI styling: an outer border and a bold header,
/// but no rule between data rows (the "condensed" preset) so grouped rows read
/// as one block. Arrangement disabled so long paths don't wrap. `unicode` picks
/// box-drawing borders for a terminal or ASCII (`+-|`) for piped output, whose
/// font may not render box-drawing uniformly. `color` drives comfy-table's
/// styling directly — it only checks for a TTY otherwise, which would drop
/// colour in Tilt's (non-TTY) log pane.
fn new_table(headers: &[&str], unicode: bool, color: bool) -> Table {
    let mut t = Table::new();
    t.load_preset(if unicode {
        UTF8_FULL_CONDENSED
    } else {
        ASCII_FULL_CONDENSED
    })
    .set_content_arrangement(ContentArrangement::Disabled)
    .set_header(
        headers
            .iter()
            .map(|h| Cell::new(*h).add_attribute(Attribute::Bold)),
    );
    if color {
        t.enforce_styling();
    } else {
        t.force_no_tty();
    }
    t
}

/// Renders the per-repo status as a single aligned table, sectioned by group
/// (labelled once per block, with alternating row shading), plus any errors.
pub fn print_status_table(statuses: &[Snapshot]) {
    let unicode = std::io::stdout().is_terminal();
    let color = color_enabled();
    let mut t = new_table(&STATUS_HEADERS, unicode, color);
    let mut row = 0;
    for (group, snaps) in group_by(statuses, |s| s.group.as_str()) {
        for (i, s) in snaps.into_iter().enumerate() {
            let striped = row % 2 == 1;
            let mut cells = vec![group_label_cell(group, i == 0)];
            cells.extend(
                status_cells(s, unicode)
                    .into_iter()
                    .map(|(t, st)| styled(t, st)),
            );
            t.add_row(cells.into_iter().map(|c| zebra(c, striped)));
            row += 1;
        }
    }
    println!("{t}");

    for s in statuses {
        if let Some(e) = &s.err {
            println!("{}", red(&format!("! {}: {}", s.name, e)));
        }
        if let Some(e) = &s.fetch_err {
            println!(
                "{}",
                dim(&format!(
                    "~ {}: fetch failed, sync counts may be stale: {}",
                    s.name, e
                ))
            );
        }
    }
}

/// Renders the resolved repo→path table as a single table, sectioned by group
/// with alternating row shading. `no_access` (repo name -> error), when
/// given, adds an ACCESS column — omitted entirely without it, since most
/// invocations never pay for the network round trips that build it.
pub fn print_list_table(repos: &[Resolved], no_access: Option<&BTreeMap<String, String>>) {
    let unicode = std::io::stdout().is_terminal();
    let color = color_enabled();
    let mut headers = vec!["GROUP", "REPO", "PATH", "ON DISK"];
    if no_access.is_some() {
        headers.push("ACCESS");
    }
    let mut t = new_table(&headers, unicode, color);
    let mut row = 0;
    for (group, rs) in group_by(repos, |r| r.repo.group.as_str()) {
        for (i, r) in rs.into_iter().enumerate() {
            let striped = row % 2 == 1;
            let on_disk = if r.present {
                styled("yes", Style::Green)
            } else {
                styled("missing", Style::Red)
            };
            let mut cells = vec![
                group_label_cell(group, i == 0),
                Cell::new(&r.repo.name),
                Cell::new(r.path.display().to_string()),
                on_disk,
            ];
            if let Some(no_access) = no_access {
                cells.push(if no_access.contains_key(&r.repo.name) {
                    styled("no access", Style::Red)
                } else {
                    styled("ok", Style::Dim)
                });
            }
            t.add_row(cells.into_iter().map(|c| zebra(c, striped)));
            row += 1;
        }
    }
    println!("{t}");
}

/// Renders every named profile and the repos/groups it enables as a table.
pub fn print_profiles_table(profiles: &BTreeMap<String, Vec<String>>) {
    let unicode = std::io::stdout().is_terminal();
    let color = color_enabled();
    let mut t = new_table(&["PROFILE", "ENABLES"], unicode, color);
    for (name, members) in profiles {
        t.add_row([Cell::new(name), Cell::new(members.join(", "))]);
    }
    println!("{t}");
}

/// Prints one line per repo plus a tallied summary for a checkout run.
pub fn print_checkout_results(results: &[OpResult], dry_run: bool) {
    if dry_run {
        println!("{}", bold("Dry run — no repos will be changed:"));
    }
    for r in results {
        println!("{}", checkout_line(r));
    }
    println!("{}", checkout_summary(results));
}

/// Prints one line per repo plus a tallied summary for a pull run.
pub fn print_pull_results(results: &[OpResult]) {
    for r in results {
        println!("{}", pull_line(r));
    }
    println!("{}", pull_summary(results));
}

/// Renders a single clone result.
fn clone_line(r: &OpResult) -> String {
    let err = r.err.as_deref().unwrap_or_default();
    match r.outcome {
        Outcome::Cloned => green(&format!("⬇ cloned {}", r.name)),
        Outcome::AlreadyPresent => dim(&format!("✓ {} already present", r.name)),
        Outcome::AccessDenied => yellow(&format!("● no access to {}", r.name)),
        Outcome::Errored => red(&format!("! {}: {}", r.name, err)),
        _ => String::new(),
    }
}

/// Renders the tallied one-line summary of a clone run.
fn clone_summary(results: &[OpResult]) -> String {
    let n = |o| count_with_outcome(results, o);
    dim(&format!(
        "— {} cloned, {} already present, {} no access, {} errored",
        n(Outcome::Cloned),
        n(Outcome::AlreadyPresent),
        n(Outcome::AccessDenied),
        n(Outcome::Errored),
    ))
}

/// Prints one line per repo plus a tallied summary for a clone run.
pub fn print_clone_results(results: &[OpResult]) {
    for r in results {
        println!("{}", clone_line(r));
    }
    println!("{}", clone_summary(results));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot {
            present: true,
            upstream: "origin/main".into(),
            ..Default::default()
        }
    }

    #[test]
    fn sync_text_shows_ahead_and_behind() {
        let s = Snapshot {
            ahead: 2,
            behind: 1,
            ..snap()
        };
        let uni = sync_text(&s, true);
        assert!(
            uni.contains("↑2") && uni.contains("↓1"),
            "unicode sync = {uni:?}"
        );
        let ascii = sync_text(&s, false);
        assert!(
            ascii.contains("+2") && ascii.contains("-1"),
            "ascii sync = {ascii:?}"
        );
    }

    #[test]
    fn sync_text_in_sync() {
        assert!(
            sync_text(&snap(), true).contains("✓"),
            "want ✓ when in sync"
        );
        assert!(
            sync_text(&snap(), false).contains("ok"),
            "want ok when piped"
        );
    }

    #[test]
    fn sync_text_no_upstream() {
        let got = sync_text(
            &Snapshot {
                present: true,
                ..Default::default()
            },
            true,
        );
        assert!(got.contains("no upstream"), "sync = {got:?}");
    }

    #[test]
    fn status_cells_mark_stale_and_dim_on_fetch_error() {
        let (text, style) = status_cells(
            &Snapshot {
                present: true,
                branch: "main".into(),
                upstream: "origin/main".into(),
                fetch_err: Some("boom".into()),
                ..Default::default()
            },
            true,
        )
        .remove(2); // SYNC cell
        assert!(text.contains("stale"), "SYNC text = {text:?}");
        assert_eq!(style, Style::Dim, "stale SYNC should be dimmed");
    }

    #[test]
    fn status_cells_colour_state_and_use_ascii_when_piped() {
        let cells = status_cells(
            &Snapshot {
                present: true,
                branch: "main".into(),
                dirty: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(cells.len(), 4);
        // STATE is yellow when dirty, and drops the box-breaking ● glyph when piped.
        assert_eq!(cells[3].1, Style::Yellow);
        assert!(!cells[3].0.contains('●'), "STATE text = {:?}", cells[3].0);
    }

    fn result(name: &str, outcome: Outcome, branch: &str) -> OpResult {
        OpResult {
            name: name.into(),
            outcome,
            branch: branch.into(),
            err: None,
        }
    }

    #[test]
    fn checkout_line_describes_outcome() {
        let cases: [(OpResult, Vec<&str>); 4] = [
            (
                result("svc", Outcome::OnBranch, "feat"),
                vec!["svc", "feat"],
            ),
            (
                result("svc", Outcome::FellBack, "develop"),
                vec!["svc", "default", "not found"],
            ),
            (
                result("svc", Outcome::SkippedDirty, "x"),
                vec!["svc", "dirty"],
            ),
            (
                result("svc", Outcome::Missing, ""),
                vec!["svc", "not cloned"],
            ),
        ];
        for (r, wants) in cases {
            let got = checkout_line(&r);
            for w in wants {
                assert!(got.contains(w), "checkout_line = {got:?}, want {w:?}");
            }
        }
    }

    #[test]
    fn checkout_summary_tallies() {
        let got = checkout_summary(&[
            result("", Outcome::OnBranch, ""),
            result("", Outcome::OnBranch, ""),
            result("", Outcome::FellBack, ""),
            result("", Outcome::SkippedDirty, ""),
        ]);
        assert!(
            got.contains("2 on branch")
                && got.contains("1 on default")
                && got.contains("1 skipped"),
            "summary = {got:?}"
        );
    }

    #[test]
    fn pull_line_and_summary() {
        let line = pull_line(&result("svc", Outcome::Pulled, "main"));
        assert!(
            line.contains("svc") && line.contains("fast-forward"),
            "line = {line:?}"
        );
        let got = pull_summary(&[
            result("", Outcome::Pulled, ""),
            result("", Outcome::UpToDate, ""),
        ]);
        assert!(
            got.contains("1 pulled") && got.contains("1 up to date"),
            "summary = {got:?}"
        );
    }
}
