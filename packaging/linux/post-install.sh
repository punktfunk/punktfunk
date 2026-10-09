#!/bin/sh
# The host state every punktfunk-host install shares, run as root once the payload is in place:
# by the deb postinst, pacman's post_install and post_upgrade, rpm's %post and punktfunk-sysext's
# post_merge. Installed as /usr/libexec/punktfunk/post-install (/usr/lib/punktfunk on Arch).
# Capabilities and the first-run hints stay with each format. Every step is best-effort and the
# script exits 0: nothing here may fail an install.

# First: 60-punktfunk.rules chgrp's the vhci nodes to 'punktfunk'.
systemd-sysusers punktfunk.conf >/dev/null 2>&1 || true
# /dev/uinput and /dev/uhid pick up the rule without a reboot (a no-op in a container).
udevadm control --reload-rules 2>/dev/null || true
udevadm trigger --subsystem-match=misc 2>/dev/null || true
# systemd-sysctl applies the UDP buffer sizes at boot; this applies them now.
sysctl -p /usr/lib/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || true
if [ -f /usr/lib/tmpfiles.d/punktfunk-seats.conf ]; then
    systemd-tmpfiles --create /usr/lib/tmpfiles.d/punktfunk-seats.conf >/dev/null 2>&1 || true
fi

# steamos-manager boxes: Valve's ds_inhibit walks /proc on every open of our virtual DualSense's
# hidraw, SELinux denies it at hundreds of AVCs a second and setroubleshootd turns that into a
# box-wide stall. Keyed on the module name, so rename the .cil when its rules change.
CIL=/usr/share/punktfunk/selinux/punktfunk-ds-inhibit.cil
if command -v semodule >/dev/null 2>&1 && [ -e /usr/lib/steamos-manager ] && [ -f "$CIL" ] &&
   ! semodule -l 2>/dev/null | grep -qx punktfunk-ds-inhibit; then
    echo "installing SELinux drop-in 'punktfunk-ds-inhibit' (silences the steamos-manager ds_inhibit audit flood)…"
    semodule -i "$CIL" ||
        echo "!! semodule -i failed — the ds_inhibit audit flood stays live; see packaging/bazzite/README.md" >&2
fi

# An open firewall keeps the ports a profile had when it was allowed: ufw stores the expanded
# rules, firewalld serves the service it last loaded. Name each profile missing a port this
# package added. `ufw status verbose` prints expanded ports, so it can tell stale from current.
if command -v ufw >/dev/null 2>&1 &&
   ufw status verbose 2>/dev/null | grep -q 'punktfunk-web' &&
   ! ufw status verbose 2>/dev/null | grep -q '47993'; then
    echo ""
    echo "punktfunk: your ufw rule for 'punktfunk-web' predates TCP 47993 (plugin UIs, served"
    echo "  from their own origin). Plugin interfaces will not load in the console until:"
    echo "    sudo ufw app update punktfunk-web && sudo ufw reload"
fi
if command -v ufw >/dev/null 2>&1 &&
   ufw status verbose 2>/dev/null | grep -q 'punktfunk-native' &&
   ! ufw status verbose 2>/dev/null | grep -q '9778'; then
    echo ""
    echo "punktfunk: your ufw rule for 'punktfunk-native' predates UDP 9778 (browser streaming)."
    echo "  A browser cannot connect to this host until:"
    echo "    sudo ufw app update punktfunk-native && sudo ufw reload"
fi
# --info-service answers from the definition the daemon loaded, i.e. the stale one.
if command -v firewall-cmd >/dev/null 2>&1 &&
   firewall-cmd --state >/dev/null 2>&1 &&
   firewall-cmd --query-service=punktfunk-web >/dev/null 2>&1 &&
   ! firewall-cmd --info-service=punktfunk-web 2>/dev/null | grep -q '47993'; then
    echo ""
    echo "punktfunk: the punktfunk-web firewalld service now also covers TCP 47993 (plugin UIs)."
    echo "  Plugin interfaces will not load in the console until:  sudo firewall-cmd --reload"
fi
if command -v firewall-cmd >/dev/null 2>&1 &&
   firewall-cmd --state >/dev/null 2>&1 &&
   firewall-cmd --query-service=punktfunk-native >/dev/null 2>&1 &&
   ! firewall-cmd --info-service=punktfunk-native 2>/dev/null | grep -q '9778'; then
    echo ""
    echo "punktfunk: the punktfunk-native firewalld service now also covers UDP 9778 (browser"
    echo "  streaming). A browser cannot connect to this host until:  sudo firewall-cmd --reload"
fi

# A Moonlight-compatible host (Sunshine, Apollo, …) next to this one: the host's own detector
# prints the warning and exits 1.
if command -v punktfunk-host >/dev/null 2>&1 &&
   ! conflict="$(punktfunk-host detect-conflicts 2>/dev/null)"; then
    echo ""
    echo "$conflict"
fi
exit 0
