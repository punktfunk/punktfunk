#!/usr/bin/env bash
# Bootstrap an Ubuntu (24.04 "noble") NVIDIA-GPU VM to build/run the punktfunk Linux host
# and the M0 capture spike (headless Sway/wlroots -> PipeWire -> NVENC).
#
# Assumes the NVIDIA driver is ALREADY installed (verify-only). Installs: rustup toolchain,
# build deps, PipeWire/portal/wlroots/Sway, DRM/EGL/VA dev libs. Does NOT auto-reboot or edit
# GRUB — it prints exact commands when a reboot-requiring change (nvidia-drm modeset) is needed.
#
# Idempotent; safe to re-run. Usage: bash scripts/bootstrap-ubuntu.sh
set -euo pipefail

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m  !!\033[0m %s\n' "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------------------
log "Preflight"
# ---------------------------------------------------------------------------
if ! have apt-get; then
    warn "This script targets Ubuntu/Debian (apt). Aborting."
    exit 1
fi
CODENAME="$(. /etc/os-release 2>/dev/null && echo "${VERSION_CODENAME:-unknown}")"
case "$CODENAME" in
    noble) ok "Ubuntu 24.04 (noble) — the recommended target" ;;
    questing) ok "Ubuntu 25.10 (questing) — newer than the tested 24.04; M0 verified here (Sway 1.10)" ;;
    jammy) warn "Ubuntu 22.04 (jammy): Sway 1.7 is too old for the M0 path. \
Strongly prefer 24.04+, or build Sway/wlroots from source here." ;;
    *)     warn "Unrecognized release '$CODENAME' — proceeding, but package names are tuned for noble/questing." ;;
esac
SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"

# ---------------------------------------------------------------------------
log "Verifying the NVIDIA + NVENC stack (no install — you said it's set up)"
# ---------------------------------------------------------------------------
if have nvidia-smi; then
    nvidia-smi --query-gpu=name,driver_version --format=csv,noheader | sed 's/^/     GPU: /'
    DRV="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -1 | cut -d. -f1)"
    [ "${DRV:-0}" -ge 535 ] 2>/dev/null \
        && ok "driver ${DRV}.x (>=535 required for headless wlroots)" \
        || warn "driver appears <535; headless wlroots EGL/dmabuf needs >=535 (550+ recommended)."
    if nvidia-smi -q 2>/dev/null | grep -qi 'vGPU Software Licensed Product'; then
        nvidia-smi -q | grep -iE 'License Status|Licensed' | sed 's/^/     vGPU /' || true
        warn "This looks like NVIDIA vGPU — NVENC needs a valid vGPU license (vWS). \
Full PCI passthrough needs no license. Confirm the smoke-encode below actually succeeds."
    else
        ok "no vGPU license line — looks like bare-metal / PCI passthrough (NVENC unlicensed-OK)"
    fi
else
    warn "nvidia-smi not found — the NVIDIA driver is not installed/visible. M0 encode will fail."
fi

ldconfig -p 2>/dev/null | grep -qi 'libnvidia-encode.so' \
    && ok "libnvidia-encode.so present (runtime NVENC lib)" \
    || warn "libnvidia-encode.so not found by ldconfig — NVENC will fail at runtime."

# ---------------------------------------------------------------------------
log "Enabling universe + multiverse (needed for xdg-desktop-portal-wlr, libnvidia-egl-gbm1)"
# ---------------------------------------------------------------------------
$SUDO add-apt-repository -y universe   >/dev/null 2>&1 || warn "could not enable universe (continuing)"
$SUDO add-apt-repository -y multiverse >/dev/null 2>&1 || warn "could not enable multiverse (continuing)"
$SUDO apt-get update -y

# apt_install GROUP_LABEL pkg...   — required group (aborts on failure)
apt_install() { local label="$1"; shift; log "apt: $label"; $SUDO apt-get install -y "$@"; }
# apt_try GROUP_LABEL pkg...       — optional group (warns, continues)
apt_try() { local label="$1"; shift; log "apt (optional): $label"; $SUDO apt-get install -y "$@" || warn "$label: some packages unavailable on $CODENAME (continuing)"; }

apt_install "build toolchain"   build-essential pkg-config cmake clang libclang-dev nasm git curl ca-certificates
apt_install "PipeWire + dev"    pipewire pipewire-pulse wireplumber libpipewire-0.3-dev libspa-0.2-dev
apt_install "desktop portals"   xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk
apt_install "Sway + wlroots"    sway swaybg xwayland wlr-randr foot seatd
apt_install "Wayland dev"       libwayland-dev wayland-protocols wayland-utils libxkbcommon-dev
apt_install "Opus (audio enc)"  libopus-dev
apt_install "DRM/EGL/GBM/VA"    libdrm-dev libgbm-dev libgbm1 libegl-dev libegl1 libgles-dev mesa-common-dev libva-dev
apt_install "capture + dbus"    wf-recorder grim dbus-user-session drm-info mesa-utils
apt_try     "NVIDIA EGL platform (multiverse)"  libnvidia-egl-wayland1 libnvidia-egl-gbm1
apt_try     "libei (reis is pure-Rust so optional)"  libei-dev

