#!/usr/bin/env bash
# Build the two rig binaries inside the container. Runs as the container's entry
# for `run.sh --build-only`, and again at the head of every run.
set -euo pipefail

export CARGO_TARGET_DIR=/target
export CARGO_HOME=${CARGO_HOME:-/cargohome}

cd /w
cargo build --release -p punktfunk-host --bin punktfunk-host
cargo build --release -p punktfunk-probe --bin punktfunk-probe
ls -l /target/release/punktfunk-host /target/release/punktfunk-probe
