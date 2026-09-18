#!/usr/bin/env bash
# punktfunk — Steam Deck HOST update: rebuild from the current source + restart the services.
# Run on the Deck after pulling/rsyncing new source. Pairings, config, and the web login persist.
#
#   bash scripts/steamdeck/update.sh           # rebuild host (+web if installed) and restart
#   bash scripts/steamdeck/update.sh --pull    # `git pull` first (if the source is a git checkout)
#
set -euo pipefail
log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
# warn was USED below but never defined — under `set -e` the first warn call ("command not
# found") aborted the whole update before the service restarts.
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
# Create a system group if it is missing (needs sudo). Idempotent, and mirrors what the
# deb/rpm/arch scriptlets do — a udev rule that chgrp's to a group nobody created fails silently.
ensure_group() {
    getent group "$1" >/dev/null 2>&1 && return 0
    sudo groupadd --system "$1" 2>/dev/null || return 1
    ok "created the '$1' system group"
}

SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
TARGET_DIR="$SRC/target-steamos"
BIN="$TARGET_DIR/release/punktfunk-host"
# The PyroWave encode worker — a separate executable next to the host, and the only one this
# script setcaps (see the capability block in the sudo section). Never a hardlink or a mode of
# $BIN: a shared inode shares the file capability and voids the KWin .desktop grant.
WORKER="$TARGET_DIR/release/punktfunk-encode-worker"
[ -d "$SRC/crates/punktfunk-host" ] || die "no punktfunk source at $SRC (set PUNKTFUNK_SRC)"
WEB=0; [ -f "$HOME/.config/systemd/user/punktfunk-web.service" ] && WEB=1

if [ "${1:-}" = "--pull" ]; then
    [ -d "$SRC/.git" ] || die "$SRC is not a git checkout — rsync new source then run without --pull"
    # web/bun.nix and sdk/bun.nix are GENERATED (bun2nix, a pure function of the matching bun.lock —
    # packaging/nix/README.md) yet COMMITTED, because the Nix build fetches node_modules only from
    # them. Until the --ignore-scripts fix below, web's `bun install` here ran its `postinstall`
    # (`bun2nix -o bun.nix`) and rewrote that tracked file on every single update. That is invisible
    # while the committed file is in sync — but main carried a STALE web/bun.nix from 1db8f763 to
    # b79d90b4, so any Deck updated in that window had the file rewritten to the *correct* content
    # and has been sitting dirty ever since. The next `git pull --ff-only` that touches it then dies
    # with "Your local changes to the following files would be overwritten by merge", and the update
    # stops before a single service is restarted.
    #
    # Restore ONLY these two derived paths. Not a blanket `git reset --hard`: $SRC is the operator's
    # own checkout (they may have patched a source file, or be carrying a cherry-pick), and silently
    # deleting that to save an update is a far worse trade than one legible error. Discarding these
    # two is provably lossless — regenerating them from the lockfiles is exactly what bun2nix does.
    git -C "$SRC" checkout -- web/bun.nix sdk/bun.nix 2>/dev/null || true
    log "git pull"
    git -C "$SRC" pull --ff-only \
        || die "git pull --ff-only failed in $SRC. If it named locally-modified files, this checkout
  has local changes: review them with 'git -C $SRC status', then commit or stash them (or discard
  one with 'git -C $SRC checkout -- <file>') and re-run. Nothing was rebuilt or restarted."
    ok "pulled"
fi

# The console tells one build from the next by its version string alone. Without the commit
# every rebuild reports the same X.Y.Z, and a finished update reads as "nothing newer".
# An empty value is ignored by the build script, which falls back to the Cargo version.
PF_BASE="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1)"
PF_SHA="$(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || true)"
PF_BUILD_VERSION=""
[ -z "$PF_BASE" ] || [ -z "$PF_SHA" ] || PF_BUILD_VERSION="$PF_BASE+g$PF_SHA"

log "Rebuilding host (release)"
# nvenc,vulkan-encode matches the packaged builds (deb/arch) — see install.sh.
# punktfunk-encode-worker rides along: host and worker version-check each other over their socket
# and fall back to the in-process encoder on any mismatch, so an update must never move one
# without the other.
distrobox enter "$BOX" -- bash -lc "set -e
export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR' PUNKTFUNK_BUILD_VERSION='$PF_BUILD_VERSION'
cd '$SRC' && cargo build -r -p punktfunk-host -p punktfunk-encode-worker --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode"
MISSING="$({ ldd "$BIN" 2>/dev/null || true; } | awk '/not found/ {print $1}' | sort -u | tr '\n' ' ')"
[ -z "$MISSING" ] || die "the host rebuilt, but SteamOS still cannot load it. Missing: $MISSING
     Another rebuild will not help, and the services were left as they are. Report it:
     https://git.unom.io/unom/punktfunk/issues"
