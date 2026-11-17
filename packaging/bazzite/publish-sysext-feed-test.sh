#!/usr/bin/env bash
# Runs publish-sysext-feed.sh against a fake registry (a curl shim on PATH) with throwaway keys
# as FEED_KEY, and checks that only a live manifest those keys signed for the feed is carried or
# re-signed. Needs gpg and GNU sed/coreutils: bash packaging/bazzite/publish-sysext-feed-test.sh
set -euo pipefail
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
T="$(mktemp -d)"; trap 'gpgconf --kill gpg-agent 2>/dev/null; rm -rf "$T"' EXIT
export GNUPGHOME="$T/gnupg"; mkdir -m 700 "$GNUPGHOME"
mkdir "$T/pkg" "$T/bin" "$T/reg"
REG="$T/reg"

gen() { # gen <name>: a new signing key's fingerprint
  gpg --batch --quiet --pinentry-mode loopback --passphrase '' \
    --quick-gen-key "$1 <$1@test.invalid>" ed25519 sign never
  gpg --list-keys --with-colons "$1@test.invalid" | awk -F: '/^fpr:/{print $10; exit}'
}
K1="$(gen signing)"
PRIV="$(gpg --batch --pinentry-mode loopback --passphrase '' --armor --export-secret-keys)"
# The key before a rotation: still listed in FEED_KEY, after the signing key.
K0="$(gen retired)"
cp "$SRC/publish-sysext-feed.sh" "$T/pkg/"
printf "FEED_KEY='%s'\n" "$(gpg --batch --armor --export)" > "$T/pkg/punktfunk-sysext.sh"
printf 'image' > "$T/punktfunk-2.0.0-x86-64.raw"

# GET serves $REG/<name>, 404 when absent; DELETE and uploads are logged to $REG/calls.
cat > "$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
out=/dev/null; fmt=; method=GET; up=
while [ $# -gt 1 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
    -w) fmt="$2"; shift ;;
    -X) method="$2"; shift ;;
    --upload-file) up="$2"; method=PUT; shift ;;
    --user) shift ;;
  esac
  shift
done
name="${1##*/}"
if [ "$method" != GET ]; then
  echo "$method $name" >> "$REG/calls"
  if [ -n "$up" ]; then cp "$up" "$REG/put-$name"; fi
  exit 0
fi
if [ -f "$REG/$name" ]; then cp "$REG/$name" "$out"; code=200; else echo 'Not Found' > "$out"; code=404; fi
if [ -n "$fmt" ]; then printf '%s' "$code"; fi
EOF
chmod +x "$T/bin/curl"

OLD="$(printf '%064d' 1)  punktfunk-1.0.0-x86-64.raw"
EVIL="$(printf '%064d' 9)  punktfunk-99.0.0-x86-64.raw"
serve() { # serve <feed> <lines…>: a live manifest bound to <feed>, signed by the key in $BY
  rm -f "$REG"/*
  local feed="$1"; shift
  { printf '# FEED %s\n# SERIAL 5\n' "$feed"; printf '%s\n' "$@"; } > "$REG/SHA256SUMS"
  gpg --batch --quiet --yes --armor --detach-sign --local-user "${BY:-$K1}" \
    --output "$REG/SHA256SUMS.asc" "$REG/SHA256SUMS"
}
run() { # run <publisher args…>: exit code in $rc, output in $T/out
  rm -f "$REG/calls" "$REG"/put-*
  rc=0
  PATH="$T/bin:$PATH" REG="$REG" TOKEN=t RPM_GPG_PRIVATE_KEY="$PRIV" \
    bash "$T/pkg/publish-sysext-feed.sh" "$@" > "$T/out" 2>&1 || rc=$?
}
fail() { echo "FAIL: $*" >&2; cat "$T/out" >&2; exit 1; }
refused() { if [ "$rc" = 0 ] || [ -e "$REG/calls" ]; then fail "$1 was not refused before any upload"; fi; }
# over <case>: the publish went ahead with a manifest holding only its own image.
over() {
  if [ "$rc" != 0 ] || [ "$(grep -vc '^#' "$REG/put-SHA256SUMS")" != 1 ] \
    || ! grep -q ' punktfunk-2.0.0-x86-64.raw$' "$REG/put-SHA256SUMS"; then
    fail "$1 did not start the feed over"
  fi
}

rm -f "$REG"/*
run f43 "$T/punktfunk-2.0.0-x86-64.raw"
if [ "$rc" != 0 ] || ! grep -qx '# FEED f43' "$REG/put-SHA256SUMS" || [ ! -s "$REG/put-SHA256SUMS.asc" ]; then
  fail "a new feed"
fi

serve f43 "$OLD"
run f43 "$T/punktfunk-2.0.0-x86-64.raw"
if [ "$rc" != 0 ] || ! grep -qxF "$OLD" "$REG/put-SHA256SUMS"; then fail "a verified line is carried"; fi
serial="$(sed -n 's/^# SERIAL //p' "$REG/put-SHA256SUMS")"
[ "$serial" -gt 5 ] || fail "the serial rises above the verified one"

serve f43 "$OLD"
echo "$EVIL" >> "$REG/SHA256SUMS"
run f43 "$T/punktfunk-2.0.0-x86-64.raw"; over "a line added after signing"
serve f43 "$OLD"
echo "$EVIL" >> "$REG/SHA256SUMS"
run --seal f43; refused "a seal over a line added after signing"

serve f43-canary "$OLD"
run f43 "$T/punktfunk-2.0.0-x86-64.raw"; over "a manifest bound to another feed"

# A run cut between its manifest and its signature.
serve f43 "$OLD"
rm "$REG/SHA256SUMS.asc"
run --seal f43; refused "a manifest with no signature"
run f43 "$T/punktfunk-2.0.0-x86-64.raw"; over "a manifest with no signature"

serve f43 "$OLD"
run --seal f43
if [ "$rc" != 0 ] || [ -e "$REG/calls" ]; then fail "a seal rewrote a feed the current key signed"; fi

BY="$K0" serve f43 "$OLD"
run --seal f43
if [ "$rc" != 0 ] || ! grep -qxF "$OLD" "$REG/put-SHA256SUMS" || [ ! -s "$REG/put-SHA256SUMS.asc" ]; then
  fail "a seal over the retired key's signature"
fi

echo "publish-sysext-feed: ok"
