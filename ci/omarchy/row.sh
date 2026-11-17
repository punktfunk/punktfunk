#!/bin/bash
# installer-smoke's omarchy row: archlinux:base wearing Omarchy's os-release and its real
# libalpm update guard (design/installer-v2.md §7, WP4.3).
#
# No Omarchy container image exists and none is needed — the trap is a file. What matters is
# that the guard is ARMED: two canaries prove it before anything else is asserted, so this row
# can never rot into testing nothing if the vendored hook drifts from what Omarchy ships.
#
# $PF is the installer under test (the stub, or the binary directly).
set -euo pipefail

PF=${PF:-sh scripts/install.sh}
HOOKS=/etc/pacman.d/hooks
GUARD=/usr/bin/omarchy-update-pacman-guard
here=$(cd "$(dirname "$0")" && pwd)

echo "==> Wear Omarchy: its os-release and its update guard"
install -m 0755 "$here/omarchy-update-pacman-guard" "$GUARD"
install -d "$HOOKS"
install -m 0644 "$here/00-omarchy-update-guard.hook" "$HOOKS/00-omarchy-update-guard.hook"
printf 'ID=omarchy\nID_LIKE=arch\nPRETTY_NAME="Omarchy"\nVERSION_ID=4.0.1\n' > /etc/os-release

# CANARY 1 — the guard's own logic, through the seam it exposes for exactly this. Deterministic
# and offline: -S and -u together abort, either alone does not.
echo "==> Canary 1: the guard aborts a sync+sysupgrade and nothing else"
for bad in "pacman -Syu" "pacman --sync --sysupgrade" "pacman -Syuu foo"; do
    if OMARCHY_PACMAN_CMDLINE="$bad" "$GUARD" >/dev/null 2>&1; then
        echo "::error::the vendored guard allowed '$bad' — it is disarmed"; exit 1
    fi
done
for good in "pacman -Sy" "pacman -S punktfunk-host" "pacman -Rns punktfunk-host"; do
    if ! OMARCHY_PACMAN_CMDLINE="$good" "$GUARD" >/dev/null 2>&1; then
        echo "::error::the vendored guard blocked '$good', which Omarchy allows"; exit 1
    fi
done

# CANARY 2 — the same guard through libalpm, which is what actually kills the transaction. The
# hook only triggers on an Upgrade operation, so a pending upgrade has to exist: a fresh
# container has none, and without one a plain -Syu would sail through and prove nothing.
echo "==> Canary 2: a real pacman -Syu aborts through the hook"
pacman -Sy --noconfirm >/dev/null 2>&1
pacman -U --noconfirm --needed \
    https://archive.archlinux.org/packages/t/tree/tree-2.0.2-1-x86_64.pkg.tar.zst >/dev/null 2>&1
if pacman -Syu --noconfirm >/tmp/canary 2>&1; then
    echo "::error::pacman -Syu was not aborted by the guard:"; tail -20 /tmp/canary; exit 1
fi
grep -q 'Woah partner' /tmp/canary || {
    echo "::error::-Syu failed, but not with the guard's message:"; tail -20 /tmp/canary; exit 1
}

# --channel canary, not the default. Omarchy's install line takes the host AND the client, which
# both depend on punktfunk-icons, the one owner of the shared io.unom.punktfunk.svg, so pacman
# sees no conflicting files. Stable splits the svg the same way since 0.35.0.
echo "==> The installer succeeds anyway, under the armed guard"
$PF --yes --no-start --no-omarchy-setup --channel canary
test -x /usr/bin/punktfunk-host
command -v punktfunk-omarchy >/dev/null || {
    echo "::error::punktfunk-omarchy is not on PATH — the host package should ship it"; exit 1
}

echo "==> --no-omarchy-setup declines the hand-off"
$PF --yes --no-start --no-omarchy-setup --channel canary > /tmp/decline 2>&1
if grep -q '+ punktfunk-omarchy setup' /tmp/decline; then
    echo "::error::the hand-off ran despite --no-omarchy-setup"; exit 1
fi

# `punktfunk-omarchy setup` puts wiring OUTSIDE the packages, and its `remove` ships IN the host
# package — so it has to run before pacman takes the binary that provides it away.
#
# Asserted from a dry run, which is where the claim lives: this is an ORDERING property of the
# plan. Executing it here is not possible and would not add anything — `punktfunk-omarchy`
# refuses to run as root (it edits the user's own config), and every smoke container is root.
# On a real box the installer refuses to run under sudo at all, so it is never root there.
echo "==> Uninstall removes the wiring before the binary that removes it"
$PF --yes --uninstall --dry-run > /tmp/rm 2>&1 || { tail -20 /tmp/rm; exit 1; }
rm_at=$(grep -n 'punktfunk-omarchy remove' /tmp/rm | head -1 | cut -d: -f1)
rns_at=$(grep -n 'pacman -Rns' /tmp/rm | head -1 | cut -d: -f1)
[ -n "$rm_at" ]  || { echo "::error::punktfunk-omarchy remove never ran"; exit 1; }
[ -n "$rns_at" ] || { echo "::error::pacman -Rns never ran"; exit 1; }
[ "$rm_at" -lt "$rns_at" ] || {
    echo "::error::punktfunk-omarchy remove is planned AFTER pacman -Rns (line $rm_at vs $rns_at)"
    exit 1
}

echo "==> omarchy row ok"
