#!/usr/bin/env bash
# punktfunk — Steam Deck HOST installer (stream FROM the Deck to other devices).
#
# SteamOS is an immutable, read-only Arch base, so the host can't be a system package. Instead we
# build it natively inside a Debian-trixie distrobox — the binary then runs natively on SteamOS —
# and wire it up as proper systemd USER services. AMD encode uses VAAPI; NVIDIA uses NVENC
# (auto-detected).
#
# Run it on the Deck (Desktop Mode "Konsole", or over ssh). Idempotent — safe to re-run to update
# config or pick up new options. To rebuild after pulling new source, use update.sh.
#
#   bash scripts/steamdeck/install.sh                 # PIN pairing required; SECURE native-only
#   bash scripts/steamdeck/install.sh --gamestream    # ALSO serve Moonlight clients (opt-in; #5/#9 caveats)
#   bash scripts/steamdeck/install.sh --open          # trusted LAN: accept unpaired clients (TOFU)
#   bash scripts/steamdeck/install.sh --no-web        # skip the management web console
#   bash scripts/steamdeck/install.sh --web-bind=127.0.0.1  # console on this Deck only (default: your network)
#   PUNKTFUNK_SRC=~/src/punktfunk bash scripts/steamdeck/install.sh   # source elsewhere
#
set -euo pipefail

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
# Create a system group if it is missing (needs sudo). Idempotent, and mirrors what the
# deb/rpm/arch scriptlets do — a udev rule that chgrp's to a group nobody created fails silently.
ensure_group() {
    getent group "$1" >/dev/null 2>&1 && return 0
    sudo groupadd --system "$1" 2>/dev/null || return 1
    ok "created the '$1' system group"
}

# --- options ---------------------------------------------------------------
SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
BOX_IMAGE="${PUNKTFUNK_BOX_IMAGE:-docker.io/library/debian:trixie}"
MGMT_PORT="${PUNKTFUNK_MGMT_PORT:-47990}"
WEB_PORT="${PUNKTFUNK_WEB_PORT:-47992}"
# Where the console listens. Your network by default, since a Deck in Game Mode has no browser;
# the console refuses peers off the local network. --web-bind=127.0.0.1 keeps it to this Deck.
WEB_BIND="${PUNKTFUNK_UI_BIND:-0.0.0.0}"
[ -n "${PUNKTFUNK_UI_BIND:-}" ] && WEB_BIND_SET=1 || WEB_BIND_SET=0
OPEN=0
WITH_WEB=1
GAMESTREAM=0 # SECURE native-only by default (opt-in on every route); --gamestream adds Moonlight compat
for arg in "$@"; do
    case "$arg" in
        --open) OPEN=1 ;;
        --no-web) WITH_WEB=0 ;;
        --gamestream) GAMESTREAM=1 ;;
        --no-gamestream) GAMESTREAM=0 ;; # explicit-off kept for old command lines / re-runs
        --src=*) SRC="${arg#--src=}" ;;
        --web-bind=*) WEB_BIND="${arg#--web-bind=}"; WEB_BIND_SET=1 ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) die "unknown option: $arg (try --help)" ;;
    esac
done
TARGET_DIR="$SRC/target-steamos"
BIN="$TARGET_DIR/release/punktfunk-host"
# The PyroWave encode worker — a separate executable next to the host, and the ONLY binary this
# installer setcaps. Never a hardlink or a mode of $BIN: a shared inode shares the file capability
# and would void the KWin .desktop grant written below.
WORKER="$TARGET_DIR/release/punktfunk-encode-worker"
CONFIG="$HOME/.config/punktfunk"
UNITS="$HOME/.config/systemd/user"
XRD="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
# Set when this run does something that only a fresh login picks up (input-group add, first-time
# KWin .desktop grant). Drives the loud "reboot before streaming" note in the summary.
NEED_RELOGIN=0

# --- 0. preflight ----------------------------------------------------------
log "Preflight"
[ -f /etc/os-release ] && . /etc/os-release || true
case "${ID:-}${ID_LIKE:-}" in
    *steamos*|*arch*) ok "SteamOS / Arch base detected (${PRETTY_NAME:-unknown})" ;;
    *) warn "This installer targets SteamOS; '${PRETTY_NAME:-unknown}' may differ — on a normal distro use the apt/rpm packages instead." ;;
esac
[ -d "$SRC/crates/punktfunk-host" ] || die "no punktfunk source at $SRC. Clone or rsync it there first, or pass --src=DIR (see scripts/steamdeck/README.md)."
ok "source: $SRC"
if ! have distrobox; then
    die "distrobox not found. Install it once (no root needed):
       curl -sfL https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local
     then re-run this script (ensure ~/.local/bin is on PATH)."
