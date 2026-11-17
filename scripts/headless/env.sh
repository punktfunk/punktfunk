# shellcheck shell=bash
# Source before launching headless Sway / the punktfunk host on an NVIDIA VM:
#   source scripts/headless/env.sh
# wlroots settings for a headless NVIDIA rig.

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export XDG_CURRENT_DESKTOP=sway          # so xdg-desktop-portal selects the wlr backend
export WLR_BACKENDS=headless             # no physical connectors on a headless VM GPU
export WLR_LIBINPUT_NO_DEVICES=1         # don't error on having no input devices
export WLR_NO_HARDWARE_CURSORS=1         # software cursor; headless has no cursor plane
export WLR_RENDERER=gles2                # wlroots' first pick; pinned, so no Vulkan fallback
export __GLX_VENDOR_LIBRARY_NAME=nvidia  # GLVND: dispatch GL/EGL to the NVIDIA driver
export GBM_BACKEND=nvidia-drm            # route GBM through nvidia-drm (libnvidia-egl-gbm1)
# Escape hatch to prove the pipeline with a software renderer if EGL/GBM misbehaves:
#   export WLR_RENDERER_ALLOW_SOFTWARE=1 WLR_RENDERER=pixman
