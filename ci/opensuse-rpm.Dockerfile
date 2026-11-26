# CI builder for the punktfunk RPM on openSUSE Tumbleweed (registry group "opensuse-tumbleweed").
# The zypper twin of ci/fedora-rpm.Dockerfile. Used by .gitea/workflows/rpm.yml; built+pushed by
# .gitea/workflows/docker.yml.
#
#   docker build -f ci/opensuse-rpm.Dockerfile -t punktfunk-opensuse-rpm ci
#
# ROLLING-RELEASE TRADEOFF, the one ci/arch-ci.Dockerfile takes: the RPM builds against the
# Tumbleweed snapshot from the last image rebuild, not a fresh `zypper dup` per run. Any ci/ edit
# re-keys and re-snapshots it, and so does bumping the date in this line (refreshed: 2026-11-26).
# rpm.yml refuses to publish a set that a current Tumbleweed box can't install, which is the
# backstop for the day this image lags a library bump.
FROM docker.io/opensuse/tumbleweed

# ci-core's package mirror carries no openSUSE, so these come from download.opensuse.org, once
# per image build.
RUN zypper --non-interactive dist-upgrade \
  && zypper --non-interactive install --no-recommends \
      # rpmbuild and the source tarball. rpmsign ships in rpm itself. nodejs runs the Gitea
      # Actions JS (checkout, cache); zstd is the cache archive's compressor; gawk and findutils
      # are not in the base image and the packaging scripts use both.
      rpm-build systemd-rpm-macros git tar gzip zstd unzip curl ca-certificates \
      gawk findutils diffutils nodejs gpg2 \
      # scripts/ci/gitea-release.sh, on a tag
      /usr/bin/python3 \
      # build toolchain + bindgen
      gcc gcc-c++ clang clang-devel cmake nasm pkgconf-pkg-config \
      # capture/audio/display link deps
      pipewire-devel wayland-devel libxkbcommon-devel libopus-devel \
      Mesa-libGL-devel libgbm-devel \
      # punktfunk-client link deps (GTK4 shell + SDL3 gamepads)
      gtk4-devel libadwaita-devel SDL3-devel \
  && zypper clean --all

# bun — both the BUILD tool and the RUNTIME for the punktfunk-web console, which the RPM vendors.
# The pin lives in ci/bun.env.
COPY bun.env install-bun.sh /tmp/
RUN sh /tmp/install-bun.sh && rm /tmp/bun.env /tmp/install-bun.sh

# No libcuda stub here: packaging/rpm/build-rpm.sh writes one from the host source on every build.

# Rustup (not openSUSE's packaged rust) so rust-toolchain.toml's pinned channel resolves, matching
# every other builder. Shared location so jobs running as any uid can use it.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
COPY rustup.env install-rustup.sh /tmp/
# docker.yml passes rust-toolchain.toml's channel, so jobs find the pinned toolchain baked in.
ARG RUST_TOOLCHAIN=stable
RUN sh /tmp/install-rustup.sh -y --no-modify-path --profile minimal \
        --default-toolchain "$RUST_TOOLCHAIN" --component rustfmt,clippy \
    && rm /tmp/rustup.env /tmp/install-rustup.sh \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustc --version && cargo --version

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache. The version and its SHA-256 are the pin
# every builder image carries: bump them together, in all of them.
ARG SCCACHE_VERSION=0.10.0
ARG SCCACHE_SHA=1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b
RUN curl -fsSL -o /tmp/sccache.tar.gz \
      "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${SCCACHE_SHA}  /tmp/sccache.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/sccache.tar.gz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && rm -f /tmp/sccache.tar.gz \
    && sccache --version
