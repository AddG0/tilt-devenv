# tilt-devenv

Manage a multi-repo dev environment as one unit, live in the [Tilt](https://tilt.dev) UI — cross-repo git branches and worktrees.

Two pieces: a Rust tool (`repos` CLI + `repos-tiltd` daemon) and a Tilt extension (`tilt/Tiltfile`) that surfaces it as live UI — a branch picker, pull button, and worktree picker per resource, a global checkout-all, and an auto-refreshing status table. Org-neutral: you supply `repos.json` and the resource mapping.

## Use it

**1. Put the binaries on PATH.** Flake package (ships `repos` + `repos-tiltd`):

```nix
inputs.repos.url = "git+https://.../tilt-devenv";
devShells.default = pkgs.mkShell {
  packages = [ inputs.repos.packages.${system}.default ];
};
```

Non-Nix: `cargo install --path crates/repos --path crates/repos-tiltd`.

**2. Add `repos.json` at your dev-env root.** An ordered list of repos:

```json
[
  { "name": "auth", "url": "git@gitlab.com:acme/auth.git", "group": "backend" },
  { "name": "web",  "url": "git@github.com:acme/web.git",  "group": "frontend" }
]
```

Path resolution: `tilt_config.json` override > `ghq` checkout > sibling dir. See `repos --help`.

**3. Load the extension** in your Tiltfile:

```python
v1alpha1.extension_repo(name='repos', url='https://.../tilt-devenv')
v1alpha1.extension(name='repos', repo_name='repos', repo_path='tilt')
load('ext://repos', 'repos_load', 'repos_status_ui', 'repos_link')

repos = repos_load()                                  # resolve repos.json; clone missing
repos_status_ui({'auth': 'auth', 'web': 'web'}, repos)  # {resource: repo} → live buttons + status
```

Local plugin dev: point `extension_repo` at `url='file:///abs/path/to/tilt-devenv'`.

## Extension API

| Function | Purpose |
| --- | --- |
| `repos_load(clone_missing=True)` | Resolve registry → `{name: struct(name, url, group, path, present)}`. |
| `repos_resolve(clone_missing=True)` | Same, as a list in `repos.json` order. |
| `repos_status_ui(branch_resources, repos, status_links=[], poll='5m', rust_log=…, labels=None)` | `repos-branches` daemon + `git-status` table. |
| `repos_browse_url(remote)` | git remote (scp/ssh/https) → browsable `https://host/path`. |
| `repos_link(remote, label='Repo')` | Tilt `link` to that URL. |

## Demo

```bash
nix develop && cd examples
./bootstrap.sh   # throwaway local repos + repos.json, offline
tilt up
```

## Develop

```bash
nix develop
cargo test               # Rust unit + integration
bash tests/tilt/run.sh   # Tilt extension test (evaluates the Starlark end to end)
nix flake check          # build, tests, clippy, rustfmt, tiltfile-test
```

Three crates: `repos-core` (git primitives, registry, `devenv` domain, Tilt client), `repos` (CLI), `repos-tiltd` (Tilt daemon).
