#!/bin/sh
# Put a retrying `curl` first on PATH for the rest of the job.
#
# WHY THIS EXISTS: `scripts/ci/retry.sh` already wraps every single-shot network command in CI,
# for the reason documented there — the runner box runs many jobs in parallel and its network
# drops packets under that load. But one of the biggest fetches in this workspace is NOT ours to
# wrap: skia-bindings downloads ~19 MB of prebuilt Skia per target from inside its build script,
# with a bare `curl -sS -f -L` and no retry at all (build_support/binary_cache/utils.rs).
#
# When that transfer truncates the job does not fail with a network error. skia-bindings'
# `try_prepare_download` swallows it, prints `DOWNLOAD AND INSTALL FAILED`, and falls through to
# `STARTING A FULL BUILD` — a from-source Skia build that the CI containers carry no deps for.
# What the operator sees is an unrelated-looking failure far below the real cause in the log:
#
#   DOWNLOAD AND INSTALL FAILED: curl error code: "18"
#   curl stderr: "curl: (18) end of response with 17054400 bytes missing"
#
# A truncated transfer is the runner-box load-shedding that retry.sh was written for.
#
# A shim is the only lever that reaches inside a build script. It is also the cheapest correct
# one: skia-bindings already passes `-C -` (resume) and caches the part-file under
# OUT_DIR/.cache, so a retry CONTINUES the truncated transfer instead of restarting it.
#
# Applies to every curl in the job, which is what we want — the workspace's other build-script
# fetches are single-shot too.
#
# POSIX sh on purpose: Gitea's act_runner executes a step's `run:` under `sh -e` (dash) inside
# the Linux job containers — see the shader-gate note in ci.yml.
#
# Usage:  sh scripts/ci/install-retrying-curl.sh
set -e

# Resolve the REAL curl before the shim is on PATH, and bake the absolute path into the shim —
# a shim that re-resolves `curl` by name would exec itself.
real_curl=$(command -v curl || true)
if [ -z "$real_curl" ]; then
  echo "::warning::no curl on PATH — skipping the retrying-curl shim"
  exit 0
fi

# RUNNER_TEMP (not /usr/local/bin): the job containers run as root but the macOS runner is a
# persistent host where a system dir is neither writable nor ours to litter.
shim_dir="${RUNNER_TEMP:-/tmp}/pf-retrying-curl"
mkdir -p "$shim_dir"

# --retry-all-errors is what makes this cover error 18: a truncated transfer is a *transfer*
# failure, not an HTTP status, so plain --retry (which only retries transient HTTP codes and
# connection errors) would let it through. Needs curl >= 7.71; the CI images are well past it.
# 10 × 10 s rides out a runner DNS drop (error 6), which lasts longer than 5 × 3 s.
cat > "$shim_dir/curl" <<EOF
#!/bin/sh
exec $real_curl --retry 10 --retry-delay 10 --retry-all-errors "\$@"
EOF
chmod +x "$shim_dir/curl"

if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$shim_dir" >> "$GITHUB_PATH"
  echo "retrying curl installed: $shim_dir/curl -> $real_curl"
else
  echo "::warning::GITHUB_PATH unset — shim written to $shim_dir but not on PATH"
fi
