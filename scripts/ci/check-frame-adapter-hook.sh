#!/bin/sh
# Runs packaging/linux/90-punktfunk-frame-adapter against a fake /sys and /run with iw, nmcli and
# logger stubbed: the Frame adapter gets both power saves off, its hardware scan is on only while
# the link is down, any other interface is left alone, and a box without iw writes the profile.
set -eu
REPO="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
HOOK="$REPO/packaging/linux/90-punktfunk-frame-adapter"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT

fail() { echo "check-frame-adapter-hook: $*" >&2; exit 1; }

# One USB Wi-Fi interface: $1 name, $2 vendor, $3 product, $4 phy.
fake_iface() {
    usb=$T/sys/devices/usb1/1-1
    mkdir -p "$usb/1-1:1.0/net/$1" "$T/sys/class/net" "$T/sys/class/ieee80211/$4"
    echo "$2" >"$usb/idVendor"
    echo "$3" >"$usb/idProduct"
    ln -sfn "$usb/1-1:1.0" "$usb/1-1:1.0/net/$1/device"
    ln -sfn "$T/sys/class/ieee80211/$4" "$usb/1-1:1.0/net/$1/phy80211"
    ln -sfn "$usb/1-1:1.0/net/$1" "$T/sys/class/net/$1"
    DM=$T/sys/kernel/debug/ieee80211/$4/rtw89/disable_dm
    mkdir -p "${DM%/*}"
    seed_dm O O
}

# debugfs renders disable_dm as a list; the fake keeps the mask the hook wrote.
# $1 is HW_SCAN, $2 INACTIVE_PS.
seed_dm() { printf 'Disabled DM: 0x4\n[2] DIG: X\n[4] HW_SCAN: %s\n[5] INACTIVE_PS: %s\n' "$1" "$2" >"$DM"; }

mkdir -p "$T/bin"
for tool in iw nmcli logger; do
    printf '#!/bin/sh\necho "%s $*" >>"%s/calls"\n' "$tool" "$T" >"$T/bin/$tool"
    chmod +x "$T/bin/$tool"
done

run() { : >"$T/calls"; env -i PATH="$T/bin:/usr/bin:/bin" PF_ROOT="$T" "$@" sh "$HOOK" "$IFACE" "$ACTION"; }
MARK=$T/run/punktfunk-frame-adapter/wlan1.hw-scan-off

fake_iface wlan1 28de 2432 phy1
IFACE=wlan1 ACTION=up
run
grep -qx 'iw dev wlan1 set power_save off' "$T/calls" || fail "no power save off: $(cat "$T/calls")"
# DIG stays off, INACTIVE_PS joins it: bits 2 and 5.
[ "$(cat "$DM")" = 0x24 ] || fail "disable_dm not 0x24"

# A link that drops with the scan on is left alone.
seed_dm O X
ACTION=down
run
[ ! -s "$T/calls" ] || fail "acted on 'down' with the scan on: $(cat "$T/calls")"
[ ! -e "$MARK" ] || fail "marked a scan it never turned on"

# Steam's low-latency mode turned the scan off: on while down, off again once up.
seed_dm X X
run
[ "$(cat "$DM")" = 0x24 ] || fail "scan not back on while down: $(cat "$DM")"
[ -e "$MARK" ] || fail "no mark after turning the scan on"
seed_dm O X
ACTION=up
run
[ "$(cat "$DM")" = 0x34 ] || fail "scan not off again after the connect: $(cat "$DM")"
[ ! -e "$MARK" ] || fail "mark left after the connect"

rm -rf "$T/sys"
fake_iface wlan0 0bda 8832 phy0
IFACE=wlan0 ACTION=up
run
[ ! -s "$T/calls" ] || fail "touched another adapter: $(cat "$T/calls")"
seed_dm X X
ACTION=down
run
[ "$(sed -n 3p "$DM")" = "[4] HW_SCAN: X" ] || fail "touched another adapter's scan"

rm -rf "$T/sys" "$T/bin/iw"
fake_iface wlan1 28de 2432 phy1
IFACE=wlan1 ACTION=up
run CONNECTION_UUID=1234
grep -qx 'nmcli connection modify 1234 802-11-wireless.powersave 2' "$T/calls" ||
    fail "no profile fallback without iw: $(cat "$T/calls")"

echo "check-frame-adapter-hook: ok"
