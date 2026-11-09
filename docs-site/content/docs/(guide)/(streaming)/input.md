---
title: Mouse, touch and pen
description: Mouse modes, scrolling, touch modes, the controller mouse, the virtual controller, second screens and stylus input.
---

Drive the host with a mouse, a touchscreen, a controller or a pen. The settings named here are in
your client's **Input** settings; see [Client settings](/docs/client-settings#input). The keys
that give your input back, the controller chords and the quick-action dial are on
[Shortcuts and quick actions](/docs/shortcuts).

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
A flick's glide is the one your device produced, on every host. On GNOME, Sway and Hyprland a
touchpad scrolls host apps by the pixel, like a local one. KDE, gamescope and Windows take only
wheel steps from a remote device, so apps there move a touchpad scroll in lines or ticks.

**Invert scroll direction** reverses every scroll you send, the controller mouse's included. To
flip it for one stream, open the dial's centre sheet and choose **Input → Invert scroll
direction**; your saved settings stay as they are.

## Touch modes

On a touchscreen (Android, iPhone, iPad, Linux, Windows), **Touch input** picks one of four
modes:

- **Trackpad** (default) — your finger moves the host cursor like a laptop touchpad. Lift and
  swipe again to cross a large screen.
- **Direct pointer** — the cursor jumps to your finger and follows it.
- **Touch passthrough** — every finger is sent as a real touch, with no gestures. Only for apps
  that understand touch.
- **Off** — touches on the stream don't reach the host. Use it with the
  [virtual controller](#virtual-controller), so a tap beside a button can't move the cursor.

Trackpad and Direct pointer share these gestures:

| Gesture | Does |
|---|---|
| Tap | Left click |
| Two-finger tap | Right click |
| Two-finger drag | Scroll |
| Tap, then press and drag; or press and hold, then drag | Held left drag |
| Three-finger tap | Cycle the [stats overlay](/docs/stats) |
| Three-finger swipe up / down (Android, iPhone, iPad) | Show or hide the on-screen keyboard |
| Two-finger twist | Open the [quick-action dial](/docs/shortcuts#the-quick-action-dial) |

In **Off**, only the last three work.

Touch passthrough depends on the host:

| Host | Touch passthrough |
|---|---|
| KDE Plasma, GNOME | Full multi-touch |
| Windows 10 1809 and newer | Full multi-touch |
| Sway, Hyprland, niri, Umbriel and other wlroots compositors | Not supported: the client uses Trackpad and says so |
| gamescope | One finger, as an absolute pointer ([gamescope](/docs/gamescope)) |

## Controller mouse

For a launcher, dialog or desktop that ignores controllers, fire **Controller mouse** on the
[dial](/docs/shortcuts#the-quick-action-dial). Each press moves the controller that opened the
dial to the next mode, and each stream starts with **Off**:

- **Off**: the controller plays.
- **Touchpad**: the controller still plays, and its touchpads move the host's pointer.
- **Full**: the whole controller drives the pointer, and the game sees an idle pad.

The touchpads work the same in both modes. A tap never clicks, so a thumb resting on the pad
mid-game fires nothing.

| Touchpad | Touchpad and Full |
|---|---|
| DualSense, DualShock 4 | Pointer — slide a finger, as on a laptop trackpad. Click for left click, click with two fingers for right click |
| Steam Deck, Steam Controller: right pad | Pointer; click for left click |
| Steam Deck, Steam Controller: left pad | Scroll; click for right click |

A controller without a touchpad plays on unchanged in **Touchpad**. In **Full**, the rest of the
controller works like this:

| Controller | Full |
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

## Virtual controller

On Android, iPhone and iPad, the dial's **Virtual controller** button draws a controller over the
stream. The host sees it as one more pad. Fingers on its controls drive the game; fingers
elsewhere still drive the touch mode. Set **Touch mode** to **Off** to keep them from reaching
the host. Under **Quick actions** in settings, pick a **Layout**
(**Full**, **Sticks and shoulders**, **D-pad and face buttons**), set **Opacity** and **Scale**,
or use **Edit layout** to move, resize and hide controls, separately for wide and upright screens.

## Dual-screen handhelds

On an Android handheld with a second screen of 7 inches or less, or a foldable half open, the
stream takes one screen and a panel takes the other: statistics, the dial's actions, a keyboard
that types on the host while the handheld's own buttons stay on the game, a trackpad and the
virtual controller. **Screens**, on the dial or the panel, cycles three layouts: the panel below
the picture, the picture below the panel, or **one picture across both screens** — its top half
above, its bottom half below, the lower screen's touch landing on its half. That last one is how a
DS or 3DS emulator's two screens land on two screens: set the emulator to a vertical layout with
no gap, and the split lands on the hinge. A DS, 3DS or Wii U title launched from the library
starts that way. The handheld remembers the layout per second screen, and an **Automatic**
resolution follows it — the picture screen's mode, or two of them stacked when the picture spans
both — at once where the host can change mode mid-stream, else from the next connect.

## Foldable iPhone

Set a foldable iPhone down half open, like a laptop, and the stream splits at the hinge: the
picture takes the upright half and the flat half becomes a trackpad, or the
[virtual controller](#virtual-controller) while it is shown. Open the phone flat and the picture
takes the whole screen again. Held half open like a book, nothing changes.

A **Native display** resolution follows the picture: the cover screen's mode when the phone is
shut, the inner screen's when it is open, and the upright half's while it is half open. A size
you picked stays as picked. With **Match window** on, the stream also follows a rotation and the
space the phone gives the app beside another one. The host changes mode mid-stream where it can.

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
for every client, switch off **Pen input** in the web console under **Host → Input**,
or set `PUNKTFUNK_PEN=0` in [`host.env`](/docs/configuration). It applies from the next session.
