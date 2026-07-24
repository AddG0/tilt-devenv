#!/usr/bin/env bash
# Create throwaway local git repos and a repos.json pointing at them, so the
# example can be driven with `tilt up` fully offline — no SSH keys, no network.
# `alpha` also gets a git worktree so its resource shows the 🌳 worktree picker.
# Everything it writes is gitignored. Re-run any time to reset the demo.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO="$HERE/.demo"

rm -rf "$DEMO" "$HERE/alpha" "$HERE/beta"
mkdir -p "$DEMO"

make_repo() {
  local name="$1"
  local bare="$DEMO/$name.git"
  local seed="$DEMO/$name-seed"
  git init -q --bare "$bare"
  git -c init.defaultBranch=main init -q "$seed"
  git -C "$seed" -c user.email=demo@example.com -c user.name=demo \
    commit -q --allow-empty -m "init $name"
  git -C "$seed" branch -q feature-x
  git -C "$seed" remote add origin "$bare"
  git -C "$seed" push -q origin --all
  rm -rf "$seed"
}

make_repo alpha
make_repo beta

cat > "$HERE/repos.json" <<EOF
[
  {"name": "alpha", "url": "file://$DEMO/alpha.git", "group": "demo"},
  {"name": "beta",  "url": "file://$DEMO/beta.git",  "group": "demo"}
]
EOF

# Pre-clone alpha and give it a git worktree at an explicit (gitignored) path, so
# the alpha resource shows a 🌳 worktree dropdown. beta is left for `tilt up` to
# clone and has no worktree, for contrast.
git clone -q "file://$DEMO/alpha.git" "$HERE/alpha"
git -C "$HERE/alpha" worktree add -q -b demo-feature "$DEMO/wt/alpha-demo-feature"

echo "Wrote $HERE/repos.json and added a git worktree (demo-feature) to alpha."
echo "Now run:  tilt up   (from $HERE)"
echo "The 'alpha' resource has a 🌳 worktree picker; choosing demo-feature reloads"
echo "and restarts it at the worktree path."