fi
DISTROBOX="$(command -v distrobox)"   # baked into the web unit (may be /usr/bin or ~/.local/bin)
ok "distrobox: $DISTROBOX"

# --- acquire sudo up front (before the ~15-min build) ----------------------
# Steps 4-5 (UDP buffers, gamepad udev rule, vhci-hcd, input group, linger) need root. Prompt NOW,
# not after the build — so you authorize once and walk away, and a non-interactive run fails LOUDLY
# here instead of silently skipping the tuning at the very end. A stock SteamOS 'deck' has no
# password, so sudo can't work until you set one.
SUDO_OK=0
if sudo -n true 2>/dev/null; then
    SUDO_OK=1
elif [ -t 0 ]; then
    warn "sudo is needed once (UDP buffers, gamepad udev rule, vhci-hcd, input group, linger):"
    if sudo -v; then
        SUDO_OK=1
        # keep the sudo timestamp warm across the long build so steps 4-5 don't re-prompt / expire
        ( while sudo -n -v 2>/dev/null; do sleep 50; done ) &
        _pf_sudo_keepalive=$!
        trap '[ -n "${_pf_sudo_keepalive:-}" ] && kill "$_pf_sudo_keepalive" 2>/dev/null || true' EXIT
    fi
fi
if [ "$SUDO_OK" != 1 ]; then
    if [ -t 0 ]; then
        warn "No sudo — a stock SteamOS 'deck' account has no password. Set one and re-run:  passwd"
    else
        warn "No TTY for the sudo prompt (non-interactive run) — system tuning + linger will be SKIPPED."
        warn "Run in Konsole or an interactive 'ssh -t' session (or pre-authorize sudo) to enable them."
    fi
fi

# --- 1. build container + toolchain ---------------------------------------
log "Build container '$BOX' ($BOX_IMAGE)"
if distrobox list 2>/dev/null | awk -F'|' '{gsub(/ /,"",$2); print $2}' | grep -qx "$BOX"; then
    ok "container '$BOX' exists"
else
    log "creating '$BOX' (first time — pulls the image)…"
    distrobox create --yes --name "$BOX" --image "$BOX_IMAGE" --home "$HOME"
    ok "created '$BOX'"
fi

log "Provisioning build dependencies in '$BOX' (idempotent; apt + rustup + bun)"
# One non-interactive provisioning pass. APT deps mirror the Linux host build (PipeWire/DRM/EGL/
# VAAPI dev libs). rustup + bun are per-user under the shared $HOME.
distrobox enter "$BOX" -- bash -lc '
set -e
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq --no-install-recommends \
    build-essential pkg-config clang cmake curl git ca-certificates \
    libpipewire-0.3-dev libspa-0.2-dev \
    libgbm-dev libegl-dev libgl-dev libdrm-dev libva-dev \
    libxkbcommon-dev libudev-dev libssl-dev libopus-dev libsdl2-dev \
    nodejs >/dev/null
command -v rustc >/dev/null 2>&1 || command -v ~/.cargo/bin/rustc >/dev/null 2>&1 || \
    curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path >/dev/null
# bun builds AND runs the web console now (the Nitro `bun` preset + our Bun.serve TLS entry —
# bun-native output, so the old srvx mis-resolution that forced node no longer applies).
command -v bun >/dev/null 2>&1 || command -v ~/.bun/bin/bun >/dev/null 2>&1 || \
    curl -fsSL https://bun.sh/install | bash >/dev/null
'
ok "build deps ready"

# --- 2. build host (+ web) -------------------------------------------------
log "Building punktfunk-host (release) — first build is slow (~15 min)"
# nvenc,vulkan-encode matches the packaged builds (deb/arch/rpm): the direct-SDK NVENC backend
# plus the raw Vulkan Video HEVC/AV1 one (real RFI loss recovery). SteamOS also runs on NVIDIA
# desktops, where a host without nvenc advertises HEVC and then dies at encoder open. Both entry
# points are dlopen'd, so an AMD Deck pays nothing to carry them.
#
# The console tells one build from the next by its version string alone. Without the commit
# every rebuild reports the same X.Y.Z, and a finished update reads as "nothing newer".
# An empty value is ignored by the build script, which falls back to the Cargo version.
PF_BASE="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1)"
PF_SHA="$(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || true)"
PF_BUILD_VERSION=""
[ -z "$PF_BASE" ] || [ -z "$PF_SHA" ] || PF_BUILD_VERSION="$PF_BASE+g$PF_SHA"

