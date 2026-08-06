#!/usr/bin/env bash
# Create throwaway local git repos and a tilt-devenv.json pointing at them, so
# `just dev` can be driven fully offline — no SSH keys, no network.
#
# Five repos across two repo groups (frontend, backend) plus one repo whose
# URL never resolves, and five profiles — enough surface to exercise
# checkout-all's group filter, auto-clone-on-profile-pick, and the
# access-denied path, all offline.
#
# `alpha` also gets a git worktree so its resource shows the 🌳 worktree
# picker. Everything this writes is gitignored. Re-run any time to reset.
#
# Independent of examples/bootstrap.sh (same idea, but examples/ stays a clean
# reference for real consumers, uncoupled from this repo's own dev workflow).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO="$HERE/.demo"

rm -rf "$DEMO" "$HERE/alpha" "$HERE/beta" "$HERE/gamma" "$HERE/delta" "$HERE/epsilon"
mkdir -p "$DEMO"

make_repo() {
  local name="$1"
  local bare="$DEMO/$name.git"
  local seed="$DEMO/$name-seed"
  git init -q --bare "$bare"
  git -c init.defaultBranch=main init -q "$seed"
  git -C "$seed" -c user.email=demo@example.com -c user.name=demo \
    -c commit.gpgsign=false commit -q --allow-empty -m "init $name"
  git -C "$seed" branch -q feature-x
  git -C "$seed" remote add origin "$bare"
  git -C "$seed" push -q origin --all
  rm -rf "$seed"
}

for name in alpha beta gamma delta; do
  make_repo "$name"
done
# epsilon has no bare repo behind it — its url below points at nothing, on
# purpose, so picking the "everything" profile exercises the access-denied
# path instead of a normal clone.

cat > "$HERE/tilt-devenv.json" <<EOF
{
  "repos": [
    {"name": "alpha",   "url": "file://$DEMO/alpha.git", "group": "frontend"},
    {"name": "beta",    "url": "file://$DEMO/beta.git",  "group": "frontend"},
    {"name": "gamma",   "url": "file://$DEMO/gamma.git", "group": "backend"},
    {"name": "delta",   "url": "file://$DEMO/delta.git", "group": "backend"},
    {"name": "epsilon", "url": "file://$DEMO/no-such-repo.git", "group": "backend"}
  ],
  "profiles": {
    "frontend-only": ["frontend"],
    "backend-only":  ["gamma", "delta"],
    "staging":       ["alpha", "gamma"],
    "prod":          ["beta", "delta"],
    "everything":    ["epsilon"]
  }
}
EOF

# Pre-clone alpha and give it a git worktree at an explicit (gitignored) path,
# so the alpha resource shows a 🌳 worktree dropdown. beta/gamma/delta are
# left for a picked profile's auto-clone to grab.
git clone -q "file://$DEMO/alpha.git" "$HERE/alpha"
git -C "$HERE/alpha" worktree add -q -b demo-feature "$DEMO/wt/alpha-demo-feature"

echo "Wrote $HERE/tilt-devenv.json and added a git worktree (demo-feature) to alpha."
echo "Now run:  just dev   (from the repo root)"
echo
echo "Nothing runs until a profile is picked. Try:"
echo "  repos profile set frontend-only,staging  # combine profiles freely"
echo "  repos profile set everything              # epsilon's bad url is the access-denied case"
echo
echo "The 'alpha' resource has a 🌳 worktree picker; choosing demo-feature reloads"
echo "and restarts it there."
