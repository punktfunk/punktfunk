# Arch CI builder: base-devel + every dependency arch.yml's two makepkg legs used to
# pacman-install per run (~1 GB of mirror traffic each time) + bun + sccache + nodejs
# (JS actions exec node INSIDE the job container — the same lesson as android-ci).
# Content-keyed and rebuilt only when the ci/ tree changes (docker.yml `builders`).
#
#   docker build -f ci/arch-ci.Dockerfile -t punktfunk-arch-ci ci
#
# ROLLING-RELEASE TRADEOFF, on purpose: packages now build against the Arch snapshot
# from the last image rebuild instead of a fresh -Syu per run. That is the same staleness
# the gamescope cache already embraces ("a stale binary against newer system libs is the
# same risk the distro's own package carries between rebuilds"), and any ci/ edit — or
# bumping the date in this line (refreshed: 2026-08-08) — re-keys and re-snapshots it.
#
# ⚠ AND KNOW WHY THAT IS NOT ENOUGH ON ITS OWN: bumping this date only helps once docker.yml has
# actually republished the image, and nothing sequences the two workflows. A release tagged four
# minutes after a system-library major once published a punktfunk-host that no up-to-date Arch
# box could install — which blocks the user's ENTIRE `pacman -Syu`, not just our package. So
# arch.yml refuses to publish anything a pristine-db `pacman -U --print` says is unsatisfiable.
# This file staying current is still the CHEAP path — that gate is the backstop, not the plan.
FROM docker.io/library/archlinux:base-devel

# Distro packages come through ci-core's LAN mirror, at build time and in every job.
COPY pkg-mirror.sh /tmp/
RUN sh /tmp/pkg-mirror.sh && rm /tmp/pkg-mirror.sh

# One transaction: the main build/runtime deps (first list) + the gamescope companion's
# deps (second list) — both copied verbatim from what arch.yml installed in-job, where
# they now no-op as `--needed` guards.
# vulkan-headers rides the first list only because arch.yml's copy does; the package it actually
# serves is the gamescope companion (packaging/gamescope/PKGBUILD makedepends). punktfunk itself
# needs no system Vulkan headers — pyrowave-sys bindgens its own vendored copy and ash dlopens the
# loader — but arch.yml builds gamescope with `makepkg -d`, so an absent makedepend would not be
# reported as a missing dependency, only as a compile failure. Keep it.
RUN pacman -Syu --noconfirm --needed \
        git nodejs rust clang cmake ninja nasm pkgconf python vulkan-headers \
        gtk4 libadwaita sdl3 pipewire wayland libxkbcommon opus libei \
        mesa libglvnd unzip libarchive \
        glslang libcap libdrm libinput libx11 libxcomposite libxdamage libxext \
        libxmu libxrender libxres libxtst libxxf86vm libavif libdecor \
        hwdata luajit seatd sdl2-compat vulkan-icd-loader \
        xcb-util-errors xcb-util-wm xorg-xwayland \
        meson glm wayland-protocols benchmark libxcursor \
        # mold: link-phase accelerator (sccache cannot cache linking). makepkg links the release
        # host, client, worker and tray on every arch.yml run. Wired via cargo-config-mold.toml
        # below. It does NOT affect the gamescope companion leg — that is meson + its own linker,
        # and its `-static-libstdc++` link is untouched.
        mold \
    && pacman -Scc --noconfirm

# bun builds the punktfunk-web console + the punktfunk-scripting runner AND is vendored as their
# runtime (PF_WITH_WEB=1 / PF_WITH_SCRIPTING=1), so these bytes end up in the package arch.yml
# signs. The pinned asset from ci/bun.env, not [extra]'s rolling bun.
COPY bun.env install-bun.sh /tmp/
RUN sh /tmp/install-bun.sh && rm /tmp/bun.env /tmp/install-bun.sh

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache (backend = RustFS S3 on the LAN,
# see .gitea/workflows — the env lives there so dev use of this image stays uncached).
# Checked by SHA-256, like the bun pin: sccache is RUSTC_WRAPPER, so it sits in front of every
# rustc invocation that produces a SHIPPED binary. Bump SCCACHE_VERSION and SCCACHE_SHA together —
# upstream publishes the sum as <asset>.tar.gz.sha256 next to the release asset.
ARG SCCACHE_VERSION=0.10.0
ARG SCCACHE_SHA=1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b
RUN curl -fsSL -o /tmp/sccache.tar.gz \
      "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${SCCACHE_SHA}  /tmp/sccache.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/sccache.tar.gz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && rm -f /tmp/sccache.tar.gz \
    && sccache --version

# CARGO_HOME is declared here only so this image agrees with what arch.yml already sets at job
# level (and so `cargo` finds the config below when the image is used by hand). The workflow still
# passes CARGO_HOME explicitly across the `sudo -u builder env …` boundary, which strips ambient
# env — that is why the C/C++ sccache wiring has to be re-exported there by name while THIS file,
# being a file, crosses the boundary for free.
ENV CARGO_HOME=/usr/local/cargo
RUN mkdir -p /usr/local/cargo && chmod -R a+w /usr/local/cargo

# Link x86_64 with mold — see cargo-config-mold.toml's header for the rustflags traps, and
# rust-ci.Dockerfile for why the `mold --version` assertion sits next to the COPY.
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml
