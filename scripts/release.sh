#!/usr/bin/env bash
# release.sh — bump version, commit, tag, push → triggers the CI release pipeline
#
# Usage:
#   ./scripts/release.sh 0.3.1
#   ./scripts/release.sh            # prints current version and exits

set -euo pipefail

CARGO="Cargo.toml"
CURRENT=$(grep '^version' "$CARGO" | head -1 | sed 's/version = "\(.*\)"/\1/')

if [[ $# -eq 0 ]]; then
    echo "Current version: $CURRENT"
    echo "Usage: $0 <new-version>   e.g. $0 0.3.1"
    exit 0
fi

NEW="$1"
TAG="v$NEW"

# ── Validate ─────────────────────────────────────────────────────────────────
if [[ ! "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "❌ Version must be semver (e.g. 1.2.3), got: $NEW"
    exit 1
fi

if git rev-parse "$TAG" &>/dev/null; then
    echo "❌ Tag $TAG already exists"
    exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
    echo "❌ Working tree is dirty — commit or stash your changes first"
    git status --short
    exit 1
fi

# ── Bump Cargo.toml ───────────────────────────────────────────────────────────
echo "Bumping $CURRENT → $NEW in $CARGO"
if [[ "$OSTYPE" == "darwin"* ]]; then
    sed -i '' "s/^version = \"$CURRENT\"/version = \"$NEW\"/" "$CARGO"
else
    sed -i  "s/^version = \"$CURRENT\"/version = \"$NEW\"/" "$CARGO"
fi

# Verify the change landed
AFTER=$(grep '^version' "$CARGO" | head -1 | sed 's/version = "\(.*\)"/\1/')
if [[ "$AFTER" != "$NEW" ]]; then
    echo "❌ sed failed — check $CARGO manually"
    exit 1
fi

# ── Commit + tag + push ───────────────────────────────────────────────────────
git add "$CARGO"
git commit -m "chore: release $TAG"
git tag "$TAG"

echo ""
echo "✅ Committed and tagged $TAG locally"
echo ""
read -r -p "Push to origin? This will trigger the CI release pipeline. [y/N] " CONFIRM
if [[ "$CONFIRM" =~ ^[Yy]$ ]]; then
    git push origin HEAD
    git push origin "$TAG"
    echo ""
    echo "🚀 Tag $TAG pushed — watch the pipeline:"
    echo "   https://github.com/Bennekrouf/ais-runner/actions"
    echo ""
    echo "Release will appear at:"
    echo "   https://github.com/Bennekrouf/ais-runner/releases/latest"
else
    echo ""
    echo "Skipped push. When ready:"
    echo "   git push origin HEAD && git push origin $TAG"
fi
