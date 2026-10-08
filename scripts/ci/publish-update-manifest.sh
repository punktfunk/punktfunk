#!/usr/bin/env bash
# Build, sign, upload, and self-verify the host UPDATE MANIFEST for one channel
# (planning: host-update-from-web-console.md §3 — the check truth the console trusts).
#
#   https://git.unom.io/api/packages/unom/generic/punktfunk-update/<channel>/manifest.json
#   https://git.unom.io/api/packages/unom/generic/punktfunk-update/<channel>/manifest.json.sig
#
# The signature is a raw 64-byte Ed25519 over the EXACT manifest bytes, base64 in the .sig —
# the same format the plugin index uses and `store::index::verify_signature` checks. The
# public half is pinned in the shared checker (OFFICIAL_UPDATE_KEYS in crates/pf-update-check);
# before signing, this script cross-checks the signing key against that constant and refuses
# on mismatch — the most likely deploy mistake is signing with a key no host trusts (the
# sysext publisher's fingerprint-crosscheck drill).
#
# Upload order is manifest THEN signature: a client caught in the replace window sees a
# mismatched pair and refuses (fail-closed), never a stale-signed document.
#
# Environment:
#   CHANNEL              stable | canary                                   (required)
#   VERSION              the announced host version string                 (required)
#   CI_RUN               CI run number                                     (required for canary)
#   COMMIT               full commit SHA of the build                      (optional; canary)
#   WINDOWS_URL          immutable per-version installer URL               (required for stable)
#   WINDOWS_SHA256       hex sha256 of that installer                      (paired with WINDOWS_URL)
#   WINDOWS_ARM64_URL    the ARM64 installer's immutable URL               (optional; same pins/subject)
#   WINDOWS_ARM64_SHA256 hex sha256 of that installer                      (paired with WINDOWS_ARM64_URL)
#   AUTHENTICODE_SHA256  comma-separated signing-leaf sha256s              (required for stable)
#   AUTHENTICODE_SUBJECT expected signing-cert subject CN — the host then  (required for stable)
#                        demands a trusted chain (S_OK) + this subject
#                        (security-review 2026-08-31 H-3)
#   NOTES_URL            release-notes link (git.unom.io only)             (optional)
#   UPDATE_MANIFEST_KEY  PKCS#8 PEM, the Ed25519 private key               (required to sign)
#   REQUIRE_KEY=1        missing key is a hard failure (announce/stable)   (optional)
#   REGISTRY_TOKEN       Gitea PAT with write:package                      (required)
#   REGISTRY / OWNER     default git.unom.io / unom
set -euo pipefail

CHANNEL="${CHANNEL:?set CHANNEL=stable|canary}"
VERSION="${VERSION:?set VERSION}"
REGISTRY="${REGISTRY:-git.unom.io}"
OWNER="${OWNER:-unom}"
BASE="https://${REGISTRY}/api/packages/${OWNER}/generic/punktfunk-update/${CHANNEL}"

case "$CHANNEL" in stable|canary) ;; *) echo "CHANNEL must be stable or canary" >&2; exit 1 ;; esac
if [ "$CHANNEL" = canary ] && [ -z "${CI_RUN:-}" ]; then
  echo "canary manifests need CI_RUN (the definitive newer-than axis)" >&2; exit 1
fi
if [ "$CHANNEL" = stable ] && [ -z "${WINDOWS_URL:-}" ]; then
  echo "stable manifests need WINDOWS_URL/WINDOWS_SHA256 (the U1 apply leg)" >&2; exit 1
fi
if [ "$CHANNEL" = stable ] && [ -z "${AUTHENTICODE_SUBJECT:-}" ]; then
  echo "stable manifests need AUTHENTICODE_SUBJECT" >&2; exit 1
fi
if [ "$CHANNEL" = stable ] && [ -z "${AUTHENTICODE_SHA256:-}" ]; then
  echo "stable manifests need AUTHENTICODE_SHA256 for existing clients" >&2; exit 1
