//! Thin wrappers over the git CLI for the operations the tools need. We shell
//! out (`git -C <path> …`) rather than linking a native git library — the
//! operations are simple, and this matches how the rest of the dev environment
//! (the Tiltfile) already drives git.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// A git operation failure.
#[derive(Debug, Error)]
pub enum Error {
    /// A git command exited non-zero; the message carries git's stderr plus
    /// what ran and where, so callers can see why.
    #[error("{0}")]
    Git(String),
    /// `switch` refused because the working tree has changes that would be
    /// overwritten. Distinct so callers can special-case a dirty checkout.
    #[error("working tree has changes that would be overwritten: {0}")]
    Dirty(String),
    /// A clone/fetch failed because credentials or ACLs don't allow it, not
    /// because anything is actually wrong — not having access to every repo
    /// in the registry is the expected case a profile scopes around, not a
    /// bug to report the same way as one.
    #[error("{0}")]
    AccessDenied(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The parsed result of `git status --porcelain=v2 --branch`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Status {
    /// Current branch name, or empty when detached.
    pub branch: String,
    /// Configured upstream ref, or empty when none.
    pub upstream: String,
    /// Commits ahead of upstream.
    pub ahead: i32,
    /// Commits behind upstream.
    pub behind: i32,
    /// Any staged, unstaged, or untracked changes.
    pub dirty: bool,
    /// HEAD is detached (no branch).
    pub detached: bool,
}

/// Runs a git command in `dir` and returns trimmed stdout. On failure the error
/// carries git's stderr so callers can see what failed and where.
fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let context = || format!("git {} (in {})", args.join(" "), dir.display());
    let start = std::time::Instant::now();
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| Error::Git(format!("{}: {e}", context())))?;
    let ms = start.elapsed().as_millis() as u64;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        let msg = if msg.is_empty() {
            format!("{}", out.status)
        } else {
            msg.to_string()
        };
        tracing::debug!(dir = %dir.display(), ?args, ms, status = %out.status, "git failed");
        return Err(Error::Git(format!("{}: {msg}", context())));
    }
    tracing::debug!(dir = %dir.display(), ?args, ms, "git");
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Reports the current branch, upstream tracking, and dirty state of the repo
/// at `path` in a single git invocation.
pub fn get_status(path: &Path) -> Result<Status> {
    let out = run(path, &["status", "--porcelain=v2", "--branch"])?;
    let mut s = Status::default();
    for line in out.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            let fields: Vec<&str> = header.split_whitespace().collect();
            if fields.len() < 2 {
                continue;
            }
            match fields[0] {
                "branch.head" => {
                    if fields[1] == "(detached)" {
                        s.detached = true;
                    } else {
                        s.branch = fields[1].to_string();
                    }
                }
                "branch.upstream" => s.upstream = fields[1].to_string(),
                // Format: "branch.ab +<ahead> -<behind>"
                "branch.ab" if fields.len() >= 3 => {
                    s.ahead = parse_signed(fields[1]);
                    s.behind = parse_signed(fields[2]);
                }
                _ => {}
            }
        } else if !line.is_empty() {
            // Any porcelain entry line (changed/renamed/unmerged/untracked)
            // means the tree is dirty.
            s.dirty = true;
        }
    }
    Ok(s)
}

fn parse_signed(tok: &str) -> i32 {
    tok.trim_start_matches(['+', '-']).parse().unwrap_or(0)
}

/// Returns the repo's default branch, detected from origin/HEAD. When
/// origin/HEAD is not set locally it falls back to the first of develop, main,
/// or master that exists (remote-tracking first, then local). Detection is
/// fully local (no network), so it works offline.
pub fn default_branch(path: &Path) -> Result<String> {
    if let Ok(out) = run(
        path,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Ok(out.strip_prefix("origin/").unwrap_or(&out).to_string());
    }
    for candidate in ["develop", "main", "master"] {
        if ref_exists(path, &format!("refs/remotes/origin/{candidate}"))
            || ref_exists(path, &format!("refs/heads/{candidate}"))
        {
            return Ok(candidate.to_string());
        }
    }
    Err(Error::Git(format!(
        "could not determine default branch for {}: origin/HEAD unset and no develop/main/master branch found",
        path.display()
    )))
}

