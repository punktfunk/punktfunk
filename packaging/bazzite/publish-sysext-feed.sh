#!/usr/bin/env bash
# Publish a punktfunk sysext image into its feed on the Gitea generic package registry —
# called by .gitea/workflows/rpm.yml after the RPM publish. A feed is one fixed URL
# (…/punktfunk-sysext/<feed>/) holding versioned .raw files plus a SHA256SUMS manifest and its
# detached OpenPGP signature SHA256SUMS.asc; punktfunk-sysext(8) on the boxes verifies that
# signature, then uses the manifest to find + check the newest image (the layout is also exactly
# what systemd-sysupdate's url-file source expects, so a .transfer feed can be added later
# without re-publishing anything).
#
# Signing uses RPM_GPG_PRIVATE_KEY — the SAME packages@unom.io key that signs the RPMs, so boxes
# have one key to trust and we have one key to rotate. Without checksums-plus-signature the feed
# was self-certifying: SHA256SUMS sits on the same registry as the images it describes, so whoever
# could swap an image could swap its checksum in the same breath.
#
# Usage: TOKEN=… [KEEP=6] bash publish-sysext-feed.sh <feed> <image.raw>
#        TOKEN=… bash publish-sysext-feed.sh --seal <feed>
#   <feed>   e.g. f43, f43-canary, f44 (Fedora major x channel)
#   KEEP     newest images to keep in the feed; 0/unset-for-stable = keep all
#   --seal   re-sign a feed's EXISTING manifest without publishing an image. For feeds published
#            before signing existed, and after a key rotation. Idempotent. Also re-publishes the
#            manifest if normalizing it changed anything, because the signature has to cover the
#            bytes a client downloads.
# Env: REGISTRY (git.unom.io), OWNER (unom), TOKEN (write:package PAT), CURL_USER (login name),
#      RPM_GPG_PRIVATE_KEY (armored private key; absent => unsigned, fatal on a v* tag)
set -euo pipefail

SEAL=0
if [ "${1:-}" = --seal ]; then
  SEAL=1; FEED="${2:?usage: publish-sysext-feed.sh --seal <feed>}"; RAW=""
else
  FEED="${1:?usage: publish-sysext-feed.sh <feed> <image.raw>}"
  RAW="${2:?usage: publish-sysext-feed.sh <feed> <image.raw>}"
  [ -f "$RAW" ] || { echo "no such image: $RAW" >&2; exit 1; }
fi
REGISTRY="${REGISTRY:-git.unom.io}"
OWNER="${OWNER:-unom}"
KEEP="${KEEP:-0}"
AUTH=(--user "${CURL_USER:-enricobuehler}:${TOKEN:?TOKEN (write:package PAT) required}")
BASE="https://$REGISTRY/api/packages/$OWNER/generic/punktfunk-sysext/$FEED"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
SUMS="$WORK/SHA256SUMS"
SIG="$WORK/SHA256SUMS.asc"
FETCHED="$WORK/SHA256SUMS.fetched"   # exactly what the registry served, before normalization

# read_manifest -> the feed's current manifest, normalized into $SUMS and kept verbatim in
# $FETCHED. Returns non-zero iff the feed has no manifest at all.
#
# -L is not optional here. The registry answers a file GET with a 303 See Other pointing at
# presigned object storage, and `curl -f` does NOT treat a 3xx as an error — so without -L the call
# "succeeds" and hands back the redirect's HTML body ('<a href="…">See Other</a>.'). Both callers
# then took that page for the manifest: every publish prepended a stale redirect page and dropped
# every prior image line, and --seal signed a page whose presigned URL expired 300 seconds later —
# a signature over bytes that exist nowhere. Clients fetch WITH -L, so they checked the real
# manifest against that signature and refused the feed, which from a Bazzite box is indistinguishable
# from someone having tampered with it.
#
# The line filter is the second layer, and the one that does not depend on getting curl's flags
# right: whatever the transport hands back, only well-formed "<sha256>  <filename>" lines are ever
# signed or re-published. It also scrubs a feed that already carries an injected page.
read_manifest() {
  : > "$SUMS"; : > "$FETCHED"
  curl -fsSL "${AUTH[@]}" -o "$FETCHED" "$BASE/SHA256SUMS" || return 1
  grep -E '^[0-9a-f]{64}  [^ ]+$' "$FETCHED" > "$SUMS" || :
}