ok "host rebuilt"
if [ "$WEB" = 1 ]; then
    log "Rebuilding web console"
    # --ignore-scripts, then `bun run codegen` explicitly: web has TWO install lifecycle scripts and
    # we want exactly one of them. `prepare` (= codegen: orval + paraglide + the i18n check) is
    # REQUIRED — src/api/gen, src/paraglide and src/routeTree.gen.ts are gitignored, and `prebuild`
    # only re-runs orval, so dropping codegen leaves the build without its i18n messages. But
    # `postinstall` (`bun2nix -o bun.nix`) writes a COMMITTED file, and an updater must never dirty
    # the tree it just pulled into — that is what broke `--pull` above. The SDK install below has
    # always passed --ignore-scripts, which is why only web/bun.nix ever went dirty.
    distrobox enter "$BOX" -- bash -lc "set -e; export PATH=\$HOME/.bun/bin:\$PATH; cd '$SRC/web' && bun install --frozen-lockfile --ignore-scripts && bun run codegen && bun run build"
    ok "web rebuilt"
fi

# Plugin runner (scripting): rebuild the user-scoped runner payload (install.sh §2b) — also
# RETROFITS it onto older installs that predate it (the "plugin runner isn't installed" console
# state on SteamOS).
log "Rebuilding plugin runner (scripting)"
mkdir -p "$HOME/.local/bin" "$HOME/.local/lib/punktfunk-scripting" "$HOME/.local/share/punktfunk-scripting"
distrobox enter "$BOX" -- bash -lc "set -e; export PATH=\$HOME/.bun/bin:\$PATH; cd '$SRC/sdk' && bun install --frozen-lockfile --ignore-scripts && bun build src/runner-cli.ts --target=bun --outfile \"\$HOME/.local/share/punktfunk-scripting/runner-cli.js\" && install -m0755 \"\$(command -v bun)\" \"\$HOME/.local/lib/punktfunk-scripting/bun\""
grep -q 'attempt=' "$HOME/.local/share/punktfunk-scripting/runner-cli.js" \
    || die "runner bundle missing the dynamic plugin import — wrong build"
cat > "$HOME/.local/bin/punktfunk-scripting" <<'WRAP'
#!/bin/sh
# Generated by scripts/steamdeck/update.sh — user-scoped punktfunk-scripting (see install.sh §2b).
exec "$HOME/.local/lib/punktfunk-scripting/bun" "$HOME/.local/share/punktfunk-scripting/runner-cli.js" "$@"
WRAP
chmod 0755 "$HOME/.local/bin/punktfunk-scripting"
sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/punktfunk-scripting|' \
    "$SRC/scripts/punktfunk-scripting.service" > "$HOME/.config/systemd/user/punktfunk-scripting.service"
systemctl --user daemon-reload
ok "plugin runner rebuilt (opt-in service: systemctl --user enable --now punktfunk-scripting)"

# HDR gamescope (punktfunk-gamescope): rebuild when the packaging tree changed or the installed
# binary stopped working — also RETROFITS it onto older installs that predate it (fast no-op
# otherwise). Best-effort; on failure the host streams SDR (see build-gamescope.sh).
log "HDR gamescope (punktfunk-gamescope)"
PUNKTFUNK_SRC="$SRC" PUNKTFUNK_BOX="$BOX" bash "$SRC/scripts/steamdeck/build-gamescope.sh"

# Retrofit the post-OS-update rebuild check (install.sh §5) onto older installs: probes the host
# binary with ldd at session start and re-runs this script when a SteamOS update broke its links.
if [ ! -f "$HOME/.config/systemd/user/punktfunk-rebuild-check.service" ]; then
    cat > "$HOME/.config/systemd/user/punktfunk-rebuild-check.service" <<EOF
# Generated by scripts/steamdeck/update.sh — rebuild the host if a SteamOS update broke its libs.
[Unit]
Description=punktfunk SteamOS post-update rebuild check
Before=punktfunk-host.service

[Service]
Type=oneshot
ExecStart=$SRC/scripts/steamdeck/rebuild-check.sh
TimeoutStartSec=1800

[Install]
WantedBy=default.target
EOF
    chmod +x "$SRC/scripts/steamdeck/rebuild-check.sh" 2>/dev/null || true
    systemctl --user daemon-reload
    systemctl --user enable punktfunk-rebuild-check.service 2>/dev/null || true
    ok "punktfunk-rebuild-check.service installed (auto-rebuild after SteamOS updates)"
fi

CONFIG="$HOME/.config/punktfunk"

