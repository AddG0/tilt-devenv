//! One module per subcommand. Each is a thin application layer: parse args →
//! call the `repos-core` library → render the result.

pub mod checkout;
pub mod list;
pub mod logs;
pub mod pull;
pub mod status;
pub mod worktree;
