#!/usr/bin/env bash
# shellcheck shell=bash
# Run a command, retrying with linear backoff:  bash scripts/ci/retry.sh <attempts> <cmd> [args…]
#
# WHY THIS EXISTS: the runner box executes many jobs in parallel and its network drops
# packets under that load — an instant "[6] Could not resolve hostname" (dropped UDP DNS) or
# "dial tcp: i/o timeout" (dropped TCP SYNs), seconds after other network calls in the same
# job succeeded. Tools with built-in mirror retries (dnf) ride it out; single-shot fetches
# (flatpak remote-add, rsync, ssh) fail the whole job on one dropped packet. Wrap every
# single-shot network command in CI with this instead.
#
# Backoff is attempt*10s (10/20/30/…), so 5 attempts spread over ~100 s — long enough
# to outlive a load burst, short enough not to eat the job timeout. The wrapped command's
# stdout passes through untouched (safe for $(…) capture); retry chatter goes to stderr.
set -u

attempts="$1"; shift
rc=1
for i in $(seq 1 "$attempts"); do
  "$@" && exit 0
  rc=$?
  [ "$i" -eq "$attempts" ] && break
  echo "::warning::attempt $i/$attempts failed (rc=$rc): $* — retrying in $((i * 10))s" >&2
  sleep $((i * 10))
done
echo "::error::all $attempts attempts failed (rc=$rc): $*" >&2
exit "$rc"
