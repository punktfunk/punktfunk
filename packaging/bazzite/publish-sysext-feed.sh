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
# have one key to trust and we have one key to rotate. Checksums alone certify themselves:
# SHA256SUMS sits on the same registry as the images it describes, so whoever can swap an image
# can swap its checksum in the same breath.
#
# Usage: TOKEN=… [KEEP=6] bash publish-sysext-feed.sh <feed> <image.raw>
#        TOKEN=… bash publish-sysext-feed.sh --seal <feed>
#   <feed>   e.g. f43, f43-canary, f44 (Fedora major x channel)
#   KEEP     newest images to keep in the feed; 0/unset-for-stable = keep all
#   --seal   re-sign a feed's EXISTING manifest under a new serial without publishing an image,
#            when the current key didn't sign it. Both modes carry lines only from a live manifest
#            that verifies against FEED_KEY in punktfunk-sysext.sh and names this feed. A publish
#            over any other starts the feed over; a seal leaves it as it is.
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

# baked_key -> the armored public key block(s) of FEED_KEY in punktfunk-sysext.sh, the key boxes
# verify the feed with. The block is a single-quoted shell literal there, so the first and last
# lines lose the `FEED_KEY='` prefix and the closing quote.
baked_key() {
  sed -n '/BEGIN PGP PUBLIC KEY BLOCK/,/END PGP PUBLIC KEY BLOCK/p' "$HERE/punktfunk-sysext.sh" \
    | sed "s/^FEED_KEY='//; s/'\$//"
}

# first_baked_fpr -> the fingerprint of FEED_KEY's first key, the one the feed is signed with.
first_baked_fpr() {
  local home
  home="$(mktemp -d)"; chmod 700 "$home"
  baked_key | GNUPGHOME="$home" gpg --batch --quiet --with-colons --import-options show-only \
    --import 2>/dev/null | awk -F: '/^fpr:/{print $10; exit}'
  rm -rf "$home"
}

# read_manifest -> the feed's live manifest, normalized into $SUMS and kept verbatim in $FETCHED,
# and the fingerprint of the key that signed it in $LIVE_SIGNER. Returns 1, with both empty, for
# a new feed and for one whose pair doesn't verify against baked_key for this feed: a run
# cancelled between its two uploads leaves one. Whoever holds a registry token without the key
# never gets a line carried or signed. An answer that is neither 200 nor 404 stops the run.
#
# -L is not optional here. The registry answers a file GET with a 303 See Other pointing at
# presigned object storage, and `curl -f` does NOT treat a 3xx as an error — so without -L the call
# "succeeds" and hands back the redirect's HTML body ('<a href="…">See Other</a>.'), which both
# callers would take for the manifest: a publish would then drop every prior image line.
#
# The line filter is the second layer, and the one that does not depend on getting curl's flags
# right: whatever the transport hands back, only well-formed "<sha256>  <filename>" lines are ever
# signed or re-published. It also scrubs a feed that already carries an injected page.
read_manifest() {
  local sums_code sig_code home status
  : > "$SUMS"; : > "$FETCHED"; LIVE_SIGNER=
  sums_code="$(curl -sSL "${AUTH[@]}" -o "$FETCHED" -w '%{http_code}' "$BASE/SHA256SUMS")" \
    || { echo "fetch $BASE/SHA256SUMS" >&2; exit 1; }
  sig_code="$(curl -sSL "${AUTH[@]}" -o "$WORK/live.asc" -w '%{http_code}' "$BASE/SHA256SUMS.asc")" \
    || { echo "fetch $BASE/SHA256SUMS.asc" >&2; exit 1; }
  case "$sums_code $sig_code" in
    "404 404") : > "$FETCHED"; return 1 ;;
    "200 200"|"200 404"|"404 200") ;;
    *) echo "$BASE answers SHA256SUMS $sums_code, SHA256SUMS.asc $sig_code — refusing to publish over a feed it can't read" >&2
       exit 1 ;;
  esac
  if [ "$sums_code $sig_code" = "200 200" ]; then
    home="$(mktemp -d)"; chmod 700 "$home"
    baked_key | GNUPGHOME="$home" gpg --batch --quiet --import 2>/dev/null || :
    status="$(GNUPGHOME="$home" gpg --batch --quiet --status-fd 1 --verify "$WORK/live.asc" \
      "$FETCHED" 2>/dev/null || :)"
    rm -rf "$home"
    LIVE_SIGNER="$(printf '%s\n' "$status" | awk '$2 == "VALIDSIG" {print $NF; exit}')"
  fi
  if [ -z "$LIVE_SIGNER" ] || [ "$(sed -n 's/^# FEED //p' "$FETCHED" | head -n1)" != "$FEED" ]; then
    echo "::warning::$BASE holds no manifest FEED_KEY signed for $FEED (an upload cut short, or not ours); nothing of it is carried or signed"
    : > "$FETCHED"; LIVE_SIGNER=; return 1
  fi
  grep -E '^[0-9a-f]{64}  [^ ]+$' "$FETCHED" > "$SUMS" || :
}

# bind_manifest — stamp the feed name and a publish serial INTO the bytes about to be signed.
#
# A bare checksum list binds nothing: the signature proves "we signed these bytes", never "…for the
# stable feed, this month". So anyone who can WRITE the registry without holding the key — a leaked
# write:package token — can copy this feed's manifest, signature and images into another
# channel's path, or put an old pair back, and every box verifies them happily and merges the
# result over /usr. punktfunk-sysext(8) refuses a manifest whose FEED is not the feed it fetched
# and one whose SERIAL is below the highest it has accepted, which is the same channel-binding
# + monotonic-serial pair the Rust updater enforces on its own manifest
# (crates/core/pf-update-check/src/{manifest,floor}.rs). Comment lines, so a `#`-skipping checksum
# reader and the clients' image-line parsers are both unaffected by them.
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
  # The signing key must be the first key in FEED_KEY, the block boxes verify with: a feed signed
  # by a key outside it is a feed nobody can install from.
  baked="$(first_baked_fpr)"
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

# --seal: re-sign the feed's verified manifest, no image, no pruning. A manifest the current key
# signed stays as it is, so a canary run never rewrites the stable feed it can't publish to.
if [ "$SEAL" = 1 ]; then
  read_manifest || { echo "no manifest FEED_KEY signed at $BASE — nothing to seal" >&2; exit 1; }
  if [ "$LIVE_SIGNER" = "$(first_baked_fpr)" ]; then
    echo "$BASE is signed with the current key — nothing to seal"
    exit 0
  fi
  # An empty result means the manifest was ALL junk. Signing that would hand clients a feed that
  # verifies and offers no images, so `punktfunk-sysext update` stops at "no image in the feed".
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
  # Said out loud on purpose: a manifest that silently shrinks loses the feed's rollback history.
  echo "carrying forward $(grep -c '' <"$SUMS") image(s) from the existing manifest"
else
  echo "no signed manifest at $BASE — starting the feed with this image alone"
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
