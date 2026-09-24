---
title: GNOME (Mutter)
description: Stream a GNOME desktop, keep it unlocked for an always-on host, and fix the NVIDIA and lock-screen traps.
---

Stream a GNOME desktop, or keep one up for an always-on host. Install the host first:
[Ubuntu](/docs/ubuntu), [Fedora](/docs/fedora) or [Arch](/docs/arch).

## What works

- Each client gets its own virtual display at its exact mode, created over Mutter's own D-Bus API,
  so no portal dialog asks for permission.
- Input goes in through Mutter's remote-desktop API, headless included.
- A display scale you set for a client comes back on its next connect.

It needs GNOME 48 or newer on a **Wayland** session.

## host.env

Nothing is required: the host finds Mutter on every connect.

Don't set `PUNKTFUNK_COMPOSITOR`, `WAYLAND_DISPLAY` or `XDG_CURRENT_DESKTOP`. A pinned compositor
turns off detection and session following, and stale session values point the host at dead
sockets. Every other option is in [Configuration](/docs/configuration).

## Start the host

Turn off the lock screen first: a locked session blocks capture, and an always-on host has nobody
to unlock it.

```sh
gsettings set org.gnome.desktop.screensaver lock-enabled false
gsettings set org.gnome.desktop.session idle-delay 0
```

Then, from inside your GNOME session:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f    # watch it start and print its fingerprint
```

Add the drop-in from
[Restart the host with your desktop](/docs/running-as-a-service#restart-the-host-with-your-desktop),
or restarting GNOME Shell leaves the host tied to a Mutter that is gone.

Open [the web console](/docs/web-console) to pair a [client](/docs/clients). To serve Moonlight
too, see [What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

## Headless session

With no monitor and no one logging in, keep a GNOME session up through GDM autologin:

```ini
# /etc/gdm3/custom.conf (Ubuntu, Debian) · /etc/gdm/custom.conf (Fedora, Arch)
[daemon]
AutomaticLoginEnable = true
AutomaticLogin = your-user
```

Turn off the lock screen as above, enable the host, and let it run without a login:

```sh
systemctl --user enable --now punktfunk-host
sudo loginctl enable-linger "$USER"
```

Reboot; the host comes up on the autologin session. Full setup:
[Running as a Service](/docs/running-as-a-service).

## Known limits

- The virtual display streams SDR: Mutter records virtual displays in 8-bit. GNOME 50 can stream
  a real monitor in HDR to Moonlight clients — see
  [HDR → Linux + GNOME](/docs/hdr#linux--gnome).
- X11 sessions can't stream.

## Troubleshooting

### Capture fails: "Session creation inhibited" [#do-not-lock-the-session]

The session is locked. Unlock it, and turn the lock screen off as in
[Start the host](#start-the-host).

### gnome-shell won't start, or the host logs "GPU … not supported by EGL" [#the-glegl-userspace]

The NVIDIA GL/EGL userspace is missing; the base driver package doesn't always pull it in. Install
it — on Ubuntu `libnvidia-gl-<version>` matching your driver; on Fedora and Arch it comes with the
driver package — then check the vendor file exists:

```sh
ls /usr/share/glvnd/egl_vendor.d/10_nvidia.json
```

More fixes — black screen, discovery, pairing — in [Troubleshooting](/docs/troubleshooting).