# punktfunk-encode-worker is built alongside: the capability-carrying PyroWave encode worker, a
# SEPARATE binary that lands next to the host in $TARGET_DIR/release (which is how the host finds
# it — sibling of /proc/self/exe). The sudo block further down setcaps that one and only that one;
# the host must stay capability-free or KWin cannot identify it and Desktop mode dies.
distrobox enter "$BOX" -- bash -lc "
set -e
export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR' PUNKTFUNK_BUILD_VERSION='$PF_BUILD_VERSION'
cd '$SRC' && cargo build -r -p punktfunk-host -p punktfunk-encode-worker --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode
"
[ -x "$BIN" ] || die "build did not produce $BIN"
# ldd out here on SteamOS, never inside the box, where every soname resolves by construction. The
# container matches SteamOS's libraries by convention only; when that drifts the build succeeds
# and leaves a binary the OS cannot load.
MISSING="$({ ldd "$BIN" 2>/dev/null || true; } | awk '/not found/ {print $1}' | sort -u | tr '\n' ' ')"
[ -z "$MISSING" ] || die "the host built, but SteamOS cannot load it. Missing: $MISSING
     That is a library the build box has and SteamOS does not. Nothing was installed — please
     report it with 'cat /etc/os-release': https://git.unom.io/unom/punktfunk/issues"
ok "host binary: $BIN"
# Not fatal if it is missing — an absent worker just means the in-process encoder at default GPU
# priority, which is what every 0.26.x Deck already runs.
if [ -x "$WORKER" ]; then
    ok "encode worker: $WORKER"
else
    warn "no punktfunk-encode-worker at $WORKER — PyroWave will encode at default GPU priority"
fi

if [ "$WITH_WEB" = 1 ]; then
    log "Building the management web console (bun)"
    distrobox enter "$BOX" -- bash -lc "
set -e
export PATH=\$HOME/.bun/bin:\$PATH
# --ignore-scripts + explicit codegen: keep in step with scripts/steamdeck/update.sh, which
# explains why (web's `postinstall` writes the COMMITTED web/bun.nix; its `prepare`/codegen is
# required because src/api/gen, src/paraglide and src/routeTree.gen.ts are gitignored).
cd '$SRC/web' && bun install --frozen-lockfile --ignore-scripts && bun run codegen && bun run build
"
    [ -f "$SRC/web/.output/server/index.mjs" ] || die "web build did not produce web/.output/server/index.mjs"
    ok "web console built"
fi

# --- 2b. plugin runner (scripting) -----------------------------------------
# The console's plugin store and `punktfunk-host plugins …` shell out to the scripting runner —
# the SDK's runner CLI bundled to one self-contained JS, run on a pinned bun (it import()s the
# operator's .ts plugins; see packaging/debian/build-scripting-deb.sh). The .deb lays it out under
# /usr, which is read-only here, so ship the SAME payload user-scoped — wrapper + private bun +
# bundle under ~/.local, where the host's runner discovery looks after the /usr layouts.
log "Building the plugin runner (scripting)"
mkdir -p "$HOME/.local/bin" "$HOME/.local/lib/punktfunk-scripting" "$HOME/.local/share/punktfunk-scripting"
distrobox enter "$BOX" -- bash -lc "
set -e
export PATH=\$HOME/.bun/bin:\$PATH
cd '$SRC/sdk'
bun install --frozen-lockfile --ignore-scripts
bun build src/runner-cli.ts --target=bun --outfile \"\$HOME/.local/share/punktfunk-scripting/runner-cli.js\"
# Pin the runtime: copy the box's bun next to the bundle so a bun self-update (or a box rebuild)
# never changes what the runner executes under.
install -m0755 \"\$(command -v bun)\" \"\$HOME/.local/lib/punktfunk-scripting/bun\"
"
grep -q 'attempt=' "$HOME/.local/share/punktfunk-scripting/runner-cli.js" \
    || die "runner bundle missing the dynamic plugin import — wrong build"
cat > "$HOME/.local/bin/punktfunk-scripting" <<'WRAP'
#!/bin/sh
# Generated by scripts/steamdeck/install.sh — user-scoped punktfunk-scripting (the .deb's /usr/bin
# wrapper, relocated): the runner bundle on its private pinned bun.
exec "$HOME/.local/lib/punktfunk-scripting/bun" "$HOME/.local/share/punktfunk-scripting/runner-cli.js" "$@"
WRAP
chmod 0755 "$HOME/.local/bin/punktfunk-scripting"
ok "plugin runner: ~/.local/bin/punktfunk-scripting"

