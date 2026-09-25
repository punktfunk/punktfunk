#!/bin/sh
# Restart the running punktfunk user services of every user after a package update.
# try-restart leaves a stopped or opted-out service stopped. Under the in-console updater
# this does nothing: the host restarts all three itself once punktfunk-update.service returns.
# Always exits 0 — a restart must never fail the package transaction.
[ "$(systemctl show -P ActiveState punktfunk-update.service 2>/dev/null)" = activating ] && exit 0
loginctl list-users --no-legend 2>/dev/null | while read -r _ user _; do
    systemctl --user -M "$user@" daemon-reload 2>/dev/null || continue
    for unit in punktfunk-web.service punktfunk-scripting.service punktfunk-host.service; do
        systemctl --user -M "$user@" --no-block try-restart "$unit" 2>/dev/null
    done
done
exit 0
