//! The developer's active worktree selection per repo, persisted so switching a
//! resource's worktree survives a Tiltfile reload.
//!
//! Stored as XDG **state** (not config or cache): it's per-developer runtime
//! state that changes as you work, not something you hand-edit and not a cache.
//! A single file keyed by dev-env root, so distinct environments never collide.
//! Functions take the state-file path explicitly so they're testable without
//! touching a real home directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The default state file: `$XDG_STATE_HOME/repos/worktrees.json`, falling back
/// to the data dir on platforms without a state dir (macOS/Windows). `None` when
/// neither is resolvable.
pub fn state_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_dir)?;
    Some(base.join("repos").join("worktrees.json"))
}

/// Ensures the state file (and its parent dir) exists, writing an empty
/// selection map if absent — so the Tiltfile can `watch_file` it before the
/// first pick is ever made.
pub fn ensure_exists(file: &Path) -> Result<()> {
    if file.exists() {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(file, b"{}\n").with_context(|| format!("creating {}", file.display()))
}

/// dev-env root -> (repo name -> selected worktree path).
type State = HashMap<String, HashMap<String, String>>;

fn load(file: &Path) -> State {
    std::fs::read(file)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// The active worktree selections for the dev-env at `root` (repo name ->
/// worktree path), read from `file`. Empty when the file is absent or has no
/// entry for this root.
pub fn selections(file: &Path, root: &Path) -> HashMap<String, String> {
    load(file).remove(&key(root)).unwrap_or_default()
}

/// Sets (`Some`) or clears (`None`) the active worktree for `repo` under `root`
/// in `file`. Clearing falls the repo back to its normal path resolution (its
/// primary checkout). Creates the file (and parent dir) if needed.
pub fn set_selection(file: &Path, root: &Path, repo: &str, worktree: Option<&str>) -> Result<()> {
    let mut state = load(file);
    let entry = state.entry(key(root)).or_default();
    match worktree {
        Some(w) => {
            entry.insert(repo.to_string(), w.to_string());
        }
        None => {
            entry.remove(repo);
        }
    }
    // Don't leave an empty map behind for a root once its last selection clears.
    if entry.is_empty() {
        state.remove(&key(root));
    }
    save(file, &state)
}

fn save(file: &Path, state: &State) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let json = serde_json::to_vec_pretty(state)?;
    std::fs::write(file, json).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn set_get_and_clear_roundtrip() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("worktrees.json");
        let root = Path::new("/dev/env");

        assert!(selections(&file, root).is_empty(), "absent file → empty");

        set_selection(&file, root, "auth", Some("/wt/auth/feat-login")).unwrap();
        set_selection(&file, root, "web", Some("/wt/web/feat-login")).unwrap();
        let got = selections(&file, root);
        assert_eq!(got["auth"], "/wt/auth/feat-login");
        assert_eq!(got["web"], "/wt/web/feat-login");

        set_selection(&file, root, "auth", None).unwrap();
        let got = selections(&file, root);
        assert!(!got.contains_key("auth"), "cleared selection is gone");
        assert_eq!(got["web"], "/wt/web/feat-login", "others untouched");
    }

    #[test]
    fn selections_are_isolated_per_root() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("worktrees.json");

        set_selection(&file, Path::new("/env/a"), "app", Some("/wt/a")).unwrap();
        set_selection(&file, Path::new("/env/b"), "app", Some("/wt/b")).unwrap();

        assert_eq!(selections(&file, Path::new("/env/a"))["app"], "/wt/a");
        assert_eq!(selections(&file, Path::new("/env/b"))["app"], "/wt/b");
    }

    #[test]
    fn ensure_exists_creates_an_empty_map_then_leaves_it_alone() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nested/worktrees.json");

        ensure_exists(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "{}");

        // A real selection then survives a second ensure_exists (no clobber).
        set_selection(&file, Path::new("/env"), "app", Some("/wt")).unwrap();
        ensure_exists(&file).unwrap();
        assert_eq!(selections(&file, Path::new("/env"))["app"], "/wt");
    }

    #[test]
    fn clearing_the_last_selection_prunes_the_root() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("worktrees.json");
        let root = Path::new("/dev/env");

        set_selection(&file, root, "only", Some("/wt/only")).unwrap();
        set_selection(&file, root, "only", None).unwrap();

        let raw = std::fs::read_to_string(&file).unwrap();
        assert_eq!(raw.trim(), "{}", "root pruned once empty");
    }
}
