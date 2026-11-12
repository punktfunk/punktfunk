---
title: Shortcuts and quick actions
description: Get your mouse and keyboard back, leave with a controller, open the quick-action dial, and change what each press does.
---

Get your mouse and keyboard back, leave a stream with a controller, reach every action from the
quick-action dial, and choose what each press does. Every key, chord and hold on this page is a
default: [change it](#changing-a-shortcut) under **Settings → Input → Shortcuts**. Mouse modes,
touch and pen are on [Mouse, touch and pen](/docs/input).

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

A stream opens with a one-line hint naming the way to leave, for a few seconds. **Exit hint**, an
advanced setting, turns it off.

Press the release shortcut again, or click the stream, to capture again. Switching to another app
releases input and takes it back when you return. Keys and buttons you were holding are let go on
the host, so nothing sticks. On Linux and Windows, controllers stop reaching the host too until
you capture again.

### Keyboard shortcuts

| Action | Linux · Windows | macOS | iPad keyboard | Android |
|---|---|---|---|---|
| Release or capture input | Ctrl+Alt+Shift+Q | ⌃⌥⇧Q, ⌘⎋ | ⌃⌥⇧Q, ⌘⎋ | Ctrl+Alt+Shift+Q |
| Switch the [mouse mode](/docs/input#mouse-modes) | Ctrl+Alt+Shift+M | ⌃⌥⇧M | — | — |
| Disconnect | Ctrl+Alt+Shift+D | ⌃⌥⇧D | ⌃⌥⇧D | — |
| Cycle the [stats overlay](/docs/stats) | Ctrl+Alt+Shift+S | ⌃⌥⇧S | ⌃⌥⇧S | — |
| [Mute your microphone](#muting-your-microphone) | Ctrl+Alt+Shift+V | ⌃⌥⇧A | ⌃⌥⇧A | — |
| Open the [quick-action dial](#the-quick-action-dial) | Ctrl+Alt+Shift+O | ⌃⌥⇧O | ⌃⌥⇧O | Ctrl+Alt+Shift+O |
| Start or stop [clipboard sharing](/docs/clipboard) | — | ⌃⌥⇧C | ⌃⌥⇧C | — |
| Fullscreen | F11 or Alt+Enter | ⌃⌘F | — | — |

On an iPad, ⌘⎋ and every ⌃⌥⇧ shortcut work while input is captured.

**macOS:** while input is captured, every other ⌘ chord goes to the host, ⌘Q included. Turn
**Capture system shortcuts** off to keep them local. ⌘⎋ always stays with the Mac; press it, then
⌃⌘F, to leave fullscreen. ⌘Space, ⌘Tab, Mission Control and other apps' global shortcuts go to
the host too, with no permission to grant.

**Android:** Android keeps **Alt+Tab**, the **Windows** key and the **Language** key for itself.
To send them, turn on **Punktfunk keyboard shortcuts** under Android's Accessibility settings;
the app offers this at launch when a keyboard is attached, and under **Settings → Input →
Keyboard shortcuts**. On Android 13 and newer a build from outside the Play Store first needs
**Allow restricted settings** on the app's info page. Without the service, **Alt+`** stands in
for Alt+Tab. **Ctrl+Space** reaches the host from Android 13 on.

### Muting your microphone

The mute lasts for the current stream and stops only the sending, so unmuting is instant. Each
switch shows **Microphone muted** or **Microphone live** over the stream for a few seconds, with
the stats overlay on or off.

- Keyboard: the shortcut in the [table above](#keyboard-shortcuts).
- Controller on Android: **Select + Y**, or a DualSense's own **Mute** button.
- Any client with a microphone: the dial's **Microphone** button.

With the microphone off in [client settings](/docs/client-settings#audio), the mute does nothing.

### Controller chords

Hold **Select** (Back, View) first, then press the second button.

| Chord | What it does | Clients |
|---|---|---|
| **Select + A** | Opens the [quick-action dial](#the-quick-action-dial). The host never sees the A | All |
| **Select + X** | Cycles the [stats overlay](/docs/stats) | Linux, Windows, Android, Apple |
| **Select + Y** | Mutes or unmutes your microphone | Android |
| **Hold Select** alone | Presses the host's guide button ([below](#the-guide-button-xbox--ps--steam-and-quick-access)) | iPhone, iPad, Apple TV, Gaming Mode |
| **L1 + R1 + Start + Select** | Leaves the stream ([below](#leaving-with-a-controller)) | All |

### Leaving with a controller

Hold **L1 + R1 + Start + Select** (LB + RB + Start + Back on an Xbox pad) on any connected pad.

| Client | Quick press | Hold for a second |
|---|---|---|
| Linux, Windows | Releases input and leaves fullscreen, unless the stream started fullscreen. Press again to capture and go back | Disconnects |
| Steam Deck in Gaming Mode, macOS, iPhone, iPad, Apple TV | Nothing | Disconnects |
| Android | Shows **Hold to quit…** | Disconnects |

On Linux and Windows the chord needs [**Forward controllers**](/docs/client-settings#input) on.
With it off, use **Ctrl+Alt+Shift+D** or the client's own controls.

### The guide button (Xbox / PS / Steam) and Quick Access

The guide button (the Xbox logo, the PS button, the Deck's **Steam** button) opens menus on the
host. Where your device keeps that button for itself, **hold Select on its own** instead: let go
after about a third of a second and the host sees a guide press. Hold on past a second and the
host sees its guide button held until you let go, so a long hold opens a SteamOS host's **Quick
Access Menu**. A quick tap of Select still reaches the game, a beat late. Pressed with another
button, Select never presses guide.

Hold Select is the **Guide button** row under **Settings → Input → Shortcuts**; remove it to send
Select at once. The advanced setting **Guide button** decides where the real button goes. By
default:

| Client | Guide button (**Automatic**) | Hold Select row |
|---|---|---|
| Linux, Windows, macOS, Android | Sent to the host | Off |
| Steam Deck and other Gaming Mode sessions | Stays with the Deck | On |
| iPhone, iPad | iOS keeps it for its Game Overlay | On |
| Apple TV | tvOS keeps it | On |

If Steam or the Xbox Game Bar on your own device also watches the guide button, both react; turn
that off on your device. On macOS and iOS 27 and newer, add Punktfunk to **Home Button Overrides**
in the system's game controller settings; while the system keeps the button, Punktfunk's
**Controllers** settings link there. The dial's **Guide button** and **Quick access menu** buttons
press the host's pad for you. **Quick access menu** needs Steam on the host: it presses the pad's
own button where the virtual pad has one, and Guide with A on every other pad. On a Deck, the
Punktfunk panel's **Host menus** buttons do the same ([Steam Deck](/docs/steam-deck)).

### TV remotes

| Remote | Press | Does |
|---|---|---|
| Apple TV | Tap **Play/Pause** · hold it about half a second | Right click · cycles the stats overlay |
| Android TV | Hold **Select** about a second | Turns pointer mode on or off |
| Android TV, pointer mode | D-pad · **Select** · **Play/Pause** · hold **Play/Pause** · **Back** | Moves the pointer · left click · right click · on-screen keyboard · leaves pointer mode |
| Android TV, no pointer mode | D-pad · **Select** | Arrow keys · Enter |

## The quick-action dial

The dial is six buttons plus a centre that opens a sheet with every action and the resolution
presets. Pick its six buttons under **Quick actions** in settings, or edit the dial itself.

| Client | Opens the dial |
|---|---|
| Linux, Windows | **Ctrl+Alt+Shift+O**, a two-finger twist on a touchscreen |
| macOS | **⌃⌥⇧O**, **Stream → Quick Actions** |
| Android | **Back**, a two-finger twist, **Ctrl+Alt+Shift+O** |
| iPhone, iPad | A two-finger twist, the corner button, **⌃⌥⇧O** on a keyboard; in **Touch passthrough**, pull two fingers in from a side edge |
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
| Session | **End stream**, **End game** (a game this device launched, then the stream), **Disconnect, keep the game running** |
| Input | **Touch mode**, **Keyboard**, **Virtual controller**, **Send text**, **Guide button**, **Quick access menu**, **Controller mouse**, **Controller type**, **Release input** (give your mouse and keyboard back, or capture them again), **Mouse mode** ([capture or desktop](/docs/input#mouse-modes)), **Clipboard sharing** (start or stop it; Mac, iPhone, iPad) |
| View · Audio | **Statistics**, **Screens** (dual-screen handhelds: the panel below, the picture below, or the picture across both), **Fullscreen**, **Minimize** (the stream keeps running), **Microphone**, **Mute this stream** (this device only) |
| Host | **Sleep host**, **Restart host**, **Shut down host** ([Host power](/docs/host-power)) |
| Key combinations | Keys you add, such as Alt+F4. A modifier alone works too: **Win** opens the host's Start menu |

A button a client can't serve is dimmed and says why. **Send text** works on Android against a
host that takes typed text. Each press of **Controller type** re-plugs your controllers as the
next type (Automatic, Xbox 360, Xbox One, DualSense, DualShock 4, Steam Deck) for this stream. The
next stream starts from your settings. See also [Controller mouse](/docs/input#controller-mouse).

## Changing a shortcut

1. Open **Settings → Input → Shortcuts**. Choose a row or **Add shortcut**.
2. **Does**: a quick action, a host action, a key combination, or **Quick actions** (the dial).
3. **Press**: choose **Record** and press the buttons or keys, or **Choose** them from a grid.
4. **Fires on**: **Press**, or **Hold** for a second.
5. Choose **Save** (**Add** for a new one). The console saves each change as you make it.

**Remove** deletes a row; **Reset to defaults** brings back the shortcuts on this page. While you
edit a [preset](/docs/presets-and-links), the page edits that preset's shortcuts. A change
applies from the next stream.

- One button on its own only works as a hold. A short press still reaches the game.
- A press can't be part of a longer one: Select + A would fire before Select + A + B can complete.
- The same buttons can be a press and a hold, like the leave chord: the press fires if you let go
  before a second.
- On a keyboard: Ctrl, Alt or Shift plus a key, or a function key alone. ⌘ and Win stay the host's.
- A hold bound to **Guide button**, **Quick access menu** or a key combination keeps it down past
  a second, until you let go: hold for Alt+Tab and the task switcher stays open.
- Touch gestures and remote buttons are listed as **Fixed**.

**What the game sees:** the buttons you press before the last one of a shortcut. The last one
waits: a press keeps it from the game, and a hold gives it back as a short tap, a beat late, if
you let go before a second. Shortcuts work while input is released and while the dial is open.
