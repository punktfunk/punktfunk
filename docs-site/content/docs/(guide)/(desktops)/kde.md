---
title: KDE Plasma (KWin)
description: Stream a KDE Plasma desktop, or run a login-less KWin appliance, and fix what KWin refuses.
---

Stream a KDE Plasma desktop, or run a KWin box that streams at boot with nobody logged in. Install
the host first: [Ubuntu](/docs/ubuntu), [Fedora](/docs/fedora), [Arch](/docs/arch) or
[Bazzite](/docs/bazzite).

## What works

- Each client gets its own virtual display at its exact mode, captured on the GPU (NVIDIA, AMD and
  Intel).
- Keyboard, mouse and touch go straight into KWin, with no "Allow remote control?" dialog.
- A display scale you set for a client comes back on its next connect.
- On Bazzite and SteamOS the host follows a switch to Game Mode and back mid-stream — see
  [Steam / gamescope](/docs/gamescope).

It needs Plasma 6 on a **Wayland** session; pick it on the login screen. KWin reads the host's
permission file when you log in, so **log out and back in once** after installing the host.

## host.env

Nothing is required: the host finds KWin on every connect.

Don't set `PUNKTFUNK_COMPOSITOR`, `WAYLAND_DISPLAY` or `XDG_CURRENT_DESKTOP`. A pinned compositor
turns off detection and session following, and stale session values point the host at dead
sockets. The [headless session](#headless-session) pins them on purpose. Every other option is in
[Configuration](/docs/configuration).

## Start the host

From inside your Plasma session:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f    # watch it start and print its fingerprint
```

Then add the drop-in from
[Restart the host with your desktop](/docs/running-as-a-service#restart-the-host-with-your-desktop),
or restarting Plasma leaves the host tied to a KWin that is gone. On a box that switches between
the desktop and Game Mode, also run `sudo loginctl enable-linger "$USER"`, or the switch stops the
host mid-stream.

Open [the web console](/docs/web-console) to pair a [client](/docs/clients). To serve Moonlight
too, see [What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

## Headless session

A box with no graphical login runs its own headless KWin session, `punktfunk-kde-session`, which
needs none of the permissions above. It needs KWin 6.5.6 or newer (`kwin_wayland --version`).

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.kde ~/.config/punktfunk/host.env  # Ubuntu: punktfunk-host/
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-kde-session punktfunk-host
sudo loginctl enable-linger "$USER"
```

Full setup: [Running as a Service](/docs/running-as-a-service).

## Known limits

- Refresh rates above 60 Hz need KWin 6.6 or newer.
- SDR only: HDR on Linux needs [gamescope](/docs/gamescope#hdr-on-gamescope) — see
  [HDR](/docs/hdr#other-linux-desktops).
- X11 sessions can't stream.

## Troubleshooting

`punktfunk-host probe-compositor`, run inside the session, exits 0 when KWin is ready and prints
the reason when it isn't.

### KWin does not expose `zkde_screencast_unstable_v1`

KWin hasn't granted the host its screencast permission. Log out and back in once. If that doesn't
fix it, `getcap /usr/bin/punktfunk-host` must print nothing: KWin can't identify a binary that
carries a capability — see
[GPU scheduling priority](/docs/running-as-a-service#gpu-scheduling-priority).

### "KWin virtual output failed: Could not find output"

KWin refused the display; the message arrives in your session's language. Its backend can't create
one (a nested KWin, or `kwin_wayland --virtual` below 6.5.6), or KWin 6.6 created it disabled. The
host enables a disabled one and retries on its own. If it keeps failing:

- `punktfunk-host list-monitors` shows a `Virtual-punktfunk-*` display marked `disabled`.
- `~/.config/kwinoutputconfig.json` holds a stale entry for it.
- `journalctl --user -b -t kwin_wayland` says "Applying output configuration failed!": KWin won't
  enable one more display beside your monitors.

Meanwhile, stream a real monitor instead: **Stream this monitor** under **Displays** in the
console, or `PUNKTFUNK_CAPTURE_MONITOR` — see
[Virtual displays](/docs/virtual-displays#stream-a-real-monitor-instead).

### "Allow remote control?" appears, or input does nothing, with `PUNKTFUNK_INPUT_BACKEND=libei`

The libei input path, and portal capture (`PUNKTFUNK_VIDEO_SOURCE=portal`), go through KDE's
RemoteDesktop portal, which needs a one-time grant. Seed it, then log out and back in:

```sh
bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh    # Fedora, Bazzite

mkdir -p ~/.local/share/flatpak/db                        # Ubuntu, Arch
cp /usr/share/punktfunk*/headless/kde-authorized ~/.local/share/flatpak/db/kde-authorized
```

### Black screen, no picture

Check you are on Wayland and, on NVIDIA, that the GL userspace is installed. More in
[Troubleshooting](/docs/troubleshooting).
