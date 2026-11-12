#!/usr/bin/env bash
# Release helper for piki-multiplex.
#
# Usage: scripts/release.sh <new-version>   (e.g. 0.2.0, no leading 'v')
#
# Bumps the crate version, refreshes Cargo.lock, runs the full quality gate,
# commits, tags, and pushes. Requires a clean working tree on `main`.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <new-version>" >&2
    exit 2
fi

NEW="$1"
if ! [[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "invalid version: $NEW" >&2
    exit 2
fi
TAG="v${NEW}"

cd "$(git rev-parse --show-toplevel)"

# Preconditions
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$BRANCH" != "main" ]]; then
    echo "must be on main (currently $BRANCH)" >&2
    exit 1
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "working tree is not clean" >&2
    exit 1
fi
git fetch origin main --quiet
if [[ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]]; then
    echo "local main is not up to date with origin/main" >&2
    exit 1
fi
if git rev-parse "$TAG" >/dev/null 2>&1; then
    echo "tag $TAG already exists" >&2
    exit 1
fi

# Quality gate before touching anything
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features

# Bump version in Cargo.toml
python3 - "$NEW" <<'PY'
import re, sys
v = sys.argv[1]
p = "Cargo.toml"
s = open(p).read()
s2 = re.sub(r'(?m)^version = ".*"$', f'version = "{v}"', s, count=1)
assert s2 != s, "version line not found"
open(p, "w").write(s2)
PY

# Refresh lockfile
cargo build --all-features

# Commit + tag
git add Cargo.toml Cargo.lock
git commit -m "chore: release ${TAG}"
git tag -a "$TAG" -m "Release ${TAG}"

# Push
git push origin main
git push origin "$TAG"

echo "Released ${TAG}"
