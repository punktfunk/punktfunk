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

## Pointer speed

In **Capture (games)** your mouse reaches the host as its own counts, as if it were plugged into
the host. The host's mouse settings decide how far and how fast the pointer travels. Set the
speed there; no client has a sensitivity setting.

| Host | Where the stream's mouse takes its speed |
|---|---|
| Windows | **Settings → Bluetooth & devices → Mouse**: pointer speed and **Enhance pointer precision** |
| KDE Plasma | **System Settings → Mouse & Touchpad**, device **Punktfunk Mouse**: a speed and acceleration of its own |
| GNOME | **Settings → Mouse & Touchpad**: the speed and acceleration every mouse shares |
| Sway, Hyprland | The compositor's input settings for the device named `Punktfunk Mouse` (Sway: `4617:20557:Punktfunk_Mouse`) |
| gamescope, a headless Linux session | One pixel a count, as gamescope moves a plugged-in mouse. No setting |
| macOS | One point a count, with no acceleration. With the virtual mouse on: **System Settings → Mouse** |

- On Linux the **Punktfunk Mouse** appears with the first input a client sends and stays until
  the host stops. Its buttons and wheel follow the host's mouse settings too: left-handed
  buttons, natural scrolling.
- Out of the box KDE Plasma moves it one pixel a count, with no acceleration. GNOME, Sway and
  Hyprland accelerate it like any mouse: a fast stroke travels up to twice as far.
- A Mac host started with `PUNKTFUNK_HID_INPUT=1` in its environment adds a virtual mouse and
  keyboard, which games that read input through Apple's GameController also see. The host's
  signature needs the HID virtual device entitlement; without it, input works as before.
- A game that reads the mouse directly gets the counts untouched on every host, so its own
  sensitivity applies, as it does to a mouse on the host.
- The Linux, Windows, Mac, iPhone, iPad and Android clients send counts. A TV's or a console's
  mouse moves the host pointer one pixel a count on Linux.
- A browser sends your device's own pointer travel, with its speed and acceleration. **Raw mouse
  input** in the browser's settings sends the mouse's counts instead. A Linux host moves the
  pointer one pixel for each unit of either.
- **Desktop (absolute)** follows your own pointer, at your device's speed.

Too fast or too slow? See [Input & controllers](/docs/troubleshooting-input#the-pointer-is-too-fast-or-too-slow).

## Scrolling

Wheels, touchpads, touchscreens and controllers all scroll the host, with no setting to tune.
A flick's glide is the one your device produced, on every host. On GNOME, Sway and Hyprland a
touchpad scrolls host apps by the pixel, like a local one. KDE, gamescope and Windows take only
wheel steps from a remote device, so apps there move a touchpad scroll in lines or ticks. Fingers
on [the host's touchpad](#the-hosts-touchpad) scroll by the host's own touchpad settings instead.

**Invert scroll direction** reverses every scroll you send, the controller mouse's included. To
flip it for one stream, open the dial's centre sheet and choose **Input → Invert scroll
direction**; your saved settings stay as they are.

## The host's touchpad

A Windows host, and a Linux desktop host, show these fingers to their OS as a touchpad of their
own. gamescope and a headless Linux session read no input devices, so there your device moves
the pointer itself:

| Your device | What reaches the host's touchpad |
|---|---|
| A touchscreen in **Trackpad** mode | Up to two fingers |
| A Mac's trackpad, with **Mouse input** on **Capture (games)** | Up to two fingers. Clicks stay your Mac's |
| An Android device's touchpad, with the mouse captured | Every finger and the pad's press |
| An iPad's trackpad | Pinch and rotate. Its motion and scroll stay a mouse's |

The host makes the pointer motion, the scrolling and the zoom from them, by its own touchpad
settings: pointer speed, scroll direction, tap to click. Change those on the host.

With **Invert scroll direction** on, your fingers stay off the host's touchpad and scroll as
that setting says.

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
| Three-finger swipe up / down (Android, iPhone, iPad) | Show or hide the [on-screen keyboard](#on-screen-keyboard) |
| Two-finger twist | Open the [quick-action dial](/docs/shortcuts#the-quick-action-dial) |

In **Off**, only the last three work.

In **Trackpad** mode on [the host's touchpad](#the-hosts-touchpad), a Windows host with tap to
click on makes the clicks itself. Its touchpad settings decide what a tap and a two-finger tap
do, a double tap and drag holds the left button, and press and hold does nothing.

Touch passthrough depends on the host:

| Host | Touch passthrough |
|---|---|
| KDE Plasma, GNOME | Full multi-touch |
| Windows 10 1809 and newer | Full multi-touch |
| macOS | As Sidecar does it: a tap clicks, a pan scrolls with momentum, a long press right-clicks and drags, two fingers scroll or pinch and rotate |
| Sway, Hyprland, niri, Umbriel and other wlroots compositors | Not supported: the client uses Trackpad and says so |
| gamescope | One finger, as an absolute pointer ([gamescope](/docs/gamescope)) |

## On-screen keyboard

Your device's keyboard types on the host. On an Android phone or tablet, a row of keys sits above
it with the keys it lacks: **Esc**, **Tab**, **Ctrl**, **Alt**, **Shift**, **Win**, the arrows,
**Del**, **Home**, **End**, **PgUp**, **PgDn** and **F1** to **F12**. Swipe the row sideways to
reach them all. On Android 9 and 10 the row sits at the top of the screen.

Tap **Ctrl**, **Alt**, **Shift** or **Win** to hold it for the next key, from the row or the
keyboard: **Ctrl**, then **c**, sends Ctrl+C. Tap it again to let it go. Hiding the keyboard lets
go of every held key.

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
| macOS | A tablet pen with pressure, tilt, rotation, both barrel buttons and eraser |

Without host support, the stylus works as a finger, without pressure or tilt. To turn pen off
for every client, switch off **Pen input** in the web console under **Host → Input**,
or set `PUNKTFUNK_PEN=0` in [`host.env`](/docs/configuration). It applies from the next session.
