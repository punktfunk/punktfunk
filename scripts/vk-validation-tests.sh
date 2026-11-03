#!/bin/sh
# Run the ignored Vulkan GPU tests under the Khronos validation layer and fail on what it
# reports. For a rig with a real device: the encoders and the decoder open their own Vulkan
# instances, so the layer is switched on from outside, by the loader's and the layer's own
# variables, and nothing in a shipped binary knows about it.
#
#   sh scripts/vk-validation-tests.sh [extra cargo test args, e.g. --release]
#
# One thread and no capture, so each error prints under the test that made it. The exit code is
# the layer's verdict alone. A test that fails for its own reasons (a codec this driver lacks)
# does not change it: compare those against main on the same box.
set -u

found=
for dir in ${VK_LAYER_PATH:+$(echo "$VK_LAYER_PATH" | tr ':' ' ')} \
  /usr/share/vulkan/explicit_layer.d /etc/vulkan/explicit_layer.d \
  /usr/local/share/vulkan/explicit_layer.d "${HOME:-/nonexistent}/.local/share/vulkan/explicit_layer.d"; do
  [ -f "$dir/VkLayer_khronos_validation.json" ] && found=$dir && break
done
if [ -z "$found" ]; then
  echo "vk-validation-tests: no VkLayer_khronos_validation.json. Install the validation layer" >&2
  echo "(vulkan-validation-layers on Arch, vulkan-validationlayers on Debian and Ubuntu)." >&2
  exit 2
fi

export VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation
# Errors only: warnings and performance notes are not what this gate is for.
export VK_KHRONOS_VALIDATION_REPORT_FLAGS=error
log=$(mktemp "${TMPDIR:-/tmp}/pf-vk-validation.XXXXXX")
trap 'rm -f "$log"' EXIT

# `--no-fail-fast`: a test binary that fails for its own reasons must not stop the ones after it.
run() {
  echo "== cargo test $*"
  cargo test --no-fail-fast "$@" -- --ignored --test-threads=1 --nocapture 2>&1 | tee -a "$log"
}
run -p pf-zerocopy --lib "$@"
run -p pf-encode --lib --features vulkan-encode,pyrowave "$@"
run -p pf-vkdecode --test gpu_smoke --test gpu_parity "$@"
# The route a device with no plane views takes, on one that has both.
PUNKTFUNK_VKDECODE_ARRANGEMENT=sampler run -p pf-vkdecode --test gpu_smoke --test gpu_parity "$@"
run -p pf-presenter --lib csc "$@"

errors=$(grep -c 'Validation Error' "$log")
if [ "$errors" -ne 0 ]; then
  echo "vk-validation-tests: $errors validation errors (layer in $found). Each is above, under its test." >&2
  exit 1
fi
echo "vk-validation-tests: no validation errors (layer in $found)"
