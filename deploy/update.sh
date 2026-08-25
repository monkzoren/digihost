#!/usr/bin/env bash
# DigiHost updater: installs the latest GitHub release over this machine's
# installation, verifying the artifact checksum first. Run as root.
#
#   digihost-update            update when a newer release exists
#   digihost-update --force    reinstall even when already current
#
# The server republishes the control-plane module itself on the next start
# whenever the shipped wasm changed, so schema updates ride along.
set -euo pipefail

REPO="${DIGIHOST_REPO:-monkzoren/digihost}"
DIR="${DIGIHOST_DIR:-/opt/digihost}"
FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

current="$("$DIR/bin/digihost-server" --version 2>/dev/null | awk '{print $2}' || echo 0.0.0)"
tag="$(curl -fsSL -H 'User-Agent: digihost-update' \
  "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
  | grep -m1 '"tag_name"' | cut -d '"' -f4 || true)"
latest="${tag#v}"

if [ -z "$tag" ]; then
  echo "no published release found for $REPO (yet?)" >&2
  exit 1
fi
if [ "$current" = "$latest" ] && [ "$FORCE" = 0 ]; then
  echo "already on $current"
  exit 0
fi

echo "updating $current -> $latest"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

base="https://github.com/$REPO/releases/download/$tag"
curl -fsSL -o "$tmp/pkg.tgz" "$base/digihost-linux-x86_64.tar.gz"
curl -fsSL -o "$tmp/pkg.sha" "$base/digihost-linux-x86_64.tar.gz.sha256"
(cd "$tmp" && echo "$(awk '{print $1}' pkg.sha)  pkg.tgz" | sha256sum -c - >/dev/null)
tar -xzf "$tmp/pkg.tgz" -C "$tmp"

# Stop, swap, start. The agent is optional — not every box runs one.
systemctl stop digihost-agent 2>/dev/null || true
systemctl stop digihost

install -m 0755 "$tmp/digihost-server" "$tmp/digihost-agent" "$DIR/bin/"
install -m 0644 "$tmp/digihost_module.wasm" "$DIR/module/"
install -m 0755 "$tmp/update.sh" "$DIR/bin/digihost-update"

systemctl start digihost
systemctl start digihost-agent 2>/dev/null || true

echo "updated to $latest"
