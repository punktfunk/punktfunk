# Builder for the `punktfunk-gamescope` .deb — Debian 13 (trixie).
#
# Debian 13 is the oldest apt distro the tree builds on, so one .deb built here installs on Debian 13
# and Ubuntu 26.04. Ubuntu 24.04 cannot build it: no libxcb-errors-dev, libdisplay-info 0.1 only.
# Debian 13 is below the vendored wlroots 0.20's floors (wayland 1.24, libdrm 2.4.129, xkbcommon 1.8,
# pixman 0.46); build-punktfunk-gamescope.sh then links pinned copies into the compositor statically.
#
# Rebuilt+pushed by .gitea/workflows/docker.yml (matrix: punktfunk-gamescope-trixie); consumed by
# the `build-publish-gamescope` job in .gitea/workflows/deb.yml. Bootstrap: like rust-ci-noble, the
# first deb.yml run after this image is added needs the image to already exist — seed it once by
# hand (docker build -f ci/gamescope-trixie.Dockerfile -t <registry>/punktfunk-gamescope-trixie:latest ci
# && docker push …) before that job can run.
FROM debian:trixie
ENV DEBIAN_FRONTEND=noninteractive

# nodejs is not optional: the Gitea runner executes the JS actions (checkout/cache) INSIDE this
# container, so an image without it fails before the first `run:` step ever starts.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config cmake meson ninja-build git curl ca-certificates nodejs \
    # .deb assembly (dpkg-shlibdeps computes the runtime Depends from the built binary)
    dpkg-dev \
    # shader compilers gamescope's meson looks for
    glslc glslang-tools \
    # wayland + protocols for the WSI layer, which links the system libwayland-client. bison and
    # libexpat1-dev build the bundled xkbcommon and wayland-scanner.
    libwayland-dev wayland-protocols bison libexpat1-dev \
    # gamescope's own dependency set. `apt-get build-dep gamescope` is useless here — Debian has
    # no gamescope package to derive it from — so the tree's needs are named outright, exactly as
    # the noble job had to. Kept as ONE transaction on purpose: in an image build a missing name
    # SHOULD fail loudly at build time, unlike the workflow's per-package best-effort loop where a
    # rename would have silently dropped a dep into a warning nobody reads.
    libxdamage-dev libxcomposite-dev libxrender-dev libxext-dev libxxf86vm-dev \
    libxtst-dev libx11-dev libxres-dev libxmu-dev libxcursor-dev libxi-dev \
    libxfixes-dev libxkbcommon-dev libxkbcommon-x11-dev libcap-dev libdrm-dev \
    # x11-xcb is needed by the VULKAN WSI LAYER (layer/meson.build), not by the compositor — so it
    # was not missed until v0.28.1 started building the layer beside the binary. Debian is the only
    # channel that needs it named: Arch's libx11 and Fedora's libX11-devel both carry x11-xcb.pc
    # themselves, while Debian splits it into its own -dev package.
    libx11-xcb-dev \
    libinput-dev libudev-dev libpipewire-0.3-dev libseat-dev libsdl2-dev \
    libluajit-5.1-dev libavif-dev libdecor-0-dev hwdata libglm-dev libbenchmark-dev \
    libvulkan-dev libxcb1-dev libxcb-composite0-dev libxcb-xfixes0-dev libxcb-res0-dev \
    libxcb-ewmh-dev libxcb-icccm4-dev libxcb-errors-dev libxcb-shape0-dev \
    libpixman-1-dev libdisplay-info-dev libgbm-dev libegl-dev xwayland libeis-dev \
    && rm -rf /var/lib/apt/lists/*

# The layer's own floor, asserted for the same reason: a missing x11-xcb does not fail the
# COMPOSITOR build, it fails `layer/meson.build` — and the layer is the only route to an HDR10
# swapchain for a nested game, so losing it silently ships a package that looks healthy and denies
# every game HDR. This is exactly how v0.28.1's deb leg broke, one release after the layer was
# added; assert it here so the next dep the layer grows fails at image build, not mid-release.
RUN set -eux; \
    pkg-config --exists x11-xcb \
      || { echo "x11-xcb absent — the Vulkan WSI layer will not configure (need libx11-xcb-dev)" >&2; exit 1; }; \
    echo "x11-xcb $(pkg-config --modversion x11-xcb) — OK"
