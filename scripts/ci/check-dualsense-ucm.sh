#!/bin/sh
# Prove that scripts/alsa-ucm2/ still puts a `SpeakerHaptic` device on the DualSense's card —
# WITHOUT a DualSense, and without replacing a single file the distro's `alsa-ucm-conf` owns.
#
# Why this check exists
# --------------------
# The fix it guards is a hook into ANOTHER project's config tree, so it can rot silently:
# `USB-Audio/USB-Audio.conf` includes `USB-Audio/conf.d/{vid}-{pid}.conf` after its device table
# has chosen `ProfileName` and before it includes that profile, and our drop-in redefines the
# variable in between. Rename the profile, drop the include, reorder the two, and our files stop
# doing anything at all — with no error anywhere. What comes back is not a silent downgrade but
# the crash: with no `SpeakerHaptic` the card's only playback route is the 1-channel `Speaker`
# split, PipeWire offers exactly one sink, GE-Proton mints its synthetic "Sony controller
# speaker" endpoint from it, and a game that opens that endpoint overruns it
# (EXCEPTION_ACCESS_VIOLATION).
#
# How it runs without hardware
# ----------------------------
# UCM's card-less path (`conf.virt.d/${OpenName}.conf`) can drive the whole include chain; the
# only things missing are the four built-ins a real card publishes. So the harness copies the
# distro tree to a scratch dir and substitutes literals for exactly those four — the USB id the
# dispatcher matches on and the three cosmetic name/id strings — and changes NOTHING else. The
# include ordering, the profile resolution and the priorities under test are the real ones.
#
# Skips (exit 0) where it cannot run: no `alsaucm`, or no distro ucm2 tree with the DualSense
# profile in it. Meant for the Fedora RPM builder image; harmless anywhere else.
set -eu

REPO="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
OVERLAY="$REPO/scripts/alsa-ucm2"
UCM2="${UCM2_DIR:-/usr/share/alsa/ucm2}"
# The pad the profile is keyed to; the Edge (0df2) resolves through the same profile.
USBID="USB054c:0ce6"

[ -d "$OVERLAY/USB-Audio" ] || { echo "check-dualsense-ucm: no $OVERLAY — wrong repo?" >&2; exit 1; }

if ! command -v alsaucm >/dev/null 2>&1; then
    echo "check-dualsense-ucm: no alsaucm on PATH (Fedora: dnf install alsa-ucm-utils) — skipped"
    exit 0
fi
if [ ! -f "$UCM2/USB-Audio/Sony/DualSense-PS5.conf" ]; then
    echo "check-dualsense-ucm: $UCM2 has no DualSense profile (Fedora: dnf install alsa-ucm) — skipped"
    exit 0
fi

TREE="$(mktemp -d)"
trap 'rm -rf "$TREE"' EXIT INT TERM
cp -a "$UCM2/." "$TREE/"

# Feed the dispatcher the pad's USB id, which normally comes off the card.
sed -i.bak \
    -e "s|String \"\${CardComponents}\"|String \"$USBID\"|" \
    -e "s|Haystack \"\${CardComponents}\"|Haystack \"$USBID\"|" \
    "$TREE/USB-Audio/USB-Audio.conf"
# ...and the three name/id strings the device sections interpolate. Cosmetic to this test: they
# only ever land in a Comment or in the `hw:` PCM name, never in a priority or a channel map.
find "$TREE/USB-Audio" "$TREE/common" -name '*.conf' -exec sed -i.bak \
    -e 's|${CardName}|DualSense|g' \
    -e 's|${CardId}|Controller|g' \
    -e 's|${CardLongName}|DualSenseLong|g' {} +
find "$TREE" -name '*.conf.bak' -delete

mkdir -p "$TREE/conf.virt.d"
printf 'Syntax 8\nInclude.a.File "/USB-Audio/USB-Audio.conf"\n' > "$TREE/conf.virt.d/pftest.conf"

ucm() { ALSA_CONFIG_UCM2="$TREE" alsaucm -c pftest "$@" 2>&1 | grep -v 'no soundcards found' || true; }

# Baseline: informational only. Upstream adopting the device would make our drop-in redundant
# rather than wrong, and that is worth seeing in the log rather than failing on.
before="$(ucm set _verb Default list _devices | grep -c 'SpeakerHaptic' || true)"

cp -a "$OVERLAY/USB-Audio/." "$TREE/USB-Audio/"
find "$TREE/USB-Audio/Punktfunk" -name '*.conf' -exec sed -i.bak -e 's|${CardId}|Controller|g' {} +
find "$TREE" -name '*.conf.bak' -delete

after="$(ucm set _verb Default list _devices || true)"
echo "$after" | grep -q 'SpeakerHaptic' || {
    echo "check-dualsense-ucm: FAIL — the drop-in did not add a SpeakerHaptic device." >&2
    echo "  The distro's $UCM2/USB-Audio/USB-Audio.conf probably no longer resolves the" >&2
    echo "  DualSense through \${var:ProfileName}, or no longer includes USB-Audio/conf.d/." >&2
    echo "  Devices seen:" >&2
    echo "$after" | sed 's/^/    /' >&2
    exit 1
}

# Existing is not enough: it is OUTRANKING the mono `Speaker` that keeps the 1-channel sink —
# and the crash path with it — from ever being minted.
prios="$(ucm set _verb Default get PlaybackPriority/SpeakerHaptic get PlaybackPriority/Speaker)"
haptic="$(echo "$prios" | sed -n 's|.*PlaybackPriority/SpeakerHaptic=\([0-9]*\).*|\1|p')"
mono="$(echo "$prios" | sed -n 's|.*PlaybackPriority/Speaker=\([0-9]*\).*|\1|p')"
[ -n "$haptic" ] && [ -n "$mono" ] || {
    echo "check-dualsense-ucm: FAIL — could not read both playback priorities:" >&2
    echo "$prios" | sed 's/^/    /' >&2
    exit 1
}
[ "$haptic" -gt "$mono" ] || {
    echo "check-dualsense-ucm: FAIL — SpeakerHaptic ($haptic) does not outrank Speaker ($mono)," >&2
    echo "  so the card can still land on the 1-channel sink that games overrun." >&2
    exit 1
}

echo "check-dualsense-ucm: ok — SpeakerHaptic present at priority $haptic over Speaker's $mono" \
     "(distro tree alone had $before)"
