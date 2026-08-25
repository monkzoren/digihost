#!/usr/bin/env bash
# DigiHost release tool: bump the version, gate on tests, tag, push, wait for
# CI to publish the artifacts — and optionally roll the release onto a server.
#
#   deploy/release.sh --patch              0.2.0 -> 0.2.1
#   deploy/release.sh --minor              0.2.0 -> 0.3.0
#   deploy/release.sh --major              0.2.0 -> 1.0.0
#   deploy/release.sh 0.4.2                exactly that version
#
# Flags:
#   --deploy    after the Linux artifact is up, run digihost-update on
#               $DIGIHOST_DEPLOY_HOST (e.g. root@your-server). Set it in the
#               environment or in a gitignored .release.env at the repo root.
#   --yes, -y   no confirmation prompt (for scripted use)
#
# The version bump only happens after the test suite passes, so a red suite
# leaves the tree untouched. Deploy waits for the Linux artifact specifically —
# a server update does not need to sit through the Windows build.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
[ -f .release.env ] && . ./.release.env

BUMP=""
DEPLOY=0
ASSUME_YES=0
for arg in "$@"; do
  case "$arg" in
    --patch|--minor|--major) BUMP="${arg#--}" ;;
    --deploy) DEPLOY=1 ;;
    --yes|-y) ASSUME_YES=1 ;;
    [0-9]*.[0-9]*.[0-9]*) BUMP="$arg" ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done
if [ -z "$BUMP" ]; then
  echo "usage: deploy/release.sh --patch|--minor|--major|<version> [--deploy] [--yes]" >&2
  exit 2
fi

# ------------------------------------------------------------------ preflight
if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree is not clean — commit or stash first" >&2
  exit 1
fi
branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$branch" != "main" ]; then
  echo "releases are cut from main (you are on $branch)" >&2
  exit 1
fi
git fetch -q origin
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "main is not in sync with origin — pull or push first" >&2
  exit 1
fi

current="$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f2)"
case "$BUMP" in
  patch) new="$(echo "$current" | awk -F. '{print $1"."$2"."$3+1}')" ;;
  minor) new="$(echo "$current" | awk -F. '{print $1"."$2+1".0"}')" ;;
  major) new="$(echo "$current" | awk -F. '{print $1+1".0.0"}')" ;;
  *) new="$BUMP" ;;
esac
tag="v$new"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "$tag already exists" >&2
  exit 1
fi

echo "release: $current -> $new"
if [ "$ASSUME_YES" = 0 ]; then
  read -r -p "tag and publish $tag? [y/N] " answer
  [ "$answer" = "y" ] || { echo "aborted"; exit 1; }
fi

# ----------------------------------------------------------------------- gate
echo "running the test suite"
cargo test --workspace --quiet
(cd crates/module && cargo check --quiet --target wasm32-unknown-unknown)

# ----------------------------------------------------------------------- bump
sed -i "s/^version = \"$current\"/version = \"$new\"/" Cargo.toml crates/module/Cargo.toml
cargo update -w -q
(cd crates/module && cargo update -w -q)

git add Cargo.toml Cargo.lock crates/module/Cargo.toml crates/module/Cargo.lock
git commit -q -m "release: $tag"
git tag "$tag"
git push -q origin main "$tag"
echo "pushed $tag — CI is building"

# ------------------------------------------------------- wait for the release
# Poll for the Linux artifact, and bail early if the CI run itself failed.
found=0
for _ in $(seq 1 80); do
  if gh release view "$tag" --json assets --jq '.assets[].name' 2>/dev/null \
    | grep -q 'digihost-linux-x86_64.tar.gz.sha256'; then
    found=1
    break
  fi
  conclusion="$(gh run list --event push --branch "$tag" \
    --json conclusion --jq '.[0].conclusion' 2>/dev/null || true)"
  if [ "$conclusion" = "failure" ] || [ "$conclusion" = "cancelled" ]; then
    echo "CI for $tag ended in $conclusion — see: gh run list --branch $tag" >&2
    exit 1
  fi
  sleep 15
done
if [ "$found" = 0 ]; then
  echo "gave up waiting for the Linux artifact — check: gh run list --branch $tag" >&2
  exit 1
fi

echo "release assets so far:"
gh release view "$tag" --json assets --jq '.assets[].name' | sed 's/^/  /'

# --------------------------------------------------------------------- deploy
if [ "$DEPLOY" = 1 ]; then
  : "${DIGIHOST_DEPLOY_HOST:?set DIGIHOST_DEPLOY_HOST (e.g. root@your-server), for example in .release.env}"
  echo "updating $DIGIHOST_DEPLOY_HOST"
  ssh "$DIGIHOST_DEPLOY_HOST" digihost-update
  ssh "$DIGIHOST_DEPLOY_HOST" '/opt/digihost/bin/digihost-server --version'
fi

# The Windows artifact may still be building; say so rather than implying done.
if ! gh release view "$tag" --json assets --jq '.assets[].name' 2>/dev/null \
  | grep -q 'digihost-windows-x86_64.zip'; then
  echo "note: the Windows artifact is still building (gh run list --branch $tag)"
fi
echo "done: $tag"
