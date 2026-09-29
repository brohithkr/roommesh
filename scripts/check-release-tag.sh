#!/usr/bin/env bash
# Fails unless <version> is a semver X.Y.Z whose tag v<version> does not exist yet on the remote.
# Used by .github/workflows/release.yml before the build and again right before publishing
# (`gh release create` would otherwise attach a new release to an existing tag).
#   scripts/check-release-tag.sh 1.1.0 [remote]     # remote defaults to origin
set -euo pipefail
[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: $0 <X.Y.Z> [remote]" >&2; exit 2; }
VERSION=$1
REMOTE=${2:-origin}
TAG="v$VERSION"

die() { echo "error: $*" >&2; exit 1; }

PART='(0|[1-9][0-9]{0,8})'
[[ "$VERSION" =~ ^$PART\.$PART\.$PART$ ]] ||
  die "version '$VERSION' is not a semver X.Y.Z (e.g. 1.1.0, without a leading 'v')"

# --exit-code: 0 = the tag exists, 2 = no such ref, anything else = git/network failure.
rc=0
git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$TAG" >/dev/null || rc=$?
case $rc in
  0) die "tag $TAG already exists on $REMOTE; pick a new version (or delete the tag and its release first)" ;;
  2) echo "ok: tag $TAG does not exist on $REMOTE" ;;
  *) die "git ls-remote $REMOTE failed (exit $rc); cannot tell whether $TAG exists" ;;
esac
