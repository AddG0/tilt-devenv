//! One module per subcommand. Each is a thin application layer: parse args →
//! call the `repos-core` library → render the result.

pub mod checkout;
pub mod clone;
pub mod list;
pub mod logs;
pub mod profile;
pub mod profiles;
pub mod pull;
pub mod status;
pub mod worktree;

use repos_core::devenv::{Outcome, Workspace};

/// Clones anything in `ws` that's missing before `checkout`/`pull` act on it,
/// reporting the outcome only when something actually happened — an
/// already-fully-cloned run stays quiet.
pub(crate) fn clone_missing_and_report(ws: &Workspace) {
    let results = ws.clone_missing();
    if results.iter().any(|r| r.outcome != Outcome::AlreadyPresent) {
        crate::output::terminal::print_clone_results(&results);
    }
}
