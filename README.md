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

**2. Add `tilt-devenv.json` at your dev-env root.** An ordered list of repos:

```json
[
  { "name": "auth", "url": "git@gitlab.com:acme/auth.git", "group": "backend" },
  { "name": "web",  "url": "git@github.com:acme/web.git",  "group": "frontend" }
]
```

Path resolution: `tilt_config.json` override > `ghq` checkout > sibling dir. See `repos --help`.

Optionally, wrap the array in `{"repos": [...], "profiles": {...}}` to name profiles — a
profile maps to the repo or group names it enables, e.g. `{"frontend": ["web"]}`. Use them
via `repos --profile=frontend` (a one-off filter on `status`/`checkout`/`pull`), `repos
profiles` to list them, or as a *persisted* selection (survives a `tilt up` restart, XDG
state) via `repos profile set frontend,backend` or the daemon's nav "apply profiles"
button — a checkbox per profile; check any number and click to save (unchecking every box
re-enables all of them). `repos profile active` reads the current selection; the Tiltfile
extension exposes it via `repos_active_profiles()`/`repos_profile_enabled()` (see below).

**3. Load the extension** in your Tiltfile:

```python
v1alpha1.extension_repo(name='repos', url='https://github.com/AddG0/tilt-devenv')
v1alpha1.extension(name='repos', repo_name='repos', repo_path='tilt')
load('ext://repos', 'repos_load', 'repos_status_ui', 'repos_link')

repos = repos_load()                                  # resolve tilt-devenv.json; clone missing
repos_status_ui({'auth': 'auth', 'web': 'web'}, repos)  # {resource: repo} → live buttons + status
```

Local plugin dev: point `extension_repo` at `url='file:///abs/path/to/tilt-devenv'`.

## Extension API

| Function | Purpose |
| --- | --- |
| `repos_load(clone_missing=True)` | Resolve registry → `{name: struct(name, url, group, path, present)}`. |
| `repos_resolve(clone_missing=True)` | Same, as a list in `tilt-devenv.json` order. |
| `repos_status_ui(branch_resources, repos, status_links=[], rust_log=…, serve_cmd='repos-tiltd', deps=[], labels=None)` | `repos-branches` daemon + `git-status` table. |
| `repos_browse_url(remote)` | git remote (scp/ssh/https) → browsable `https://host/path`. |
| `repos_link(remote, label='Repo')` | Tilt `link` to that URL. |
| `repos_profiles_load()` | Resolve `tilt-devenv.json`'s `profiles` → `{name: [repo-or-group, ...]}`. |
| `repos_active_profiles()` | The persisted active profile selection (empty = every profile enabled); watches it for changes. |
| `repos_profile_enabled(repo, profiles, active)` | Whether `repo` belongs to any of `active` profiles (or `active` is empty). |

## Demo

```bash
nix develop
just demo   # examples/bootstrap.sh (throwaway local repos + tilt-devenv.json, offline) + tilt up
just dev    # same, but repos-tiltd runs `cargo run` from source instead of the Nix-packaged binary
```

## Develop

```bash
nix develop
cargo test               # Rust unit + integration
bash tests/tilt/run.sh   # Tilt extension test (evaluates the Starlark end to end)
nix flake check          # build, tests, clippy, rustfmt, tiltfile-test
```

Three crates: `repos-core` (git primitives, registry, `devenv` domain, Tilt client), `repos` (CLI), `repos-tiltd` (Tilt daemon).