# --- 3. config -------------------------------------------------------------
log "Configuration ($CONFIG)"
mkdir -p "$CONFIG"
# Owner-only: this directory holds web.env (console password + session secret), the mgmt token and
# the host key. A plain `mkdir -p` leaves it 0755 at the Deck's default umask, so the secrets below
# sat in a world-TRAVERSABLE directory (2026-08-05 review L-19). Matches what the host itself does
# via `pf_paths::create_private_dir`, and is idempotent on an existing dir.
chmod 700 "$CONFIG" 2>/dev/null || true
if [ ! -f "$CONFIG/host.env" ]; then
    cat > "$CONFIG/host.env" <<'EOF'
# punktfunk Steam Deck host config (sourced by the punktfunk-host user service).
# Auto encoder: Vulkan Video (or VAAPI fallback) on the Deck's AMD GPU, NVENC on NVIDIA.
PUNKTFUNK_ENCODER=auto
# Van Gogh (LCD/OLED Deck) RADV still gates VK_KHR_video_encode_* behind this perftest flag;
# without it the Vulkan backend can't open and sessions fall back to VAAPI. Harmless on
# GPUs where encode is exposed by default.
RADV_PERFTEST=video_encode
# The host auto-detects the live session (Game Mode gamescope / Desktop KDE) per connect.
# Override the compositor only if detection misbehaves: PUNKTFUNK_COMPOSITOR=gamescope
EOF
    ok "wrote host.env"
else
    ok "host.env exists (left as-is)"
fi

# KWin authorization for Desktop-Mode streaming (and mid-stream Game↔Desktop switches): KWin
# resolves a connecting client's /proc/<pid>/exe against a .desktop `Exec=` and only then grants
# the restricted Wayland globals it lists (see packaging/linux/io.unom.Punktfunk.Host.desktop).
# Exec must therefore be THIS install's binary path, not the packaged /usr/bin one. KWin reads
# grants at session start — after first install, restart the Desktop session (Game Mode and back).
DESKTOP_DST="$HOME/.local/share/applications/io.unom.Punktfunk.Host.desktop"
# First-time install of the grant: KWin only reads it at session start, so a fresh login is required
# before Desktop-mode capture works. A re-run that just rewrites it needs no relogin.
[ -f "$DESKTOP_DST" ] || NEED_RELOGIN=1
mkdir -p "$HOME/.local/share/applications"
sed "s|^Exec=.*|Exec=$BIN|" "$SRC/packaging/linux/io.unom.Punktfunk.Host.desktop" > "$DESKTOP_DST"
ok "KWin desktop-capture authorization (io.unom.Punktfunk.Host.desktop → $BIN)"

# KDE Desktop-mode INPUT: a normal Plasma login lacks the RemoteDesktop portal grant the host's libei
# input path needs, so it would pop an "Allow remote control?" dialog a headless host can't answer.
# Seed it once (per-user, no root) — mirrors packaging/bazzite/kde-desktop-setup.sh. Game Mode
# (gamescope) needs none of this; the .desktop above already grants org_kde_kwin_fake_input.
GRANT_SRC="$SRC/scripts/headless/kde-authorized"
GRANT_DST="$HOME/.local/share/flatpak/db/kde-authorized"
if [ -s "$GRANT_DST" ]; then
    ok "KDE RemoteDesktop grant already present"
elif [ -s "$GRANT_SRC" ]; then
    mkdir -p "$(dirname "$GRANT_DST")"
    install -m644 "$GRANT_SRC" "$GRANT_DST"
    systemctl --user restart xdg-permission-store 2>/dev/null || true
    ok "seeded KDE RemoteDesktop grant (Desktop-mode input)"
fi

if [ "$WITH_WEB" = 1 ] && [ ! -f "$CONFIG/web.env" ]; then
    # Random login password + session secret for the web console, generated once.
    # `|| true` swallows the SIGPIPE `tr` takes when `head` closes the pipe (pipefail would abort).
    WEB_PW="$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom 2>/dev/null | head -c 12 || true)"
    WEB_SECRET="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom 2>/dev/null | head -c 32 || true)"
    # `umask 077` around the redirect, not `chmod 600` after it: the heredoc CREATES the file at
    # the ambient umask (0022 on a Deck ⇒ world-readable), so the console password and session
    # secret existed group/world-readable for the window between the redirect and the chmod.
    # Setting the mask first means the file is never readable at all. The chmod stays as the
    # idempotent belt for a pre-existing file. The console swaps the clear password for a salted
    # hash in this same file on the first sign-in, keeping PUNKTFUNK_UI_SECRET beside it.
    (umask 077; cat > "$CONFIG/web.env" <<EOF
PUNKTFUNK_UI_PASSWORD=$WEB_PW
PUNKTFUNK_UI_SECRET=$WEB_SECRET
# Where the console listens: 127.0.0.1 (this Deck), 0.0.0.0 (your network), or one address.
PUNKTFUNK_UI_BIND=$WEB_BIND
EOF
    )
    chmod 600 "$CONFIG/web.env"
    ok "wrote web.env (generated login password — read it before your first sign-in)"
    ok "console bind: $WEB_BIND"