# ---------------------------------------------------------------------------
log "NVIDIA GL/EGL userspace (headless GPU Wayland needs this — nvidia-utils alone is NOT enough)"
# ---------------------------------------------------------------------------
# nvidia-utils-NNN ships nvidia-smi and libnvidia-encode-NNN ships NVENC; neither ships the
# GL/EGL libs. Without libnvidia-gl-NNN there is no libEGL_nvidia.so.0 and no GLVND vendor JSON
# (/usr/share/glvnd/egl_vendor.d/10_nvidia.json), so libglvnd falls back to Mesa, wlroots
# can't init EGL on the NVIDIA GPU, Sway is forced to the pixman software renderer, and the
# ScreenCast portal then can't negotiate a dmabuf buffer format (capture fails). Install the
# GL package matching the running driver.
if have nvidia-smi; then
    if [ -e /usr/share/glvnd/egl_vendor.d/10_nvidia.json ] && ldconfig -p 2>/dev/null | grep -qi 'libEGL_nvidia'; then
        ok "NVIDIA GL/EGL userspace present (libEGL_nvidia + 10_nvidia.json)"
    elif [ -n "${DRV:-}" ]; then
        apt_try "NVIDIA GL/EGL userspace (libnvidia-gl-$DRV)" "libnvidia-gl-$DRV"
        [ -e /usr/share/glvnd/egl_vendor.d/10_nvidia.json ] \
            && ok "10_nvidia.json now present — GPU EGL should work" \
            || warn "10_nvidia.json still missing — install the libnvidia-gl package matching driver $DRV by hand."
    else
        warn "Couldn't determine the driver branch; install libnvidia-gl-<NNN> matching 'nvidia-smi' by hand."
    fi
fi

# ---------------------------------------------------------------------------
log "GPU device group membership (render + video) — required to open /dev/dri/*"
# ---------------------------------------------------------------------------
# wlroots opens the render node (/dev/dri/renderD128, group 'render') and the DRM card
# (/dev/dri/card*, group 'video'); both are 0660 on Ubuntu 24.04 and 26.04. Without
# membership Sway aborts with 'Permission denied' on the render node.
TARGET_USER="${SUDO_USER:-$USER}"
NEED_GROUPS=""
for g in render video; do
    id -nG "$TARGET_USER" 2>/dev/null | tr ' ' '\n' | grep -qx "$g" || NEED_GROUPS="$NEED_GROUPS $g"
done
if [ -n "$NEED_GROUPS" ]; then
    warn "$TARGET_USER is not in:$NEED_GROUPS — adding (takes effect on next LOGIN; re-login or reboot):"
    for g in $NEED_GROUPS; do echo "       $SUDO usermod -aG $g $TARGET_USER"; done
    $SUDO usermod -aG "$(echo "$NEED_GROUPS" | tr ' ' ',' | sed 's/^,//')" "$TARGET_USER" 2>/dev/null \
        && ok "added; LOG OUT AND BACK IN (or reboot) for it to apply" \
        || warn "could not usermod automatically — run the commands above."
else
    ok "$TARGET_USER already in render + video"
fi

# ---------------------------------------------------------------------------
log "Rust toolchain (rustup — never apt's rustc/cargo, which would conflict)"
# ---------------------------------------------------------------------------
if ! have cargo; then
    log "installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1090
    . "$HOME/.cargo/env"
fi
have cargo && { rustup component add clippy rustfmt >/dev/null 2>&1 || true; ok "$(rustc --version)"; }

# ---------------------------------------------------------------------------
log "Checking NVIDIA DRM modeset (required for any Wayland on NVIDIA)"
# ---------------------------------------------------------------------------
MODESET="$(cat /sys/module/nvidia_drm/parameters/modeset 2>/dev/null || echo '?')"
if [ "$MODESET" = "Y" ]; then
    ok "nvidia-drm.modeset=Y"
else
    warn "nvidia-drm.modeset=$MODESET — Wayland/wlroots will not start. Enable it, then REBOOT:"
    echo "       echo 'options nvidia-drm modeset=1 fbdev=1' | sudo tee /etc/modprobe.d/nvidia-drm.conf"
    echo "       sudo update-initramfs -u && sudo reboot"
fi
[ -e /dev/dri/renderD128 ] && ok "/dev/dri/renderD128 present (used for headless render + NVENC)" \
                           || warn "/dev/dri/renderD128 missing — check the NVIDIA driver / DRM."

# ---------------------------------------------------------------------------
log "Installing headless-Sway + portal config templates (only if absent)"
# ---------------------------------------------------------------------------
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_cfg() { # src dest
    if [ -e "$2" ]; then ok "exists, leaving as-is: $2"; else mkdir -p "$(dirname "$2")"; cp "$1" "$2"; ok "wrote $2"; fi
}
install_cfg "$HERE/headless/sway.config"  "$HOME/.config/sway/config"
install_cfg "$HERE/headless/xdpw.config"  "$HOME/.config/xdg-desktop-portal-wlr/config"
install_cfg "$HERE/headless/portals.conf" "$HOME/.config/xdg-desktop-portal/sway-portals.conf"

# ---------------------------------------------------------------------------
log "Done. Next steps:"
# ---------------------------------------------------------------------------
cat <<'NEXT'
  1. If modeset was not Y above: enable it and reboot (commands printed above).
  2. Confirm the core still builds + passes on Linux:
       cargo test --workspace
  3. Bring up the headless compositor (prints WAYLAND_DISPLAY, default wayland-1):
       bash scripts/headless/run-headless-sway.sh
  4. In a second shell on the same user, prove capture->NVENC end to end (no Rust yet):
       export XDG_RUNTIME_DIR=/run/user/$(id -u) WAYLAND_DISPLAY=wayland-1
       swaymsg -t get_outputs                         # confirm HEADLESS-1
       bash scripts/headless/capture-smoke-test.sh    # wf-recorder -> hevc_nvenc -> /tmp/*.mkv
  5. Then start M0 proper: see design/linux-setup.md.
NEXT
