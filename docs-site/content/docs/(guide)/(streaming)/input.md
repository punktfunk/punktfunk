---
title: Mouse, touch and pen
description: The shortcuts that give your mouse and keyboard back, the controller chords, mouse and touch modes, the quick-action dial and stylus input.
---

Get your mouse and keyboard back from a stream, then drive the host with a mouse, a touchscreen,
a controller or a pen. The settings named here are in your client's **Input** settings; see
[Client settings](/docs/client-settings#input).

## Getting your input back

A stream captures your mouse and keyboard when it starts and whenever you click the video: your
cursor disappears and keys go to the host.

| Client | Release input | Leave the stream |
|---|---|---|
| Linux, Windows, Steam Deck | **Ctrl+Alt+Shift+Q** | **Ctrl+Alt+Shift+D** |
| macOS | **⌃⌥⇧Q** or **⌘⎋** | **⌃⌥⇧D** |
| iPad with a keyboard | **⌃⌥⇧Q** or **⌘⎋** | **⌃⌥⇧D** |
| Android | **Ctrl+Alt+Shift+Q** (pointer capture) | **End stream** on the [dial](#the-quick-action-dial) |
| Apple TV | — | Hold the remote's **Back** about a second, then let go |
| Any client, with a controller | Press **L1 + R1 + Start + Select** (Linux, Windows) | Hold it ([details](#leaving-with-a-controller)) |

Press the release shortcut again, or click the stream, to capture again. Switching to another app
releases input and takes it back when you return. Keys and buttons you were holding are let go on
the host, so nothing sticks. On Linux and Windows, controllers stop reaching the host too until
you capture again.

### Keyboard shortcuts

| Action | Linux · Windows | macOS | iPad keyboard | Android |
|---|---|---|---|---|
| Release or capture input | Ctrl+Alt+Shift+Q | ⌃⌥⇧Q, ⌘⎋ | ⌃⌥⇧Q, ⌘⎋ | Ctrl+Alt+Shift+Q |
| Switch the [mouse mode](#mouse-modes) | Ctrl+Alt+Shift+M | ⌃⌥⇧M | — | — |
| Disconnect | Ctrl+Alt+Shift+D | ⌃⌥⇧D | ⌃⌥⇧D | — |
| Cycle the [stats overlay](/docs/stats) | Ctrl+Alt+Shift+S | ⌃⌥⇧S | ⌃⌥⇧S | — |
| [Mute your microphone](#muting-your-microphone) | Ctrl+Alt+Shift+V | ⌃⌥⇧A | ⌃⌥⇧A | — |
| Open the [quick-action dial](#the-quick-action-dial) | Ctrl+Alt+Shift+O | ⌃⌥⇧O | — | Ctrl+Alt+Shift+O |
| Start or stop [clipboard sharing](/docs/clipboard) | — | ⌃⌥⇧C | ⌃⌥⇧C | — |
| Fullscreen | F11 or Alt+Enter | ⌃⌘F | — | — |

On an iPad only ⌃⌥⇧Q and ⌘⎋ always work while input is captured. If another shortcut doesn't
respond, release input first.

To see the list without a stream: **Keyboard Shortcuts** in the Linux client's main menu,
**Shortcuts** on the Windows client's host list, or **About → Shortcuts** in the Apple apps'
Settings. The macOS **Stream** menu lists them too.

**macOS:** while input is captured, every other ⌘ chord goes to the host, ⌘Q included. Turn
**Capture system shortcuts** off to keep them local. ⌘⎋ and ⌃⌘F always stay with the Mac.

**Android:** Android keeps **Alt+Tab**, the **Windows** key and the **Language** key for itself.
To send them, turn on **Punktfunk keyboard shortcuts** under Android's Accessibility settings;
the app offers this at launch when a keyboard is attached, and under **Settings → Input →
Keyboard shortcuts**. On Android 13 and newer a build from outside the Play Store first needs
**Allow restricted settings** on the app's info page. Without the service, **Alt+`** stands in
for Alt+Tab. **Ctrl+Space** reaches the host from Android 13 on.

### Muting your microphone

The mute lasts for the current stream and stops only the sending, so unmuting is instant. While
muted, a **Microphone muted** badge shows over the stream, with the stats overlay on or off.

- Keyboard: the shortcut in the [table above](#keyboard-shortcuts).
- Controller on Android: **Select + Y**, or a DualSense's own **Mute** button.
- Any client with a microphone: the dial's **Microphone** button.

With the microphone off in [client settings](/docs/client-settings#audio), there is nothing to
mute and the shortcut does nothing.

### Controller chords

Hold **Select** (Back, View) first, then press the second button.

| Chord | What it does | Clients |
|---|---|---|
| **Select + A** | Opens the [quick-action dial](#the-quick-action-dial). The host never sees the A | All |
| **Select + X** | Cycles the [stats overlay](/docs/stats). Both presses still reach the game | All |
| **Select + Y** | Mutes or unmutes your microphone | Android |
| **Hold Select** alone | Presses the host's guide button ([below](#the-guide-button-xbox--ps--steam-and-quick-access)) | Where it is on |
| **L1 + R1 + Start + Select** | Leaves the stream ([below](#leaving-with-a-controller)) | All |

On an Apple TV remote, **hold Play/Pause** about half a second to cycle the stats overlay; a tap
is a right click.

### Leaving with a controller

Hold **L1 + R1 + Start + Select** (LB + RB + Start + Back on an Xbox pad) on any connected pad.

| Client | Quick press | Hold |
|---|---|---|
| Linux, Windows | Releases input and leaves fullscreen, unless the stream started fullscreen | About 1.5 s disconnects |
| Steam Deck | Releases input | About 1.5 s disconnects |
| macOS, iPhone, iPad, Apple TV | Nothing | About 1.5 s disconnects |
| Android | Shows **Hold to quit…** | About 1 s disconnects |

On Linux and Windows the chord needs [**Forward controllers**](/docs/client-settings#input) on.
With it off, use **Ctrl+Alt+Shift+D** or the client's own controls.

### The guide button (Xbox / PS / Steam) and Quick Access

The guide button (the Xbox logo, the PS button, the Deck's **Steam** button) opens menus on the
host. Where your device keeps that button for itself, **hold Select on its own** for about a
third of a second instead: the host sees its guide button held for as long as you hold, and a
long hold opens a SteamOS host's **Quick Access Menu**. A quick tap of Select still reaches the
game, a beat late.

Two settings control this: **Steam / guide button** (**Guide button** on Apple and Android) and
**Hold Select for guide**. **Automatic** does this:

| Client | Guide button | Hold Select |
|---|---|---|
| Linux, Windows, macOS, Android | Sent to the host | Off |
| Steam Deck and other Gaming Mode sessions | Stays with the Deck | On |
| iPhone, iPad | iOS keeps it for its Game Overlay | On |
| Apple TV | tvOS keeps it | On |

If Steam or the Xbox Game Bar on your own device also watches the guide button, both react; turn
that off on your device. On iOS 27 and newer you can hand the Home button to the app in the
controller's system settings. The dial's **Guide button** and **Quick access menu** buttons send
one tap each; **Quick access menu** needs a host whose virtual pad is a Steam controller. On a
Deck, the Punktfunk panel's **Host menus** buttons do the same ([Steam Deck](/docs/steam-deck)).

## Mouse modes

**Mouse input** picks one of two modes:

| Mode | Pointer | Use it for |
|---|---|---|
| **Capture (games)** | Locked to the stream. Only relative motion is sent | Mouse-look in games. Default on Linux, Windows and macOS |
| **Desktop (absolute)** | Moves freely in and out of the stream; its position is sent | Remote desktop work. Default on Android |

In both modes the only cursor over the stream is the host's. With **Capture system shortcuts**
on, Alt+Tab and the Windows/Super key go to the host while input is captured; on Linux and
Windows that holds in Desktop mode too, until you click another local window.

Switch live with **Ctrl+Alt+Shift+M** (**⌃⌥⇧M**). On Android, **Ctrl+Alt+Shift+Q** toggles
capture instead. On an iPad, **Capture pointer for games** locks a mouse for mouse-look; it needs
the stream fullscreen and in front.

- **gamescope hosts only take relative motion**, so Desktop mode stays captured there (see
  [gamescope](/docs/gamescope)).
- **Hosts that send their cursor separately:** the Linux and Windows clients switch to relative
  motion while a host app grabs or hides the pointer, and back when it lets go. The mouse-mode
  shortcut overrides this until the host app changes its mind.

## Scrolling

Wheels, touchpads, touchscreens and controllers all scroll the host, with no setting to tune.
On GNOME, Sway and Hyprland hosts a touchpad or touchscreen flick hands off to the desktop's own
glide; on KDE, gamescope and Windows hosts the glide your device produced is sent instead.

**Invert scroll direction** reverses every scroll you send, the controller mouse's included. To
flip it for one stream, open the dial's centre sheet and choose **Input → Invert scroll
direction**; your saved settings stay as they are.

## Touch modes

On a touchscreen (Android, iPhone, iPad, Linux, Windows), **Touch input** picks one of three
modes:

- **Trackpad** (default) — your finger moves the host cursor like a laptop touchpad. Lift and
  swipe again to cross a large screen.
- **Direct pointer** — the cursor jumps to your finger and follows it.
- **Touch passthrough** — every finger is sent as a real touch, with no gestures. Only for apps
  that understand touch.

Trackpad and Direct pointer share these gestures:

| Gesture | Does |
|---|---|
| Tap | Left click |
| Two-finger tap | Right click |
| Two-finger drag | Scroll |
| Tap, then press and drag; or press and hold, then drag | Held left drag |
| Three-finger tap | Cycle the [stats overlay](/docs/stats) |
| Three-finger swipe up / down (Android, iPhone, iPad) | Show or hide the on-screen keyboard |
| Two-finger twist | Open the [quick-action dial](#the-quick-action-dial) |

Touch passthrough depends on the host:

| Host | Touch passthrough |
|---|---|
| KDE Plasma, GNOME | Full multi-touch |
| Windows 10 1809 and newer | Full multi-touch |
| Sway, Hyprland and other wlroots compositors | Not supported: the client uses Trackpad and says so |
| gamescope | One finger, as an absolute pointer ([gamescope](/docs/gamescope)) |

## The quick-action dial

The dial is six buttons plus a centre that opens a sheet with every action and the resolution
presets. Pick its six buttons under **Quick actions** in settings, or edit the dial itself.

| Client | Opens the dial |
|---|---|
| Linux, Windows | **Ctrl+Alt+Shift+O**, a two-finger twist on a touchscreen |
| macOS | **⌃⌥⇧O**, **Stream → Quick Actions** |
| Android | **Back**, a two-finger twist, **Ctrl+Alt+Shift+O** |
| iPhone, iPad | A two-finger twist, the corner button; in **Touch passthrough**, two fingers pulled in from a side edge |
| Apple TV | A short press of the remote's **Back** |
| Any client, with a controller | **Select + A** |

A twist commits at about 30°; lift earlier and nothing is sent. On Android, turn off **Back opens
quick actions** to make Back do nothing mid-stream; a mouse's Back button always goes to the
host.

While the dial is up, input belongs to it and the host sees nothing:

- **Controller:** the left stick aims, the D-pad steps, **A** fires, **Y** returns to the centre,
  **B** closes.
- **Keyboard:** **Tab** steps through the buttons, the arrows aim, **Enter** fires, **Esc**
  closes.
- **Mouse:** the desktop clients hand the pointer back so you can click a button.

What the dial can hold:

| Group | Actions |
|---|---|
| Session | **End stream**, **Disconnect, keep the game running** |
| Input | **Touch mode**, **Keyboard**, **Virtual controller**, **Send text**, **Guide button**, **Quick access menu**, **Controller mouse** |
| View · Audio | **Statistics**, **Microphone**, **Mute this stream** (this device only) |
| Host | **Sleep host**, **Restart host**, **Shut down host** ([Host power](/docs/host-power)) |
| Shortcuts | Key combinations you add, such as Alt+F4 |

A button a client can't serve is dimmed and says why. **Send text** works on Android against a
host that takes typed text.

### Controller mouse

For a launcher, dialog or desktop that ignores controllers, fire **Controller mouse** on the dial:
the controller that opened the dial now drives the host's pointer, and the game sees an idle pad.
Fire it again to hand the controller back. Each stream starts with controllers in the game.

| Controller | Controller mouse |
|---|---|
| Left stick | Pointer — push further to move faster |
| Right stick | Scroll |
| A, right trigger | Left click; hold to drag |
| X, left trigger | Right click |
| Y | Middle click |
| B | Escape |
| D-pad | Arrow keys |
| Start | Enter |
| LB, RB | Ctrl, Alt (held) |
| Left stick click, right stick click | Shift (held), Space |
| Guide | Super / Windows key |

Select stays with the dial, so **Select + A** still opens it. The button is dimmed with no
controller connected, or when the host only lets this device send controller input.

### Virtual controller

On Android, iPhone and iPad, the dial's **Virtual controller** button draws a controller over the
stream. The host sees it as one more pad. Fingers on its controls drive the game; fingers
elsewhere still drive the touch mode. Under **Quick actions** in settings, pick a **Layout**
(**Full**, **Sticks and shoulders**, **D-pad and face buttons**), set **Opacity** and **Scale**,
or use **Edit layout** to move, resize and hide controls, separately for wide and upright screens.

## Pen and stylus

A stylus sends position, pressure, tilt, barrel roll, hover, the eraser and two barrel buttons,
so drawing apps on the host see a real pen.

| Client | Pen input |
|---|---|
| iPhone, iPad | Apple Pencil with hover. Double-tap is barrel button 2; squeeze is barrel button 1 and barrel roll works on iOS 17.5+ |
| Android | Active stylus: pressure, tilt, hover, eraser, both buttons. No barrel roll |
| [Moonlight](/docs/moonlight) | Clients that send pen events |
| Linux, Windows, macOS, Apple TV | None |

| Host | Appears as |
|---|---|
| Linux | A **Punktfunk Pen** tablet for the session, mapped by your compositor. Needs `/dev/uinput`, the `input` group step of your [install guide](/docs/install) |
| Windows 10 1809 and newer | A system pen with pressure, tilt, rotation, barrel button and eraser |

Without host support, the stylus works as a finger, without pressure or tilt. To turn pen off
for every client, switch off **Pen input** in the web console under **Host → Settings → Input**,
or set `PUNKTFUNK_PEN=0` in [`host.env`](/docs/configuration). It applies from the next session.