elif [ "$WITH_WEB" = 1 ] && [ -f "$CONFIG/web.env" ]; then
    # THE belt the comment above promises. It used to live inside the create-only branch, so it
    # only ever ran on files that had just been written 0600 anyway — every install that predates
    # the L-19 fix still has its console password and session secret on disk at the Deck's ambient
    # umask (0644, world-readable). Tighten it here, and say so out loud: a chmod does not un-leak
    # a secret that was already readable by every local account, so the password needs rotating.
    if find "$CONFIG/web.env" -maxdepth 0 -perm /0077 2>/dev/null | grep -q .; then
        chmod 600 "$CONFIG/web.env"
        warn "web.env was group/world-readable — an older install wrote it at the default umask."
        warn "Tightened to 0600, but that does NOT un-expose the password it already leaked to every"
        warn "local account. Reset it: put a PUNKTFUNK_UI_PASSWORD line in $CONFIG/web.env, then"
        warn "  systemctl --user restart punktfunk-web"
    else
        ok "web.env exists (login password unchanged, mode already 0600)"
    fi
    # Name the bind once so the operator finds it; --web-bind on a re-run picks the value. An
    # existing line always wins — edit web.env to change it.
    if ! grep -q '^[[:space:]]*PUNKTFUNK_UI_BIND=' "$CONFIG/web.env"; then
        printf '# Where the console listens: 0.0.0.0 (your network), 127.0.0.1 (this Deck), or one address.\nPUNKTFUNK_UI_BIND=%s\n' "$WEB_BIND" >> "$CONFIG/web.env"
        ok "web.env: console bind is now $WEB_BIND"
    elif [ "$WEB_BIND_SET" = 1 ]; then
        warn "web.env already names PUNKTFUNK_UI_BIND — --web-bind was ignored; edit $CONFIG/web.env"
    fi
fi

# --- 3b. HDR gamescope (punktfunk-gamescope, best-effort) ------------------
# Stock gamescope offers only 8-bit capture, so Game Mode streams SDR. The punktfunk build adds
# the 10-bit BT.2020 PQ formats, and the host attempts HDR by default the moment it is present
# (PUNKTFUNK_GAMESCOPE_HDR=0 forces SDR). Best-effort by design: on any failure this warns and
# the host streams SDR — see build-gamescope.sh, which also wires PUNKTFUNK_GAMESCOPE_BIN into
# host.env only while the binary provably runs on SteamOS.
PUNKTFUNK_SRC="$SRC" PUNKTFUNK_BOX="$BOX" bash "$SRC/scripts/steamdeck/build-gamescope.sh"