# bind_manifest — stamp the feed name and a publish serial INTO the bytes about to be signed.
#
# A bare checksum list binds nothing: the signature proves "we signed these bytes", never "…for the
# stable feed, this month". So anyone who can WRITE the registry without holding the key — a leaked
# write:package token, the kind that sits in half our CI jobs — can copy this feed's manifest,
# signature and images into another channel's path, or put an old pair back, and every box verifies
# them happily and merges the result over /usr. punktfunk-sysext(8) refuses a manifest whose FEED is
# not the feed it fetched and one whose SERIAL is below the highest it has accepted, which is the
# same channel-binding + monotonic-serial pair the Rust updater enforces on its own manifest
# (crates/core/pf-update-check/src/manifest.rs). Comment lines, so a `#`-skipping checksum reader and the
# clients' image-line parsers are both unaffected by them.
bind_manifest() {
  local serial prev
  serial="$(date +%s)"
  # Monotonic per feed, and never below what the feed already claims. Clients keep the highest
  # serial they have accepted and refuse anything lower, so one runner with a fast clock would
  # otherwise lock every box out of this feed until wall-clock caught up.
  prev="$(sed -n 's/^# SERIAL //p' "$FETCHED" | head -n1)"
  case "$prev" in ''|*[!0-9]*) prev=0 ;; esac
  [ "$serial" -gt "$prev" ] || serial=$((prev + 1))
  { printf '# FEED %s\n# SERIAL %s\n' "$FEED" "$serial"; cat "$SUMS"; } > "$WORK/bound"
  mv -f "$WORK/bound" "$SUMS"
  echo "bound manifest to feed $FEED, serial $serial"
}

# sign_manifest — detached-sign $SUMS into $SIG with RPM_GPG_PRIVATE_KEY. Prints nothing and
# returns 1 if no key is available; the caller decides whether that is survivable.
sign_manifest() {
  [ -n "${RPM_GPG_PRIVATE_KEY:-}" ] || return 1
  local home keyid baked
  home="$(mktemp -d)"; chmod 700 "$home"
  # Loopback pinentry for the same reason sign-rpms.sh needs it: no TTY in CI.
  printf 'pinentry-mode loopback\n' > "$home/gpg.conf"
  printf 'allow-loopback-pinentry\n' > "$home/gpg-agent.conf"
  printf '%s' "$RPM_GPG_PRIVATE_KEY" | GNUPGHOME="$home" gpg --batch --quiet --import
  keyid="$(GNUPGHOME="$home" gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')"
  # A feed signed by a key the client script doesn't carry is a feed nobody can install from, and
  # the failure would only surface on someone's Bazzite box. Compare fingerprints here instead: the
  # baked-in public key in punktfunk-sysext.sh must be the key we are about to sign with.
  # The armor block is a shell single-quoted literal in that script, so the range's first and last
  # lines carry the `FEED_KEY='` prefix and the closing quote — strip them or gpg sees no armor at
  # all and hands back an empty fingerprint, which would look like a mismatch on every publish.
  baked="$(sed -n '/BEGIN PGP PUBLIC KEY BLOCK/,/END PGP PUBLIC KEY BLOCK/p' "$HERE/punktfunk-sysext.sh" \
           | sed "s/^FEED_KEY='//; s/'\$//" \
           | GNUPGHOME="$home" gpg --batch --quiet --with-colons --import-options show-only --import 2>/dev/null \
           | awk -F: '/^fpr:/{print $10; exit}')"
  if [ -z "$baked" ] || [ "$keyid" != "$baked" ]; then
    echo "signing key $keyid does not match the key baked into punktfunk-sysext.sh ($baked)" >&2
    echo "-> rotate BOTH (packaging/rpm/README.md), or clients cannot verify this feed." >&2
    rm -rf "$home"; return 1
  fi
  GNUPGHOME="$home" gpg --batch --yes --armor --detach-sign --local-user "$keyid" \
    --output "$SIG" "$SUMS"
  rm -rf "$home"
  echo "signed SHA256SUMS with $keyid"
}

