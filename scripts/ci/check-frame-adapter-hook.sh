#!/bin/sh
# Runs packaging/linux/90-punktfunk-frame-adapter against a fake /sys with iw, nmcli and logger
# stubbed: the Frame adapter gets Steam's low-latency mode on connect, 'down' and any other
# interface are left alone, and a box without iw writes power save into the connection profile.
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
    DBG=$T/sys/kernel/debug/ieee80211/$4/rtw89
    mkdir -p "$DBG"
    printf 'Disabled DM: 0x4\n[2] DIG: X\n[4] HW_SCAN: O\n[5] INACTIVE_PS: O\n' >"$DBG/disable_dm"
    echo "0" >"$DBG/static_pd_th"
}

mkdir -p "$T/bin"
for tool in iw nmcli logger; do
    printf '#!/bin/sh\necho "%s $*" >>"%s/calls"\n' "$tool" "$T" >"$T/bin/$tool"
    chmod +x "$T/bin/$tool"
done

run() { : >"$T/calls"; env -i PATH="$T/bin:/usr/bin:/bin" PF_ROOT="$T" "$@" sh "$HOOK" "$IFACE" "$ACTION"; }
called() { grep -qx "$1" "$T/calls" || fail "no '$1': $(cat "$T/calls")"; }

fake_iface wlan1 28de 2432 phy1
IFACE=wlan1 ACTION=up
run
called 'iw dev wlan1 set power_save off'
called 'iw dev wlan1 set tidconf tids 0xff amsdu off'
called 'iw dev wlan1 set bitrates he-mcs-5 1:0-4 2:0-4 he-mcs-6 1:0-4 2:0-4'
# DIG stays off, HW_SCAN and INACTIVE_PS join it: bits 2, 4 and 5.
[ "$(cat "$DBG/disable_dm")" = 0x34 ] || fail "disable_dm not 0x34: $(cat "$DBG/disable_dm")"
[ "$(cat "$DBG/static_pd_th")" = "1 -75" ] || fail "static_pd_th not '1 -75'"

fake_iface wlan1 28de 2432 phy1
ACTION=down
run
[ ! -s "$T/calls" ] || fail "acted on 'down': $(cat "$T/calls")"
[ "$(sed -n 3p "$DBG/disable_dm")" = "[4] HW_SCAN: O" ] || fail "wrote disable_dm on 'down'"

rm -rf "$T/sys"
fake_iface wlan0 0bda 8832 phy0
IFACE=wlan0 ACTION=up
run
[ ! -s "$T/calls" ] || fail "touched another adapter: $(cat "$T/calls")"
[ "$(cat "$DBG/static_pd_th")" = 0 ] || fail "touched another adapter's firmware"

rm -rf "$T/sys" "$T/bin/iw"
fake_iface wlan1 28de 2432 phy1
IFACE=wlan1 ACTION=up
run CONNECTION_UUID=1234
called 'nmcli connection modify 1234 802-11-wireless.powersave 2'

echo "check-frame-adapter-hook: ok"
