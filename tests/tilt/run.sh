#!/usr/bin/env bash
# Automated test for the `repos` Tilt extension.
#
# Builds a throwaway workspace of local git repos, points the `repos` tool at it
# via REPOS_ROOT, evaluates tests/tilt/Tiltfile with `tilt alpha tiltfile-result`
# (which runs the extension's Starlark end to end without starting any service),
# and asserts the evaluation succeeded and produced the expected resources. The
# fixture Tiltfile also unit-tests repos_browse_url via fail().
#
# Fully offline and hermetic: no network, no SSH, its own temp HOME. Requires
# `repos`, `tilt`, `git`, and `python3` on PATH — `nix develop` provides them.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for tool in repos tilt git python3; do
  command -v "$tool" >/dev/null || { echo "FAIL: '$tool' not on PATH" >&2; exit 1; }
done

# The fixture asserts the unset defaults, so a caller's override must not leak in.
# To test a source build: PATH=target/debug:$PATH bash tests/tilt/run.sh
unset REPOS_BIN REPOS_TILTD_BIN

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Tilt writes state under HOME; give it a throwaway one so the test can't touch
# the developer's ~/.tilt-dev and doesn't depend on their kube config.
export HOME="$WORK/home"
mkdir -p "$HOME"

# Bare repos the extension clones from, over `file://` so there's no network.
mkdir -p "$WORK/.bare"
for name in alpha beta; do
  bare="$WORK/.bare/$name.git"
  seed="$WORK/.seed-$name"
  git init -q -b main --bare "$bare"
  git -c init.defaultBranch=main init -q "$seed"
  git -C "$seed" -c user.email=test@example.com -c user.name=test \
    -c commit.gpgsign=false commit -q --allow-empty -m "init $name"
  git -C "$seed" branch -q feature-x
  git -C "$seed" remote add origin "$bare"
  git -C "$seed" push -q origin --all
  rm -rf "$seed"
done

cat > "$WORK/tilt-devenv.json" <<EOF
{
  "repos": [
    {"name": "alpha", "url": "file://$WORK/.bare/alpha.git", "group": "demo"},
    {"name": "beta",  "url": "file://$WORK/.bare/beta.git",  "group": "demo"}
  ],
  "profiles": {
    "alpha-only": ["alpha"],
    "beta-only": ["beta"]
  }
}
EOF

# repos resolves its registry from REPOS_ROOT (and clones missing repos beside
# tilt-devenv.json), so everything lands inside the temp workspace.
export REPOS_ROOT="$WORK"

# What the fixture points $REPOS_BIN at — if the extension ever hardcodes `repos`
# again, this log stays empty.
shim_log="$WORK/shim-calls.txt"
mkdir -p "$WORK/bin"
cat > "$WORK/bin/repos-shim" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "$shim_log"
exec repos "\$@"
EOF
chmod +x "$WORK/bin/repos-shim"
export REPOS_SHIM="$WORK/bin/repos-shim"

result="$WORK/result.json"
if ! tilt alpha tiltfile-result -f "$HERE/Tiltfile" > "$result" 2> "$WORK/stderr.txt"; then
  echo "FAIL: tilt alpha tiltfile-result exited non-zero" >&2
  sed 's/^/  tilt: /' "$WORK/stderr.txt" >&2
  exit 1
fi

python3 - "$result" "$REPOS_SHIM" <<'PY'
import json, sys

data = json.load(open(sys.argv[1]))

err = data.get("Error")
if err:
    print("FAIL: Tiltfile evaluation error:\n  %s" % err, file=sys.stderr)
    sys.exit(1)

names = sorted((m.get("Name") or m.get("name")) for m in data.get("Manifests", []))
want = sorted(["alpha", "beta", "repos-controls", "git-status"])
if names != want:
    print("FAIL: resources %r, want %r" % (names, want), file=sys.stderr)
    sys.exit(1)

# The extension must watch the worktree-selection file, or picking a worktree
# would never reload the Tiltfile.
config_files = data.get("ConfigFiles", [])
if not any(str(f).endswith("repos/worktrees.json") for f in config_files):
    print("FAIL: worktree state file not watched; ConfigFiles = %r" % config_files, file=sys.stderr)
    sys.exit(1)

# The long-lived resources must serve the overridden binaries, not `repos`.
serve = {
    m.get("Name") or m.get("name"):
        (((m.get("DeployTarget") or {}).get("ServeCmd") or {}).get("Argv") or [""])[-1]
    for m in data.get("Manifests", [])
}
for name, want_cmd in [("git-status", "%s status --watch" % sys.argv[2]),
                       ("repos-controls", "shim-tiltd")]:
    if serve.get(name) != want_cmd:
        print("FAIL: %s serve_cmd = %r, want %r" % (name, serve.get(name), want_cmd), file=sys.stderr)
        sys.exit(1)

print("PASS: Tiltfile evaluated clean; resources = %s; worktree state watched; "
      "serve cmds from env" % names)
PY

if [[ ! -s "$shim_log" ]]; then
  echo "FAIL: no CLI call went through \$REPOS_BIN (shim log empty)" >&2
  exit 1
fi
echo "PASS: \$REPOS_BIN routed $(wc -l < "$shim_log") CLI call(s)"