# --- 4. system tuning (needs sudo: UDP buffers + udev rule + vhci-hcd + input/punktfunk groups) -----
log "System tuning (UDP buffers + gamepad rules + vhci-hcd + input/punktfunk groups)"
# sudo was acquired up front in preflight (SUDO_OK) so this never stalls behind the long build; a
# skip here (no password / no TTY) was already reported loudly there.
if [ "$SUDO_OK" = 1 ]; then
    printf 'net.core.wmem_max=33554432\nnet.core.rmem_max=33554432\n' \
        | sudo tee /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
    sudo sysctl -q -p /etc/sysctl.d/99-punktfunk-net.conf >/dev/null
    ok "UDP socket buffers raised to 32 MB (persisted)"
    # Nice-limit headroom for the host's data-plane threads (audio/send): without it (or rtkit,
    # which SteamOS does not guarantee) the per-thread renice silently no-ops and a busy game can
    # deschedule the 5 ms audio loop. SteamOS's /usr is read-only, so unlike the packaged installs
    # this lands in /etc — same drop-in, same effect, from the next login. NEVER a file capability
    # on the host binary (see the setcap note above — KWin identification).
    if [ -f "$SRC/packaging/linux/50-punktfunk-nice.conf" ]; then
        sudo install -Dm644 "$SRC/packaging/linux/50-punktfunk-nice.conf" \
            /etc/systemd/system/user@.service.d/50-punktfunk-nice.conf
        sudo systemctl daemon-reload || true
        ok "nice-limit drop-in installed (data-plane thread priority; applies from next login)"
    fi
    if [ -f "$SRC/scripts/60-punktfunk.rules" ]; then
        sudo install -m644 "$SRC/scripts/60-punktfunk.rules" /etc/udev/rules.d/60-punktfunk.rules
        sudo udevadm control --reload-rules && sudo udevadm trigger || true
        ok "installed udev rule (virtual gamepads + native Steam Deck controller)"
    fi
    # vhci-hcd: the usbip transport that makes the virtual Steam Deck pad a *real* USB device so Steam
    # Input adopts it (else it degrades to plain UHID, which Steam ignores — "no controller appears").
    # Persist the autoload AND load it now so passthrough works without waiting for a reboot.
    if [ -f "$SRC/scripts/punktfunk-modules.conf" ]; then
        sudo install -m644 "$SRC/scripts/punktfunk-modules.conf" /etc/modules-load.d/punktfunk.conf
        sudo modprobe vhci-hcd 2>/dev/null || warn "could not load vhci-hcd now (loads on next boot) — needed for the native Steam Deck pad"
        ok "vhci-hcd autoload installed (native Steam Deck controller transport)"
    fi
    if id -nG "$USER" | grep -qw input; then
        ok "already in the 'input' group"
    else
        sudo usermod -aG input "$USER"
        NEED_RELOGIN=1
        warn "added $USER to the 'input' group (applies on next login)"
    fi
    # The 'punktfunk' group owns the usbip vhci attach/detach nodes (see 60-punktfunk.rules).
    # Deliberately NOT 'input': writing 'attach' hands the kernel a caller-supplied socket fd and
    # materialises an arbitrary emulated USB device — a root-only primitive that must not ride on
    # the group every gamepad guide tells you to join (security-review 2026-08-05 M-4).
    #
    # The deb/rpm/arch scriptlets groupadd this; NOTHING on the Deck path did. So the rule we just
    # installed ran `chgrp punktfunk` against a group that did not exist, the chgrp failed, the
    # attach/detach files stayed root-only, and the native Steam Deck pad never attached — with no
    # error anywhere the user would look. Create it and join it here: unlike a general-purpose
    # host, running THIS script IS the statement "make my Deck a host with native pad passthrough".
    # `if ensure_group` (not `ensure_group || true`): a failed groupadd must not fall through to a
    # usermod against a group that does not exist, which under `set -e` would kill the installer
    # here — after the long build and before the services are installed.
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
        warn "could not create the 'punktfunk' group — the native Steam Deck pad will not attach"
        warn "(everything else works; the pad arrives as a generic Xbox 360 controller). By hand:"
        warn "  sudo groupadd --system punktfunk; sudo usermod -aG punktfunk $USER"
    fi
    # NO CAP_SYS_NICE on the host binary — and a removal of the one 0.26.0-1 granted here.
    #
    # 0.26.0-1 setcap'd this binary for the GPU-priority lever, which on a Van Gogh APU is a real
    # win. It also broke Desktop-mode streaming outright. Just above, this installer writes
    # ~/.local/share/applications/io.unom.Punktfunk.Host.desktop with Exec=$BIN so KWin will grant
    # the host its restricted protocols — and KWin makes that grant by resolving the client's
    # /proc/<pid>/exe and matching it against that Exec=. The kernel refuses that readlink to any
    # reader whose effective set is not a superset of the target's PERMITTED set
    # (cap_ptrace_access_check), and KWin holds no capabilities. So the capability silently voided
    # the .desktop written six lines earlier, and every Desktop-mode session died with
    # "KWin does not expose zkde_screencast_unstable_v1 to this client". Gaming Mode (gamescope) is
    # unaffected — it has no such gate. Full matrix in packaging/arch/punktfunk-host.install.
    #
    # Costs pacing only: pf-zerocopy walks REALTIME -> HIGH -> default when a class is refused.
    # `setcap -r` exits non-zero on a file that has no capability, hence the redirect.
    if [ -x "$BIN" ]; then
        sudo setcap -r "$BIN" 2>/dev/null || true
    fi
    # CAP_SYS_NICE on the ENCODE WORKER — the same grant, on the binary that can hold it, and the
    # Deck is the box that wants it most: one small Van Gogh GPU shared between the game and
    # PyroWave's compute-shader encode. punktfunk-encode-worker is a separate executable spawned
    # per session that speaks one socketpair to the host and never touches Wayland, D-Bus or the
    # network — nothing resolves ITS /proc/<pid>/exe, so the .desktop grant written above stays
    # valid. Every driver tested (RADV included) refuses EVERY elevated global-priority class
    # without this capability, so without it the lever is decoration.
    #
    # Like $BIN it lives under $HOME, so it survives a SteamOS A/B update on its own — but a
    # REBUILD is a new inode and file capabilities do not follow, so it must be re-applied after
    # every rebuild. Re-running this installer does that, and so does update.sh.
    #
    # Best-effort: a failure just means the encode runs at default priority, as it does today.
    if [ -x "$WORKER" ]; then
        if sudo setcap 'cap_sys_nice=ep' "$WORKER" 2>/dev/null; then
            ok "granted CAP_SYS_NICE to the encode worker (PyroWave can outrank a GPU-bound game)"
        else
            warn "could not grant CAP_SYS_NICE to $WORKER — PyroWave stays at default GPU priority"
        fi
    fi
    # SteamOS A/B updates rebuild /etc and DROP everything not on Valve's keep list — verified
    # live: an OS update stripped the udev rule + vhci autoload + UDP sysctl (gamepads silently
    # degrade to Xbox 360, buffers back to 208 KB). The sanctioned fix is a preserve drop-in in
    # /etc/atomic-update.conf.d/ (itself on the stock keep list, so it self-preserves).
    if [ -f "$SRC/scripts/punktfunk-atomic-keep.conf" ]; then
        sudo install -Dm644 "$SRC/scripts/punktfunk-atomic-keep.conf" /etc/atomic-update.conf.d/punktfunk.conf
        ok "system tuning registered to survive SteamOS updates (atomic-update.conf.d)"
    fi
