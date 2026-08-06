# tilt-devenv

Manage a multi-repo dev environment as one unit, live in the [Tilt](https://tilt.dev) UI — cross-repo git branches and worktrees.

Two pieces: a Rust tool (`repos` CLI + `repos-tiltd` daemon) and a Tilt extension (`tilt/Tiltfile`) that surfaces it as live UI — a branch picker, pull button, and worktree picker per resource, a global checkout-all, an auto-refreshing status table, and (when `tilt-devenv.json` defines any) a profile switcher: check any number of profiles and click to enable just their repos, live, no restart. Org-neutral: you supply `tilt-devenv.json` and the resource mapping.

## Use it

**1. Put the binaries on PATH.** Flake package (ships `repos` + `repos-tiltd`):

```nix
inputs.repos.url = "github:AddG0/tilt-devenv";
devShells.default = pkgs.mkShell {
  packages = [ inputs.repos.packages.${system}.default ];
};
```

Non-Nix: `cargo install --path crates/repos --path crates/repos-tiltd`.

Off-PATH builds: `$REPOS_BIN` / `$REPOS_TILTD_BIN` replace those invocations everywhere the
extension uses them. They're command prefixes, not just paths, so they can be builders —
`REPOS_BIN='cargo run --manifest-path /abs/Cargo.toml -p repos --'` (see `dev/Tiltfile`).

**2. Add `tilt-devenv.json` at your dev-env root.** An ordered list of repos:

```json
[
  { "name": "auth", "url": "git@gitlab.com:acme/auth.git", "group": "backend" },
  { "name": "web",  "url": "git@github.com:acme/web.git",  "group": "frontend" }
]
```

Path resolution: `tilt_config.json` override > `ghq` checkout > sibling dir. See `repos --help`.

Optionally, wrap the array in `{"repos": [...], "profiles": {...}}` to name profiles — a
profile maps to the repo or group names it enables, e.g. `{"frontend": ["web"]}`. Set the
*persisted* active selection (survives a `tilt up` restart, XDG state) via `repos profile
set frontend,backend` or the daemon's nav "profiles" button — a checkbox per profile; check
any number and click to save. `repos profile active` reads the current selection; the
Tiltfile extension exposes it via `repos_active_profiles()`/`repos_enabled()` (see
below) — with profiles defined, nothing is enabled until one is picked, so a fresh `tilt up`
runs no repo resources by default.

Once a profile is active, every command (`clone`, `status`, `checkout`, `pull`) is capped to
it by default — a repo outside the active selection isn't part of what you're working on, so
`repos clone` alone only clones its repos, not the whole registry. `--profile=other`/
`--group=other` still can't reach outside the active selection (that's an error, not a silent
no-op); naming a repo exactly via `--only`, or passing `--all`, is a deliberate override and
always works. `repos profiles` lists every profile regardless of what's active.

**3. Load the extension** in your Tiltfile:

```python
v1alpha1.extension_repo(name='repos', url='https://github.com/AddG0/tilt-devenv')
v1alpha1.extension(name='repos', repo_name='repos', repo_path='tilt')
load('ext://repos', 'repos_load', 'repos_status_ui', 'repos_link')

repos = repos_load()          # resolve tilt-devenv.json; `repos clone` grabs missing repos
repos_status_ui({'auth': 'auth', 'web': 'web'}, repos)  # {resource: repo} → live buttons + status
```

Local plugin dev: point `extension_repo` at `url='file:///abs/path/to/tilt-devenv'`.

## Extension API

| Function | Purpose |
| --- | --- |
| `repos_load(clone_missing=True)` | Resolve registry → `{name: struct(name, url, group, path, present)}`. `clone_missing` runs `repos clone` first (scoped to the active profile — see above), a full registry inventory either way. |
| `repos_resolve(clone_missing=True)` | Same, as a list in `tilt-devenv.json` order. |
| `repos_status_ui(branch_resources, repos, status_links=[], rust_log=…, deps=[], labels=None)` | `repos-branches` daemon + `git-status` table. |
| `repos_bin()` / `repos_tiltd_bin()` | The configured invocations: `$REPOS_BIN` (default `repos`), `$REPOS_TILTD_BIN` (default `repos-tiltd`). |
| `repos_browse_url(remote)` | git remote (scp/ssh/https) → browsable `https://host/path`. |
| `repos_link(remote, label='Repo')` | Tilt `link` to that URL. |
| `repos_profiles_load()` | Resolve `tilt-devenv.json`'s `profiles` → `{name: [repo-or-group, ...]}`. |
| `repos_active_profiles()` | The persisted active profile selection (empty = none picked yet); watches it for changes. |
| `repos_enabled(repo, profiles, active)` | Whether `repo` belongs to any of `active` profiles. True with no `active` only when `profiles` is empty (nothing to scope to). |

## Demo

```bash
nix develop
just demo   # examples/bootstrap.sh (throwaway local repos + tilt-devenv.json, offline) + tilt up
just dev    # same, but every repos/repos-tiltd call runs `cargo run` from source (via $REPOS_BIN)
```

## Develop

```bash
nix develop
cargo test               # Rust unit + integration
bash tests/tilt/run.sh   # Tilt extension test (evaluates the Starlark end to end)
nix flake check          # build, tests, clippy, rustfmt, tiltfile-test
```

Three crates: `repos-core` (git primitives, registry, `devenv` domain, Tilt client), `repos` (CLI), `repos-tiltd` (Tilt daemon).