fi
if [ -n "${COMMIT:-}" ] && ! printf '%s' "$COMMIT" | grep -Eq '^[0-9a-f]{40}$'; then
  echo "COMMIT must be a full 40-hex commit SHA" >&2; exit 1
fi
if [ -n "${WINDOWS_URL:-}" ] && ! printf '%s' "${WINDOWS_SHA256:-}" | grep -Eq '^[0-9a-f]{64}$'; then
  echo "WINDOWS_SHA256 must be 64 hex chars when WINDOWS_URL is set" >&2; exit 1
fi
if [ -n "${AUTHENTICODE_SHA256:-}" ] && ! printf '%s' "$AUTHENTICODE_SHA256" \
    | grep -Eq '^[0-9a-fA-F]{64}(,[0-9a-fA-F]{64})*$'; then
  echo "AUTHENTICODE_SHA256 must be comma-separated SHA-256 hex values" >&2; exit 1
fi

# ---- key handling (fail-closed where it matters) --------------------------------------------
if [ -z "${UPDATE_MANIFEST_KEY:-}" ]; then
  if [ "${REQUIRE_KEY:-0}" = 1 ] || case "${GITHUB_REF:-}" in refs/tags/v*) true ;; *) false ;; esac; then
    echo "ERROR: UPDATE_MANIFEST_KEY is not set — refusing to publish an unsigned manifest" >&2
    exit 1
  fi
  echo "WARN: UPDATE_MANIFEST_KEY not set — skipping the ${CHANNEL} update manifest" >&2
  exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
KEY="$WORK/key.pem"
# Tolerate secret stores that mangle newlines into literal \n (the plugin-index signer's fix).
if printf '%s' "$UPDATE_MANIFEST_KEY" | grep -q '\\n' && ! printf '%s' "$UPDATE_MANIFEST_KEY" | grep -q '^-----BEGIN.*-----$'; then
  printf '%s' "$UPDATE_MANIFEST_KEY" | sed 's/\\n/\n/g' > "$KEY"
else
  printf '%s\n' "$UPDATE_MANIFEST_KEY" > "$KEY"
fi

# Cross-check: the key we are about to sign with must be one the host binary pins.
PUB="ed25519:$(openssl pkey -in "$KEY" -pubout -outform DER | tail -c 32 | base64)"
# The pin list moved to the shared checker when the Linux client started verifying the same
# manifest (crates/pf-update-check/src/lib.rs, OFFICIAL_UPDATE_KEYS) — one list, so the host
# and the client can never disagree about who may announce a release.
KEYS_FILE="crates/pf-update-check/src/lib.rs"
# A MISSING file is fatal, not a warning. This check is the guard against signing with a key
# no build trusts; if its path ever goes stale the old `else` branch would have skipped it
# silently and published an unverifiable manifest — the exact failure it exists to prevent.
if [ ! -f "$KEYS_FILE" ]; then
  echo "ERROR: $KEYS_FILE not in this checkout — refusing to sign without the pinned-key cross-check" >&2
  exit 1
fi
if ! grep -qF "\"$PUB\"" "$KEYS_FILE"; then
  echo "ERROR: signing key $PUB is not pinned in $KEYS_FILE (OFFICIAL_UPDATE_KEYS) — wrong key?" >&2
  exit 1
fi

# ---- build the manifest ---------------------------------------------------------------------
SERIAL="$(date +%s)"
PUBLISHED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
MANIFEST="$WORK/manifest.json"

