//! Where per-developer runtime state lives: the worktree and profile
//! selections, and each supervisor's restart marker.
//!
//! `$XDG_STATE_HOME` is resolved by hand, on every platform: the `dirs` crate
//! honours it on Linux only, so anywhere else a test or a sandboxed build that
//! redirects state through the environment would land in the real home
//! directory instead.

use std::ffi::OsString;
use std::path::PathBuf;

/// This tool's state directory. `None` when neither `$XDG_STATE_HOME` nor a
/// platform state/data dir resolves.
pub fn dir() -> Option<PathBuf> {
    base(std::env::var_os("XDG_STATE_HOME")).map(|base| base.join("repos"))
}

/// The state root, given the raw `$XDG_STATE_HOME`. Split out from [`dir`] so
/// the override rules are testable without mutating the process environment.
fn base(xdg_state_home: Option<OsString>) -> Option<PathBuf> {
    xdg_state_home
        .map(PathBuf::from)
        // A relative path resolves against each process's cwd, so the CLI and
        // the daemon would disagree about where state lives.
        .filter(|p| p.is_absolute())
        .or_else(dirs::state_dir)
        .or_else(dirs::data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_state_home_wins_over_the_platform_directory() {
        let dir = std::env::temp_dir().join("repos-state-test");
        assert_eq!(base(Some(dir.clone().into_os_string())), Some(dir));
    }

    #[test]
    fn a_relative_xdg_state_home_is_ignored() {
        let relative = PathBuf::from("relative/state");
        assert_ne!(
            base(Some(relative.clone().into_os_string())),
            Some(relative)
        );
    }

    #[test]
    fn an_empty_xdg_state_home_is_ignored() {
        assert_eq!(base(Some(OsString::new())), base(None));
    }

    #[test]
    fn the_state_directory_is_namespaced_to_this_tool() {
        let dir = dir().expect("a state or data dir on every supported platform");
        assert_eq!(dir.file_name().unwrap(), "repos");
    }
}
