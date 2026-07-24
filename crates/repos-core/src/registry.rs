//! Loads the shared `repos.json` registry and resolves each declared repo to its
//! on-disk location, mirroring the Tiltfile's path-resolution rules (active
//! worktree > per-repo override > ghq checkout > sibling directory).
//!
//! This is the base the CLI builds on: it answers "which repos exist, and where
//! do they live on this machine".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// One entry from `repos.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Repo {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub group: String,
}

/// A [`Repo`] paired with its resolved on-disk path and whether that path is a
/// git working tree right now.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub repo: Repo,
    pub path: PathBuf,
    pub present: bool,
}

/// The parsed registry plus the resolution config (workspace base and per-repo
/// overrides) discovered alongside `repos.json`.
#[derive(Debug)]
pub struct Registry {
    /// Directory containing `repos.json` (the dev-env repo root).
    pub root: PathBuf,
    pub repos: Vec<Repo>,
    /// Base dir for the sibling layout.
    workspace: PathBuf,
    /// Repo name -> explicit path override.
    overrides: HashMap<String, String>,
    /// Repo name -> the developer's active worktree git id (from XDG state),
    /// resolved against the base checkout and overlaid on top of it.
    worktrees: HashMap<String, String>,
    ghq_roots: Vec<PathBuf>,
}

impl Registry {
    /// Finds `repos.json` (searching cwd upward), parses it, and reads any
    /// `tilt_config.json` overrides next to it so the tool and Tilt agree on paths.
    pub fn load() -> Result<Registry> {
        Registry::load_from(&find_root()?)
    }

    /// Loads the registry rooted at a specific directory. Exposed for tests.
    pub fn load_from(root: &Path) -> Result<Registry> {
        let data = std::fs::read(root.join("repos.json"))
            .with_context(|| format!("reading repos.json in {}", root.display()))?;
        let repos: Vec<Repo> = serde_json::from_slice(&data).context("parsing repos.json")?;

        let mut reg = Registry {
            root: root.to_path_buf(),
            repos,
            workspace: root.to_path_buf(),
            overrides: HashMap::new(),
            worktrees: HashMap::new(),
            ghq_roots: Vec::new(),
        };
        if let Err(e) = reg.load_tilt_config(root) {
            // Non-fatal: overrides just don't apply. But warn — a silently
            // ignored malformed config is exactly the debugging trap to avoid.
            tracing::warn!("{e}");
        }
        reg.worktrees = crate::worktree::state_path()
            .map(|p| crate::worktree::selections(&p, root))
            .unwrap_or_default();
        reg.ghq_roots = ghq_roots();
        Ok(reg)
    }

