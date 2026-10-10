#!/usr/bin/env bash
# Re-vendors PyroWave (+ the minimal Granite subset it builds against) into
# crates/codec/pyrowave-sys/vendor/pyrowave. Network access required; run manually,
# never from CI or build.rs (the flatpak/CI builders are offline — that is the
# whole reason the tree is committed).
#
# ⚠️ Bumping PYROWAVE_COMMIT is a protocol-affecting change: the PyroWave
# bitstream has no version field, so the CODEC_PYROWAVE wire bit means
# "PyroWave bitstream as of this pin" (design/pyrowave-codec-plan.md §4.2).
# A bump that changes the bitstream must bump the punktfunk protocol version,
# and the Apple Metal hand-port (§4.7) must re-diff the two decode shaders +
# bitstream header structs.
set -euo pipefail

PYROWAVE_COMMIT=c0b997f84ced7bd827ca737aa5145f4ec811de8d
# The Granite pin + submodule set come from upstream's checkout_granite.sh at
# that commit; recorded here for the vendor manifest only.

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$REPO_ROOT/crates/codec/pyrowave-sys/vendor/pyrowave"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

git clone https://github.com/Themaister/pyrowave "$WORK/pyrowave"
git -C "$WORK/pyrowave" checkout "$PYROWAVE_COMMIT"
(cd "$WORK/pyrowave" && bash checkout_granite.sh)

GRANITE_COMMIT="$(git -C "$WORK/pyrowave/Granite" rev-parse HEAD)"
VOLK_COMMIT="$(git -C "$WORK/pyrowave/Granite/third_party/volk" rev-parse HEAD)"
VKHDR_COMMIT="$(git -C "$WORK/pyrowave/Granite/third_party/khronos/vulkan-headers" rev-parse HEAD)"

cd "$WORK/pyrowave"
rm -rf .git Granite/.git
rm -f Granite/third_party/volk/.git Granite/third_party/khronos/vulkan-headers/.git
# Upstream's own .gitignore files ignore the Granite checkout (`/Granite`) —
# fatal for a committed vendor tree; strip them all.
find . -name .gitignore -delete

# Everything below is never entered by the standalone configure that
# crates/codec/pyrowave-sys/CMakeLists.txt performs (GRANITE_SHIPPING=ON,
# GRANITE_RENDERER=OFF, GRANITE_PLATFORM=null, no PYROWAVE_DEVEL) — verified
# empirically: configure fails loudly if a needed dir goes missing.
# third_party/renderdoc stays: Granite adds it unconditionally.
rm -rf Granite/renderer Granite/ui Granite/scene-export \
       Granite/audio Granite/physics Granite/tests Granite/tools \
       Granite/viewer Granite/assets Granite/slangmosh Granite/network \
       Granite/.github Granite/third_party/mikktspace
# pyrowave_c.cpp compiles Granite's scaler; the rest of video/ is FFmpeg glue.
find Granite/video -mindepth 1 ! -name scaler.cpp ! -name scaler.hpp -delete
# Not built here: the evaluation data and the official Metal port (the Apple
# client carries its own decode port).
rm -rf assets eval-results metal
# vulkan-headers: the build needs the C headers + CMake package only.
rm -rf Granite/third_party/khronos/vulkan-headers/registry \
       Granite/third_party/khronos/vulkan-headers/tests
rm -f Granite/third_party/khronos/vulkan-headers/include/vulkan/*.hpp \
      Granite/third_party/khronos/vulkan-headers/include/vulkan/*.cppm

mkdir -p "$(dirname "$DEST")"
rm -rf "$DEST"
cp -a "$WORK/pyrowave" "$DEST"

# Local patches on top of the pin (crates/codec/pyrowave-sys/patches/*.patch, applied
# in order). Each patch documents its upstream status; drop it when a vendor
# bump includes the fix.
# Patch 0002 carries a regenerated shaders/slangmosh.hpp, so it conflicts whenever
# upstream regenerates the bank. Re-apply its .comp hunk, then rebuild the bank with
# the Granite slangmosh that reproduces upstream's committed bank byte for byte.
for p in "$REPO_ROOT"/crates/codec/pyrowave-sys/patches/*.patch; do
  [ -e "$p" ] || continue
  git -C "$REPO_ROOT" apply "$p"
  echo "applied $(basename "$p")"
done

cat > "$DEST/PUNKTFUNK-VENDOR.txt" <<EOF
Vendored by scripts/vendor-pyrowave.sh — do not edit by hand.

pyrowave:        $PYROWAVE_COMMIT
Granite:         $GRANITE_COMMIT
volk:            $VOLK_COMMIT
vulkan-headers:  $VKHDR_COMMIT

Tree is pruned to what the pyrowave-sys standalone build needs (see the
rm -rf list in the script; Granite/video keeps only scaler.cpp and .hpp).
All parts are MIT-licensed (pyrowave, Granite) or Apache-2.0/MIT (volk,
Vulkan-Headers).

Local patches (crates/codec/pyrowave-sys/patches/, re-applied on re-vendor).
These are OUR fixes — kept local by decision (we vendor anyway), not filed
upstream. The numbers are stable names that code and tests cite; a retired
number is never reused.
  0001-payload-data-444-sizing.patch — encoder payload_data worst-case buffer
    was sized for 4:2:0's 1.5 samples/px; busy 4:4:4 (3 samples/px) overran it
    on the GPU → nondeterministic corrupt bitstreams/crashes at any bitrate.
    Found + validated 2026-07-18 (RTX 5070 Ti, 1080p/4K, 8/16-bit).
  0002-rdo-saving-clamp.patch — analyze_rate_control.comp accumulates the full
    32-bit per-block rate saving into the RDO bucket totals but packs only 16
    bits into each RDOperation; once a block's saving exceeds 65535 cost units
    the resolve pass over-credits applied ops and the bitstream can overshoot
    the hard rate target (the same overrun class as 0001). Clamped to the
    packed width (conservative direction). Includes the regenerated
    shaders/slangmosh.hpp: built with the slangmosh of the Granite pin that
    preceded 1b2d1801 (44362775 and its glslang/spirv-tools), -O --strip per
    upstream's slangmosh.sh. That toolchain reproduces upstream's committed
    bank byte-for-byte, so analyze_rate_control is the only changed program.
    NOTE the related UNPATCHED limit: the packed 16-bit block_index wraps when
    block_count_32x32 > 65535 (~8K 4:4:4) — guarded host-side instead
    (pf-encode rejects such modes for PyroWave).
  0003-devel-encode-16bit-read.patch — devel tool encode.cpp read y4m planes
    with texel-count math (bytes for 8-bit): 16-bit inputs got half-plane
    reads and a desynced stream after frame 1. Tool-only (our build never
    compiles the devel tools; kept so the vendored source is honest).
  0004-encoder-buffer-pool.patch — the encode body allocated four Vulkan
    buffers (meta + bitstream, Device + CachedHost) on EVERY encode. At 240 fps
    with MB-scale bitstreams that churn stalled the encode itself: on an
    RTX 4090 the 5120x1440 submit+fence-wait was ~15 ms (~64 fps ceiling) and
    dropped to ~1 ms (~1025 fps) once the buffers are pooled on the encoder and
    reused. Safe under the synchronous encode model. Perf fix, not correctness.
  0005-global-priority-queue.patch — Context::create_device requests a global-
    priority Vulkan compute queue (VK_KHR_global_priority,
    PYROWAVE_QUEUE_PRIORITY=off|high|realtime, default realtime) so the
    wavelet encode can preempt a GPU-bound game on the shared shader cores. A
    create loop downgrades on NOT_PERMITTED / INITIALIZATION_FAILED so a refused
    class never regresses the encoder. Gated on !inherit_info, so it is live
    only on the Windows path (pyrowave_create_device_by_compat, where Granite
    builds its own device); Linux passes its own create-infos and the same
    request lives in crates/host/pf-encode/src/enc/linux/pyrowave.rs
    (queue_priority_candidates). Change one, change both. Upstream's compat2
    request (c0b997f8) is opt-in, needs Vulkan 1.4 and moves the encode to the
    async compute queue, so it does not replace this; the patch stands down
    when the caller uses upstream's flags.
  0007-encoder-sequence-override.patch — pyrowave_encoder_set_next_sequence()
    stamps the 3-bit wire sequence from the caller, so two alternating encoder
    handles emit 1,2,3... instead of 1,1,2,2... (the decoder swallows a
    repeated value as more blocks of the same frame). Inert when unused.
  0008-decoder-reject-zero-length-packet.patch — push_packet rejects a packet
    smaller than its own header before decode_packet's duplicate-block early
    return, so the parse cursor always advances (a payload_words == 0 duplicate
    block otherwise spun the decode thread forever).
  0009-keep-vulkan-module-resident.patch — init_loader keeps a dlopen
    reference to libvulkan on every path, so the embedder dropping its handle
    cannot unmap the loader under Granite's global procs.
  0010-encoder-cached-plane-views.patch — the encoder keeps its three plane
    views while the caller passes the same images, instead of wrapping and
    destroying them every frame (0.15-0.25 ms of CPU per frame).

Retired (upstream carries the fix):
  0006 external-memory consume-on-success-only — Granite b6cffd5c (2026-09-23)
    closes an imported by-reference handle after Device::create_image
    succeeds and not inside the allocator; semaphore import follows the same
    rule.
  0009-granite-android-null-platform — Granite db34ed01 (2026-09-07) tests
    GRANITE_PLATFORM=null before ANDROID in application/platforms.
EOF

echo "Vendored pyrowave@${PYROWAVE_COMMIT:0:12} (Granite ${GRANITE_COMMIT:0:12}) into $DEST"
du -sh "$DEST"