# require_signature — on a v* tag an unsigned feed is a hard stop, exactly like sign-rpms.sh:
# clients refuse unsigned feeds, so publishing one would strand every box on the stable channel.
require_signature() {
  case "${GITHUB_REF:-}" in
    refs/tags/v*)
      echo "release build (${GITHUB_REF}) but the sysext feed could not be signed — aborting." >&2
      exit 1 ;;
  esac
  # Deliberately mode-neutral wording: in --seal mode nothing is published at all, and a log line
  # claiming otherwise is the kind of thing that costs an hour six months from now.
  echo "WARNING: $FEED left UNSIGNED (no usable RPM_GPG_PRIVATE_KEY); clients will refuse it." >&2
}

# --seal: re-sign whatever manifest the feed already has, no image, no pruning.
if [ "$SEAL" = 1 ]; then
  read_manifest || { echo "no SHA256SUMS at $BASE — nothing to seal" >&2; exit 1; }
  # An empty result means the manifest was ALL junk. Signing that would hand clients a feed that
  # verifies and offers no images, which reads as "up to date" to `punktfunk-sysext update`.
  if [ ! -s "$SUMS" ]; then
    echo "$BASE/SHA256SUMS lists no images — refusing to seal it (feed needs a republish)" >&2
    exit 1
  fi
  IMAGES="$(grep -c '' <"$SUMS")"
  cmp -s "$SUMS" "$FETCHED" \
    || echo "normalizing $BASE/SHA256SUMS: $(grep -c '' <"$FETCHED") line(s) served, $IMAGES kept"
  bind_manifest
  if ! sign_manifest; then
    require_signature   # non-release: warn and leave the live feed exactly as it was
    exit 0
  fi
  # The signature must cover the bytes a client actually downloads, and bind_manifest has just
  # rewritten the header, so the manifest ALWAYS goes back up with its new signature — an .asc
  # describing a file the registry does not have is the very failure this mode repairs. Manifest
  # first, signature second: a client caught in the window sees an unsigned feed and refuses.
  curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS" || true
  curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SUMS" "$BASE/SHA256SUMS"
  curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS.asc" || true
  curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SIG" "$BASE/SHA256SUMS.asc"
  echo "sealed $BASE ($IMAGES image(s))"
  exit 0
fi

FNAME="$(basename "$RAW")"
SHA="$(sha256sum "$RAW" | cut -d' ' -f1)"

# Merge into the existing manifest: drop any prior line for this filename, append ours.
if read_manifest; then
  sed -i "\| $FNAME\$|d" "$SUMS"
  # Said out loud on purpose. A manifest that silently shrinks is how a feed loses its rollback
  # history, and that went unnoticed precisely because nothing ever reported the carry-over.
  echo "carrying forward $(grep -c '' <"$SUMS") image(s) from the existing manifest"
else
  echo "no manifest at $BASE yet — starting a new feed"
fi
printf '%s  %s\n' "$SHA" "$FNAME" >> "$SUMS"

# Prune: keep only the newest $KEEP images (by version sort) in manifest + registry.
PRUNE=()
if [ "$KEEP" -gt 0 ]; then
  mapfile -t PRUNE < <(awk '{print $2}' "$SUMS" | sort -V | head -n -"$KEEP")
  for f in "${PRUNE[@]:-}"; do
    [ -n "$f" ] && sed -i "\| $f\$|d" "$SUMS"
  done
fi

IMAGES="$(grep -c '' <"$SUMS")"

# Sign the finished manifest BEFORE anything is uploaded — a signing failure on a release must
# abort while the live feed is still whole, not halfway through being replaced.
bind_manifest
sign_manifest || require_signature

# Upload order keeps consumers consistent: image first, then the manifest referencing it, then its
# signature, then prune deletions (already absent from the manifest). A client that catches the
# window between the manifest and its signature sees an unsigned feed and refuses — it does not see
# a manifest vouched for by a stale signature, which is why the .asc goes last and never first.
# Delete-before-put makes workflow re-runs idempotent (the registry 409s on duplicate filenames;
# first-publish 404s are fine).
curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/$FNAME" || true
curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$RAW" "$BASE/$FNAME"
curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS" || true
curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SUMS" "$BASE/SHA256SUMS"
if [ -f "$SIG" ]; then
  curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS.asc" || true
  curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SIG" "$BASE/SHA256SUMS.asc"
fi
for f in "${PRUNE[@]:-}"; do
  [ -n "$f" ] && { echo "pruning $f"; curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/$f" || true; }
done
echo "published $FNAME -> $BASE ($IMAGES image(s) in the feed)"
