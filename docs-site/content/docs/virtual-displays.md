---
title: Virtual displays
description: Control how Punktfunk creates, keeps alive, and arranges the virtual displays it streams — presets, keep-alive, exclusive vs. extend, and persistent per-client scaling.
---

When a client connects, Punktfunk creates a **virtual display** at that client's resolution and
refresh, renders your desktop or game onto it, and streams it. This page is the **policy** for that
display: how long it survives a disconnect, whether it takes over your physical monitors, what a
second client gets, and how per-client settings like scaling persist.

Set it in the **web console** (the **Virtual displays** page), or edit
`~/.config/punktfunk/display-settings.json` (`%ProgramData%\punktfunk\display-settings.json` on
Windows). A change applies to the **next** connection.

> **You rarely need to touch this** — reach for a preset when you want a specific experience.
> Monitors that stayed dark, or a streamed screen showing only wallpaper? Go to
> [Troubleshooting](#troubleshooting).

To stream a monitor the host **already has** instead, see
[Stream a real monitor instead](#stream-a-real-monitor-instead) — it turns most of this page off.

## Stream a real monitor instead

> **Linux only.** A Windows host enumerates its monitors but can't capture one — the Streamed
> screen card is read-only there.

Set **Virtual displays → Streamed screen** to a listed monitor and Punktfunk streams that physical
monitor instead of creating a virtual display; every client sees it at *its* resolution.

- The monitor is **never touched** — not resized, moved, disabled or restored. Keep-alive, topology
  and multi-monitor layout don't apply.
- **The resolution is the monitor's.** A client asking for a different one is told no and scales
  its own picture; mid-stream resize is off.
- **Every client sees the same screen.**
- Naming a monitor this host **doesn't have, while it has others**, is a **hard error**: the session
  fails with `no monitor named "DP-9" — this host has: HDMI-1`. On a session with **no physical
  heads** (nested or headless compositor) the pin is set aside with a log warning instead.
- **Virtual screen (default)** in the same card puts you back on the normal path.

Supported on **KDE/KWin**, **GNOME/Mutter**, **Sway/wlroots**, **Hyprland** and **gamescope Game
Mode** — through the compositor's own screen-recording API, so there is **no chooser dialog** (a
background [service](/docs/running-as-a-service) has nobody to answer one). On gamescope only the
head the session is driving is listed; a *nested* or headless gamescope has none, so the picker is
empty there.

### Naming the monitor from the host

Monitors are named by **connector** — `HDMI-A-1`, `DP-2`, `eDP-1`. List this host's (Linux-only
subcommands, like the setting):

```sh
punktfunk-host list-monitors
```

```
Kwin:
  HDMI-A-1        1920x1080@60 at +0,+0    scale 1  Dell U2412M  [primary]
  DP-2            2560x1440@144 at +1920,+0  scale 1  ACME 27
```

To pin it **from the host's configuration** instead — the appliance route — set it in
[`host.env`](/docs/configuration):

```sh
PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1
```

The environment variable **wins over the console setting**; the console shows the card as locked
while it's set.

Check the whole path — mirror, capture, frames — without a client:

```sh
punktfunk-host mirror-test --monitor HDMI-A-1 --seconds 20
```

Screen recording is **damage-driven**: an idle desktop produces almost no frames, so move the mouse
on the host while it runs or a working mirror reads as a stall.

### Absolute input follows the pin

Pinning a monitor also re-aims **absolute** mouse and pen input to that head's origin, so a click
lands where you point on *that* screen. Heads are matched by position, not size. The host resolves
the pin at startup and whenever the console writes it — no restart; the log line is
`capture monitor: …`.

`punktfunk-host anchor-test --monitor HDMI-A-1` checks it with no client: it lists the heads, walks
the pointer through the centre and corners, and prints the region it mapped into (`--none` runs the
walk unanchored, as an A/B). The anchor rides the **libei** injector — on KWin, Sway and Hyprland
the host injects through a different protocol, and `anchor-test` says so rather than reporting a
green run that proves nothing.

## Pick a preset

Select one in the console and you're done. Each expands to a bundle of the options documented
further down.

| Preset | What it's for |
|---|---|
| **Default** | Most setups. Reconnects resume quickly, the streamed output becomes the whole desktop, extra viewers each get their own screen. |
| **Headless box** | A monitorless machine you only stream from. Game and display survive disconnects indefinitely (keep-alive **forever**); whoever connects next takes the box over. Release it from the console when you're done. |
| **Shared desktop** | A PC you also use in person. Never blanks your real monitors, never leaves a leftover display behind; extra viewers each get their own screen. |
| **Hot-desk** | One person at a time — roam between your own devices with an instant reconnect. Anyone else is told the box is busy; each device+resolution keeps its own scaling. |
| **Workstation** | Your multi-monitor daily driver. Displays come back exactly where you arranged them, each client keeps its own settings, the desktop is yours alone. |

