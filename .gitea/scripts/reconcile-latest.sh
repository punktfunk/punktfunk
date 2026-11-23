#!/usr/bin/env bash
# Assert that a builder image's :latest is the SAME manifest as its content key, and
# re-point it when it isn't.
#
# This is what we do instead of pinning consumers by @sha256: digest
# (security-review-2026-08-05, H-6 — see the reasoning at the top of docker.yml). The
# content key is a hash of the ci/ tree and rust-toolchain.toml, so "which image should
# :latest be?" has an answer derivable
# from the commit alone. Checking it on every run turns :latest from a tag someone
# remembered to move into a function of the tree.
#
# Two different things make them diverge and neither is distinguishable from here:
#
#   - Someone overwrote :latest out of band. That needs the push credential, but it
#     is exactly the H-6 attack and it must not pass silently.
#   - ci/ was reverted. The older key is already a cache hit, so nothing rebuilds and
#     nothing re-points :latest — it stays on the newer build forever while every
#     consumer pulls a builder that does not match the tree it is building.
#
# Both are repaired identically, so: repair, and shout. Failing the build instead would
# turn a legitimate revert into a red main with no way forward.
#
# Reads go to the anonymous port, the single write to the authenticated one.
set -euo pipefail

IMAGE="${1:?usage: reconcile-latest.sh <image> <content-key>}"
KEY="${2:?usage: reconcile-latest.sh <image> <content-key>}"
: "${CI_REGISTRY:?CI_REGISTRY not set}"
: "${CI_REGISTRY_PUSH:?CI_REGISTRY_PUSH not set}"
: "${CI_REGISTRY_PASSWORD:?CI_REGISTRY_PASSWORD not set}"

ACCEPT='Accept: application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json'

# Digest of a tag, or empty if the tag does not exist. Never fails the script itself —
# "missing" is a state this has to reason about, not an error to abort on.
digest_of() {
  curl -sfI -H "$ACCEPT" "http://$CI_REGISTRY/v2/$IMAGE/manifests/$1" 2>/dev/null \
    | tr -d '\r' | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: //p' || true
}

key_digest=$(digest_of "$KEY")
latest_digest=$(digest_of latest)

if [ -z "$key_digest" ]; then
  echo "::error::$IMAGE:$KEY has no manifest — the build or push above did not land"
  exit 1
fi

if [ "$key_digest" = "$latest_digest" ]; then
  echo "$IMAGE:latest == :$KEY ($key_digest)"
  exit 0
fi

echo "::warning::$IMAGE:latest did not match its content key :$KEY — re-pointing it. If ci/ was not just reverted, someone overwrote this tag out of band: check the registry access log on home-ci-core."
echo "  was:    ${latest_digest:-<no :latest tag>}"
echo "  wanted: $key_digest  (:$KEY)"

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
media_type=$(curl -sfI -H "$ACCEPT" "http://$CI_REGISTRY/v2/$IMAGE/manifests/$KEY" \
  | tr -d '\r' | sed -n 's/^[Cc]ontent-[Tt]ype: //p')
curl -sf -H "$ACCEPT" -o "$tmp" "http://$CI_REGISTRY/v2/$IMAGE/manifests/$KEY"
curl -sf -u "ci:$CI_REGISTRY_PASSWORD" -X PUT -H "Content-Type: $media_type" \
  --data-binary @"$tmp" "http://$CI_REGISTRY_PUSH/v2/$IMAGE/manifests/latest"

now=$(digest_of latest)
[ "$now" = "$key_digest" ] || { echo "::error::re-point failed: :latest is $now"; exit 1; }
echo "$IMAGE:latest re-pointed to $key_digest"