/// Reports whether `path` is a git working tree (has a `.git` entry). A pure
/// filesystem check — no subprocess.
pub fn is_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// The repo's common git directory, absolute: `<path>/.git` for a main checkout,
/// or the main repo's `.git` when `path` is a linked worktree. Its `worktrees/`
/// subdirectory holds every linked worktree's metadata. `None` when `path` isn't
/// a working tree.
pub fn common_dir(path: &Path) -> Option<PathBuf> {
    let dir = PathBuf::from(run(path, &["rev-parse", "--git-common-dir"]).ok()?);
    // `--git-common-dir` is relative to `path` for a main checkout, absolute for
    // a linked worktree.
    Some(if dir.is_absolute() {
        dir
    } else {
        path.join(dir)
    })
}

/// A worktree of a repo, from [`worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The checked-out branch, or empty when detached.
    pub branch: String,
    /// The repo's primary worktree (git lists it first).
    pub is_main: bool,
}

/// Lists the repo's worktrees — its main checkout plus any linked worktrees —
/// straight from git's own metadata (`git worktree list --porcelain`, i.e. the
/// `.git/worktrees` folder). Best-effort: a non-repo or git failure yields an
/// empty list, since this only populates a picker.
pub fn worktrees(path: &Path) -> Vec<Worktree> {
    match run(path, &["worktree", "list", "--porcelain"]) {
        Ok(out) => parse_worktrees(&out),
        Err(_) => Vec::new(),
    }
}

/// Parses `git worktree list --porcelain`: blank-line-separated blocks, each led
/// by a `worktree <path>` line and optionally a `branch refs/heads/<name>` line
/// (absent when detached). The first block is the main worktree.
fn parse_worktrees(out: &str) -> Vec<Worktree> {
    let mut worktrees: Vec<Worktree> = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            worktrees.push(Worktree {
                path: PathBuf::from(p),
                branch: String::new(),
                is_main: worktrees.is_empty(),
            });
        } else if let Some(b) = line.strip_prefix("branch ")
            && let Some(w) = worktrees.last_mut()
        {
            w.branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        }
    }
    worktrees
}