    /// Reads workspace + `repo-<name>` overrides from `tilt_config.json` (the
    /// same file Tilt persists per-developer overrides in). An absent file is
    /// fine (`Ok`) — the sibling-directory default applies. A present but
    /// malformed file is an error so the caller can warn rather than silently
    /// drop the developer's overrides.
    fn load_tilt_config(&mut self, root: &Path) -> Result<()> {
        let data = match std::fs::read(root.join("tilt_config.json")) {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        let cfg: HashMap<String, serde_json::Value> =
            serde_json::from_slice(&data).map_err(|e| {
                anyhow!(
                    "ignoring malformed tilt_config.json in {}: {e}",
                    root.display()
                )
            })?;

        let home = dirs::home_dir();
        if let Some(ws) = cfg.get("workspace").and_then(|v| v.as_str())
            && !ws.is_empty()
        {
            self.workspace = expand_path(ws, root, home.as_deref());
        }
        for (k, v) in &cfg {
            if let Some(name) = k.strip_prefix("repo-")
                && let Some(s) = v.as_str()
            {
                self.overrides.insert(
                    name.to_string(),
                    expand_path(s, root, home.as_deref())
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        Ok(())
    }

    /// Computes the on-disk path for every repo. Priority matches the Tiltfile:
    /// active worktree > per-repo override > ghq checkout > sibling directory.
    pub fn resolve(&self) -> Vec<Resolved> {
        self.repos
            .iter()
            .map(|repo| {
                let path = self.path_for(repo);
                let present = crate::git::is_repo(&path);
                Resolved {
                    repo: repo.clone(),
                    path,
                    present,
                }
            })
            .collect()
    }

    fn path_for(&self, repo: &Repo) -> PathBuf {
        let base = self.base_path_for(repo);
        // Overlay the active worktree selection (a git worktree id) onto the base
        // checkout. Resolving the id follows a `git worktree move` and, when the
        // worktree is gone, resolves to nothing — so a removed/stale selection
        // falls back to the main checkout on its own.
        if let Some(id) = self.worktrees.get(&repo.name)
            && !id.is_empty()
            && let Some(wt) = crate::git::resolve_worktree(&base.join(".git"), id)
        {
            return wt;
        }
        base
    }

    /// The repo's main checkout, ignoring any worktree selection: per-repo
    /// override > ghq checkout > sibling directory.
    fn base_path_for(&self, repo: &Repo) -> PathBuf {
        if let Some(o) = self.overrides.get(&repo.name)
            && !o.is_empty()
        {
            return PathBuf::from(o);
        }
        if let Some(p) = self.ghq_path(&repo.url) {
            return p;
        }
        self.workspace.join(&repo.name)
    }

    /// Maps a git URL to its ghq checkout path (via [`ghq_relpath`]) if one
    /// exists on disk.
    fn ghq_path(&self, url: &str) -> Option<PathBuf> {
        let rel = ghq_relpath(url)?;
        self.ghq_roots
            .iter()
            .map(|root| root.join(&rel))
            .find(|p| p.exists())
    }
}

/// The ghq `<host>/<path>` layout for a git remote — e.g.
/// `git@gitlab.com:acme/Bar.git` -> `gitlab.com/acme/Bar`. Handles the scp-SSH
/// (`[user@]host:path`), `ssh://[user@]host[:port]/path`, and `scheme://host/path`
/// forms, dropping any userinfo, port, and `.git` suffix. `None` for a string
/// that isn't one of those shapes.
fn ghq_relpath(url: &str) -> Option<String> {
    let (authority, path) = match url.split_once("://") {
        Some((_scheme, rest)) => rest.split_once('/')?, // scheme://[user@]host[:port]/path
        None => url.split_once(':')?,                   // scp-like: [user@]host:path
    };
    // Reduce the authority to the bare host: drop any `user@` and `:port`.
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let path = path.strip_suffix(".git").unwrap_or(path);
    Some(format!("{host}/{path}"))
}

/// The dev-environment root: walks up from the working directory for
/// `repos.json`, or `REPOS_ROOT` if set. The directory tools resolve paths
/// (and per-developer state) against.
pub fn find_root() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("REPOS_ROOT")
        && !env.is_empty()
    {
        return Ok(PathBuf::from(env));
    }
    let start = std::env::current_dir()?;
    start
        .ancestors()
        .find(|dir| dir.join("repos.json").exists())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow!(
                "repos.json not found in {} or any parent (set REPOS_ROOT to override)",
                start.display()
            )
        })
}

/// Returns the configured ghq roots, or empty if ghq is not installed.
fn ghq_roots() -> Vec<PathBuf> {
    let Ok(out) = Command::new("ghq").args(["root", "--all"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Resolves a leading `~` / `$HOME` and makes relative paths absolute against
/// `base`. `home` is injected (rather than read from the environment) so it's
/// testable without mutating process env.
fn expand_path(p: &str, base: &Path, home: Option<&Path>) -> PathBuf {
    if p.is_empty() {
        return PathBuf::new();
    }
    let expanded = match home {
        Some(home) => {
            let home = home.to_string_lossy();
            let tilde = if p == "~" {
                home.to_string()
            } else if let Some(rest) = p.strip_prefix("~/") {
                format!("{home}/{rest}")
            } else {
                p.to_string()
            };
            tilde.replace("${HOME}", &home).replace("$HOME", &home)
        }
        None => p.to_string(),
    };
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gittest;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    /// Renders `s` as a JSON string literal (handles path separators safely).
    fn quote(s: &Path) -> String {
        serde_json::to_string(&s.to_string_lossy()).unwrap()
    }

    #[test]
    fn load_from_resolves_sibling_by_default() {
        let root = TempDir::new().unwrap();
        write_file(
            root.path(),
            "repos.json",
            r#"[{"name":"foo","url":"git@gitlab.com:X/Y/Foo.git","group":"service"}]"#,
        );

        let reg = Registry::load_from(root.path()).unwrap();
        let got = reg.resolve();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].repo.name, "foo");
        assert_eq!(got[0].path, root.path().join("foo"), "want sibling path");
        assert!(!got[0].present, "no .git on disk, so not present");
    }

    #[test]
    fn per_repo_override_wins() {
        let root = TempDir::new().unwrap();
        write_file(
            root.path(),
            "repos.json",
            r#"[{"name":"foo","url":"u","group":"g"}]"#,
        );
        let custom = TempDir::new().unwrap();
        let custom_path = custom.path().join("elsewhere/Foo");
        write_file(
            root.path(),
            "tilt_config.json",
            &format!(r#"{{"repo-foo":{}}}"#, quote(&custom_path)),
        );

        let reg = Registry::load_from(root.path()).unwrap();
        assert_eq!(reg.resolve()[0].path, custom_path, "want override path");
    }

    #[test]
    fn active_worktree_overlays_base_and_reverts_when_gone() {
        gittest::isolate();
        let root = TempDir::new().unwrap();
        write_file(
            root.path(),
            "repos.json",
            r#"[{"name":"foo","url":"u","group":"g"}]"#,
        );
        // Point foo at a real repo (its base/main checkout, via an override).
        let base = gittest::init_repo();
        write_file(
            root.path(),
            "tilt_config.json",
            &format!(r#"{{"repo-foo":{}}}"#, quote(base.path())),
        );

        let mut reg = Registry::load_from(root.path()).unwrap();
        assert_eq!(reg.resolve()[0].path, base.path(), "no selection → base");

        // Select a real worktree by id → resolve overlays it onto the base.
        let wt_home = TempDir::new().unwrap();
        let wt = wt_home.path().join("wt");
        gittest::git(
            base.path(),
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );
        let id = crate::git::worktree_id(&wt).unwrap();
        reg.worktrees.insert("foo".to_string(), id);
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(
            canon(&reg.resolve()[0].path),
            canon(&wt),
            "selection overlaid"
        );

        // Remove the worktree → the stale id resolves to nothing → back to base.
        gittest::git(base.path(), &["worktree", "remove", wt.to_str().unwrap()]);
        assert_eq!(
            reg.resolve()[0].path,
            base.path(),
            "gone worktree reverts to base"
        );
    }

    #[test]
    fn workspace_override_changes_base() {
        let root = TempDir::new().unwrap();
        write_file(
            root.path(),
            "repos.json",
            r#"[{"name":"foo","url":"u","group":"g"}]"#,
        );
        let ws = TempDir::new().unwrap();
        write_file(
            root.path(),
            "tilt_config.json",
            &format!(r#"{{"workspace":{}}}"#, quote(ws.path())),
        );

        let reg = Registry::load_from(root.path()).unwrap();
        assert_eq!(reg.resolve()[0].path, ws.path().join("foo"));
    }

    #[test]
    fn present_detects_git_dir() {
        let root = TempDir::new().unwrap();
        write_file(
            root.path(),
            "repos.json",
            r#"[{"name":"foo","url":"u","group":"g"}]"#,
        );
        std::fs::create_dir_all(root.path().join("foo/.git")).unwrap();
        let reg = Registry::load_from(root.path()).unwrap();
        assert!(reg.resolve()[0].present, "want present when .git exists");
    }

    #[test]
    fn load_from_missing_file_errors() {
        let dir = TempDir::new().unwrap();
        assert!(
            Registry::load_from(dir.path()).is_err(),
            "expected error when repos.json is absent"
        );
    }

    #[test]
    fn malformed_tilt_config_is_reported_not_swallowed() {
        let root = TempDir::new().unwrap();
        write_file(
            root.path(),
            "repos.json",
            r#"[{"name":"foo","url":"u","group":"g"}]"#,
        );
        write_file(root.path(), "tilt_config.json", "{ this is not valid json");

        // LoadFrom must not fail on a malformed tilt_config.json.
        let mut reg = Registry::load_from(root.path()).unwrap();
        // Overrides don't apply, so the sibling default stands.
        assert_eq!(reg.resolve()[0].path, root.path().join("foo"));
        // The parse failure is surfaced, not silently dropped.
        assert!(
            reg.load_tilt_config(root.path()).is_err(),
            "load_tilt_config should return an error for malformed config"
        );
    }

    #[test]
    fn missing_tilt_config_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let mut reg = Registry {
            root: dir.path().to_path_buf(),
            repos: Vec::new(),
            workspace: dir.path().to_path_buf(),
            overrides: HashMap::new(),
            worktrees: HashMap::new(),
            ghq_roots: Vec::new(),
        };
        assert!(
            reg.load_tilt_config(dir.path()).is_ok(),
            "load_tilt_config on an absent file should be Ok"
        );
    }

    #[test]
    fn expand_path_handles_home_and_relative() {
        let home = Path::new("/home/tester");
        let base = Path::new("/base");
        let cases = [
            ("", PathBuf::new()),
            ("~", PathBuf::from("/home/tester")),
            ("~/repos", PathBuf::from("/home/tester/repos")),
            ("$HOME/x", PathBuf::from("/home/tester/x")),
            ("${HOME}/x", PathBuf::from("/home/tester/x")),
            ("/abs/path", PathBuf::from("/abs/path")),
            ("rel/path", PathBuf::from("/base/rel/path")),
        ];
        for (input, want) in cases {
            assert_eq!(
                expand_path(input, base, Some(home)),
                want,
                "input={input:?}"
            );
        }
    }

    #[test]
    fn ghq_relpath_handles_every_remote_form() {
        let cases = [
            ("git@gitlab.com:acme/Bar.git", Some("gitlab.com/acme/Bar")),
            (
                "git@gitlab.com:acme/group/Bar.git",
                Some("gitlab.com/acme/group/Bar"),
            ),
            ("gitlab.com:acme/Bar.git", Some("gitlab.com/acme/Bar")), // no user
            ("git@gitlab.com:acme/Bar", Some("gitlab.com/acme/Bar")), // no .git suffix
            (
                "ssh://git@gitlab.com/acme/Bar.git",
                Some("gitlab.com/acme/Bar"),
            ),
            (
                "ssh://git@host.example.com:2222/acme/Bar.git",
                Some("host.example.com/acme/Bar"), // port dropped
            ),
            (
                "https://gitlab.com/acme/Bar.git",
                Some("gitlab.com/acme/Bar"),
            ),
            (
                "https://user@github.com/acme/Bar.git",
                Some("github.com/acme/Bar"), // userinfo dropped
            ),
            ("not-a-url", None),
            ("", None),
        ];
        for (url, want) in cases {
            assert_eq!(ghq_relpath(url).as_deref(), want, "url={url:?}");
        }
    }
}
