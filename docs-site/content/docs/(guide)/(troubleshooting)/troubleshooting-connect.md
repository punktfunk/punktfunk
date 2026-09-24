---
title: Connection & discovery
description: Fixes for a client that can't find or reach the host — other streaming hosts, discovery, the video port, wake, pairing and plugin pages.
---

Fixes for a client that can't find or reach the host. A Windows host on a **Public** network is
under [Windows firewall](/docs/troubleshooting#windows-firewall).

## Finding the host

### Another streaming host (Sunshine, Apollo, …) is installed

Sunshine and its forks (Apollo, Vibeshine, …) want the same ports, TCP 47990 even with GameStream
off. You see `address already in use` in the host log, pairing that fails, or the wrong host
answering.

Run `punktfunk-host detect-conflicts`, or read **Competing streaming server** on the console's
**Troubleshooting** page. Then uninstall the other host, or keep both by moving Punktfunk's port:
[Switching from Sunshine](/docs/switching-from-sunshine).

### The host isn't found on the network

Discovery uses mDNS on the local network, and anything that blocks it hides the host. Check:

- The host is running: `systemctl --user status punktfunk-host` (Linux),
  `punktfunk-host service status` (Windows, elevated).
- Android: tap **Allow…** under **Local network access is off** at the top of the host list.
- Both devices are on the same subnet. mDNS doesn't cross routers or most VPNs; add the host by
  its IP address instead.
- The host firewall lets discovery in: [Ports → Enable the profiles](/docs/ports#enable-the-profiles).
- **Local discovery** is on under **Host → Settings** (an advanced setting).

### The host is asleep and won't wake

A client wakes only a host it has seen awake, and only if the host's network card is armed for it.
A Linux host logs `Wake-on-LAN is NOT armed` with the command for its card. Arming for every host:
[Wake-on-LAN → Arming the machine](/docs/wake-on-lan#arming-the-machine).

### Pairing is rejected / the client can't connect

The host admits only paired devices. Approve the device in the web console, or pair with a PIN:
[Pairing](/docs/pairing). A host that lost its identity (config deleted, OS reinstalled) needs
pairing again.

## After connecting

### Video is slow to start, or fails across subnets

A host firewall drops the client's first packet to the video port, which is random per session.
Each start then waits about 2.5 s, and the host log warns
`no hole-punch reached this host's data port`.

[Pin the video port](/docs/friends-over-the-internet#pin-the-video-port), then open it on a Linux
host, for example `sudo ufw allow 9779/udp`. A pinned port carries one session at a time; a second
one falls back to a random port.

### A plugin's interface doesn't load

Plugin pages come from their own port, TCP 47993, next to the console's 47992.

- **Trust this plugin's port once**: your browser hasn't accepted the host's certificate for that
  port. Press **Open in a new tab**, accept the warning, and come back.
- **The panel stays empty**: a firewall rule saved before 47993 existed blocks it. Refresh it:

  ```sh
  sudo ufw app update punktfunk-web && sudo ufw reload    # ufw
  sudo firewall-cmd --reload                               # firewalld
  ```

  On Windows, run the setup again; it adds both console rules.
- **Plugin interfaces are unavailable**: the console couldn't open that port. Set another with
  `PUNKTFUNK_UI_PLUGIN_PORT` in `host.env` and restart the console.
