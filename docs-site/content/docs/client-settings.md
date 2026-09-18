---
title: Client settings
description: Every setting a Punktfunk client stores — what it does, what it defaults to, and which of them the host can overrule.
---

The host has [its own settings reference](/docs/configuration). This page is the other half: the
settings each **client** keeps. Most are a *request* — the client asks, the host answers in the
handshake, and a setting the host can't honor is a quiet downgrade rather than an error.

## Where the settings live

The Linux, Windows, Mac, iPhone/iPad, Apple TV and Android apps group settings the same way —
**General**, **Display**, **Input**, **Audio**, **Controllers** — under *Preferences* on Linux and
*Settings* elsewhere. The Apple TV has no **Input**, gives the quick-action dial its own **Quick
Actions** category, and keeps its categories down the left of the **Settings** tab. Any settings
screen reached with a controller shows one steppable list instead — **Stream**, **Video**,
**Presentation**, **Audio**, **Controller**, **Touchscreen**, **Interface**, **Presets** — the
client's **console home** (not the host's [web console](/docs/web-console)). On a Steam Deck that
list *is* the settings surface: the [Decky plugin](/docs/steam-deck) is a launcher with no settings
of its own, and its **Open Punktfunk** button opens the console home from the Quick Access Menu.

Linux stores them in `~/.config/punktfunk/client-gtk-settings.json` (shared with the console home);
Windows in `%APPDATA%\punktfunk\client-windows-settings.json`; the Apple and Android apps use their
own stores.

Changes apply to the **next** session. (*Match window* is also read at connect, but once on, every
window resize renegotiates the mode.) Names below are the Linux app's; differences that matter are
noted per setting.

## Video

