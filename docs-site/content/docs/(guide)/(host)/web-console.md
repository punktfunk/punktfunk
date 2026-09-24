---
title: The Web Console
description: Open the Punktfunk web console, sign in, arm pairing, and find your way around its pages.
---

The web console is where you run your host from a browser: pairing, displays, the game library,
settings, logs and plugins.

> A streaming host is remote control of the machine. Read [Security & Safe Use](/docs/security)
> first, and keep the host on a trusted LAN or VPN.

## Open the console

Browse to **`https://<host-ip>:47992`** and accept the certificate warning once. The certificate
is the host's own self-signed identity.

The guided installer, the Windows installer and the SteamOS script start the console for you. A
Linux host installed by hand needs it enabled as your desktop user:

```sh
systemctl --user enable --now punktfunk-web
```

`Unit not found` means the console package is missing:
[install it](/docs/troubleshooting-startup#systemctl---user-status-punktfunk-web-unit-not-found).

The console answers devices on your local network or a VPN and refuses the internet. To keep it to
the host machine, set `PUNKTFUNK_UI_BIND=127.0.0.1` in `host.env`
([Configuration](/docs/configuration)) and restart the console.

## Two ports, not one

Plugin interfaces are served on **TCP 47993**, a separate origin, so plugin code can't act with
your console login.

- Open **47992 and 47993** on the host's firewall. The packaged `punktfunk-web` firewall profile
  lists both.
- Your browser trusts the certificate per port. The first time you open a plugin, the console
  shows **Trust this plugin's port once**: click **Open in a new tab**, accept the warning, and
  come back.

An empty plugin panel: see
[A plugin's interface doesn't load](/docs/troubleshooting-connect#a-plugins-interface-doesnt-load).

## Login password

The guided installer asks whether to generate a password or take yours, and the Windows wizard shows
its password on the last page. A silent or winget install shows nothing.

The password file is readable only until your first sign-in; the console then keeps a salted hash.
To read it before that, or reset it after, see [Forgot your Password?](/docs/forgot-password).

## Arm pairing

The host requires PIN pairing. Open **Devices** → **Pair a device**, and enter the 4-digit PIN it
shows on your [client](/docs/clients). A device that already tried to connect waits under
**Waiting for approval**; **Approve** pairs it without a PIN. [Pairing & Trust](/docs/pairing)
covers access levels and removing devices.

## The pages

The sidebar holds five pages, then a **Manage** group. On a phone, **More** holds the rest.

![Live status during a stream: video and audio streaming, the running game, the session's codec, resolution, frame rate and bitrate](/img/console-live-status.png)

| Page | What you do there |
|---|---|
| **Home** | Health warnings, live status, the running game and the last session. The **Sessions** card has one row per connected device: change its [access level](/docs/access-levels) or player number, mute it, request a keyframe, or stop it. A Moonlight row offers only keyframe and stop. |
| **Devices** | Arm a PIN, approve or deny waiting devices, edit a device's access or display settings, unpair. A second PIN box for [Moonlight](/docs/moonlight) appears when GameStream is on. |
| **Displays** | What happens to your screens when a device connects. See [Virtual displays](/docs/virtual-displays). |
| **Library** | Turn game sources on or off, add or edit a custom title. See [Your game library](/docs/game-library). |
| **Host** | Address and deep link for a new device, identity, codecs, ports, GPU choice, [updates](/docs/updating), [host power](/docs/host-power), and **Settings** ([Configuration](/docs/configuration#settings-in-the-web-console)). A setting pinned in `host.env` shows as locked. |
| **Controllers** | The controllers the host holds for a live session, lit by what it receives. |
| **Performance** | Record a capture and read per-stage latency, throughput and drops. See [Recording a capture](/docs/stats#recording-a-capture-for-a-bug-report). |
| **Troubleshooting** | Health checks above the live log of the host, your plugins and the logs clients sent. **Export all** saves one file for a [bug report](/docs/report-an-issue). |
| **Automation** | Run a command or call a webhook when the host does something. See [Events & hooks](/docs/automation). |
| **Plugins** | **Browse**, **Installed** and **Sources**, plus the plugin runner switch. See [Plugins](/docs/plugins). |
| **Settings** | Language, appearance, sign out, and which plugin pages get their own sidebar entry. |