## Save your own preset

- **Save as preset** — names the settings currently in force (all the options below **plus**
  *Dedicated game sessions*) and adds it to the picker alongside the built-ins.
- **Apply** — writes exactly those settings, like picking a built-in.
- **Edit / delete** — rename, update to your current settings, or remove. Deleting never changes
  what's running.

The built-in presets leave *Dedicated game sessions* alone, so switching presets never changes your
game-launch routing; a **custom preset captures your full setup**, including that axis. Custom
presets live on the host in `display-presets.json` (next to `display-settings.json`); editing a
preset never disturbs a running session.

## Options reference

Choose **Custom** in the console to set these directly.

### Keep alive

How long the virtual display survives after your last session disconnects. On a gamescope game host
this also keeps the **game itself running**.

- **Off** — tear the display down at session end.
- **A duration** (seconds) — a reconnect inside the window drops you straight back in, with no
  re-negotiation and no desktop reshuffle.
- **Forever** — keep it until you stop the host or **release it** from the console (**Virtual
  displays** → *Release*). The headless-box model.

Default: **10 seconds**.

**Hyprland lists the kept head; Sway does not.** On Hyprland the named output stays in the
registry for the linger window, so the console and `ctl display` show live and lingered heads.
Sway's capture arrives over a portal handle the host can't re-open per attach, so those displays
never enter the list and cannot linger, whatever a preset's lifetime says.