# Secret hygiene, retrofitted. install.sh §3 does this for fresh installs — but only install.sh
# ever did, so a Deck that was set up once and only ever *updated* since kept the old modes
# forever. This directory holds web.env (console login password + session secret), the mgmt token
# and the host key; a plain `mkdir -p` left it 0755 at the Deck's ambient umask and web.env itself
# 0644, i.e. readable by every local account (2026-08-05 review L-19). Both chmods are idempotent.
[ -d "$CONFIG" ] && chmod 700 "$CONFIG" 2>/dev/null || true
if [ -f "$CONFIG/web.env" ] && find "$CONFIG/web.env" -maxdepth 0 -perm /0077 2>/dev/null | grep -q .; then
    chmod 600 "$CONFIG/web.env"
    warn "web.env was group/world-readable — an older install wrote it at the default umask."
    warn "Tightened to 0600, but that does NOT un-expose the password it already leaked to every"
    warn "local account. Reset it: put a PUNKTFUNK_UI_PASSWORD line in $CONFIG/web.env, then"
    warn "  systemctl --user restart punktfunk-web"
fi

# Retrofit config that install.sh now writes but older installs predate (both idempotent):
# RADV_PERFTEST — Van Gogh RADV still gates VK_KHR_video_encode_* behind it; without it the
# Vulkan backend can't open and sessions silently fall back to VAAPI. The KWin .desktop —
# KWin only grants the restricted capture/input globals to the exe a .desktop authorizes.
HOST_ENV="$CONFIG/host.env"
if [ -f "$HOST_ENV" ] && ! grep -q '^RADV_PERFTEST=' "$HOST_ENV"; then
    printf '\n# Van Gogh RADV gates VK_KHR_video_encode_* behind this (Vulkan Video encode).\nRADV_PERFTEST=video_encode\n' >> "$HOST_ENV"
    ok "host.env: added RADV_PERFTEST=video_encode"
fi
mkdir -p "$HOME/.local/share/applications"
sed "s|^Exec=.*|Exec=$TARGET_DIR/release/punktfunk-host|" "$SRC/packaging/linux/io.unom.Punktfunk.Host.desktop" \
    > "$HOME/.local/share/applications/io.unom.Punktfunk.Host.desktop"
ok "KWin desktop-capture authorization refreshed"

# Retrofit the system bits install.sh now sets up but older installs predate (idempotent). vhci-hcd =
# usbip transport for the native Steam Deck pad; 60-punktfunk.rules = /dev/uhid + vhci access; input
# group = uhid write; the kde-authorized grant (per-user, no root) = Desktop-mode input. A stock Deck
# needs a sudo PASSWORD, so PROMPT for it rather than silently skipping (skipping = gamepads stay dead).
SUDO_OK=0
if sudo -n true 2>/dev/null; then
    SUDO_OK=1
elif [ -t 0 ]; then
    warn "sudo needs your password to (re)apply the gamepad udev rule, vhci-hcd, input group, and UDP buffers:"
    sudo -v && SUDO_OK=1 || true
fi
if [ "$SUDO_OK" = 1 ]; then
    if [ -f "$SRC/scripts/60-punktfunk.rules" ]; then
        sudo install -m644 "$SRC/scripts/60-punktfunk.rules" /etc/udev/rules.d/60-punktfunk.rules
        sudo udevadm control --reload-rules >/dev/null 2>&1 || true
        sudo udevadm trigger >/dev/null 2>&1 || true
        ok "gamepad udev rule ensured"
    fi
    if [ -f "$SRC/scripts/punktfunk-modules.conf" ]; then
        sudo install -m644 "$SRC/scripts/punktfunk-modules.conf" /etc/modules-load.d/punktfunk.conf
        sudo modprobe vhci-hcd 2>/dev/null || true
        ok "vhci-hcd autoload ensured (native Steam Deck controller)"
    fi
    # UDP buffers: older installs (or sudo-skipped ones) still run the stock 416 KB cap.
    if [ ! -f /etc/sysctl.d/99-punktfunk-net.conf ]; then
        printf 'net.core.wmem_max=33554432\nnet.core.rmem_max=33554432\n' | sudo tee /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
        sudo sysctl -q -p /etc/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || true
        ok "UDP socket buffers raised to 32 MB (persisted)"
    fi
    if id -nG "$USER" | grep -qw input; then :; else
        sudo usermod -aG input "$USER"
        warn "added $USER to the 'input' group — REBOOT (or log out/in) for it to apply"
    fi
    # 'punktfunk' owns the usbip vhci attach/detach nodes (60-punktfunk.rules), deliberately NOT
    # 'input' — writing 'attach' materialises an arbitrary emulated USB device, a root-only kernel
    # primitive that must not ride on the group every gamepad guide tells you to join
    # (security-review 2026-08-05 M-4). No Deck install ever created it, so the rule's chgrp failed
    # and the native Steam Deck pad silently never attached. Retrofit both group and membership.
    # `if ensure_group` (not `ensure_group || true`): a failed groupadd must not fall through to a
    # usermod against a group that does not exist — under `set -e` that would abort the update
    # before the service restarts at the bottom, leaving the host down.
    if ensure_group punktfunk; then
        if id -nG "$USER" | grep -qw punktfunk; then :; else
            sudo usermod -aG punktfunk "$USER"
            warn "added $USER to the 'punktfunk' group (usbip vhci — the native Steam Deck pad needs it)"
            warn "  — REBOOT (or log out/in) for it to apply. That group can emulate arbitrary USB"
            warn "  devices; 'sudo gpasswd -d $USER punktfunk' drops it if you do not want the native pad."
        fi
    else
        warn "could not create the 'punktfunk' group — the native Steam Deck pad will not attach."
        warn "By hand: sudo groupadd --system punktfunk; sudo usermod -aG punktfunk $USER"
    fi
    # Capabilities, re-applied because this script just REBUILT both binaries and a rebuilt file is
    # a new inode — file capabilities do not follow it.
    #
    #   host   -> `setcap -r`. It must carry NO capability, ever: KWin identifies a client by
    #             resolving /proc/<pid>/exe against a .desktop Exec= (the one refreshed above), and
    #             the kernel refuses that readlink for a capability-carrying process, so every
    #             Desktop-mode session dies with "KWin does not expose zkde_screencast_unstable_v1
    #             to this client". This also heals a Deck that ran 0.26.0-1's installer and has
    #             only ever updated since — install.sh's removal is not reachable from this path.
    #   worker -> `cap_sys_nice=ep`. Separate binary, spawned per PyroWave session, no Wayland/
    #             D-Bus/network, so nothing resolves its /proc/<pid>/exe. Without the capability
    #             every driver refuses every elevated global-priority class and the lever is inert.
    #
    # Both best-effort: `setcap -r` exits non-zero on a file that has no capability, and a failed
    # grant just means the encode runs at default priority.
    if [ -x "$BIN" ]; then
        sudo setcap -r "$BIN" 2>/dev/null || true
    fi
    if [ -x "$WORKER" ]; then
        if sudo setcap 'cap_sys_nice=ep' "$WORKER" 2>/dev/null; then
            ok "re-granted CAP_SYS_NICE to the encode worker (rebuild = new inode)"
        else
            warn "could not grant CAP_SYS_NICE to $WORKER — PyroWave stays at default GPU priority"
        fi
    fi
    # Register the tuning on Valve's atomic-update preserve list (see install.sh §4): without
    # this, every SteamOS A/B update strips the three files above again (verified live —
    # gamepads silently degrade to Xbox 360, UDP buffers back to 208 KB).
    if [ -f "$SRC/scripts/punktfunk-atomic-keep.conf" ]; then
        sudo install -Dm644 "$SRC/scripts/punktfunk-atomic-keep.conf" /etc/atomic-update.conf.d/punktfunk.conf
        ok "system tuning registered to survive SteamOS updates (atomic-update.conf.d)"
    fi
else
    warn "no usable sudo — SKIPPED gamepad/udev/vhci/UDP tuning (all root-only; no user-space alternative)."
    warn "A stock SteamOS 'deck' account has NO password — set one with 'passwd', then re-run. Gamepads stay"
    warn "Xbox-360 until this runs and you reboot."
fi
echo
warn "If the controller still shows as an Xbox 360 pad, REBOOT the Deck once — the 'input' group and the"
warn "vhci-hcd module only become live for the host service on a fresh login."
GRANT_SRC="$SRC/scripts/headless/kde-authorized"
GRANT_DST="$HOME/.local/share/flatpak/db/kde-authorized"
if [ ! -s "$GRANT_DST" ] && [ -s "$GRANT_SRC" ]; then
    mkdir -p "$(dirname "$GRANT_DST")"
    install -m644 "$GRANT_SRC" "$GRANT_DST"
    ok "seeded KDE RemoteDesktop grant (Desktop-mode input)"
fi

log "Restarting services"
# --no-block: when this script runs INSIDE punktfunk-rebuild-check.service (ordered
# Before=punktfunk-host), a blocking restart would deadlock — the restart job waits for the
# check unit, which waits for this script, which waits for the restart. Enqueue and move on;
# systemd starts the service the moment the ordering allows.
systemctl --user restart --no-block punktfunk-host.service
ok "punktfunk-host restart queued"
if [ "$WEB" = 1 ]; then systemctl --user restart --no-block punktfunk-web.service; ok "punktfunk-web restart queued"; fi
# The runner was rebuilt above; try-restart leaves an opted-out runner off.
systemctl --user try-restart --no-block punktfunk-scripting.service 2>/dev/null || true
echo
log "Updated. Status: systemctl --user status punktfunk-host"
