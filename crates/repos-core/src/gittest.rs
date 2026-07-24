//! Helpers for building throwaway git repositories in tests. Isolates git from
//! the developer's global/system config (signing, default branch, etc.) so
//! tests are deterministic and hermetic.
//!
//! Available to other crates via the `testing` feature; repos-core's own tests get
//! it under `cfg(test)`.

use std::path::Path;
use std::process::Command;
use std::sync::Once;

use tempfile::TempDir;

static ISOLATE: Once = Once::new();

/// Points git at empty global/system config and a fixed identity for the whole
/// test process, so no user gitconfig (commit signing, default branch, …) can
/// leak in. Set exactly once (via [`Once`]) so parallel tests never race on the
/// process environment. Call before any git operation.
pub fn isolate() {
    ISOLATE.call_once(|| {
        // SAFETY: guarded by Once, so this runs on a single thread before any
        // other test spawns git; the values are process-wide constants.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
            std::env::set_var("GIT_AUTHOR_NAME", "test");
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@example.com");
            std::env::set_var("GIT_COMMITTER_NAME", "test");
            std::env::set_var("GIT_COMMITTER_EMAIL", "test@example.com");
        }
    });
}

/// Runs a git command in `dir`, panicking (failing the test) on error.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git -C {} {args:?}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git -C {} {args:?} failed:\n{}{}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Creates a new repo in a fresh temp dir with default branch `main` and one
/// initial commit. Keep the returned [`TempDir`] alive for the test's duration.
pub fn init_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    commit(dir.path(), "README.md", "hello\n", "initial commit");
    dir
}

/// Writes `file` (relative to `dir`) with `content`, stages, and commits it.
pub fn commit(dir: &Path, file: &str, content: &str, msg: &str) {
    write(dir, file, content);
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", msg]);
}

/// Creates/overwrites `file` (relative to `dir`) without committing, making the
/// working tree dirty.
pub fn write(dir: &Path, file: &str, content: &str) {
    let path = dir.join(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Creates a bare clone of `src`; the returned [`TempDir`]'s path is the bare repo.
pub fn clone_bare(src: &Path) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(
        src,
        &["clone", "--bare", ".", &dir.path().to_string_lossy()],
    );
    dir
}

/// Clones `origin` into a fresh temp dir; the returned [`TempDir`]'s path is the
/// working copy.
pub fn clone(origin: &Path) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(origin, &["clone", ".", &dir.path().to_string_lossy()]);
    dir
}
