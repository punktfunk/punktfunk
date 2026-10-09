# shellcheck shell=bash disable=SC2034
# Sourced by install.sh, update.sh and rebuild-check.sh: where an on-device build lives, the log
# helpers, the system tuning both installers apply, and the host's user unit. The variables it sets
# are for those scripts.

SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
UNITS="$HOME/.config/systemd/user"

# pf_set_src <dir>: the source checkout and the build output under it.
pf_set_src() {
    SRC="$1"
    TARGET_DIR="$SRC/target-steamos"
    BIN="$TARGET_DIR/release/punktfunk-host"
    # The PyroWave encode worker, the only binary these scripts setcap. Never a hardlink or a mode
    # of $BIN: a shared inode shares the capability and voids KWin's .desktop grant.
    WORKER="$TARGET_DIR/release/punktfunk-encode-worker"
}
pf_set_src "$SRC"

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# Create a missing system group from packaging/linux/punktfunk.sysusers (needs sudo). A udev rule
# that chgrp's to a group nobody created fails silently.
ensure_group() {
    getent group "$1" >/dev/null 2>&1 && return 0
    sudo systemd-sysusers "$SRC/packaging/linux/punktfunk.sysusers" >/dev/null 2>&1 || true
    getent group "$1" >/dev/null 2>&1 || return 1
    ok "created the '$1' system group"
}

# The repo's system files into /etc, the groups and the capabilities, on every run, so a changed
# file reaches an installed Deck. Needs sudo. Sets NEED_RELOGIN=1 when it adds a group membership.
# SteamOS A/B updates keep only the /etc files scripts/punktfunk-atomic-keep.conf lists.
apply_system_tuning() {
    sudo install -Dm644 "$SRC/scripts/99-punktfunk-net.conf" /etc/sysctl.d/99-punktfunk-net.conf
    sudo sysctl -q -p /etc/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || true
    ok "UDP socket buffers raised to 32 MB (persisted)"
    # Nice-limit headroom for the host's data-plane threads: SteamOS does not guarantee rtkit, and
    # without either the per-thread renice no-ops. /usr is read-only, so the drop-in lands in /etc.
    sudo install -Dm644 "$SRC/packaging/linux/50-punktfunk-nice.conf" \
        /etc/systemd/system/user@.service.d/50-punktfunk-nice.conf
    sudo systemctl daemon-reload || true
    ok "nice-limit drop-in installed (data-plane thread priority; applies after a reboot)"
    sudo install -Dm644 "$SRC/scripts/60-punktfunk.rules" /etc/udev/rules.d/60-punktfunk.rules
    sudo udevadm control --reload-rules >/dev/null 2>&1 || true
    sudo udevadm trigger >/dev/null 2>&1 || true
    ok "installed udev rule (virtual gamepads + native Steam Deck controller)"
    # vhci-hcd makes the virtual Steam Deck pad a real USB device, the only kind Steam Input
    # adopts. Loaded now so passthrough works before the next boot.
    sudo install -Dm644 "$SRC/scripts/punktfunk-modules.conf" /etc/modules-load.d/punktfunk.conf
    sudo modprobe vhci-hcd 2>/dev/null || warn "couldn't load vhci-hcd now (loads on next boot) — needed for the native Steam Deck pad"
    ok "vhci-hcd autoload installed (native Steam Deck controller transport)"
    if id -nG "$USER" | grep -qw input; then
        ok "already in the 'input' group"
    else
        sudo usermod -aG input "$USER"
        NEED_RELOGIN=1
        warn "added $USER to the 'input' group (applies after a reboot)"
    fi
    # 'punktfunk' owns the vhci attach/detach nodes; running this script is the opt-in to the
    # native pad, so the user joins it. `if ensure_group`: a usermod against a missing group would
    # end the run under set -e before the services restart.
    if ensure_group punktfunk; then
        if id -nG "$USER" | grep -qw punktfunk; then
            ok "already in the 'punktfunk' group (usbip vhci access)"
        else
            sudo usermod -aG punktfunk "$USER"
            NEED_RELOGIN=1
            warn "added $USER to the 'punktfunk' group — the native Steam Deck pad needs it. That group"
            warn "can emulate arbitrary USB devices; drop it with 'sudo gpasswd -d $USER punktfunk' if"
            warn "you would rather stream without the native pad."
        fi
    else
        warn "couldn't create the 'punktfunk' group — the native Steam Deck pad will not attach"
        warn "(everything else works; the pad arrives as a generic Xbox 360 controller). By hand:"
        warn "  sudo groupadd --system punktfunk; sudo usermod -aG punktfunk $USER"
    fi
    # The host carries no capability: KWin cannot identify a capability-carrying client, and every
    # Desktop-mode session dies. The worker carries cap_sys_nice=ep for PyroWave's GPU priority. A
    # rebuild is a new inode, so both are re-applied. Matrix: packaging/arch/punktfunk-host.install.
    if [ -x "$BIN" ]; then
        sudo setcap -r "$BIN" 2>/dev/null || true
    fi
    if [ -x "$WORKER" ]; then
        if sudo setcap 'cap_sys_nice=ep' "$WORKER" 2>/dev/null; then
            ok "granted CAP_SYS_NICE to the encode worker (PyroWave can outrank a GPU-bound game)"
        else
            warn "couldn't grant CAP_SYS_NICE to $WORKER — PyroWave stays at default GPU priority"
        fi
    fi
    sudo install -Dm644 "$SRC/scripts/punktfunk-atomic-keep.conf" /etc/atomic-update.conf.d/punktfunk.conf
    ok "system tuning registered to survive SteamOS updates (atomic-update.conf.d)"
}

# write_host_unit <ExecStart value>: scripts/punktfunk-host.service with this install's command,
# and the Deck's session environment in a drop-in beside it.
write_host_unit() {
    local xrd="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    mkdir -p "$UNITS/punktfunk-host.service.d"
    {
        echo "# Generated by scripts/steamdeck/lib.sh from scripts/punktfunk-host.service; update.sh rewrites it."
        sed "s|^ExecStart=.*|ExecStart=$1|" "$SRC/scripts/punktfunk-host.service"
    } > "$UNITS/punktfunk-host.service"
    cat > "$UNITS/punktfunk-host.service.d/steamdeck.conf" <<EOF
# Generated by scripts/steamdeck/lib.sh — the Steam Deck session the host runs in.
[Service]
Environment=XDG_RUNTIME_DIR=$xrd
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=$xrd/bus
EOF
}