/// The git worktree id — the `.git/worktrees/<id>` directory name — for the
/// linked worktree at `worktree_path`, or `None` for a main checkout. The id is
/// stable across `git worktree move` and branch switches, so it's the durable
/// handle for a worktree (unlike its path or checked-out branch).
pub fn worktree_id(worktree_path: &Path) -> Option<String> {
    let git_dir = PathBuf::from(run(worktree_path, &["rev-parse", "--git-dir"]).ok()?);
    // Linked worktree: `<common>/worktrees/<id>`. Main checkout: `.git`.
    if git_dir.parent()?.file_name()? == "worktrees" {
        Some(git_dir.file_name()?.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// The current on-disk path of the worktree with git id `id`, read from the
/// repo's `<common_git_dir>/worktrees/<id>/gitdir` pointer — which git keeps
/// updated across moves. `None` if that worktree no longer exists (removed or
/// pruned), so a stale selection naturally falls back to the main checkout.
pub fn resolve_worktree(common_git_dir: &Path, id: &str) -> Option<PathBuf> {
    let gitdir = common_git_dir.join("worktrees").join(id).join("gitdir");
    let content = std::fs::read_to_string(gitdir).ok()?;
    // The file points at the worktree's `.git`; the worktree is its parent.
    Path::new(content.trim()).parent().map(Path::to_path_buf)
}

/// Reports whether `name` exists as a local branch and/or as an origin
/// remote-tracking branch.
pub fn branch_exists(path: &Path, name: &str) -> (bool, bool) {
    (
        ref_exists(path, &format!("refs/heads/{name}")),
        ref_exists(path, &format!("refs/remotes/origin/{name}")),
    )
}

fn ref_exists(path: &Path, refname: &str) -> bool {
    run(path, &["show-ref", "--verify", "--quiet", refname]).is_ok()
}

/// Branch names for completion: local branches plus origin remote-tracking
/// branches (the `origin/` prefix stripped, `origin/HEAD` dropped), deduped and
/// sorted. Best-effort — a git failure yields an empty list rather than an
/// error, since this only feeds shell completion.
pub fn branch_names(path: &Path) -> Vec<String> {
    let Ok(out) = run(
        path,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes/origin",
        ],
    ) else {
        return Vec::new();
    };
    let mut names: Vec<String> = out
        .lines()
        .map(|l| l.strip_prefix("origin/").unwrap_or(l))
        // Skip origin/HEAD, which short-forms to either `origin` or `HEAD`.
        .filter(|n| !n.is_empty() && *n != "origin" && *n != "HEAD")
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Switches the repo at `path` to `name`. `git switch` auto-creates a local
/// tracking branch from origin/<name> when only the remote side exists. A dirty
/// tree that would be overwritten yields [`Error::Dirty`].
pub fn checkout(path: &Path, name: &str) -> Result<()> {
    // --end-of-options so a branch name beginning with '-' is treated as a ref,
    // not parsed as a git flag.
    match run(path, &["switch", "--end-of-options", name]) {
        Ok(_) => Ok(()),
        Err(Error::Git(msg)) if msg.contains("would be overwritten") => {
            Err(Error::Dirty(name.to_string()))
        }
        Err(e) => Err(e),
    }
}

/// Switches to `name` with the local branch force-reset to `origin/<name>`,
/// discarding any local-only commits on it. For branches that must always mirror
/// the remote (e.g. `nightly`); requires `origin/<name>` to exist.
pub fn checkout_reset_to_remote(path: &Path, name: &str) -> Result<()> {
    let start = format!("origin/{name}");
    match run(path, &["switch", "-C", name, &start]) {
        Ok(_) => Ok(()),
        Err(Error::Git(msg)) if msg.contains("would be overwritten") => {
            Err(Error::Dirty(name.to_string()))
        }
        Err(e) => Err(e),
    }
}

/// Updates remote-tracking refs for origin and prunes deleted branches.
pub fn fetch(path: &Path) -> Result<()> {
    run(path, &["fetch", "--quiet", "--prune", "origin"]).map(|_| ())
}

/// Advances the current branch to its upstream (`@{u}`) when that's a
/// fast-forward. It does not fetch — call [`fetch`] first. Errors if the branch
/// has diverged from its upstream or has none, so it never creates a merge commit.
pub fn fast_forward(path: &Path) -> Result<()> {
    run(path, &["merge", "--ff-only", "@{u}"]).map(|_| ())
}

/// Clones `url` into `path`, which must not yet exist. Creates `path`'s parent
/// directories first, since a sibling-layout repo's parent may not exist yet.
pub fn clone(url: &str, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Git(format!("creating {}: {e}", parent.display())))?;
    }
    let out = Command::new("git")
        .args(["clone", "--quiet", url])
        .arg(path)
        .output()
        .map_err(|e| Error::Git(format!("git clone {url} {}: {e}", path.display())))?;
    if !out.status.success() {
        return Err(classify_failure(
            &out,
            &format!("git clone {url} {}", path.display()),
        ));
    }
    Ok(())
}

/// Checks whether `url` is reachable and permitted, without cloning it —
/// `git ls-remote` just asks the remote to list its refs. Used before
/// persisting a profile selection, so picking a profile with a repo you
/// don't have access to fails immediately, not as a confusing clone failure
/// after the fact.
pub fn can_access(url: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["ls-remote", "--exit-code", url])
        .output()
        .map_err(|e| Error::Git(format!("git ls-remote {url}: {e}")))?;
    if !out.status.success() {
        return Err(classify_failure(&out, &format!("git ls-remote {url}")));
    }
    Ok(())
}

/// Builds the [`Error`] for a failed git invocation, classifying its stderr
/// as [`Error::AccessDenied`] or a generic [`Error::Git`].
fn classify_failure(out: &std::process::Output, context: &str) -> Error {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let msg = stderr.trim();
    let msg = if msg.is_empty() {
        out.status.to_string()
    } else {
        msg.to_string()
    };
    let full = format!("{context}: {msg}");
    if is_access_denied(&msg) {
        Error::AccessDenied(full)
    } else {
        Error::Git(full)
    }
}

