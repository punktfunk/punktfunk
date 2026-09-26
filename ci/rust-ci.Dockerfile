# CI builder for the Rust workspace — Ubuntu 26.04 to match the dev/host boxes
# (PipeWire 1.6). Used by .gitea/workflows/ci.yml as the job container; rebuilt+pushed
# by .gitea/workflows/docker.yml.
#
#   docker build -f ci/rust-ci.Dockerfile -t punktfunk-rust-ci ci
#
# The workspace links real system libs at build time:
# PipeWire, Opus, GL/EGL/GBM — and libcuda, which has no real driver here; the
# zerocopy path only needs the symbols at link time, so a driver userspace package plus a
# libcuda.so -> libcuda.so.1 symlink stands in for it (CI never executes the CUDA path).
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    # toolchain + bindgen; nodejs runs the JS actions (checkout/cache); unzip extracts the pinned bun zip
    build-essential clang libclang-dev pkg-config cmake git curl ca-certificates nodejs unzip \
    # mold: the link-phase accelerator. Linking is the one thing sccache cannot cache, and this
    # image relinks the whole workspace on every job. Wired via cargo-config-mold.toml below.
    mold \
    # capture / audio / display stacks (+xkbcommon for the wlr input backend)
    libpipewire-0.3-dev libopus-dev libwayland-dev libxkbcommon-dev \
    # zerocopy link deps (GL via libglvnd, EGL, GBM)
    libgl-dev libegl-dev libgbm-dev \
    # punktfunk-client-linux (GTK4/libadwaita shell, SDL3 gamepads)
    libgtk-4-dev libadwaita-1-dev libsdl3-dev \
    # No libvulkan-dev: nothing in the workspace compiles or links against Vulkan (pyrowave-sys
    # bindgens its own vendored headers, and both host and client reach Vulkan through ash, which
    # dlopens the loader), so neither the build nor deb.yml's dpkg-shlibdeps ever asks for it.
    && rm -rf /var/lib/apt/lists/*

# bun — builds the punktfunk-web console in deb.yml (which runs the web build in THIS image).
# ci.yml's web/docs jobs use the oven/bun image instead, so this is only for the deb job.
#
# A PINNED release asset, checked by SHA-256 — never `curl https://bun.sh/install | bash`.
# build-web-deb.sh VENDORS this very binary into the punktfunk-web .deb, so the installer would be
# upstream code choosing bytes a signing job then publishes. ONE bun across the repo: same version,
# asset and sum as deb.yml and rpm.yml — bump BUN_VERSION and BUN_SHA together (the sums are in the
# release's SHASUMS256.txt). `-baseline` on purpose: it needs no AVX2, so the bun we ship starts on
# every x86-64 box — something the auto-detecting installer never promised, since it reads the
# BUILDER's CPU, not the user's.
ARG BUN_VERSION=1.4.2
ARG BUN_SHA=c678040f14fe0440eb839d37cbd0ce4c051a32da72806ac97de6a6aab6bf728f
RUN curl -fsSL -o /tmp/bun.zip \
      "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-x64-baseline.zip" \
    && echo "${BUN_SHA}  /tmp/bun.zip" | sha256sum -c - \
    && unzip -q -o -j /tmp/bun.zip '*/bun' -d /tmp \
    && install -m0755 /tmp/bun /usr/local/bin/bun \
    && rm -f /tmp/bun.zip /tmp/bun \
    && bun --version

# libcuda link stub: the NVIDIA userspace library (no kernel module needed) provides
# every cuXxx symbol. On 26.04 the package already ships the libcuda.so dev symlink;
# -sf keeps this idempotent if a future package drops it again.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libnvidia-compute-580-server \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf libcuda.so.1 /usr/lib/x86_64-linux-gnu/libcuda.so \
    && test -e /usr/lib/x86_64-linux-gnu/libcuda.so.1

# Toolchain shared across CI users (jobs may run as different uids).
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
        --component rustfmt,clippy \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustc --version && cargo clippy --version && cargo fmt --version

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache (backend = RustFS S3 on the LAN,
# see .gitea/workflows — the env lives there so dev use of this image stays uncached).
# musl build: one static binary serves the Ubuntu and Fedora images alike.
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

# Prebuilt Skia for the jobs' SKIA_BINARIES_URL=file:///opt/skia-binaries/…, checked by SHA-256.
COPY skia-binaries.sh /tmp/
RUN sh /tmp/skia-binaries.sh x86_64-unknown-linux-gnu wasm32-unknown-emscripten \
    && rm /tmp/skia-binaries.sh

# Link x86_64 with mold (see the file's own header for the rustflags-precedence traps).
#
# The assertion is the point: an image carrying the flag but NOT the linker would fail every cargo
# invocation in every consuming job, which is a catastrophic way to find out that a base image
# renamed the package. `mold --version` fails the docker build instead, so nothing is pushed and
# `:latest` keeps pointing at the previous working image — consumers never see it.
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml
