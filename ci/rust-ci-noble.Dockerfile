# LTS builder for the punktfunk HOST .deb — Ubuntu 24.04 (noble), the current Ubuntu LTS.
#
# WHY THIS EXISTS (see packaging/debian/README.md → "Ubuntu 24.04 LTS"):
# The default builder (ci/rust-ci.Dockerfile) is Ubuntu 26.04, so the host .deb it produces bakes
# in a glibc 2.41 floor and is uninstallable on Ubuntu 24.04 LTS (glibc 2.39) — apt reports the
# dep as "too recent". Building the host on 24.04 instead lowers the floor to 2.39, so the binary
# runs on 24.04 → 26.04. Everything the host links (PipeWire, Wayland, xkbcommon, GL/EGL/GBM,
# Vulkan; opus is vendored via cmake) is soname-compatible across that range, so this ONE
# universal host .deb replaces the 26.04-built one for every Ubuntu user.
#
# libcuda is deliberately NOT provided: the host dlopen's libcuda.so.1 at runtime (pf-zerocopy /
# pf-encode) and never link-imports it, so — unlike the full-workspace rust-ci image, which builds
# tests that DO link a cuda stub — this host-only build needs no NVIDIA driver package. NVENC/EGL
# come from whatever driver the target runs, out of band.
#
# Rebuilt+pushed by .gitea/workflows/docker.yml (matrix: punktfunk-rust-ci-noble); consumed by the
# `build-publish-host` job in .gitea/workflows/deb.yml. Bootstrap: like rust-ci, the first deb.yml
# run after this image is added uses the image from a PRIOR docker.yml push — seed it once manually
# (docker build -f ci/rust-ci-noble.Dockerfile -t … ci && docker push) before the host job can run.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    # toolchain + bindgen; nodejs runs the JS actions (checkout/cache); unzip for the rustup installer's deps
    build-essential clang libclang-dev pkg-config cmake git curl ca-certificates nodejs unzip \
    # zstd: actions/cache compresses with it on all cores instead of single-threaded gzip.
    zstd \
    # mold: link-phase accelerator (sccache cannot cache linking). This image links the release
    # host + encode worker on every deb.yml run. Wired via cargo-config-mold.toml below.
    mold \
    # .deb assembly: dpkg-shlibdeps/dpkg-deb
    dpkg-dev \
    # libdrm: render-node enumeration. libva itself is dlopen'd by pf-libva, headers not needed.
    libdrm-dev \
    # host link deps present on 24.04 with sonames compatible up to 26.04
    libpipewire-0.3-dev libwayland-dev libxkbcommon-dev \
    libgl-dev libegl-dev libgbm-dev libvulkan-dev \
    && rm -rf /var/lib/apt/lists/*

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

# Link x86_64 with mold — see cargo-config-mold.toml's header for the rustflags traps, and
# rust-ci.Dockerfile for why the `mold --version` assertion sits next to the COPY.
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml
