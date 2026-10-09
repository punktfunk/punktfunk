#!/usr/bin/env bash
# punktfunk — Steam Deck HOST update: rebuild from the current source + restart the services.
# Run on the Deck after pulling/rsyncing new source. Pairings, config, and the web login persist.
#
#   bash scripts/steamdeck/update.sh           # rebuild host (+web if installed) and restart
#   bash scripts/steamdeck/update.sh --pull    # `git pull` first (if the source is a git checkout)
#
# The branch the checkout follows is its channel: `stable` moves at each release, `main` is canary.
# Switch with the guided installer's --channel, or `git switch <branch>` then --pull.
#
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
[ -d "$SRC/crates/host/punktfunk-host" ] || die "no punktfunk source at $SRC (set PUNKTFUNK_SRC)"
WEB=0; [ -f "$HOME/.config/systemd/user/punktfunk-web.service" ] && WEB=1

if [ "${1:-}" = "--pull" ]; then
    [ -d "$SRC/.git" ] || die "$SRC is not a git checkout — rsync new source then run without --pull"
    git -C "$SRC" symbolic-ref -q HEAD >/dev/null \
        || die "$SRC isn't on a branch, so there is nothing to pull. Pick a channel, then re-run:
  git -C $SRC fetch && git -C $SRC switch stable   # releases
  git -C $SRC fetch && git -C $SRC switch main     # canary"
    # A build regenerates these committed files (bun2nix). When main carries a stale copy, the
    # rebuild dirties it and the next pull that touches it aborts. Restoring derived paths is
    # lossless. Not `reset --hard`: this is the operator's own checkout.
    git -C "$SRC" checkout -- web/bun.nix sdk/bun.nix 2>/dev/null || true
    log "git pull"
    git -C "$SRC" pull --ff-only \
        || die "git pull --ff-only failed in $SRC. If it named locally-modified files, this checkout
  has local changes: review them with 'git -C $SRC status', then commit or stash them (or discard
  one with 'git -C $SRC checkout -- <file>') and re-run. Nothing was rebuilt or restarted."
    ok "pulled"
    # Bash keeps running the text it read before the pull. Build with the pulled tree's recipe.
    PUNKTFUNK_SRC="$SRC" PUNKTFUNK_BOX="$BOX" exec bash "$SRC/scripts/steamdeck/update.sh"
fi

# The version the console shows, in the same scheme as this channel's feed (build-version.sh).
PF_BUILD_VERSION="$(bash "$SRC/scripts/steamdeck/build-version.sh" "$SRC")"

bash "$SRC/scripts/steamdeck/heal-box.sh" "$BOX"
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

# Status tray: rebuilt with the host, and retrofitted onto installs that predate it.
log "Rebuilding the status tray"
PUNKTFUNK_SRC="$SRC" PUNKTFUNK_BOX="$BOX" bash "$SRC/scripts/steamdeck/install-tray.sh"

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

# The system tuning install.sh applies, from this checkout's files (lib.sh). A stock Deck needs a
# sudo PASSWORD, so PROMPT for it rather than silently skipping (skipping = gamepads stay dead).
SUDO_OK=0
if sudo -n true 2>/dev/null; then
    SUDO_OK=1
elif [ -t 0 ]; then
    warn "sudo needs your password to (re)apply the gamepad udev rule, vhci-hcd, input group, and UDP buffers:"
    sudo -v && SUDO_OK=1 || true
fi
if [ "$SUDO_OK" = 1 ]; then
    apply_system_tuning
else
    warn "no usable sudo — SKIPPED gamepad/udev/vhci/UDP tuning (all root-only; no user-space alternative)."
    warn "A stock SteamOS 'deck' account has NO password — set one with 'passwd', then re-run. Gamepads stay"
    warn "Xbox-360 until this runs and you reboot."
fi
echo
warn "If the controller still shows as an Xbox 360 pad, REBOOT the Deck once — the 'input' group and the"
warn "vhci-hcd module only become live for the host service after a reboot."
GRANT_SRC="$SRC/scripts/headless/kde-authorized"
GRANT_DST="$HOME/.local/share/flatpak/db/kde-authorized"
if [ ! -s "$GRANT_DST" ] && [ -s "$GRANT_SRC" ]; then
    mkdir -p "$(dirname "$GRANT_DST")"
    install -m644 "$GRANT_SRC" "$GRANT_DST"
    ok "seeded KDE RemoteDesktop grant (Desktop-mode input)"
fi

# The host unit follows scripts/punktfunk-host.service; its ExecStart keeps this install's flags.
# GameStream is the console's setting and a unit flag locks its toggle, so an older install's
# --gamestream moves into the store and stays on.
HOST_UNIT="$UNITS/punktfunk-host.service"
EXEC="$(sed -n 's/^ExecStart=//p' "$HOST_UNIT" 2>/dev/null | head -n1)"
case "$EXEC" in *" --gamestream"*)
    if "$BIN" settings set gamestream true >/dev/null; then
        EXEC="${EXEC/ --gamestream/}"
        ok "GameStream moved to the console's Host settings (still on)"
    else
        warn "GameStream stays pinned in $HOST_UNIT, so the console can't change it"
    fi ;;
esac
if [ -n "$EXEC" ]; then
    write_host_unit "$EXEC"
    systemctl --user daemon-reload
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
