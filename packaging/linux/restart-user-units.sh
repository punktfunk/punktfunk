#!/bin/sh
# Restart the running punktfunk services after a package update. try-restart leaves a stopped or
# opted-out service stopped. Under the in-console updater the host restarts its own three once
# punktfunk-update.service returns. Always exits 0 — a restart must never fail the transaction.
# The unit table: check-packaging-policy.py holds every other updater to these two lines. This is
# the only place a package restarts them, once the host they run is unpacked.
SYSTEM_UNITS="punktfunk-seats.service punktfunk-door.service punktfunk-web-door.service"
USER_UNITS="punktfunk-web.service punktfunk-scripting.service punktfunk-host.service"
# A unit file this update changed counts only after a reload, which rpm's own trigger may not
# have done yet.
# shellcheck disable=SC2086 # three unit names
if systemctl show -P NeedDaemonReload $SYSTEM_UNITS 2>/dev/null | grep -qx yes; then
    systemctl daemon-reload 2>/dev/null
fi
# shellcheck disable=SC2086
systemctl --no-block try-restart $SYSTEM_UNITS 2>/dev/null
[ "$(systemctl show -P ActiveState punktfunk-update.service 2>/dev/null)" = activating ] && exit 0
loginctl list-users --no-legend 2>/dev/null | while read -r _ user _; do
    systemctl --user -M "$user@" daemon-reload 2>/dev/null || continue
    for unit in $USER_UNITS; do
        systemctl --user -M "$user@" --no-block try-restart "$unit" 2>/dev/null
    done
done
exit 0
