# Build and run image for the ABR netem rig. rustc comes from the stock image.
FROM rust:bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential clang libclang-dev pkg-config cmake \
      libpipewire-0.3-dev libopus-dev libwayland-dev libxkbcommon-dev \
      libgl-dev libegl-dev libgbm-dev \
      iproute2 iperf3 iputils-ping jq procps \
 && rm -rf /var/lib/apt/lists/*