**A reconnect always resumes the kept display**; **deliberately quitting** (closing the client, not
a network drop) tears it down at once, skipping the linger. How quickly a *dropped* client is
noticed is the QUIC idle timeout — 8 s, tunable with `PUNKTFUNK_IDLE_TIMEOUT_MS` (see
[Legacy environment knobs](#legacy-environment-knobs)).

> **Keep-alive + Exclusive keeps your physical monitors dark after you disconnect**, until the
> linger expires or you release the display. Intentional for a dedicated gaming box — on a machine
> whose monitors you also use in person, use **Shared desktop**.

### Topology

What Punktfunk does with your monitor layout while it streams.

- **Extend** — add the virtual display alongside your real monitors; touch nothing else.
- **Primary** — make the virtual display your primary output; physical monitors stay on.
- **Exclusive** — the virtual display becomes your **only** enabled output (physical monitors are
  disabled, then restored when streaming ends), so panels and windows land on it.
- **Automatic** *(default)* — Exclusive on Windows and on an auto-detected KDE/GNOME desktop;
  Extend when you've pinned a specific compositor with `PUNKTFUNK_COMPOSITOR`.

Per-backend support:

| | KWin | Mutter/GNOME | Sway/wlroots · Hyprland | Windows |
|---|---|---|---|---|
| Extend | ✅ | ✅ | ✅ | ✅ |
| Primary | ✅ | ✅ | ⚠️ treated as Extend | ✅ |
| Exclusive | ✅ | ✅ | ✅ | ✅ |

**Sway/wlroots and Hyprland have no primary-output concept**; they have a *focused* output, and the
host points that at the streamed display — at session start, and again immediately before it
launches anything from your library, which is what puts the game on the streamed display. Primary
therefore behaves as Extend (the host says so in the log), and a window that opens *later* follows
whatever has focus then — clicking a physical monitor mid-launch can still pull a window over.

**Exclusive** on both compositor families:

- Punktfunk only disables monitors it did not create, so a second concurrent client never goes dark.
- On Hyprland the restore is a `hyprctl reload` — nothing else re-enables a monitor a rule disabled.
  The reload runs at real teardown (linger expiry or release), not at disconnect, so exclusive
  physicals stay dark for the keep-alive window. It re-reads your Hyprland config; settings changed
  at runtime with `hyprctl keyword` are dropped and a non-Lua config re-runs its `exec =` lines
  (`exec-once` is not). Only happens if a session actually disabled something.

### Conflict handling · identity · layout

- **Conflict handling** — a *different* client connects mid-stream asking for a different
  resolution: give it its own display (**separate**), take the box over (**steal**), share the
  existing display (**join**), or refuse it (**reject**). Under **join** both clients hear the
  same audio: the second one reads the sink the first one captures, and keeps it if the first
  one leaves. The console's **Session** card mutes either one, and a custom entry's **Who hears
  this title** picks per game. A same-client *reconnect* never conflicts — it resumes.
  Moonlight clients are the exception: that plane holds one session on fixed ports, so it
  cannot share a display. **Join** there keeps the client that is streaming and turns the
  second one away; **steal** and **separate** both hand the box over.
- **Identity** — whether each client gets a **stable display identity** so your desktop environment
  remembers its settings (see [Persistent scaling](#persistent-scaling)): one shared identity, one
  **per client**, or one **per client + resolution**.
- **Layout / max displays** — several clients as monitors of one desktop: side by side (**auto**) or
  exactly where you arrange them in the console (**manual**, keyed to each client), up to **max
  displays**.

### Dedicated game sessions

How a session that *launches a game from [your library](/docs/game-library)* is served (Linux
hosts):

- **Auto** (default) — the launch rides whatever session the box is in: the managed Steam session
  on a couch box, a bare gamescope, or your live desktop.
- **Dedicated** — every library launch gets its **own headless gamescope at your exact resolution
  and refresh**, with just the game inside. Steam titles launch with the client hidden
  (`steam -silent`); non-Steam titles start almost instantly. Combined with **keep alive**, the
  game keeps running when you disconnect.

Dedicated needs `gamescope` installed; without it a launch falls back to **Auto**. On a box already
in Steam game mode, a dedicated Steam launch frees game mode's Steam first and restores it when the
session ends. GameStream / Moonlight launches follow the same routing.

## Advanced Windows options

Three extra levers on the **Virtual displays** page, shown only on a Windows host that can act on
them. All three take effect with **Exclusive** topology, where your physical monitors are turned
off for the duration of the stream, and all three are undone when the stream ends — or, if the
host crashes mid-stream, the next time it starts.

### Power monitors off (DDC/CI)

Before removing your physical monitors from the desktop, the host also tells each one to power its
panel off over the DDC/CI monitor-control channel, and wakes them again afterwards.

Some setups see a periodic stutter when the streamed display is the only active one: the dark
monitor keeps probing its inputs, and a panel that was told to sleep does not. Monitors without
DDC/CI support are skipped. If a monitor does not wake up afterwards, press its power button once
and turn this off.

### Disable monitor devices (PnP)

On top of removing the monitors from the desktop, the host disables their Windows device entries
for the duration of the stream. This one is **on by default**; turn it off if a monitor ever fails
to come back after a stream.

A standby monitor or TV that keeps waking its connection — auto input scan, instant-on — can then
no longer interrupt the stream, because Windows ignores its wake events entirely while the device
is disabled. If the host crashes mid-stream, the monitors are re-enabled the next time it starts;
until then you can re-enable them by hand in Device Manager.

### Hold monitor identity (EDID)

**AMD graphics cards only.** While streaming, the host tells the AMD driver to keep treating each
connected monitor as present with its current identity (EDID) even while the monitor sleeps — the
software equivalent of a dummy plug.

This stops the driver from periodically probing a sleeping monitor's connection, which on some
setups causes a rhythmic stutter every couple of seconds while the virtual display is the only
active one. The option appears only where the AMD driver's control library (`atiadlxx.dll`) is
actually installed. If a monitor misbehaves afterwards, turn this off and unplug/replug it — or,
in the worst case, reinstall the graphics driver.

## When a game ends, and when a session does

Two switches, on the **Virtual displays** page under **When a game or a session ends**, tie a
session to the game the host launched for it. They apply to every store and both protocols — and
only to a game **this host launched for the session**: a game you started yourself is never touched.

### When the game exits

**End the session** (default) — quit the game and your client goes back to its own library, on
every path (live desktop, attached gamescope, Moonlight). **Keep streaming** if you stream the
desktop and treat the game as incidental.

### When the session ends

Whether stopping — or losing — a session also closes the game.

- **Leave it running** (default). Nothing is ever closed.
- **Close it on Stop** — closing the client, or *Stop* in the console, closes the game. A network
  drop does not.
- **Always close it** — a drop closes it too, after a **reconnect window** (5 minutes by default).
  Reconnect inside the window and nothing happens; the console shows the countdown, with an **End
  now** button.

Closing a game costs whatever it hadn't saved, which is why nothing closes by default. The host
asks first — a polite close, the same as clicking the window's X — and only forces the issue after
ten seconds of being ignored.

> **Keep alive and this setting are different clocks.** Keep-alive decides how long the *display*
> outlives a disconnect (10 s default); the reconnect window decides how long the *game* does
> (5 min). A display set to **Forever** stays up regardless of what happens to the game.

### On a gamescope session, the display has the final say

When a launch gets its **own gamescope** — a dedicated game session, the usual setup on a Steam
Deck or Bazzite couch box — the game runs *inside* the streamed display and lives exactly as long
as it does, so **Keep alive decides, not the setting above**:

| you disconnect by | what happens to the game |
|---|---|
| pressing **Stop** (or the console's stop) | the display tears down at once — keep-alive is skipped for a real stop — and the game goes with it, even on *Leave it running* |
| dropping out (network, sleep) | the display lingers for your keep-alive window, then tears down; the game ends with it |
| dropping out, keep-alive **Forever** | the display is pinned, so the game genuinely survives — and *Always close it* still ends it when the reconnect window closes |

On a gamescope box, "leave the game running after I disconnect" means **keep-alive Forever** (or a
window long enough to come back in). On a desktop session — KWin, GNOME, Sway — the game is an
ordinary process and the setting above is the whole story.

### Automation

The host publishes `game.running` and `game.exited` events (the latter says whether the player quit
it or the host closed it), so a hook or plugin can react without polling. See
[Automation](/docs/automation).

## Persistent scaling

Set your display **scaling** once and have it stick across reconnects. Each client gets a *stable
display identity*, so your desktop environment keys its per-monitor settings to it.

| Host | Supported | How |
|---|---|---|
| **Windows** | ✅ today | Set scaling in Settings while streaming — Windows remembers it per client. |
| **KDE / KWin** | ✅ today | Set scaling in System Settings while streaming; KWin keys it to a stable per-client output name and reapplies it on reconnect. |
| **GNOME / Mutter** | ✅ today | GNOME's virtual-monitor API exposes no stable identity, so the **host persists the scale itself**: set scaling in Settings while streaming — the host captures it per client and reapplies on reconnect. |
| **Sway / wlroots** | ❌ | Headless outputs can't carry a stable identity; pin scale in your sway config instead. |

## Legacy environment knobs

These `PUNKTFUNK_*` variables still work, but the console (and `display-settings.json`) supersede
them — when a settings file exists, it wins.

| Legacy knob | Now expressed as |
|---|---|
| `PUNKTFUNK_MONITOR_LINGER_MS` | **Keep alive** → duration *(Windows)* |
| `PUNKTFUNK_NO_ISOLATE` | **Topology** → Extend *(Windows)* |
| `PUNKTFUNK_KWIN_VIRTUAL_PRIMARY` / `PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY` | **Topology** → Exclusive (when set) / Extend (when `0`) |

One knob has no console equivalent — transport tuning, not display policy:

- **`PUNKTFUNK_IDLE_TIMEOUT_MS`** (host, default `8000`) — how long before a *dropped* client is
  declared gone, which is when a kept display starts its linger (or is freed). Lower it (e.g.
  `3000`) to reclaim kept displays sooner; it's clamped to ≥1 s and its keep-alive ping scales with
  it, so a live session never false-disconnects. A deliberate quit is instant regardless. Also
  `--idle-timeout-ms` on `punktfunk1-host`.

## Troubleshooting

**My physical monitors stayed off after I disconnected.** Keep-alive is set together with Exclusive
topology — the display is kept for the linger window. Release it from the console (**Virtual
displays**), or switch to the **Shared desktop** preset.

**The virtual output shows only my wallpaper.** Your topology is Extend, so the streamed display is
an empty extension. Use **Primary** or **Exclusive** so your desktop lands on it.

**KWin can't create the virtual output.** On a normal Plasma session KWin runs its **DRM backend**,
which creates virtual outputs at any version. The 6.5.6 floor applies only to the **virtual
backend** (`kwin_wayland --virtual`, headless and test sessions) — below that the request fails
with "Could not find output". On **KWin 6.6+** that same message also covers an output KWin *did*
create and then left disabled; [KDE Plasma](/docs/kde#troubleshooting) walks that one. See
[requirements](/docs/requirements).

**Reconnecting into game mode reconnects cleanly now.** On a Steam Deck / Bazzite box,
disconnect/reconnect within game mode reuses the still-warm session (or cleanly recreates it), and
switching between game mode and the desktop mid-stream follows the switch. If a launched game
**exits**, a dedicated session ends and returns you to your library; a game mode / desktop session
keeps streaming.

**My keep-alive / topology / layout settings do nothing.** Check whether **Streamed screen** is set
to a real monitor — those options are about a display Punktfunk created. Switch the card back to
*Virtual screen (default)*.

**The console won't let me change Streamed screen.** `PUNKTFUNK_CAPTURE_MONITOR` is set in this
host's [`host.env`](/docs/configuration) and outranks the console. Unset it (and restart the host).

**My session fails with "no monitor named …".** The pinned connector isn't among this host's
monitors — renamed, unplugged, or the host is in a different session. Run
`punktfunk-host list-monitors` to see the real names. Punktfunk will not quietly stream a different
screen.

**My couch box's TV stayed on the streamed session after I disconnected.** With the **Headless
box** preset (keep alive = *forever*), a managed Steam session is held indefinitely — return to
game mode on the box (or restart the host) to hand the TV back.