else
    warn "no usable sudo — SKIPPED system tuning. Gamepad passthrough + clean streaming need root (udev"
    warn "rule, 'input' + 'punktfunk' groups, vhci-hcd, UDP buffers) — there is no user-space way to do these."
    warn "A stock SteamOS 'deck' account has NO password, so sudo can't work until you set one:"
    warn "  passwd            # set a sudo password once, then re-run this script"
    warn "Or apply it by hand (then reboot):"
    warn "  sudo install -m644 $SRC/scripts/60-punktfunk.rules /etc/udev/rules.d/ &&"
    warn "  sudo install -m644 $SRC/scripts/punktfunk-modules.conf /etc/modules-load.d/punktfunk.conf &&"
    warn "  sudo groupadd --system punktfunk;"
    warn "  sudo usermod -aG input,punktfunk $USER &&"
    warn "  printf 'net.core.wmem_max=33554432\\nnet.core.rmem_max=33554432\\n' | sudo tee /etc/sysctl.d/99-punktfunk-net.conf &&"
    warn "  sudo sysctl --system && sudo udevadm control --reload-rules && sudo udevadm trigger"
    warn "('punktfunk' owns the usbip vhci nodes the native Steam Deck pad attaches through — without"
    warn " it the pad silently never appears. Omit it if you do not want that pad.)"
fi

# --- 5. systemd user services ---------------------------------------------
log "Installing systemd user services"
mkdir -p "$UNITS"
# The native punktfunk/1 plane is always on; --gamestream additionally enables the Moonlight-compat
# planes — OPT-IN, matching every other install route (#5/#9 surface; the default is the secure
# native-only host, native clients only).
SERVE_ARGS="serve --mgmt-bind 0.0.0.0:$MGMT_PORT"
[ "$GAMESTREAM" = 1 ] && SERVE_ARGS="$SERVE_ARGS --gamestream"
[ "$OPEN" = 1 ] && SERVE_ARGS="$SERVE_ARGS --open"
cat > "$UNITS/punktfunk-host.service" <<EOF
# Generated by scripts/steamdeck/install.sh — punktfunk Steam Deck host (native binary).
[Unit]
Description=punktfunk host (punktfunk/1; +GameStream when installed with --gamestream)
After=pipewire.service

[Service]
EnvironmentFile=-%h/.config/punktfunk/host.env
Environment=XDG_RUNTIME_DIR=$XRD
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=$XRD/bus
ExecStart=$BIN $SERVE_ARGS
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
ok "punktfunk-host.service ($SERVE_ARGS)"

if [ "$WITH_WEB" = 1 ]; then
    # The console is a Nitro server run by bun (Bun.serve, HTTPS — HTTP/1.1 over TLS — with the host's
    # identity cert); it lives in the build container and proxies to the host's loopback HTTPS mgmt API.
    # No HOST/NITRO_HOST in the export below: web.env is sourced first and carries PUNKTFUNK_UI_BIND,
    # which the server prefers — an export here would silently out-rank the file the operator edits.
    cat > "$UNITS/punktfunk-web.service" <<EOF
# Generated by scripts/steamdeck/install.sh — punktfunk web console (bun in the '$BOX' distrobox).
[Unit]
Description=punktfunk management web console
After=punktfunk-host.service