/// Whether a clone/fetch failure's stderr reads as a credentials/ACL
/// rejection rather than a generic failure (bad URL, network down, disk
/// full). Hosts phrase this differently (GitHub, GitLab, self-hosted, SSH vs
/// HTTPS), so this matches the common phrasings rather than any one host's
/// exact wording — including "not found", since GitHub returns that for a
/// private repo you can't see, indistinguishable from one that doesn't exist.
fn is_access_denied(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("permission denied")
        || s.contains("could not read from remote repository")
        || s.contains("repository not found")
        || s.contains("access denied")
        || s.contains("authentication failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gittest;
    use tempfile::TempDir;

    /// Sets up a working repo cloned from a bare origin (so upstream tracking
    /// and origin/HEAD are configured). Returns all three temp dirs so the
    /// caller keeps them alive for the duration of the test.
    fn with_remote() -> (TempDir, TempDir, TempDir) {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let work = gittest::clone(origin.path());
        (seed, origin, work)
    }

    #[test]
    fn get_status_reports_branch_and_clean_tree() {
        let (_seed, _origin, work) = with_remote();
        let s = get_status(work.path()).unwrap();
        assert_eq!(s.branch, "main");
        assert_eq!(s.upstream, "origin/main");
        assert!(!s.dirty, "expected clean tree");
        assert_eq!((s.ahead, s.behind), (0, 0));
    }

    #[test]
    fn get_status_detects_dirty_tree() {
        let (_seed, _origin, work) = with_remote();
        gittest::write(work.path(), "new.txt", "uncommitted\n");
        let s = get_status(work.path()).unwrap();
        assert!(
            s.dirty,
            "expected dirty tree after writing an untracked file"
        );
    }

    #[test]
    fn get_status_counts_ahead_and_behind() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let work = gittest::clone(origin.path());

        // One local commit not pushed -> ahead by 1.
        gittest::commit(work.path(), "a.txt", "a\n", "local commit");
        assert_eq!(get_status(work.path()).unwrap().ahead, 1);

        // A second clone pushes a commit; after fetch the first is behind by 1.
        let other = gittest::clone(origin.path());
        gittest::commit(other.path(), "b.txt", "b\n", "remote commit");
        gittest::git(other.path(), &["push", "origin", "main"]);
        gittest::git(work.path(), &["fetch", "origin"]);
        assert_eq!(get_status(work.path()).unwrap().behind, 1);
    }

    #[test]
    fn get_status_reports_detached_head() {
        let (_seed, _origin, work) = with_remote();
        gittest::git(work.path(), &["checkout", "--detach"]);
        let s = get_status(work.path()).unwrap();
        assert!(s.detached, "want detached after `git checkout --detach`");
        assert_eq!(s.branch, "", "branch must be empty when HEAD is detached");
    }

    #[test]
    fn get_status_returns_error_for_non_repo() {
        gittest::isolate();
        let dir = TempDir::new().unwrap();
        assert!(
            get_status(dir.path()).is_err(),
            "get_status on a non-git directory should error"
        );
    }

    #[test]
    fn default_branch_from_origin_head() {
        let (_seed, _origin, work) = with_remote();
        assert_eq!(default_branch(work.path()).unwrap(), "main");
    }

    #[test]
    fn default_branch_falls_back_to_local_branch() {
        gittest::isolate();
        let repo = gittest::init_repo(); // main branch, no remote, no origin/HEAD
        assert_eq!(
            default_branch(repo.path()).unwrap(),
            "main",
            "want main via local fallback"
        );
    }

    #[test]
    fn common_dir_for_main_and_linked_worktree() {
        gittest::isolate();
        let repo = gittest::init_repo();
        let want = std::fs::canonicalize(repo.path().join(".git")).unwrap();
        assert_eq!(
            std::fs::canonicalize(common_dir(repo.path()).unwrap()).unwrap(),
            want,
            "main checkout"
        );

        // A linked worktree resolves to the same common dir.
        let wt_home = TempDir::new().unwrap();
        let wt = wt_home.path().join("wt");
        gittest::git(
            repo.path(),
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );
        assert_eq!(
            std::fs::canonicalize(common_dir(&wt).unwrap()).unwrap(),
            want,
            "linked worktree"
        );
    }

    #[test]
    fn parse_worktrees_reads_main_branch_and_detached() {
        let out = "worktree /repos/app\nHEAD abc\nbranch refs/heads/develop\n\n\
                   worktree /wt/app/feat-login\nHEAD def\nbranch refs/heads/feat/login\n\n\
                   worktree /wt/app/spike\nHEAD 012\ndetached\n";
        let got = parse_worktrees(out);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].path, PathBuf::from("/repos/app"));
        assert_eq!(got[0].branch, "develop");
        assert!(got[0].is_main, "first block is the main worktree");
        assert_eq!(got[1].branch, "feat/login");
        assert!(!got[1].is_main);
        assert_eq!(got[2].branch, "", "detached worktree has no branch");
    }

    #[test]
    fn worktrees_lists_main_and_linked() {
        gittest::isolate();
        let repo = gittest::init_repo();
        let wt_home = TempDir::new().unwrap();
        let wt = wt_home.path().join("wt");
        gittest::git(
            repo.path(),
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );
        let got = worktrees(repo.path());
        assert_eq!(got.len(), 2);
        assert!(got[0].is_main);
        assert!(
            got.iter().any(|w| w.branch == "feature" && !w.is_main),
            "want the linked feature worktree; got {got:?}"
        );
    }

    #[test]
    fn worktree_id_resolves_and_survives_move_and_remove() {
        gittest::isolate();
        let repo = gittest::init_repo();
        let wt_home = TempDir::new().unwrap();
        let wt = wt_home.path().join("wt");
        gittest::git(
            repo.path(),
            &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
        );
        let common = repo.path().join(".git");

        assert_eq!(worktree_id(repo.path()), None, "main checkout has no id");
        let id = worktree_id(&wt).expect("linked worktree has an id");
        let canon = |p| std::fs::canonicalize(p).unwrap();
        assert_eq!(
            canon(resolve_worktree(&common, &id).unwrap()),
            canon(wt.clone()),
            "id resolves to the worktree path"
        );

        // Move: the same id resolves to the new path (git rewrites the pointer).
        let moved = wt_home.path().join("moved");
        gittest::git(
            repo.path(),
            &[
                "worktree",
                "move",
                wt.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        assert_eq!(
            canon(resolve_worktree(&common, &id).unwrap()),
            canon(moved.clone()),
            "id still resolves after a move"
        );

        // Remove: the id no longer resolves — a stale selection reverts to main.
        gittest::git(
            repo.path(),
            &["worktree", "remove", moved.to_str().unwrap()],
        );
        assert!(resolve_worktree(&common, &id).is_none());
    }

    #[test]
    fn branch_exists_local_and_remote() {
        let (_seed, _origin, work) = with_remote();

        assert_eq!(branch_exists(work.path(), "main"), (true, true));
        assert_eq!(branch_exists(work.path(), "nope"), (false, false));

        gittest::git(work.path(), &["branch", "feature"]);
        assert_eq!(
            branch_exists(work.path(), "feature"),
            (true, false),
            "feature should be local only"
        );
    }

    #[test]
    fn branch_names_lists_local_and_origin_deduped() {
        let (_seed, _origin, work) = with_remote();
        // feature is local-only; main (from with_remote) exists on both sides.
        gittest::git(work.path(), &["branch", "feature"]);

        let names = branch_names(work.path());

        assert!(
            names.contains(&"main".to_string()),
            "want main; got {names:?}"
        );
        assert!(
            names.contains(&"feature".to_string()),
            "want the local-only branch; got {names:?}"
        );
        // main exists as both refs/heads/main and origin/main but appears once.
        assert_eq!(
            names.iter().filter(|n| *n == "main").count(),
            1,
            "origin/ and local main should dedupe to one entry; got {names:?}"
        );
        // origin/HEAD must never leak in as a completion candidate.
        assert!(
            !names.iter().any(|n| n == "HEAD" || n == "origin"),
            "origin/HEAD should be dropped; got {names:?}"
        );
    }

    #[test]
    fn branch_names_empty_for_non_repo() {
        gittest::isolate();
        let dir = TempDir::new().unwrap();
        assert!(
            branch_names(dir.path()).is_empty(),
            "a non-git directory should yield no branch names, not an error"
        );
    }

    #[test]
    fn checkout_switches_branch() {
        let (_seed, _origin, work) = with_remote();
        gittest::git(work.path(), &["branch", "feature"]);
        checkout(work.path(), "feature").unwrap();
        assert_eq!(get_status(work.path()).unwrap().branch, "feature");
    }

    #[test]
    fn checkout_reset_to_remote_takes_origin_over_stale_local() {
        gittest::isolate();
        let seed = gittest::init_repo();
        gittest::git(seed.path(), &["branch", "nightly"]);
        let origin = gittest::clone_bare(seed.path());
        let work = gittest::clone(origin.path());

        // Give local nightly a commit that origin doesn't have; it must be discarded.
        gittest::git(work.path(), &["switch", "nightly"]);
        gittest::commit(work.path(), "stale.txt", "x\n", "stale local nightly");

        checkout_reset_to_remote(work.path(), "nightly").unwrap();

        assert_eq!(get_status(work.path()).unwrap().branch, "nightly");
        let head = gittest::git(work.path(), &["rev-parse", "HEAD"]);
        let origin_nightly = gittest::git(work.path(), &["rev-parse", "origin/nightly"]);
        assert_eq!(
            head.trim(),
            origin_nightly.trim(),
            "local nightly should be reset to origin/nightly"
        );
    }

    #[test]
    fn checkout_returns_err_dirty_when_switch_would_overwrite() {
        let (_seed, _origin, work) = with_remote();

        // feature edits README.md, so switching to it from a working tree with
        // an uncommitted README.md edit would overwrite local changes.
        gittest::git(work.path(), &["switch", "-c", "feature"]);
        gittest::commit(
            work.path(),
            "README.md",
            "feature version\n",
            "edit on feature",
        );
        gittest::git(work.path(), &["switch", "main"]);
        gittest::write(work.path(), "README.md", "local uncommitted edit\n");

        match checkout(work.path(), "feature") {
            Err(Error::Dirty(_)) => {}
            other => panic!("want Err(Dirty), got {other:?}"),
        }
    }

    #[test]
    fn clone_creates_a_working_tree_at_the_target_path() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());
        let dest = TempDir::new().unwrap();
        let target = dest.path().join("nested").join("repo");

        clone(&origin.path().to_string_lossy(), &target).unwrap();

        assert!(is_repo(&target), "clone should leave a working tree behind");
        assert_eq!(get_status(&target).unwrap().branch, "main");
    }

    #[test]
    fn can_access_succeeds_for_a_reachable_repo_without_cloning_it() {
        gittest::isolate();
        let seed = gittest::init_repo();
        let origin = gittest::clone_bare(seed.path());

        can_access(&origin.path().to_string_lossy()).unwrap();
    }

    #[test]
    fn can_access_errors_for_an_unreachable_url() {
        gittest::isolate();
        assert!(can_access("/no/such/remote").is_err());
    }

    #[test]
    fn clone_of_a_bad_url_errors_with_git_stderr() {
        gittest::isolate();
        let dest = TempDir::new().unwrap();
        let err = clone("/no/such/remote", &dest.path().join("repo")).unwrap_err();
        assert!(
            matches!(err, Error::Git(_)),
            "a nonexistent local path is a generic failure, not an access-denied one: {err:?}"
        );
    }

    #[test]
    fn is_access_denied_recognizes_common_host_phrasings() {
        let denied = [
            "Permission denied (publickey).\nfatal: Could not read from remote repository.",
            "remote: HTTP Basic: Access denied\nfatal: Authentication failed for 'https://example.com/repo.git/'",
            "remote: Repository not found.\nfatal: repository 'https://example.com/acme/private.git/' not found",
        ];
        for stderr in denied {
            assert!(is_access_denied(stderr), "expected denied for: {stderr:?}");
        }
    }

    #[test]
    fn is_access_denied_ignores_generic_failures() {
        let generic = [
            "fatal: repository '/no/such/remote' does not exist",
            "fatal: unable to access 'https://example.com/repo.git/': Could not resolve host",
            "fatal: destination path 'repo' already exists and is not an empty directory.",
        ];
        for stderr in generic {
            assert!(
                !is_access_denied(stderr),
                "expected not denied for: {stderr:?}"
            );
        }
    }

    #[test]
    fn checkout_treats_flag_like_branch_name_as_ref() {
        let (_seed, _origin, work) = with_remote();
        // A branch whose name begins with '-' must not be parsed as a git flag.
        gittest::git(work.path(), &["update-ref", "refs/heads/-weird", "HEAD"]);
        checkout(work.path(), "-weird").expect("name must be treated as a ref");
        assert_eq!(get_status(work.path()).unwrap().branch, "-weird");
    }
}
