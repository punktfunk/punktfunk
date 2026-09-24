---
title: Multi-seat contract
description: What the Windows host promises the opt-in punktfunk-seats supervisor — seat variables, the connector reservation and per-seat behaviour.
---

The rules between the host and [`punktfunk-seats`](https://git.unom.io/unom/punktfunk-seats), the
opt-in Windows supervisor that runs one host per seat so several people can play on one box. A
normal install has one host on the console session, and none of this applies to it.

The supervisor owns the seat accounts, the Windows sessions, RDP and its own configuration. The host
never learns any of it, so the add-on installs, updates and uninstalls on its own.

**Contract version: 1.** There is no runtime handshake: the supervisor and the host ship versioned
together, and a mismatch shows up as a marker or variable below not being honoured.

## What Windows requires

- **Windows Server** plus a **Remote Desktop Services CAL for every seat**. Per-Device CALs fit
  fixed seats: a seat is a place, not a person.
- **A client edition (Windows 10 or 11) serves one session at a time.** A single seat works there;
  concurrent seats don't.

## The reservation marker

```
HKLM\SOFTWARE\Punktfunk\Seats
```

The key's existence is the signal; its values are ignored. It splits the virtual-display
connectors:

| Host | Without the key | With the key |
|---|---|---|
| Console host | 0–15 | 0–11 (0 serves clients without an identity) |
| Seat host | refuses every virtual-display session | exactly one of 12–15, from `PUNKTFUNK_SEAT_DISPLAY_SLOT` |

- Only the elevated seats installer or service writes the key. It lives under `HKLM` so a seat host,
  an ordinary process, can't reserve connectors from its own environment.
- When the driver places a monitor outside the host's range, the host removes it and refuses the
  session.

## The environment a seat host is started with

The supervisor starts an ordinary `punktfunk-host serve` with these. Don't set them by hand; set all
three together.

| Variable | Value | Rule |
|---|---|---|
| `PUNKTFUNK_SEAT_SESSION` | `1` | Marks a seat host. Unset or any other value is the console host. |
| `PUNKTFUNK_SEAT_ID` | 32 lowercase hexadecimal characters | Required with `PUNKTFUNK_SEAT_SESSION=1`. Missing or any other form: the host mints no audio devices. |
| `PUNKTFUNK_SEAT_DISPLAY_SLOT` | `12`–`15` | The seat's connector. Not a number, out of range, or the marker absent: the host refuses every virtual-display session. |

It also sets these ordinary [host settings](/docs/configuration), so seats don't collide. Each seat
is an independent host on the network, with its own pairing and name.

| Variable | Supervisor's value |
|---|---|
| `PUNKTFUNK_CONFIG_DIR` | a per-seat directory |
| `PUNKTFUNK_MGMT_BIND` | `127.0.0.1:<per-seat port>` |
| `PUNKTFUNK_NATIVE_PORT` | a per-seat port |
| `PUNKTFUNK_HOST_NAME` | the seat's name |
| `PUNKTFUNK_GAMESTREAM` | `0` |
| `PUNKTFUNK_NO_ISOLATE` | `1`: extend the desktop, never turn other displays off |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `follow_default` |

The supervisor must keep each seat's RDP session active. A seat host runs outside the console
session by design, and display activation fails while its session is inactive.

## What the host does differently on a seat

- **One connector, one lock.** A seat host creates its virtual display on its own connector only
  and holds the mutex `Global\punktfunk-vdisplay-manager-seat-<slot>`; the console host holds
  `Global\punktfunk-vdisplay-manager`. Hosts on different connectors start independently. A second
  host on the same connector can't open the display driver.
- **Driver calls are per process.** Every host sees, changes and clears only the monitors it
  created. A crashed host's monitors depart when its handles close or it misses the driver
  watchdog, so no host reaps a neighbour's.
- **Launches stay in its session.** Games, store launches and hook commands run as the signed-in
  user of the host's own Windows session, not the console's.
- **Its audio devices are its own.** It mints `Punktfunk Speakers [seat <id>]` and
  `Punktfunk Microphone [seat <id>]`, matches them by a marker derived from the seat id, and never
  adopts a neighbour's. It captures its own speakers and never changes the box's default playback
  or recording device.
- **Its virtual mouse is its own.** The resident HID mouse that makes Windows draw a cursor into
  the stream is `pf_mouse_<slot>`, with mailbox `Global\pfmouse-boot-<slot>`. The console host uses
  index `0`.
- **Gamepad indices are shared.** The box has 16 pad indices; each host takes the lowest one that
  no other host's pad holds.
- **No status tray.** A seat host doesn't start or supervise one; the supervisor is the control
  surface.

## Checking a seat can actually stream

Run this inside the seat's session:

```
punktfunk-host spike --source virtual --hdr --seconds 5
```

It creates a virtual display, captures it and encodes in the display driver. It exits non-zero on
any gap: no driver, no captured frame, no GPU encoder (a software encoder counts as a failure), no
10-bit path, or no encoded output. The supervisor runs it before it reports a seat healthy. Drop
`--hdr` to skip the 10-bit requirement.

## The seat display driver is a second package

Windows' terminal-services stack starts a seat's display with whichever driver claims the hardware
id `RdpIdd_IndirectDisplay`. Claiming it **takes over every RDP session on the machine**, ordinary
Remote Desktop included, so the build produces two packages from the same `.inx` and signing key:

| Package | Claims | Installed by |
|---|---|---|
| `pf_vdisplay.{inf,cat}` | the console display device, `Root\pf_vdisplay` | every punktfunk install |
| `pf_vdisplay_seats.{inf,cat}` | `RdpIdd_IndirectDisplay` only | the seats add-on, on an explicit choice |

A machine without the seats package never claims the id.

**Losing the claim is silent.** Windows' own `rdpidd.inf` ranks the same as ours (`0x00FF0000`), so
the newer `DriverVer` date wins. A seat on Microsoft's adapter still logs in and never streams.
Check it with at least one seat connected, since seat devnodes exist only while a session does:

```
powershell -File check-seat-display.ps1
```

[`check-seat-display.ps1`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/check-seat-display.ps1)
reports the driver on each live seat display and exits 1 if any isn't ours or none is live. It
warns when our driver date isn't newer than `rdpidd.inf`'s, and on a client edition.

## Seat audio needs a driver on disk

A seat usually has no sound card, so the host mints a render endpoint per seat by binding Valve's
Remote Play streaming drivers. `SteamStreamingSpeakers.inf` and `SteamStreamingMicrophone.inf` must
be bound to a device already, or sit in
`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\<x64|arm64>\`. Steam never has to run. Without
them a seat streams video with no audio.

```
powershell -File check-seat-audio.ps1
```

[`check-seat-audio.ps1`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/check-seat-audio.ps1)
exits 1 when neither is present. A virtual cable is no substitute: the host never loopback-captures
a cable, and `PUNKTFUNK_MIC_DEVICE` only pins the microphone.
