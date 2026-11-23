# Cross-compiling CI builder: amd64 host toolchain + an arm64 multiarch sysroot, for the
# aarch64 Linux CLIENT artifacts (punktfunk-client + punktfunk-session).
#
#   docker build -f ci/rust-ci-arm64cross.Dockerfile -t punktfunk-rust-ci-arm64cross .
#
# Derived from punktfunk-rust-ci so the Rust toolchain, clang, and CMake are byte-identical
# to the amd64 legs — this image only adds the target side. Kept as a SEPARATE image rather
# than folded into the base because the :arm64 dev libs are ~1 GB that every other CI job
# would otherwise pull for nothing.
#
# Client only: the Linux HOST stays amd64 (its encode stack is NVENC/QSV/AMF), so none of the
# host's CUDA/GBM link deps are mirrored here.
#
# Ubuntu splits archives by architecture: amd64 lives on archive.ubuntu.com, every port
# (arm64 included) on ports.ubuntu.com. Both stanzas therefore have to be pinned with an
# explicit `Architectures:` or apt tries to fetch arm64 from the amd64 mirror and 404s.
#
# Built from the REPO ROOT context (not ci/) — see the rust-toolchain.toml copy below.
FROM 192.168.1.58:5010/punktfunk-rust-ci:latest

ENV DEBIAN_FRONTEND=noninteractive

# 1. Pin the stock sources to amd64, add ports.ubuntu.com for arm64 (through ci-core's mirror,
#    like the sources rust-ci already rewrote).
RUN sed -i 's|^Types: deb$|Types: deb\nArchitectures: amd64|' /etc/apt/sources.list.d/ubuntu.sources \
    && . /etc/os-release \
    && printf 'Types: deb\nArchitectures: arm64\nURIs: http://192.168.1.58:8090/ports.ubuntu.com/ubuntu-ports/\nSuites: %s %s-updates %s-backports %s-security\nComponents: main universe restricted multiverse\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n' \
        "$VERSION_CODENAME" "$VERSION_CODENAME" "$VERSION_CODENAME" "$VERSION_CODENAME" \
        > /etc/apt/sources.list.d/ubuntu-ports-arm64.sources \
    && dpkg --add-architecture arm64

# 2. The cross toolchain + every arm64 dev lib the client links. Mirrors the client half of
#    rust-ci.Dockerfile's list (PipeWire, Opus, SDL3, GTK4/libadwaita, xkbcommon). No
#    Vulkan dev package: nothing compiles or links against Vulkan — ash dlopens the loader, and
#    pyrowave-sys bindgens its own vendored headers.
RUN apt-get update && apt-get install -y --no-install-recommends \
    crossbuild-essential-arm64 \
    libpipewire-0.3-dev:arm64 libopus-dev:arm64 \
    libsdl3-dev:arm64 libgtk-4-dev:arm64 libadwaita-1-dev:arm64 \
    libwayland-dev:arm64 libxkbcommon-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

# 3. The Rust target — installed against the toolchain the WORKSPACE pins, not the image's
#    default. The base image bakes whatever `stable` was at its build time, while every build
#    in the repo switches to the exact channel in rust-toolchain.toml; adding the target to
#    the default toolchain instead leaves the pinned one without an aarch64 std, and the build
#    dies on `can't find crate for core` a few hundred crates in. Running rustup from a
#    directory that contains the pin file resolves the right toolchain (and pre-downloads it,
#    which every workspace job would otherwise pay for on first use).
COPY rust-toolchain.toml /opt/pf-toolchain/
WORKDIR /opt/pf-toolchain
RUN rustup target add aarch64-unknown-linux-gnu && rustup show
WORKDIR /

# Prebuilt aarch64 Skia next to the base image's x86_64 one, checked by SHA-256 (see the script).
COPY ci/skia-binaries.sh /tmp/
RUN sh /tmp/skia-binaries.sh aarch64-unknown-linux-gnu && rm /tmp/skia-binaries.sh

# 4. Cross wiring. Everything in this image is a cross build, so the plain (un-suffixed)
#    variables are safe and cover the crates that roll their own pkg-config/bindgen calls
#    instead of going through the target-scoped lookups.
#      * PKG_CONFIG uses Debian's multiarch wrapper, which resolves the arm64 .pc files and
#        rewrites -I/-L into the sysroot without per-crate cooperation.
#      * BINDGEN_EXTRA_CLANG_ARGS: clang defaults to the host triple, so bindgen would parse
#        arm64 headers with amd64 type layouts (silently wrong, not a build error) — the
#        explicit --target plus the multiarch include dir is what keeps the layouts honest.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    PKG_CONFIG=aarch64-linux-gnu-pkg-config \
    PKG_CONFIG_ALLOW_CROSS=1 \
    BINDGEN_EXTRA_CLANG_ARGS="--target=aarch64-unknown-linux-gnu -I/usr/include/aarch64-linux-gnu"

# Fail the BUILD, not some later CI job, if the wrapper or a sysroot .pc is missing.
RUN command -v aarch64-linux-gnu-pkg-config \
    && aarch64-linux-gnu-pkg-config --cflags sdl3 gtk4 libpipewire-0.3 \
    && aarch64-linux-gnu-gcc -dumpmachine | grep -q aarch64
