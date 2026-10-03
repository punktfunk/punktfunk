# Builder for the `punktfunk-gamescope` .deb — Debian 13 (trixie).
#
# WHY THIS EXISTS, AND WHY IT IS NOT THE NOBLE IMAGE:
# The gamescope .deb was built in the host job's Ubuntu 24.04 (noble) image, and it has NEVER once
# succeeded there — v0.26.0 and v0.27.0 both shipped with no gamescope package while the release
# notes and docs-site said it was apt-installable. The failure is structural, not a flaky dep:
#
#   wlroots| Dependency wayland-server found: NO found 1.22.0 but need: '>=1.23.1'
#   subprojects/wlroots/meson.build:96:17: ERROR: Dependency 'wayland-server' is required but not found
#
# Our gamescope pin vendors wlroots 0.19.3, which floors wayland-server at 1.23.1. Noble ships
# 1.22.0 and will never ship more — so no amount of `apt-get install` in that image can fix it.
# Noble also has no `libxcb-errors-dev` at all and only libdisplay-info 0.1.1 (the tree wants 0.2).
#
# Debian 13 ships wayland 1.23.1 exactly, libxcb-errors 1.0.1 and libdisplay-info 0.2.0 — the
# oldest apt distro the tree actually builds on. Building HERE rather than on Ubuntu 26.04
# (wayland 1.24, libdisplay-info 0.3) is deliberate twice over: it keeps the glibc floor low, and
# it stays on the libdisplay-info 0.2 line the pin was developed against.
#
# WHAT THE RESULTING BINARY RUNS ON — verified by building it and reading the ELF:
#   * glibc floor  GLIBC_2.38  (the C++ runtime is linked statically by
#     build-punktfunk-gamescope.sh, so libstdc++ never enters the NEEDED list)
#   * NEEDED       libwayland-server.so.0 / libwayland-client.so.0 — wlroots 0.19 calls symbols
#                  added in 1.23.1, so THAT, not glibc, is the real floor.
#   ⇒ Debian 13 (1.23.1) and Ubuntu 26.04 (1.24.0) YES; Ubuntu 24.04 (1.22.0) NO — and 24.04
#     could not run this binary however it was built, so nothing is lost by moving off noble.
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
    # wayland + protocols. libwayland-dev 1.23.1 is the whole reason this image is Debian.
    libwayland-dev wayland-protocols \
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

# Assert the ONE version that decides whether this image can do its job, so a future Debian base
# bump that regressed it fails HERE (loudly, at image build) instead of in a deb.yml run whose
# gamescope failure has historically been a `::warning::` nobody saw.
RUN set -eux; \
    have="$(pkg-config --modversion wayland-server)"; \
    pkg-config --atleast-version=1.23.1 wayland-server \
      || { echo "wayland-server $have < 1.23.1 — the vendored wlroots will not configure" >&2; exit 1; }; \
    echo "wayland-server $have — OK"

# The layer's own floor, asserted for the same reason: a missing x11-xcb does not fail the
# COMPOSITOR build, it fails `layer/meson.build` — and the layer is the only route to an HDR10
# swapchain for a nested game, so losing it silently ships a package that looks healthy and denies
# every game HDR. This is exactly how v0.28.1's deb leg broke, one release after the layer was
# added; assert it here so the next dep the layer grows fails at image build, not mid-release.
RUN set -eux; \
    pkg-config --exists x11-xcb \
      || { echo "x11-xcb absent — the Vulkan WSI layer will not configure (need libx11-xcb-dev)" >&2; exit 1; }; \
    echo "x11-xcb $(pkg-config --modversion x11-xcb) — OK"
