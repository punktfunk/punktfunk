---
title: mango
description: Stream a mango session through its own headless outputs, and fix socket and capture problems.
---

Stream a [mango](https://github.com/mangowm/mango) session, the dwl-based wlroots compositor. mango
creates headless outputs itself, so it needs no fork or plugin. Install the host first:
[Arch](/docs/arch), [Ubuntu](/docs/ubuntu) or [Fedora](/docs/fedora).

This backend hasn't been run on real hardware yet.

## What works

- Each client gets its own headless output at its size and refresh rate, with a name starting
  `PF-`, created over mango's IPC. When the client leaves, the output is switched off and the next
  client reuses it.
- The host captures it straight from the compositor into the GPU encoder, with no copy. No chooser
  appears. A [real monitor](/docs/stream-a-monitor) streams the same way, and one mango put to
  sleep wakes for it.
- Keyboard and mouse go in through the wlroots virtual pointer and keyboard protocols.
- Library launches open on a tag of the streamed output, and the host switches back when the game
  ends: see [Automation](/docs/automation#a-launch-on-its-own-workspace).
- **Turn off** in [Your monitors while streaming](/docs/virtual-displays#topology) switches your
  monitors off and puts each back where it was when the stream ends.

It needs mango **0.17.1 or newer**, the first to name the outputs it creates. It was written
against 0.18.6.

## host.env

Nothing is required: the host finds mango on every connect, so you don't export
`MANGO_INSTANCE_SIGNATURE`.

Leave `PUNKTFUNK_COMPOSITOR` unset: a pin turns off detection and session following. It accepts
`mango` if you need it. Every other option is in [Configuration](/docs/configuration).

## Start the host

Pick **mango** in your display manager, or run `mango` from a TTY. From inside that session:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f    # watch it start and print its fingerprint
```

Open [the web console](/docs/web-console) to pair a [client](/docs/clients).

## Known limits

- Streams need a GPU encoder. The host captures mango only straight from the compositor, never
  through a screen cast: see [below](#no-direct-capture).
- SDR only. HDR on Linux needs [gamescope](/docs/gamescope#hdr-on-gamescope) or
  [Hyprland](/docs/hyprland) — see [HDR](/docs/hdr#other-linux-desktops).
- Switched-off `PF-` outputs stay in mango's output list until nobody streams. mango removes
  headless outputs only all at once, so with a headless output of your own they stay until mango
  restarts.
- A config reload during a stream lights the switched-off outputs; the host turns them off again
  when a client connects or leaves.

## Troubleshooting

### "no running mango IPC socket"

The host found no running mango for its user: it started outside your mango session, or
`PUNKTFUNK_COMPOSITOR` pins mango while another desktop runs. Start the host from inside the
session, as above, and leave the pin unset.

### "This mango session can't be captured directly; the stream needs a GPU encoder" [#no-direct-capture]

The encoder is pinned to `software`, or **Direct capture** is off, and the host has no screen cast
of mango to fall back to. Set **Encoder** to `auto` and turn **Direct capture** back on
([Configuration](/docs/configuration)). If `auto` finds no GPU encoder, see
[GPU and driver](/docs/requirements#gpu-and-driver).

### "mango … can't name a virtual output"

This mango is older than 0.17.1. Update it; the host refuses rather than leave an output it
can't find.

### "mango runs nested"

mango runs inside another desktop, where each output it creates is a window. Start mango from your
display manager or a TTY instead.

### "mango listed no PF-… within 1.5s"

mango didn't create the headless output. Its own log says why; it needs its headless backend,
which it starts with every session.

More fixes in [Troubleshooting](/docs/troubleshooting).