# `printf '%s\n'`, not '%s': an EMPTY value must still hand jq one (empty) input line —
# with no line at all, `jq -R` emits nothing and `--argjson auth ""` is invalid JSON
# (bit the first live canary publish). The fallback belts the suspenders.
AUTH_JSON="$(printf '%s\n' "${AUTHENTICODE_SHA256:-}" | jq -R 'split(",") | map(select(length > 0))')"
AUTH_JSON="${AUTH_JSON:-[]}"
jq -n \
  --arg channel "$CHANNEL" \
  --arg version "$VERSION" \
  --arg published_at "$PUBLISHED_AT" \
  --arg notes_url "${NOTES_URL:-}" \
  --argjson serial "$SERIAL" \
  --arg win_url "${WINDOWS_URL:-}" \
  --arg win_sha "${WINDOWS_SHA256:-}" \
  --arg a64_url "${WINDOWS_ARM64_URL:-}" \
  --arg a64_sha "${WINDOWS_ARM64_SHA256:-}" \
  --argjson auth "$AUTH_JSON" \
  --arg auth_subject "${AUTHENTICODE_SUBJECT:-}" \
  --arg ci_run "${CI_RUN:-}" \
  --arg commit "${COMMIT:-}" \
  '
  {schema: 1, channel: $channel, serial: $serial, published_at: $published_at, version: $version}
  + (if $notes_url != "" then {notes_url: $notes_url} else {} end)
  + (if $ci_run != "" then {ci_run: ($ci_run | tonumber)} else {} end)
  + (if $commit != "" then {commit: $commit} else {} end)
  + (if $win_url != "" then {windows_host: ({url: $win_url, sha256: $win_sha, authenticode_sha256: $auth}
      + (if $auth_subject != "" then {authenticode_subject: $auth_subject} else {} end))} else {} end)
  + (if $a64_url != "" then {windows_host_arm64: ({url: $a64_url, sha256: $a64_sha, authenticode_sha256: $auth}
      + (if $auth_subject != "" then {authenticode_subject: $auth_subject} else {} end))} else {} end)
  ' > "$MANIFEST"
echo "manifest:"; cat "$MANIFEST"

# ---- sign + local verify --------------------------------------------------------------------
SIG_BIN="$WORK/sig.bin"
openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$MANIFEST" -out "$SIG_BIN"
[ "$(wc -c < "$SIG_BIN")" -eq 64 ] || { echo "ERROR: signature is not 64 bytes" >&2; exit 1; }
SIG="$WORK/manifest.json.sig"
base64 < "$SIG_BIN" | tr -d '\n' > "$SIG"; printf '\n' >> "$SIG"

PUBPEM="$WORK/pub.pem"
openssl pkey -in "$KEY" -pubout -out "$PUBPEM"
openssl pkeyutl -verify -pubin -inkey "$PUBPEM" -rawin -in "$MANIFEST" -sigfile "$SIG_BIN" >/dev/null

# ---- upload (manifest first, then signature) ------------------------------------------------
: "${REGISTRY_TOKEN:?set REGISTRY_TOKEN}"
upload() { # file url
  curl -fsS -o /dev/null --user "enricobuehler:${REGISTRY_TOKEN}" -X DELETE "$2" 2>/dev/null || true
  curl -fsS -o /dev/null --user "enricobuehler:${REGISTRY_TOKEN}" --upload-file "$1" "$2"
  echo "published: $2"
}
upload "$MANIFEST" "$BASE/manifest.json"
upload "$SIG"      "$BASE/manifest.json.sig"

# ---- self-verify the LIVE feed (bytes must round-trip; -L follows the 303) ------------------
curl -fsSL "$BASE/manifest.json" -o "$WORK/live.json"
curl -fsSL "$BASE/manifest.json.sig" -o "$WORK/live.sig"
cmp -s "$MANIFEST" "$WORK/live.json" || { echo "ERROR: live manifest differs from what was uploaded" >&2; exit 1; }
cmp -s "$SIG" "$WORK/live.sig" || { echo "ERROR: live signature differs from what was uploaded" >&2; exit 1; }
openssl pkeyutl -verify -pubin -inkey "$PUBPEM" -rawin -in "$WORK/live.json" -sigfile "$SIG_BIN" >/dev/null
echo "OK: ${CHANNEL} update manifest ${VERSION} (serial ${SERIAL}) is live and verifies"
