---
title: Steam / gamescope
description: Stream Steam Gaming Mode from a gamescope box — how the host gets a gamescope, the patched build for HDR, and the limits.
---

Stream Steam **Gaming Mode**: the gamescope session on Bazzite, SteamOS, Nobara or any distro that
runs one. Install the host first: [Bazzite](/docs/bazzite) or [SteamOS (Host)](/docs/steamos-host).

## What works

- Each client gets Gaming Mode at its own resolution and refresh rate (managed and bare spawn,
  [below](#how-the-host-gets-a-gamescope)).
- On Bazzite and SteamOS it follows **Switch to Desktop** and back mid-stream, with no reconnect.
  Elsewhere, turn on **Follow mode switches** in the console (`PUNKTFUNK_SESSION_WATCH=1`).
- Controllers, including a virtual Steam Deck pad when you are in the
  [`punktfunk` group](#the-punktfunk-group).
- With [`punktfunk-gamescope`](#hdr-on-gamescope): HDR, the real resolution and refresh rate in
  Steam, the performance overlay, and your keyboard layout.

## host.env

Nothing is required: the host finds gamescope from your live session. The gamescope settings in
[Configuration](/docs/configuration) only force a [mode](#how-the-host-gets-a-gamescope).

## Start the host

The [Bazzite](/docs/bazzite) and [SteamOS](/docs/steamos-host) guides enable the host for you. On
any other distro with a gamescope session:

```sh
systemctl --user enable --now punktfunk-host
```

Then open [the web console](/docs/web-console) to pair a client.

## Headless session

Managed and bare-spawn sessions already run gamescope with no screen. For a box that boots into
Gaming Mode with nobody at it, see
[A headless, always-on host](/docs/running-as-a-service#a-headless-always-on-host).

## How the host gets a gamescope

The host picks one of three modes per session and logs it:
`journalctl --user -u punktfunk-host | grep 'gamescope sub-mode'`.

| Mode | When | What happens |
|---|---|---|
| **Managed** | Default on a box with a gamescope session (Bazzite, SteamOS, Nobara). Force it with `PUNKTFUNK_GAMESCOPE_MANAGED=1`. | The host takes Gaming Mode over and relaunches it headless at the client's resolution and refresh. A Steam running on the desktop is shut down to free it. Your screens drop out; the box gets its session back after you disconnect. |
| **Attach** | `PUNKTFUNK_GAMESCOPE_ATTACH=1`, or a gamescope already runs on a box without a gamescope session. | The host streams the running gamescope and never stops it. It stays at its own mode, except a headless box on its own autologin session, which restarts at the client's resolution. |
| **Bare spawn** | No gamescope session and none running, or a [dedicated game session](/docs/virtual-displays#dedicated-game-sessions). | The host starts its own headless gamescope at the client's mode, running the launched game. Nothing on the box is touched. |

### Nobara and other autologin display managers

When a display manager autologs into Gaming Mode, the managed takeover idles that session for the
stream instead of stopping it: a drop-in under `$XDG_RUNTIME_DIR` swaps its `ExecStart` for a
sleep. Steam is free, the display manager keeps running, and **Switch to Desktop** still works
while you stream. It needs no privilege, and a reboot clears it.

If nothing lights the box's screen 25 seconds after the host hands it back, the host stops that
session so the display manager logs in again, then restarts the display manager through its
packaged helper (`pf-dm-helper`, polkit action `io.unom.punktfunk.dm-helper`). The helper serves
members of the [`punktfunk` group](#the-punktfunk-group) only. If both fail, the host runs
`PUNKTFUNK_RECOVER_SESSION_CMD` when you set one.

### The `punktfunk` group

Join the `punktfunk` group on any box you stream Gaming Mode from, then log out and back in:

```sh
sudo usermod -aG punktfunk "$USER"
```

Without it the virtual Steam Deck pad arrives as an Xbox 360 controller, and the host can't restart
a display manager that fails to come back. The package creates the group empty: it can attach
emulated USB devices, so join it only on a machine you trust. The guided installer joins you by
default; `--no-punktfunk-group` opts out.

## Stream the screen the box is already driving

To stream Gaming Mode exactly as the TV shows it, pick that screen under **Displays** →
**Stream this monitor** in the console, or set `PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1` (names from
`punktfunk-host list-monitors`). Nothing is stopped or relaunched and no mode is imposed. A headless
gamescope has no screen to list. See
[Stream a real monitor instead](/docs/virtual-displays#stream-a-real-monitor-instead).

With **Your monitors while streaming** → **Turn off**, managed and bare-spawn sessions switch the
box's screen off for the stream and back on after. GNOME can't be asked to, and the log says so.
Attach streams that screen, so it stays on.

## HDR on gamescope

A stock gamescope captures 8-bit SDR: it tone-maps HDR games down, correctly, before the host sees
them. For a 10-bit HDR stream install `punktfunk-gamescope`, gamescope plus Punktfunk's capture
patches. It installs under its own name; your Gaming Mode keeps the system gamescope.

| System | How to get it |
|---|---|
| Bazzite, Fedora Atomic | In the Punktfunk sysext: `punktfunk-sysext update`. |
| Fedora, Nobara, other RPM systems | `sudo dnf install punktfunk-gamescope` from the Punktfunk repo. |
| Debian 13, Ubuntu 26.04 | `sudo apt install punktfunk-gamescope` from the Punktfunk repo. |
| Ubuntu 24.04 | Not available: its wayland is too old. Build from source or upgrade. |
| Arch | `sudo pacman -S punktfunk-gamescope` |
| SteamOS | Built by the Steam Deck installer (`scripts/steamdeck/install.sh`, `update.sh`). |
| NixOS | `services.punktfunk.host.gamescopeHdr` (on by default). |
| Anything else | `bash packaging/gamescope/build-punktfunk-gamescope.sh` in the source tree. |

**Restart the host after installing it:** the host checks the gamescope binary once, at startup.
The log then says `using the punktfunk build (10-bit HDR capture available)` instead of
`gamescope has no +pfhdr marker`.

HDR is on by default once the build is present; **Game Mode HDR** in the console
(`PUNKTFUNK_GAMESCOPE_HDR=0`) forces SDR. `PUNKTFUNK_GAMESCOPE_BIN` picks one binary; unset, the
host prefers `punktfunk-gamescope` over `gamescope`.

The build only reaches sessions the host starts: managed and bare spawn. An attached session runs
the distro's gamescope, so it streams SDR and the host draws the pointer. The rest of the HDR
chain, and what to check when a stream comes out SDR, is on [HDR](/docs/hdr#linux--gamescope).

## Known limits

- **gamescope 3.16.22 or newer**; below it capture can deadlock against PipeWire 1.6. The Steam
  overlay (Quick Access Menu) needs **3.16.23**. The host logs the version it found.
- **On a stock gamescope, Steam shows one refresh rate and no resolutions** — 60 Hz if the launch
  lost its flag — and games that pace to the display hold there, though the stream runs at the
  client's rate. `punktfunk-gamescope` reports the real mode;
  `PUNKTFUNK_GAMESCOPE_REFRESH_RATES=60,90,120` adds rates to Steam's menu.
- **The performance overlay** (mangoapp) reaches the stream only with `punktfunk-gamescope`.
- **The keyboard types US layout** on a stock gamescope, whatever the box uses.
- **The pointer costs a full-frame pass** on a stock gamescope, because the host draws it.
  `punktfunk-gamescope` paints it itself.
- **Touch is a single-finger pointer**: taps and drags work, pinch doesn't. The trackpad and pointer
  [touch modes](/docs/input#touch-modes) are unaffected.
- **Desktop [mouse mode](/docs/input#mouse-modes) is unavailable**; the mouse stays captured.
- **No [clipboard](/docs/clipboard)**: gamescope offers the host none.

To stream the Plasma desktop of a Steam box instead, see [KDE Plasma](/docs/kde).

## Troubleshooting

### The host log says "the session did not start at the mode we asked for"

A file in `/etc/gamescope-session-plus/sessions.d/` overrides `GAMESCOPE_BIN` or sets
`GAMESCOPECMD`. Remove that line.

### The box's screen stays dark after a stream

The hand-back failed. Join the [`punktfunk` group](#the-punktfunk-group), or set
`PUNKTFUNK_RECOVER_SESSION_CMD`. To get the screen back now:
`sudo systemctl restart display-manager.service`.

### The stream is black on connect, or stuck at the box's resolution

The session ran in attach mode, which streams the box's own screen at its own mode. Check
`journalctl --user -u punktfunk-host | grep 'gamescope sub-mode'`. Remove
`PUNKTFUNK_GAMESCOPE_ATTACH` or `PUNKTFUNK_GAMESCOPE_NODE` from `host.env` if set. If the log says
`managed takeover unavailable`, the reason is on the same line.
