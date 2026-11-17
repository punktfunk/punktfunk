#!/usr/bin/env bash
# Launch headless Sway on an NVIDIA box for punktfunk capture.
#
# Runs on the user's *shared* session bus (NOT a private dbus-run-session) so that the
# ScreenCast portal (xdg-desktop-portal-wlr) and the punktfunk host share one bus. After this
# is up, run `prepare-session.sh` from a second shell to set the mode + portal env.
#
# Prereqs (see design/linux-setup.md / scripts/bootstrap-ubuntu.sh):
#   - nvidia-drm.modeset=Y
#   - the NVIDIA GL/EGL userspace (libnvidia-gl-NNN) — provides libEGL_nvidia + the GLVND
#     vendor JSON; without it wlroots can't init EGL on the GPU and falls back to pixman,
#     which the ScreenCast portal cannot capture.
#   - membership in the `render` + `video` groups (re-login after `usermod -aG`).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$HERE/env.sh"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
mkdir -p "$XDG_RUNTIME_DIR" 2>/dev/null || true
chmod 700 "$XDG_RUNTIME_DIR" 2>/dev/null || true

echo "starting headless Sway (renderer=$WLR_RENDERER) on bus $DBUS_SESSION_BUS_ADDRESS"
echo "from another shell on this user:    bash $HERE/prepare-session.sh"
echo

# wlroots opens the render node (/dev/dri/renderD128, group 'render'); NVIDIA's EGL uses it.
# If this login predates `usermod -aG render,video` (groups not yet active), bridge with
# `sg render` for this launch. The clean fix is to re-login so the groups apply natively.
if id -nG | tr ' ' '\n' | grep -qx render; then
    exec sway --unsupported-gpu
else
    echo "note: 'render' group not active in this login — bridging via 'sg render' (re-login to avoid)"
    exec sg render -c 'sway --unsupported-gpu'
fi
