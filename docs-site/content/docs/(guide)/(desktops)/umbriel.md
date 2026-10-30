---
title: Umbriel
description: Stream an Umbriel session, the Noctalia project's compositor, through its own virtual outputs, and fix socket, capture and monitor problems.
---

Stream an [Umbriel](https://github.com/noctalia-dev/umbriel) session, the Noctalia project's
wlroots compositor. Umbriel creates virtual outputs itself, so it needs no fork or plugin. Install
the host first: [Arch](/docs/arch), [Ubuntu](/docs/ubuntu) or [Fedora](/docs/fedora).

## What works

- Each client gets its own virtual output at its size and refresh rate, with a name starting
  `PF-`, created over Umbriel's IPC and removed when the client leaves. A resize mid-stream works, and a
  reconnect inside the [keep-alive](/docs/virtual-displays#keep-alive) window gets the same output
  back.
- The host captures it straight from the compositor into the GPU encoder, with no copy. No chooser
  appears. A [real monitor](/docs/virtual-displays#stream-a-real-monitor-instead) streams the
  same way.
- Keyboard and mouse go in through the wlroots virtual pointer and keyboard protocols, so Umbriel's
  keybinds fire on keys from the client. The clipboard goes through `ext-data-control`.
- Library launches open on a workspace of the streamed output, and the host switches back when the
  game ends: see [Automation](/docs/automation#a-launch-on-its-own-workspace). A client can move a
  window onto the stream, make it full-screen or close it.
- **Turn off** under [Your monitors while streaming](/docs/virtual-displays#topology) switches your
  monitors off while you stream and back on afterwards. Monitors set to **Stay on** stay lit. A
  [gamescope](/docs/gamescope) session started on the Umbriel desktop switches them off and on too.

It finds Umbriel on every connect; you don't export `UMBRIEL_SOCKET`.

## Install

Use an Umbriel built on **2026-09-28 or later**: older builds can't create virtual outputs.

| System | How to get it |
|---|---|
| Arch | AUR `umbriel-git` |
| Fedora | `umbriel-nightly` from the Terra repository |
| openSUSE | `umbriel-git` from the Noctalia repository on OBS |
| Debian trixie | `umbriel` from the NickH APT repository, through `trixie-backports` |
| Debian sid, Ubuntu 26.04 | `umbriel` from the NickH APT repository |
| NixOS | The `github:noctalia-dev/umbriel` flake: import its module and set `programs.umbriel.enable = true;` |
| Guix | `umbriel` from the `midnight` channel |

The repository lines and keys are in
[Umbriel's install guide](https://docs.noctalia.dev/umbriel/installation/). The host doesn't need
`xdg-desktop-portal-umbriel`; the NixOS module installs it anyway.

## Noctalia

Noctalia's bar, and its dock when turned on, appear on the streamed output as on any monitor. To
keep the bar on your own monitors only, turn it off by default and back on per monitor, in a
`.toml` file under `~/.config/noctalia/`:

```toml
[bar.default]
enabled = false

[bar.default.monitor."DP-1"]   # your monitor's connector
enabled = true
```

Use your bar's name in place of `default`. To scope the dock, set `enabled` the same way under
`[dock]` and `[dock.monitor."DP-1"]`. See
[Noctalia's bar settings](https://docs.noctalia.dev/noctalia/bar/).

## host.env

Nothing is required: the host finds Umbriel on every connect.

Leave `PUNKTFUNK_COMPOSITOR` unset: a pin turns off detection and session following. It accepts
`umbriel` if you need it. Every other option is in [Configuration](/docs/configuration).

## Start the host

Pick **Umbriel** in your display manager, or run `start-umbriel` from a TTY. From inside that
session:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f    # watch it start and print its fingerprint
```

Open [the web console](/docs/web-console) to pair a [client](/docs/clients). To serve Moonlight
too, see [What the unit starts](/docs/running-as-a-service#what-the-unit-starts). To restart the
host with Umbriel, see
[Restart the host with your desktop](/docs/running-as-a-service#restart-the-host-with-your-desktop).

## Headless session

Umbriel starts with no monitor plugged in, and virtual outputs should work there; this is not yet
run on a box. Autologin into the **Umbriel** session through your display manager, and enable
lingering so the host starts at boot: `sudo loginctl enable-linger "$USER"`.

## Known limits

- Streams need a GPU encoder. The host captures Umbriel only straight from the compositor, never
  through a screen cast: see [below](#no-direct-capture).
- SDR only: virtual outputs carry no HDR. HDR on Linux needs
  [gamescope](/docs/gamescope#hdr-on-gamescope) — see [HDR](/docs/hdr#other-linux-desktops).
- No per-device scale: the streamed output is always at scale 1, and **Scale its screen at**
  doesn't apply.
- Saving an output change in Umbriel's config turns your monitors back on mid-stream: see
  [below](#config-save).

## Troubleshooting

### "find the Umbriel IPC socket (no UMBRIEL_SOCKET, no umbriel-*.sock)"

The host found no running Umbriel for its user: it started outside your Umbriel session, or
`PUNKTFUNK_COMPOSITOR` pins Umbriel while another desktop runs. Start the host from inside the
session, as above, and leave the pin unset.

### "This Umbriel session can't be captured directly; the stream needs a GPU encoder" [#no-direct-capture]

The encoder is pinned to `software`, or **Direct capture** is off, and Umbriel has no screen cast
to fall back to. Set **Encoder** to `auto` and turn **Direct capture** back on
([Configuration](/docs/configuration)). If `auto` finds no GPU encoder, see
[GPU and driver](/docs/requirements#gpu-and-driver).

### "The host can't talk to this version of Umbriel" [#ipc-mismatch]

This Umbriel changed how programs talk to it, and your host doesn't know the new way yet. Update
Punktfunk on the host. If no update covers it yet, roll Umbriel back to the last build that
streamed.

### My monitors came back on after I saved my Umbriel config [#config-save]

A saved change to any `[output.*]` section makes Umbriel apply its config to every output again,
which lights the monitors **Turn off** switched off. Change output sections while nobody streams;
the next stream switches the monitors off again.

More fixes in [Troubleshooting](/docs/troubleshooting).
