---
title: Mouse, touch and pen
description: The in-stream keyboard shortcuts that give your mouse back, the two mouse modes, the three touch modes, and full-fidelity stylus input.
---

A stream takes your mouse and keyboard the moment you click into it. This page starts with how to
get them back, then covers driving the host with a mouse, a touchscreen and a pen. The rows that
pick these modes sit in your client's **Input** settings; the toggles that share that page are in
[Client settings](/docs/client-settings#input).

## Getting your input back

On the Linux and Windows clients the stream runs in its own session window. Input is **captured**
when the stream starts and whenever you click the video: your local cursor disappears and keys go to
the host. In the default mouse mode the pointer is also locked to the window — see
[Mouse modes](#mouse-modes).

| Shortcut | What it does |
|---|---|
| **Ctrl+Alt+Shift+Q** | Release captured input (press again, or click the stream, to take it back) |
| **Ctrl+Alt+Shift+M** | Switch the mouse mode (capture ⇄ desktop) |
| **Ctrl+Alt+Shift+D** | Disconnect |
| **Ctrl+Alt+Shift+S** | Cycle the [stats overlay](/docs/stats) — off · compact · normal · detailed |
| **Ctrl+Alt+Shift+V** | Mute or unmute your microphone |
| **Ctrl+Alt+Shift+O** | Open the [quick-action dial](#the-quick-action-dial) |
| **F11** or **Alt+Enter** | Toggle fullscreen |

While input is released the session window prints the shortest list over the stream:

```text
Click the stream to capture input · Ctrl+Alt+Shift+Q releases · Ctrl+Alt+Shift+M mouse mode ·
Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats
```

With a controller in use the hint names the controller chord instead of the mouse-mode and stats
entries. The full list is always available without a stream running — see below.

### Muting your microphone

**Ctrl+Alt+Shift+V** stops sending your microphone to the host; pressing it again resumes. The
uplink keeps running underneath, so unmuting is instant.

While muted, a **Microphone muted** badge sits in the top-right corner of the stream — separate
from the [stats overlay](/docs/stats), so it shows even with stats off.

The mute lasts for that stream only — the next session starts unmuted; nothing is written to your
settings. With **Stream microphone** off in [client settings](/docs/client-settings#audio) the
shortcut does nothing and no badge appears.

The **keyboard** chord is **Linux and Windows** only (a Steam Deck stream is the Linux client, so an
attached keyboard gets it). On **Android** a controller can reach the same toggle: **Select + Y**,
and on a DualSense the pad's own **Mute** button does it too — one toggle per press, and the badge
is the same. On **Apple** clients there is no shortcut; turn **Stream microphone** off in settings
instead.

Alt-Tabbing away releases input on its own and takes it back when you return. A release you asked
for with the chord stays released until you opt back in. Either way, keys and buttons you were
holding are released on the host, so nothing sticks down.

Without a stream running, the Linux client lists the shortcuts under **Keyboard Shortcuts** in its
main menu, and the Windows client on a **Shortcuts** screen reached from its host list. Both list the
microphone mute; the in-stream hint over the video doesn't, to stay one readable line.

### On the other clients

- **macOS** honours the release, mouse-mode, disconnect and stats combos, written
  **⌃⌥⇧Q / M / D / S**, plus **⌃⌥⇧A** for the microphone mute and **⌃⌥⇧O** for the
  [quick-action dial](#the-quick-action-dial). **⌘⎋** also toggles capture, **⌃⌘F** toggles
  fullscreen, and **⌃⌥⇧C** starts or stops [clipboard sharing](/docs/clipboard). The **Stream** menu
  lists them all except the mouse-mode combo, which works but has no menu item. Every *other* ⌘
  chord goes to the host while input is captured — ⌘Q reaches the host's compositor rather than
  quitting the app — unless you turn **Capture system shortcuts** off in
  [client settings](/docs/client-settings#input). ⌘⎋ and ⌃⌘F are held back either way, so there is
  always a way out.
- **iPhone and iPad** with a hardware keyboard: **⌃⌥⇧Q** releases input while it is captured, and
  **⌘⎋** toggles capture in either direction. **⌃⌥⇧D** (disconnect) and **⌃⌥⇧S** (stats) come from
  the app's Stream shortcuts rather than from the stream itself; if they don't respond while you're
  captured, release first or use the on-screen controls.
- **Android and Android TV** honour **Ctrl+Alt+Shift+Q** (pointer capture) and **Ctrl+Alt+Shift+O**
  (the [quick-action dial](#the-quick-action-dial)). Android keeps **Alt+Tab**, every **Win** chord
  and a keyboard's **Language** key for itself before any app sees them. Turn on **Punktfunk
  keyboard shortcuts** under Android's Accessibility settings and they reach the host too — it
  sends keys only while a stream is on screen. The app offers this once at launch when a hardware
  keyboard is attached, and **Settings → Input → Keyboard shortcuts** offers it any time. On Android 13 and newer a build installed outside the Play Store first needs **Allow
  restricted settings** from the app's info page. Without the service, **Alt+`** stands in for
  Alt+Tab (with Shift to walk backwards) and Win chords go on the dial as shortcuts. Every other
  key reaches the host: the Korean **한/영** and **한자** keys, the JIS **変換**, **無変換**,
  **カタカナ/ひらがな**, **半角/全角**, **ろ** and **¥** keys, and the ABNT2 **/?** and keypad
  **.** keys included. **Ctrl+Space** reaches the host from Android 13 on; Android 12 and older
  use it to switch their own layout. The system Back button opens the dial; turn off **Settings →
  Input → Back opens quick actions** and Back does nothing while the twist, a keyboard or a pad can
  open it instead. A mouse's Back button
  goes to the host — also on builds like One UI 8 that map it to Back themselves, where Android
  hands the app a Back key and the dial would otherwise open. With a mouse attached, that Back
  is the mouse's; the Back gesture still opens the dial.
- **Apple TV** has no keyboard path, and a short press of the Siri Remote's Back button deliberately
  does nothing — so a controller's B button can't end your session by accident. To leave, **hold
  Back for about a second and let go**. During a session the remote's touch surface drives the host
  cursor, a press is a left click, and Play/Pause is a right click — **hold Play/Pause** instead and
  it cycles the [stats overlay](/docs/stats). With a controller in hand, **Select + X** does the
  same on every Apple client.

### Leaving with a controller

Every client reserves one controller chord: **L1 + R1 + Start + Select** (LB + RB + Start + Back on
an Xbox pad), held on any connected pad.

- **Linux, Windows** — a press releases captured input, and leaves fullscreen if you didn't start
  fullscreen. Hold about 1.5 seconds and it disconnects.
- **Steam Deck** — a press releases captured input only. The Decky plugin always launches the client
  fullscreen, and a stream that started fullscreen stays that way. Holding disconnects, as above.
- **macOS, iPhone/iPad, Apple TV** — holding about 1.5 seconds disconnects. There is no quick-press
  step.
- **Android** — holding about a second disconnects. A quick press does nothing; the moment the chord
  completes a **Hold to quit…** cue appears so you know it registered.

The chord is read off the pads a client forwards, so turning
[**Forward controllers**](/docs/client-settings#input) off takes it away on **Linux and Windows** —
there the client stops opening the controller at all. Use
**Ctrl+Alt+Shift+D** or the client's own UI to leave instead. The Apple and Android apps keep
watching for the chord either way.

### Statistics with a controller

Every client reserves a second chord: **Select + X**, which cycles the
[stats overlay](/docs/stats) one level each time you complete it — for a pad with no keyboard and
no free screen for the three-finger tap; on **Apple TV** it is the only way there with a pad. Both
buttons still reach the game; only the overlay changes locally.

On the **Siri Remote**, **hold Play/Pause** for about half a second instead. A quick tap is still a
right click, sent when you let go.

### The guide button (Xbox / PS / Steam) and Quick Access

A controller's **guide button** — the Xbox logo, the PS button, the Deck's **Steam** button — is
meant to open menus **on the host**. Some devices want that button for themselves, so every client
also carries a gesture that works everywhere: **hold Select (Back / View) on its own for about a
third of a second**. The host sees its guide button held for as long as you hold — a long press,
which is how SteamOS opens the **Quick Access Menu** for a regular pad. A quick tap of Select still
reaches the game, delivered when you let go (a beat late); Select in a combo — including the leave
chord above — passes through untouched.

What the raw button does, per client:

- **Linux & Windows desktop, macOS, Android** — the guide press is forwarded to the host. If Steam
  Big Picture or the Xbox Game Bar is also watching for it *on the device in your hands*, both may
  react — that's a local setting on that device, not something the stream can suppress.
- **Steam Deck / Gaming Mode** — the **Steam** and **`…`** buttons stay with the Deck by default
  (SteamOS always opens its own menus for them; forwarding the raw press too opens both menus at
  once). Reach the host's menus with **hold-Select**, or the Punktfunk panel's **Host menus**
  buttons ([Steam Deck page](/docs/steam-deck)); **Steam / guide button → Send to host** restores
  the old behavior.
- **iPhone / iPad** — iOS reserves the Home press for its own Game Overlay, so hold-Select is the
  reliable route to the host's overlay. On iOS 27 or later you can also hand the button to the app
  yourself, in the system's per-controller Home-button setting.
- **Apple TV** — tvOS never delivers the Home press to apps; hold-Select is the only route.

The [quick-action dial](#the-quick-action-dial) carries both buttons too — **Guide button** and
**Quick access menu** — for a controller whose own guide the device keeps, or none at all. Each
sends one tap on the host's pad; **Quick access menu** only reaches a host whose virtual controller
is Steam-shaped.

Both halves are [settings](/docs/client-settings#input), per preset like everything else:
**Steam / guide button** (Automatic / Send to host / This device) and **Hold Select for guide**
(Automatic / On / Off). Automatic picks the behavior above for each platform — the gesture stays off
where the raw button already works, so games that use a *held* Select keep it.

## Mouse modes

There are two, and they are a per-client setting called **Mouse input**:

- **Capture (games)** — the pointer locks to the stream and only relative movement is sent. The only
  cursor you see is the host's. This is what mouse-look in a game needs. The session window also
  grabs the keyboard, so Alt+Tab and the Windows key (Super on Linux) reach the host rather than
  your own desktop — on macOS that is the ⌘ chords, ⌘Q included, with ⌘⎋ kept back as the way out.
  Turn **Capture system shortcuts** off in [client settings](/docs/client-settings#input) to keep
  them local.
- **Desktop (absolute)** — the pointer is not locked. It moves in and out of the stream freely and
  its position is sent as an absolute point — what you want for remote desktop work. Your local
  cursor is hidden over the stream; the one you see there is the host's. On Linux and Windows,
  Alt+Tab and the Windows/Super key go to the host here too while **Capture system shortcuts** is
  on — the host's Start menu is part of the desktop you're driving — and clicking any other local
  window takes them back. (On a Mac the ⌘ chords stay local in this mode.)

**Capture is the default** on the Linux, Windows and macOS clients. **Android defaults to Desktop**.

Switch live with **Ctrl+Alt+Shift+M** (**⌃⌥⇧M** on macOS), whether input is captured or not. On
Android, Ctrl+Alt+Shift+Q flips the capture instead. The picker is macOS-only among the Apple apps;
on iPad the equivalent is the **Capture pointer for games** toggle (on by default), which needs the
stream fullscreen and frontmost.

Two things can override your choice. **gamescope hosts can't take absolute pointer input**: ask for
desktop mode against one and the session quietly stays captured, and the chord has nothing to offer
(see [gamescope](/docs/gamescope)). And against a host that forwards its cursor separately instead of
drawing it into the video, the Linux and Windows clients flip to relative motion by themselves when
an app on the host grabs or hides the pointer, then back when it lets go. Using the chord yourself
overrides that until the host's intent next changes. The macOS client ignores the signal on purpose.

## Touch modes

On a touchscreen client the **Touch input** setting picks one of three models. All three exist on
Android, iPhone/iPad, Linux and Windows.

- **Trackpad** (the default) — your finger drives the host cursor like a laptop touchpad. The cursor
  stays put when you touch down and moves by your finger's travel, so you can lift and re-swipe to
  walk it across a screen far larger than your own.
- **Direct pointer** — the cursor jumps to your finger and follows it.
- **Touch passthrough** — every finger is forwarded as a real touch contact, with no gesture
  interpretation at all. Only useful for apps and games that genuinely understand touch.

Trackpad and Direct pointer share one gesture vocabulary: tap = left click, two-finger tap = right
click, two-finger drag = scroll, tap-then-press-and-drag = a held left drag, **three-finger tap =
cycle the stats overlay**. On Android and iPhone/iPad a **three-finger swipe up or down** summons or
dismisses the local on-screen keyboard for typing on the host; the Linux and Windows clients have no
such keyboard, and there any two-or-more-finger drag scrolls.

Touch passthrough depends on the host being able to inject touch, and that varies:

| Host | Touch passthrough |
|---|---|
| KDE Plasma (KWin), GNOME | Full multi-touch |
| Windows 10 1809 and newer | Full multi-touch |
| Sway, Hyprland and other wlroots compositors | Not injected — contacts are dropped |
| gamescope Gaming Mode | Degraded to a single absolute pointer — see [gamescope](/docs/gamescope) |

Wherever the compositor offers no touchscreen device to drive, only the first finger is used, as
an absolute pointer — tapping still clicks; pinches and multi-finger gestures don't survive. The
trackpad and pointer models are unaffected: they send ordinary mouse events.

A host says whether it injects touch at all. Against one that does not (the wlroots row above,
or Windows before 1809), a client set to Touch passthrough runs the trackpad model for that
session and says so in a short notice when the stream starts, instead of forwarding contacts the
host would drop.

## The quick-action dial

On Android, iPhone and iPad a **two-finger twist** on the stream opens a dial of six buttons under
your fingers: about 10° starts it opening, 30° commits it, and lifting short of that winds it back
in and sends nothing. In the **Touch** model on iPhone and iPad the twist is gone — every finger
belongs to the host there — so the dial opens on a **two-finger pull from either side edge**
instead: both fingers land on the bezel and come inward together. It takes nothing from the game
until the pull finishes, and a pull that stops short was simply two touches. The centre button opens a sheet with the whole catalogue and the resolution
presets. On Android the **Back** gesture opens the same dial at the screen centre instead of ending
the session; on iPhone and iPad the corner disc does; on Apple TV a short press of the remote's
Back; on Android, macOS, Linux and Windows **Ctrl+Alt+Shift+O** (**⌃⌥⇧O** on a Mac, also the
**Stream** menu's Quick Actions item); with a controller, **Select+A** (Select first) on every
client, and the host never sees the two presses. A mouse's Back button goes to the host, not to
the dial. What the six buttons hold is the **Quick actions** setting, and the
editor is the dial itself on every client, Apple TV included (**Settings → Quick Actions**).

While the dial is up the controller belongs to it and the host sees nothing. The **left stick
aims**: the dial highlights whatever slot your thumb points at, and letting go returns to the
centre — the D-pad is what steps disc by disc. **A** fires the highlight, the centre one opens the
sheet, **Y** returns to the centre and **B** closes.

On a keyboard, **Tab** steps through the six buttons and then the centre, the **arrows** aim (up is
the top button, down the bottom, and again from either lands on the centre), **Enter** fires and
**Esc** closes. The dial opens with the centre lit, so a first Enter opens the sheet.

Every desktop hands your pointer back for as long as the dial is up, so you can click a button, and
takes capture again when it closes. Buttons a platform cannot serve are dimmed and say why: **Touch
mode**, **Virtual controller** and **Keyboard** on a Mac, which has no touch screen and no software
keyboard. **Guide button** and **Quick access menu** are dimmed wherever controller input is not
forwarded — they ride the same wire pad.

### Controller mouse

A launcher, a dialog or a crashed game on the host desktop ignores controllers. Add **Controller
mouse** to the dial and fire it: the controller that opened the dial now moves the host's pointer.
The host's virtual controller stays connected and idle, so the game sees nothing held. Fire it again
to hand the controller back to the game. A dial opened by touch or keyboard switches every
connected controller. Every session starts with controllers in the game.

| Controller | Controller mouse |
|---|---|
| Left stick | Pointer — the further you push, the faster it moves |
| Right stick | Scroll, both directions |
| A, right trigger | Left click; hold to drag |
| X, left trigger | Right click |
| Y | Middle click |
| B | Escape |
| D-pad | Arrow keys |
| Start | Enter |
| LB, RB | Ctrl, Alt — held while you hold them |
| Left stick click, right stick click | Shift (held), Space |
| Guide | Super / Windows key |

Select stays with the dial, so **Select+A** still opens it. Pointer speed follows the stream's
resolution, so it feels the same at 1080p and 4K. For a combination the table lacks, such as
Alt+F4, add a shortcut to the dial. The button is dimmed when no controller is connected, or when
the host lets this device send controller input only.

#### Chords

A **chord** is a set of controller buttons that sends a keyboard shortcut. Hold **RB** and press
**B** and the host gets Alt+F4; nothing sends B's own Escape, because while every button of a chord
is down none of them acts on its own. Each chord picks when it fires:

| Fires | |
|---|---|
| On press | The moment the last button of the chord goes down |
| On a tap | On release, if you held it for less than the long-press time |
| On a hold | Once, when you reach the long-press time |
| Held | The keys go down at the long-press time and stay down until you let the chord go |

The same buttons can carry a tap chord and a hold chord at once — that is how **RB+B** closes a
window on a tap and force-quits it on a hold. **Held** is the one for desktop work: put Super on a
bumper, hold it and push the left stick to drag a window where you want it. A button borrowed by a
chord you did not hold long enough still sends what it normally sends, so a bumper can be Ctrl on a
tap and Super on a hold.

#### Customising the layout

Every button above is a starting point, not a fixed wiring. The whole table — which button sends
what, the chords, pointer and scroll speed, the stick deadzone and the long-press time — is one
document your client hands to the host:

```json
{
  "settings": { "pointer": 1.0, "scroll": 1.0, "deadzone": 0.2, "long_press_ms": 400 },
  "buttons": { "A": "mouse:left", "RT": "key:Meta", "B": "key:Escape" },
  "chords": [
    { "name": "Close window", "buttons": ["RB", "B"], "press": "short", "keys": ["Alt", "F4"] }
  ]
}
```

Buttons are named `A B X Y LB RB LT RT LS RS Guide Start Back Up Down Left Right`. An output is
`mouse:left`, `mouse:middle`, `mouse:right`, or `key:` and a key name — the same names the dial's
shortcut editor takes, so `key:Escape`, `key:F4` and `key:Meta` all work. `press` is `any`, `short`,
`long` or `hold`, matching the table above. `pointer` and `scroll` multiply the shipped speeds, so
`2.0` is twice as fast. Two buttons may share one output: `A` and `RT` both send the left click, and
holding either keeps it down.

A change takes effect the next time you switch that controller into Controller mouse — a drag in
progress is never re-wired under your thumb — and clearing the layout puts the shipped table back.

### Virtual controller

Android and iPhone/iPad can draw a controller over the stream, for a game that needs one when no
controller is attached. Show or hide it from the dial's **Virtual controller** button. The host sees
one controller arrive when it appears and one leave when it goes, exactly as for a real pad, on the
next free pad index beside any real controller you have connected. A finger on one of its controls
drives the game; a finger anywhere else still drives the touch mode, so tap-to-click keeps working
beside it. A stick follows your thumb from wherever it lands, the D-pad reads eight directions, and a
trigger reads how far down its pill your finger sits, so a slow press is a slow press. **L3** and
**R3** are discs of their own, beside the triggers: a stick and its click are separate controls
here, so a thumb can hold a direction while another finger clicks. **Layout**, **Opacity** and
**Scale** live under Quick actions in the [client settings](/docs/client-settings#input):
Full (two sticks, D-pad, face buttons, bumpers, triggers and the stick clicks), Sticks and
shoulders, or D-pad and face buttons. **Edit layout** there rearranges the preset by hand — drag
any control where your thumbs actually sit, grow or shrink it, or hide the ones a game never
needs — with separate arrangements for wide and upright screens. Not on Apple TV, a Steam Deck
or the desktop clients.

## Pen and stylus

A stylus is not treated as a finger. Punktfunk carries **position, tip pressure, tilt angle and tilt
direction, barrel roll, hover distance, the eraser end, and two barrel buttons** on their own input
plane.

**Clients that send pen input:**

- **iPhone and iPad** with an Apple Pencil, including hover. The Pencil has no hardware eraser or
  barrel buttons, so a **double-tap** is sent as barrel button 2, and on iOS 17.5 or newer a
  **squeeze** is sent as barrel button 1. Pencil Pro's barrel roll also needs iOS 17.5.
- **Android** phones and tablets with an active stylus — pressure, tilt, hover, the eraser tool and
  both barrel buttons. Android exposes no barrel-roll axis, so roll is not sent from there.
- **[Moonlight](/docs/moonlight) clients** that send pen events reach the same host-side pen.

The Linux, Windows, macOS and Apple TV clients do not send stylus input.

**What the host presents it as:**

- On **Linux**, a virtual tablet named **Punktfunk Pen** appears the first time you use the stylus
  and is removed when the session ends. Applications see a real pen through the usual tablet path,
  so Krita, GIMP and Xournal++ treat it as a graphics tablet. It is a screen tablet, mapped by your
  compositor's own default tablet mapping — correct on a single output; multi-monitor pinning is up
  to the compositor.
- On **Windows**, a per-session synthetic pen pointer feeds Windows' normal pen system: pressure,
  tilt, rotation, the barrel button and the eraser. This needs **Windows 10 1809 or newer**.

**Before it can work on Linux**, the host needs access to `/dev/uinput` — the same `input` group step
the virtual gamepads need, step 3 of your [install guide](/docs/install). Without it the host never
offers pen at all.

**If the host is too old, or pen is switched off**, the client folds the stylus into its ordinary
touch or pointer path — you can still draw, without pressure and tilt. Whether pen splits out is
decided by the host, not your touch mode.

**Operators** can turn the whole feature off by setting `PUNKTFUNK_PEN=0` in the host's `host.env`
(see [Configuration](/docs/configuration)). The host then stops advertising pen to Punktfunk and
Moonlight clients alike, and every client falls back to touch.
