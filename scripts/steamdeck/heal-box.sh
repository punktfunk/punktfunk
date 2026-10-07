#!/usr/bin/env bash
# Remount a build box whose fuse-overlayfs helper died. podman keeps the dead mount and reuses it
# on every start ("transport endpoint is not connected"), so the box never starts again until a
# reboot. Detaching it in podman's namespace makes the next start mount afresh. A healthy or
# stopped box is left alone.
#
#   bash scripts/steamdeck/heal-box.sh <box>
set -euo pipefail
BOX="${1:?usage: heal-box.sh <box>}"
MERGED="$(podman inspect --format '{{.GraphDriver.Data.MergedDir}}' "$BOX" 2>/dev/null)" || exit 0
ERR="$(podman unshare stat "$MERGED/etc" 2>&1 >/dev/null)" || true
[[ "$ERR" == *"not connected"* ]] || exit 0
printf '\033[1;33m  !!\033[0m %s\n' "the '$BOX' container lost its files (its helper process died) — remounting them" >&2
podman unshare umount -l "$MERGED"
podman stop "$BOX" >/dev/null 2>&1 || true