**Resolution** — *default: Native display.* The host builds a virtual display at exactly this size;
nothing is scaled. Native resolves at connect to the mode of the display your window is on. The
Apple app stores an explicit size (1920 × 1080 out of the box) with a **Use this display's mode**
button on iPhone/iPad/Mac; Apple TV picks a combined **Stream mode** preset ("This TV (native)",
720p, 1080p or 4K at 60 Hz). A host pinned to stream a *real* monitor declines the request and your
client scales — see [Virtual displays](/docs/virtual-displays#stream-a-real-monitor-instead).

**Match window** — *default: off.* The stream mode follows your window; each resize renegotiates
the host's display and encoder. Fullscreen degenerates to the display's native mode. Linux,
Windows, Mac, iPhone/iPad and the console home (inside the Resolution picker; a Gaming-Mode stream
is always fullscreen, so there it lands on native); not Android.

**Refresh rate** — *default: Native*, the refresh of the display your window is on. The Apple app
stores an explicit rate (60 Hz default): iPhone and iPad offer the device's displayable rates, on a
Mac you type one in, and Apple TV's rate rides with the Stream mode preset.

**Bitrate** — *default: Automatic.* The number is a **total wire budget**: video, error-correction
parity, packet framing and the audio plane's share all fit inside it, so "20 Mbps" means 20 Mbps on
your network. For H.264, HEVC and AV1, Automatic is the host's default **20 Mbps** plus two things
an explicit rate switches off: adaptive bitrate, and a link-capacity probe about two seconds in that
lets the rate climb past 20 Mbps. Automatic never descends below **2 Mbps**; an explicit rate is
fixed for the session, clamped to **500 kbps – 8 Gbps**. Every client takes a rate that isn't on
its list: type it, or slide to it on iPhone, iPad and Mac. A host card's menu has **Test network
speed…** to suggest a value.

PyroWave is **always Automatic**: a fixed per-pixel budget for the negotiated mode (hundreds of
Mbps). A fixed kbps is meaningless for the all-intra codec, so the bitrate setting is disabled
while PyroWave is selected — your stored value is kept.

**Render scale** — *default: Native (1×).* The host renders and encodes at your mode times this;
your device resamples to its window. Above 1× supersamples at more bandwidth and decode work; below
1× is lighter on both. Stops 0.5×–4×; the result is floored to an even size and capped per axis at
4096 px for H.264, 8192 px otherwise. Offered everywhere.

**Picture fit** — *default: Fit.* What happens when the stream's shape differs from your window or
screen — a 16:9 stream on a 20:9 phone, or a device that joined a display another device sized.
**Fit** shows the whole picture with black bars. **Crop to fill** fills the screen and cuts the
edges off. **Stretch to fill** fills the screen and distorts the picture. The mouse, touch and pen
follow the picture you see. A size within a few pixels of 1:1, or of an exact 2× or 3×, snaps to it:
a pixel or two of bar beats blurring the whole picture. Linux, Windows, Mac, iPhone, iPad, Apple
TV, Android and the console home. On Android before 10, Crop to fill shows the picture squeezed
instead: that path has no way to cut the edges off. When you join a display another device sized,
a Linux host with a hardware encoder crops and scales the picture for your screen before it
encodes, so your device decodes only what it shows. A Windows host sends the whole picture.

**Video codec** — *default: Automatic.* A soft preference: your choice when the host can produce
it, else the best codec you both speak, in the order HEVC → AV1 → H.264. **PyroWave** is never
auto-picked — pick it explicitly on Linux, Windows, the console home, or an Apple or Android device
whose decode probe passes; elsewhere asking for it lands on that same order. See
[PyroWave](/docs/pyrowave). Android and Apple hide AV1 without a hardware AV1 decoder, and hide
PyroWave without the GPU it needs (on Android, a 64-bit device with Vulkan 1.3 and the codec's
compute feature set).

**10-bit HDR** — *default: on.* Off means "never send me 10-bit". On, the stream goes 10-bit
BT.2020 PQ only when the host has HDR content *and* the encoder can do 10-bit. Android disables the
toggle, and never advertises HDR, on a panel that can't present HDR10. Full detail: [HDR](/docs/hdr).

**Full chroma (4:4:4)** — *default: off.* Crisp small text and thin lines, at more bandwidth. Needs
HEVC or PyroWave, the host's 4:4:4 policy on, a capture path that delivers full chroma, and a GPU
that can encode it; if any gate fails the host says 4:2:0 before your decoder is built. Apple
(hardware decode probe required), Linux, Windows and the console home; not Android.

**10-bit SDR** — *default: off.* Encodes at 10-bit precision without HDR: gradients that band under
an 8-bit encode — skies, fog, dark scenes — come through smooth, and the displays keep their colour
settings. It's the *encoder's* precision, not a 10-bit capture — the desktop stays 8-bit. Needs
an NVIDIA or AMD host, or Intel under Linux, on HEVC; AV1 works too except on AMD under Windows.
Anywhere else the session stays 8-bit and the handshake says so. When HDR engages it takes over and
the row dims. Unlike HDR it asks nothing of your display, so the TV apps offer it too; on webOS it
needs HEVC.

**Prioritize** — *default: Lowest latency.* **Lowest latency** shows every frame the moment the
display can take it — a network hiccup becomes an occasional repeated or skipped frame.
**Smoothness** holds a small buffer that evens hiccups out, at that buffer's worth of added delay.
Linux, Windows, the console home, Apple and Android — stored under the same name everywhere, so a
[preset](/docs/presets-and-links) means the same thing on every device.

**Smoothness buffer** — *default: Automatic (two frames).* Frames held back before showing. Each
absorbs roughly one screen refresh of jitter and costs one refresh of delay — on a 120 Hz screen,
two frames ≈ 17 ms both ways. Appears wherever **Prioritize** is offered, once **Smoothness** is
picked.

**V-Sync** — *default: on.* Tear-free presentation. Off shows each frame the instant it's ready:
the lowest delay a display can give, with visible tearing on fast motion. Best-effort — where the
driver or compositor has no tearing mode the stream stays tear-free, and the Detailed
[stats overlay](/docs/stats) names the mode actually in use. Linux, Windows, console home.

**Follow variable refresh rate** — *default: on.* On a VRR / FreeSync / G-Sync screen, the panel
refreshes in step with the stream. Applies to **fullscreen** sessions; harmless on a fixed-refresh
screen. Needs a driver with the modern queue-free display mode; on an older driver it does nothing
unless `PUNKTFUNK_VRR_FIFO=1` is set (see [configuration](/docs/configuration)). The stats overlay
reports `vrr yes` once it has *measured* the panel following. Linux, Windows, console home.

**Host compositor** — *default: Automatic.* Which backend a **Linux** host uses to drive the
virtual output. Advisory: a host without that backend auto-detects instead.

## Audio

**Audio channels** — *default: Stereo.* **5.1** or **7.1** on request; anything else reads as
stereo. The count the host will really send comes back in the handshake and your decoder is built
from *that*. A **Linux** host claims a sink with exactly that many channels (real surround); a
**Windows** host loopback-captures the current output endpoint and lets Windows convert — 5.1 from
a stereo endpoint is an upmix. Offered everywhere.

**Keep host audio playing** — *default: off.* Normally a session parks the host's playback on a
silent endpoint so sound comes out of the client only, and the host PC goes quiet. On, the session
asks the host to capture whatever its default playback device already is instead — the speakers or
headphones plugged into the host keep playing, and both ends hear the same audio (Moonlight's
"Mute host PC speakers" box, unchecked). Per preset, so a laptop-in-the-house preset can keep the
host's headphones live while the TV preset mutes them. Best-effort: it needs a host on 0.32 or
newer, and with several clients streaming at once, any one asking wins for all of them. The
host-wide equivalent is
[`PUNKTFUNK_AUDIO_OUTPUT_MODE=follow_default`](/docs/configuration). Offered everywhere.

**Microphone** — *default: off on Linux, Windows, Android and the console home; on in the Apple
app.* Sends this device's microphone to the host's virtual mic. Spelled *Stream microphone* on
Linux and Windows; **Ctrl+Alt+Shift+V** mutes it mid-stream — see
[Muting your microphone](/docs/input#muting-your-microphone).

**Echo cancellation** — *default: on.* Stops the host's audio, playing from this device's speakers,
from re-entering the microphone. It uses the system's own canceller: an echo-cancelled PipeWire
source on **Linux**, the WASAPI Communications stream category on **Windows**, the platform
voice-processing mode on **Apple** and **Android**. Turn it off if your microphone runs its own
processing or the canceller thins your voice. Sits under the microphone toggle, greyed out while
the mic is off. Linux, Windows, Apple, Android, console home. See
[Why do I hear myself](/docs/echo).

**Speaker** and **Microphone** device pickers — *default: System default.* Which endpoint plays the
stream, and which input feeds the uplink. Only the Linux app (PipeWire nodes) and the **Mac** app
(plus a microphone *channel* picker) have these; the Windows app ignores a stored speaker choice. A
vanished device keeps a "(not detected)" entry on Linux, "Unavailable device" on the Mac. A Steam
Deck in Gaming Mode has no endpoint picker: the session uses what the Desktop-Mode app last stored.

## Input

Touch modes, mouse modes and the in-stream chords have their own page: [Input](/docs/input). The
settings below are worth naming here.

**Forward controllers** — *default: on*, everywhere. Off, controllers connected to *this* device
are not sent to the host — what you want when the controller already reaches the host another way
([USB passthrough](/docs/automation#recipe-full-controller-passthrough-virtualhere) such as
VirtualHere, or a pad plugged into the host), where forwarding would hand games two controllers for
one pair of hands.

On Linux and Windows, opening a controller *claims* it (SDL takes the device node), and a
passthrough tool can't bind a claimed device; off, the session never opens the pad. Consequence:
the [controller escape chord](/docs/input#leaving-with-a-controller) is read off forwarded pads, so
on those two it is unavailable while this is off — leave with the keyboard chord or the client's
UI. The Apple and Android apps claim nothing through their normal controller paths, so their chords
keep working; Android and the Mac do stop their DualSense and Steam Controller 2 USB captures,
which claim the device. The rows below grey out while this is off.

**Steam Controller passthrough** (`sc2_capture`) — *default: on for Android, off for Apple*. Reads
an already-paired Steam Controller 2 directly and passes it to the host **as itself**: the host
presents a real `28DE:1302` that its own Steam drives, so the trackpads, gyro and haptics behave as
they do locally instead of being flattened into a generic pad. Android captures over USB, the Puck
dongle, or Bluetooth; a Mac over USB — a cable or the Puck dongle, up to four pads on one dongle —
or Bluetooth; an iPhone or iPad has no USB path, so Bluetooth only (Apple TV has neither). Needs
*Forward controllers* on, and a Linux or Windows host — elsewhere the pad falls back to its
ordinary type.

It defaults **off on Apple** because switching it on costs a permission question, which is worth
asking only from a controller the app can see you own: Input Monitoring on a Mac (the controller
shares its USB interface with the pad's built-in keyboard mode, so macOS treats opening it as
keyboard listening), Bluetooth on an iPhone or iPad. A Mac takes the cable only when a pad is
attached at the moment the stream starts, so a Mac without one falls back to the radio and is
asked for Bluetooth instead. Android needs no such prompt for a pad
already attached, so it defaults on and simply does nothing when no SC2 is present. The capture
engages at the next stream, and a badge confirms it.

**Gamepad type** (*Controller type* on Apple, Android and the console home) — *default: Automatic*,
which matches each physical controller. Pickers offer Xbox 360, Xbox One, DualSense and DualShock 4
everywhere, plus Steam Deck on Linux, Android and the console home. The host builds each virtual
pad from the declared type; a type the host has no backend for degrades to an Xbox 360 pad (Xbox
One on a Windows host, any Sony pad on a Linux host that can't open `/dev/uhid`).

That degrade matters for **motion**: an Xbox-class virtual pad has no gyroscope, so a session on
one throws every motion sample away. Automatic lands there for any pad not recognised as Sony or
Valve (an 8BitDo with a gyro, say), and for a Switch Pro streaming to a Windows host. **If you want
motion, pick a DualSense-class type** — DualSense, DualSense Edge, DualShock 4, Switch Pro or Steam
Deck all carry a motion plane. Clients say so on-screen when it happens; the setting applies from
the next session. On a **Steam Deck as the client**, motion also needs Steam Input off for
punktfunk — with it on, Steam hands the app its own virtual Xbox pad.

**Forwarded controller** (*Use controller* on Apple and the console home) — *default: Automatic*,
which forwards *every* connected controller, each as its own player. Pinning one restricts the
session to that controller alone. Linux, Windows, Apple, the desktop console home; not Android or
webOS.

**Steam / guide button** (*Guide button* on Apple and Android) — *default: Automatic*, everywhere.
Where guide (Xbox/PS/Steam) and quick-access presses go while streaming: **Send to host** forwards
them raw, **This device** keeps them local. Automatic forwards everywhere except Gaming Mode, where
SteamOS opens its own menus for those buttons regardless — forwarding raw there opens both menus at
once. Full story:
[the guide button](/docs/input#the-guide-button-xbox--ps--steam-and-quick-access).

**Hold Select for guide** — *default: Automatic*, everywhere. The gesture that presses the host's
guide button from any controller: hold Select (Back/View) alone ~⅓ s; keep holding for the host's
long-press. Automatic arms it only where the raw press can't reach the host cleanly — Gaming Mode,
iPhone/iPad, Apple TV — because the gesture costs: a Select *tap* arrives a beat late, and a game
expecting a *held* Select would trigger it. **On**/**Off** overrule.

**Controller haptics** — *default: on*, and **Controller speaker** — *default: on* on Linux and
Windows, *off* on Android. The two halves of [controller audio](/docs/controller-audio): a
DualSense's voice-coil haptics and the pad's speaker. Both need a **wired** DualSense or DualSense
Edge — Bluetooth exposes no audio device, and both settings quietly do nothing. The plane is
negotiated, so neither costs anything without a host that sends them. Linux, Windows and Android.
On Linux the client also switches the pad's sound card to Pro Audio while it needs the voice coils
and puts it back afterwards — see
[the controller-audio page](/docs/controller-audio#on-a-linux-client-the-pads-own-profile-matters-too).

**Capture system shortcuts** — *default: on.* Linux, Windows (spelled *Capture system shortcuts
(Alt+Tab, Win, …)*), macOS and the console home; on a Deck it matters only for an attached keyboard
(gamescope holds nothing back). On, Alt+Tab and the Windows/Super key reach the host while input is
captured; off, they act locally. Either way the chords return when you release capture with
**Ctrl+Alt+Shift+Q**, the window loses focus, or the stream ends. It applies in
[both mouse modes](/docs/input#mouse-modes) — in Desktop mode the unlocked pointer can always
click another window to hand them back. Leaving it on means **Ctrl+Alt+Shift+Q is your way out**
of a captured stream, since Alt+Tab no longer is.

On macOS the chords in question are the **⌘** ones — on, ⌘Q, ⌘W, ⌘H and the rest go to the host
while input is captured (⌘Q arrives as Super+Q); off, they act on the Mac, which means ⌘Q quits
Punktfunk mid-stream. **⌘⎋ always stays local** — it releases capture, as does ⌃⌥⇧Q, and ⌃⌘F keeps
working on the window. ⌘Tab, ⌘Space and the Mission Control keys never reach the host either way —
macOS claims them first.

On Linux this needs a compositor with keyboard-shortcuts-inhibit — KDE Plasma, GNOME and wlroots
compositors have it, X11 sessions grab the keyboard directly. Under [gamescope](/docs/gamescope)
there is nothing to inhibit.

**Invert scroll direction** — *default: off*, i.e. the host scrolls the way this machine does.

**Quick actions** — *Android, iPhone/iPad, macOS, Apple TV and the console home.* What the six
buttons of the in-stream dial hold, and the custom shortcut chords they can send. The editor is the dial
itself on every client — tap or click a button to change it, drag one onto another to swap; with a
controller, the stick walks the buttons, A changes one, Y lifts it and A drops it on another; on
Apple TV, Select changes a button and holding it moves or clears it. A shortcut is a name, the
modifiers, and a key picked on a keyboard. The console keeps the global dial only (it never edits
presets). Apple TV's default dial leaves out touch mode, the keyboard, the microphone and the
virtual controller, which a TV can't use. See [the quick-action dial](/docs/input#the-quick-action-dial).

**Virtual controller** — *Android and iPhone/iPad only*, under Quick actions. **Layout**
(*default: Full*) picks which controls the on-screen controller shows: Full, Sticks and shoulders,
or D-pad and face buttons. **Opacity** (*default: 45 %*) and **Scale** (*default: 100 %*) set how
strongly and how large it draws over the picture. **Edit layout** opens the controller itself over
a stand-in backdrop: drag a control to move it, tap one to size it (50–200 %) or hide it, and reset
one control or the whole layout. Wide and upright screens keep separate layouts, so a phone tuned
in landscape keeps its portrait preset untouched; the overrides ride the same preset-scoped
setting as the dial, so a Game preset can carry its own arrangement. The controller itself is
shown and hidden from the dial's Virtual controller button, per session. Not on Apple TV (no touch
screen), a Steam Deck (real sticks) or the desktop clients (a keyboard).

## Behavior

**Start in** — *default: Host list.* Where the app opens. **Host list** is the first screen,
**Library** lands on your default host's games, and **Stream** goes on to its desktop. Back leaves
either landing on the host list, so a wrong guess costs one press. With one paired host saved, that
host is the default and there is nothing to set; with several, use **Make default host** on a
host's card or ▲ menu, and until you do, every value opens the host list. Naming one explicitly
also matters later: pairing a second host drops a derived default, and keeps an explicit one.
Every client, plus `punktfunk default-host` for a box you only reach over ssh.

**Auto-wake on connect** — *default: on.* Connecting to a saved host that looks offline sends
Wake-on-LAN and waits — only for a host whose MAC this client has learned. Turn it off for hosts
reached over a VPN, where the wake only adds delay. Linux, Windows, Apple, Android and the console
home; on a Steam Deck it also governs the [Decky plugin's](/docs/steam-deck) launches. The console
home additionally offers wake as an explicit action on an offline host, whatever the toggle says.
See [Wake-on-LAN](/docs/wake-on-lan).

**Start streams in fullscreen** — *default: on.* On Linux and Windows, F11 or Alt+Enter leaves
fullscreen live. On a Mac the setting is **Fullscreen while streaming**, and the window returns
with the host list. The console home carries the row for the desktop client that shares the store —
a Gaming-Mode launch is fullscreen regardless. Not on iPhone, iPad, Apple TV or Android.

## Interface

How the client itself looks. None touches a stream, so none can live in a
[preset](/docs/presets-and-links).

**Gamepad-optimized browsing** — *default: on.* Swaps the touch/desktop home for the
controller-optimized one: host carousel, larger focus targets, a swipeable cover browser, steppable
settings. Apple and Android have the switch — on Android in both places, ordinary Settings and the
controller-optimized settings themselves, so that home can be left from inside it; on Linux, Windows
and the Steam Deck the controller-optimized home is a separate entry point. An Android TV is always
in this mode, so the switch is not offered there.

**Show it** — *default: With a controller.* Shown while the switch above is on. **With a
controller**: the controller-optimized home appears as a pad connects, the touch interface returns
when the last one disconnects. **Always** keeps it either way — for a phone or tablet docked to a
TV. Apple and Android (an Android TV is in that mode regardless, so the row is not offered there).

**Background** — *default: Violet.* The colour family of the controller-optimized home's backdrop.
Thirteen: seven dark — **Violet**, **OLED**, **Nebula**, **Abyss**, **Ember**, **Moss**,
**Graphite** — and six pale — **Holo**, **Sunset**, **Bloom**, **Dawn**, **Mint**, **Opal** — which
flip the interface to dark text on a light field. The backdrop recolours as you step the row.
**OLED** is true black: most of the frame is pixels switched off — no glow, no power on an
OLED/AMOLED panel. Stored under the same name on every client. The row lives in the
controller-optimized settings (**X**, or **down** on the host carousel, from the controller-optimized
home) everywhere that has one, including the Steam Deck and the Linux/Windows console home — down is
the route where there are no face buttons to press, such as an Android TV remote, and the hint bar
names whichever your device has; the Apple TV carries it in ordinary Settings next to **Show it**
instead, so it's reachable from the Siri Remote.

**Reduce interface resolution** — *default: off.* Android only, in the controller-optimized
settings. Draws the menus at 1080p and lets the display scale them up, instead of drawing at the
panel's own resolution. Text goes a little softer; the interface gets much smoother. It is for 4K
televisions and projectors, whose graphics chips are built to decode and composite video rather
than to draw a moving interface, and are far slower than the ones in phones — at 4K every part of
the interface costs four times what it does at 1080p, on hardware nowhere near four times faster.
A premium 4K box is *more* likely to want this than a cheap 1080p stick, which never had the extra
pixels in the first place. Nothing about a stream changes: picture quality is
[**Resolution** and **Bitrate**](#video), and this is the interface only.

## Overlay

**Statistics overlay** — *default: Normal.* Four tiers — Off, Compact, Normal, Detailed — each a
superset of the last. This picks the tier a session *starts* at; cycle live in-stream with a
per-platform shortcut. The Apple app also picks the corner (Top/Bottom × Left/Right). The console
home has the tier picker under **Interface**. Shortcuts and every number:
[Understanding the stats overlay](/docs/stats).

**Advanced statistics** — *default: off.* Off shows the figures Moonlight's overlay also shows, as
averages. On shows Punktfunk's own view: capture to screen as a median and a slow-frame figure, and
every stage between. It belongs to the device, so a settings preset never changes it.

## Settings that are facts about your device

These describe the machine you're sitting at, stay global, and **cannot be put in a settings
preset**:

- **Video decoder** and **GPU** — the decode path and adapter this device uses. Automatic is
  vendor-ordered and falls back on its own; change only when debugging; `PUNKTFUNK_DECODER`
  overrides it ([Configuration](/docs/configuration#client-side-native-clients)). Decoder picker:
  Linux, Windows, console home. GPU picker: Windows, and Linux with more than one adapter. Apple
  and Android have neither.
- **Speaker** and **Microphone** device pickers — this device's audio endpoints.
- **Forwarded controller** — which physical pad is in your hands. (The *type* the host creates is a
  preference and can live in a preset, as can **Forward controllers**.)
- **Auto-wake on connect**.
- Everything under **Interface**.

One switch you might expect here isn't in Settings at all: **Share clipboard** lives in a saved
host's own edit sheet, because handing a machine your clipboard is a decision about that one host —
see [Shared clipboard](/docs/clipboard).

Everything else on this page can be overridden per preset and bound to a host; the rows above are
exactly [what a preset can't change](/docs/presets-and-links#what-a-preset-cant-change).

## When the client and the host disagree

| You ask for | What the host does |
|---|---|
| Resolution and refresh | Builds a display at exactly that mode. A host pinned to a real monitor keeps that monitor's resolution and you scale locally. A size the encoder can't take — odd, or past the codec's per-axis limit — fails the connect rather than being quietly changed. |
| A bitrate | Clamps it to 500 kbps – 8 Gbps, or uses its 20 Mbps default for Automatic. PyroWave ignores the number entirely — every PyroWave session gets the per-pixel budget. |
| A codec | Honors it when it can encode it, else the best shared codec in the order HEVC → AV1 → H.264. |
| 10-bit HDR | Upgrades only for HDR content on an encoder that can do 10-bit; otherwise 8-bit SDR. |
| 4:4:4 chroma | Sends it only when every gate passes; otherwise 4:2:0. |
| A channel count | Normalizes it to 2, 6 or 8. |
| A gamepad type | Uses it as the session default; an unsupported type becomes an Xbox 360 pad. |
| A compositor | Treats it as advisory and auto-detects when it isn't available. |

Every one of those answers arrives before your decoder and speakers are set up, so what you see and
hear is built from what the host really sent — never from what you asked for.