[Service]
ExecStart=$DISTROBOX enter $BOX -- bash -lc 'cd $SRC/web; set -a; . $CONFIG/mgmt-token; . $CONFIG/web.env; set +a; export PUNKTFUNK_MGMT_URL=https://127.0.0.1:$MGMT_PORT PORT=$WEB_PORT NITRO_PORT=$WEB_PORT PUNKTFUNK_UI_TLS_CERT=$CONFIG/cert.pem PUNKTFUNK_UI_TLS_KEY=$CONFIG/key.pem PUNKTFUNK_UI_SECURE=1 PUNKTFUNK_UI_PASSWORD_FILE=$CONFIG/web.env; exec bun .output/server/index.mjs'
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF
    ok "punktfunk-web.service (port $WEB_PORT)"
fi

# The runner's user unit (OPT-IN, matching the .deb: installed but NOT enabled — the runner is
# inert until you add scripts/plugins). ExecStart is rewritten from the packaged /usr wrapper to
# the user-scoped one section 2b installed.
sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/punktfunk-scripting|' \
    "$SRC/scripts/punktfunk-scripting.service" > "$UNITS/punktfunk-scripting.service"
ok "punktfunk-scripting.service (opt-in: systemctl --user enable --now punktfunk-scripting)"

# Post-OS-update self-heal: SteamOS A/B updates can bump library sonames the host binary links
# (PipeWire, libva, …) — this oneshot probes the binary with ldd before punktfunk-host starts
# and re-runs update.sh only when it actually stopped loading. Milliseconds on a normal boot.
cat > "$UNITS/punktfunk-rebuild-check.service" <<EOF
# Generated by scripts/steamdeck/install.sh — rebuild the host if a SteamOS update broke its libs.
[Unit]
Description=punktfunk SteamOS post-update rebuild check
Before=punktfunk-host.service

[Service]
Type=oneshot
ExecStart=$SRC/scripts/steamdeck/rebuild-check.sh
# A cold-ish rebuild is minutes, not seconds.
TimeoutStartSec=1800

[Install]
WantedBy=default.target
EOF
chmod +x "$SRC/scripts/steamdeck/rebuild-check.sh" 2>/dev/null || true
systemctl --user enable punktfunk-rebuild-check.service 2>/dev/null || true
ok "punktfunk-rebuild-check.service (auto-rebuild after SteamOS updates)"

systemctl --user daemon-reload
loginctl show-user "$USER" 2>/dev/null | grep -q 'Linger=yes' || { sudo loginctl enable-linger "$USER" 2>/dev/null && ok "enabled linger (services run without login)" || warn "could not enable linger — services stop when you log out (sudo loginctl enable-linger $USER)"; }
# enable + restart (not `enable --now`): restart picks up unit-file changes on a re-run, where
# `--now` would no-op against an already-running service.
systemctl --user enable punktfunk-host.service 2>/dev/null
systemctl --user restart punktfunk-host.service
ok "punktfunk-host started"
if [ "$WITH_WEB" = 1 ]; then
    # The host writes the mgmt token on first start; give it a moment so the web unit finds it.
    for _ in $(seq 1 10); do [ -f "$CONFIG/mgmt-token" ] && break; sleep 0.5; done
    systemctl --user enable punktfunk-web.service 2>/dev/null
    systemctl --user restart punktfunk-web.service
    ok "punktfunk-web started"
fi
# A re-run rebuilds the runner (§2b); try-restart leaves an opted-out runner off.
systemctl --user try-restart punktfunk-scripting.service 2>/dev/null || true

# --- 6. summary ------------------------------------------------------------
IP="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -1 || true)"
echo
log "Done — punktfunk host is running on this Steam Deck"
echo "  • Host status:   systemctl --user status punktfunk-host"
if [ "$WITH_WEB" = 1 ]; then
    case "$WEB_BIND" in
        127.*|::1|localhost)
            echo "  • Web console:   https://127.0.0.1:$WEB_PORT   (this Deck only — see $CONFIG/web.env)" ;;
        *)
            echo "  • Web console:   https://${IP:-steamdeck.local}:$WEB_PORT   (login: see $CONFIG/web.env)" ;;
    esac
    echo "  • Pair a device: open the web console → Devices → arm pairing → enter the PIN on the client"
fi
if [ "$OPEN" = 1 ]; then
    echo "  • Mode: --open (unpaired clients accepted — trusted LAN only)"
else
    echo "  • Pairing required (secure default). From a client, pick this host and enter the PIN the host shows."
fi
echo "  • Update later:  bash $SRC/scripts/steamdeck/update.sh"
if [ "$NEED_RELOGIN" = 1 ]; then
    echo
    warn "ONE MORE STEP before streaming — reboot the Deck (or fully log out and back in)."
    echo "     KWin only authorizes Desktop-mode screen capture on a fresh session, and the new 'input'"
    echo "     group (native Steam Deck controller passthrough) only applies to a new login. Streaming"
    echo "     Game Mode with a generic Xbox pad works now; Desktop capture + the native Deck pad need the reboot."
fi
