//! The developer's active profile selection, persisted so picking a profile in
//! the daemon's nav button survives a Tiltfile reload — and a `tilt up` restart,
//! unlike Tilt's own `tilt args`.
//!
//! Stored as XDG **state** (not config or cache), same reasoning as
//! [`crate::worktree`]: per-developer runtime state that changes as you work.
//! A single file keyed by dev-env root, so distinct environments never collide.
//! Functions take the state-file path explicitly so they're testable without
//! touching a real home directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The default state file: `$XDG_STATE_HOME/repos/profiles.json`, falling back
/// to the data dir on platforms without a state dir (macOS/Windows). `None` when
/// neither is resolvable.
pub fn state_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_dir)?;
    Some(base.join("repos").join("profiles.json"))
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

/// dev-env root -> the active profile names.
type State = HashMap<String, Vec<String>>;

fn load(file: &Path) -> State {
    std::fs::read(file)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// The active profile selection for the dev-env at `root`, read from `file`.
/// Empty when the file is absent or has no entry for this root — meaning every
/// profile is enabled, same as never having picked one.
pub fn active(file: &Path, root: &Path) -> Vec<String> {
    load(file).remove(&key(root)).unwrap_or_default()
}

/// Sets the active profile selection for `root` in `file` to `profiles`. An
/// empty selection clears it (every profile enabled), pruning the root's entry
/// rather than leaving a stale empty one behind. Creates the file (and parent
/// dir) if needed.
pub fn set_active(file: &Path, root: &Path, profiles: &[String]) -> Result<()> {
    let mut state = load(file);
    if profiles.is_empty() {
        state.remove(&key(root));
    } else {
        state.insert(key(root), profiles.to_vec());
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
        let file = dir.path().join("profiles.json");
        let root = Path::new("/dev/env");

        assert!(active(&file, root).is_empty(), "absent file → empty");

        set_active(
            &file,
            root,
            &["frontend".to_string(), "backend".to_string()],
        )
        .unwrap();
        assert_eq!(active(&file, root), vec!["frontend", "backend"]);

        set_active(&file, root, &[]).unwrap();
        assert!(active(&file, root).is_empty(), "cleared selection is gone");
    }

    #[test]
    fn selections_are_isolated_per_root() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("profiles.json");

        set_active(&file, Path::new("/env/a"), &["frontend".to_string()]).unwrap();
        set_active(&file, Path::new("/env/b"), &["backend".to_string()]).unwrap();

        assert_eq!(active(&file, Path::new("/env/a")), vec!["frontend"]);
        assert_eq!(active(&file, Path::new("/env/b")), vec!["backend"]);
    }

    #[test]
    fn ensure_exists_creates_an_empty_map_then_leaves_it_alone() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nested/profiles.json");

        ensure_exists(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "{}");

        // A real selection then survives a second ensure_exists (no clobber).
        set_active(&file, Path::new("/env"), &["frontend".to_string()]).unwrap();
        ensure_exists(&file).unwrap();
        assert_eq!(active(&file, Path::new("/env")), vec!["frontend"]);
    }

    #[test]
    fn clearing_the_last_selection_prunes_the_root() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("profiles.json");
        let root = Path::new("/dev/env");

        set_active(&file, root, &["frontend".to_string()]).unwrap();
        set_active(&file, root, &[]).unwrap();

        let raw = std::fs::read_to_string(&file).unwrap();
        assert_eq!(raw.trim(), "{}", "root pruned once empty");
    }
}
