---
title: Client settings
description: Every setting a Punktfunk client stores — what it does, what it defaults to, and which of them the host can overrule.
---

Every setting a Punktfunk app keeps, with its default and the apps that offer it. The host's own
settings are on [Configuration](/docs/configuration).

## Where the settings live

The apps group settings as **General**, **Display**, **Input**, **Audio** and **Controllers**, under
**Preferences** on Linux and **Settings** elsewhere. Apple TV has no **Input** and gives **Quick
Actions** its own category. The controller interface (the console) has its own **Settings**, with
**Stream**, **Video**, **Audio**, **Controller**, **Input**, **Interface** and **Presets**. On a
Steam Deck the console is the only settings screen: **Open Punktfunk** in the
[Decky panel](/docs/steam-deck).

Linux stores settings in `~/.config/punktfunk/client-gtk-settings.json`, shared with the console;
Windows in `%APPDATA%\punktfunk\client-windows-settings.json`. Changes apply to the next session.

Names below are the Linux app's. **All** means Linux, Windows, Mac, iPhone, iPad, Apple TV and
Android. Most settings can differ per [preset](/docs/presets-and-links); the exceptions are
[listed below](#settings-that-are-facts-about-your-device).

## Video

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Resolution** | Native display | The host builds a display at exactly this size. **Native display** is the screen the window is on. **Aspect ratio** picks which sizes are listed. | All. Apple: 1920 × 1080, with **Use this display's mode**; the Mac takes a typed size. Apple TV: **Stream mode** (720p, 1080p or 4K at 60 Hz). Android adds **Native display (safe area)**. |
| **Match window** | Off | The stream follows the window's size; each resize renegotiates. Fullscreen uses the screen's own mode. | Linux, Windows, Mac, iPhone, iPad, console |
| **Refresh rate** | Native | The stream's frame rate. | All. Apple: 60 Hz; the Mac takes a typed rate; Apple TV sets it with **Stream mode**. |
| **Bitrate** | Automatic | The whole budget on the wire: video, error correction and audio. **Automatic** starts at 20 Mbps, adapts to the link and never drops below 2 Mbps. A fixed rate holds for the session. | All. iPhone, iPad and Mac: **Automatic bitrate** plus a slider; elsewhere type any rate. |
| **Render scale** | Native (1×) | The host renders at your resolution times this, 0.5× to 4×; your device scales the picture to the window. Above 1× is sharper and costs bandwidth. | All |
| **Picture fit** | Fit | When the stream's shape differs from the screen: **Fit** adds black bars, **Crop to fill** cuts the edges, **Stretch to fill** distorts. | All. On Android 9, **Crop to fill** squeezes instead. |
| **Video codec** | Automatic | Your pick when the host can encode it, otherwise HEVC, then AV1, then H.264. [PyroWave](/docs/pyrowave) is never picked automatically. | All. Apple and Android hide AV1 and PyroWave when the device can't decode them; the console marks them **(unsupported)**. |
| **10-bit HDR** | On | Off never sends HDR. On sends 10-bit HDR when the host has HDR content and can encode it. See [HDR](/docs/hdr). | All. Android: **HDR**, greyed on a screen without HDR10. Windows: **HDR (10-bit, BT.2020 PQ)**. |
| **Full chroma (4:4:4)** | Off | Sharper text and thin lines, at more bandwidth. Needs HEVC or PyroWave and a host that can encode it. | Linux, Windows, Mac, iPhone, iPad, Apple TV |
| **10-bit SDR** | Off | Smoother gradients without HDR, where the host's encoder supports it. HDR takes over when it engages. | All. Android: **10-bit colour**. |
| **Prioritize** | Lowest latency | **Lowest latency** shows each frame at once. **Smoothness** holds a small buffer that evens out network hiccups, at that much delay. | All |
| **Smoothness buffer** | Automatic (2 frames) | Frames held under **Smoothness**, 1 to 3. Each adds about one screen refresh of delay. | All. Apple: **Buffer**. |
| **V-Sync** | On (Mac: off) | Off shows each frame as soon as it's ready, with tearing. A driver without a tearing mode stays tear-free. | Linux, Windows, Mac |
| **Follow variable refresh rate** | On | A VRR, FreeSync or G-Sync screen refreshes in step with the stream, in fullscreen. | Linux, Windows, Mac, iPhone, iPad. Apple: **Allow VRR**. |
| **Host compositor** | Automatic | Which backend a Linux host uses for the virtual display. A host without it picks its own. | All |
| **Video decoder**, **GPU** | Automatic | The decoder and graphics card this device uses. Change them only to debug; `PUNKTFUNK_DECODER` [overrides](/docs/configuration#client-side-native-clients) the decoder. | Linux, Windows. **GPU**: Windows, and Linux with more than one GPU. |
| **Low-latency mode** | On | Asks the decoder and system for their low-latency paths. Turn it off if a device misbehaves. | Android |

**Test network speed…** in a host card's menu suggests a bitrate (Android: **Network speed test**;
Apple: on the host page). With PyroWave the bitrate row is greyed: the host sets the rate from its
[bits per pixel](/docs/pyrowave#bits-per-pixel).

## Audio

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Audio channels** | Stereo | Stereo, 5.1 or 7.1. A Linux host sends real surround; a Windows host converts its current output. | All |
| **Audio format** | Standard (Opus) | **Lossless** sends uncompressed PCM, 2.3 Mbps and up, outside the video budget. Stereo only. The host can refuse it: see [Configuration](/docs/configuration#audio--microphone). | All. Linux and Windows offer 48 and 96 kHz; Apple and Android add 44.1, 88.2 and 176.4 kHz. Apple and the console: **Audio quality**. |
| **Keep host audio playing** | Off | Off, the host goes quiet while you stream. On, its own speakers keep playing too. With several clients, one asking is enough. | All |
| **Stream microphone** | Off (Apple: on) | Sends this device's microphone to the host. **Ctrl+Alt+Shift+V** [mutes it](/docs/input#muting-your-microphone). | All but Apple TV. Windows: **Stream microphone to the host**. Apple: **Send microphone to the host**. |
| **Echo cancellation** | On | Keeps the host's audio, playing from your speakers, out of the microphone. Turn it off if your microphone does its own processing. See [Why do I hear myself](/docs/echo). | All but Apple TV |
| **Speaker**, **Microphone** | System default | Which output plays the stream and which input feeds the microphone. | Linux, Windows, Mac (plus **Microphone channel**) |

## Input

Touch modes, mouse modes and the in-stream keys are explained on [Input](/docs/input).

### Controllers

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Forward controllers** | On | Off, this device's controllers aren't sent. Use it when a pad reaches the host another way, such as [USB passthrough](/docs/automation#recipe-full-controller-passthrough-virtualhere). On Linux and Windows, off also disables the [controller exit chord](/docs/input#leaving-with-a-controller). | All |
| **Gamepad type** | Automatic | The virtual pad the host creates; Automatic matches each controller. An Xbox pad has no gyro: for motion, pick DualSense, DualShock 4 or Steam Deck. | All. **Steam Controller 2**: Linux, Windows, console. Apple, Android, console: **Controller type**. |
| **Forwarded controller** | Automatic (all controllers) | Forward only the controller you pick. | Linux, Windows, Apple. Apple and console: **Use controller**. |
| **Steam / guide button** | Automatic | Where the guide and quick-access buttons go: **Send to host** or **This device**. Automatic keeps them on the device only in Steam Deck Gaming Mode. See [the guide button](/docs/input#the-guide-button-xbox--ps--steam-and-quick-access). | All. Apple, Android: **Guide button**. |
| **Hold Select for guide** | Automatic | Hold Select about ⅓ s to press the host's guide button. A Select tap then arrives a beat late. Automatic turns it on in Gaming Mode and on iPhone, iPad and Apple TV. | All |
| **Controller haptics** | On | Plays a wired DualSense's voice-coil haptics. See [Controller audio](/docs/controller-audio). | Linux, Windows, Android |
| **Controller speaker** | On (Android: off) | Plays the game's pad audio on a wired DualSense's speaker. | Linux, Windows, Android |
| **Steam Controller 2 passthrough** | On (Apple: off) | Passes a Steam Controller 2 to a Linux or Windows host as itself, so its trackpads, gyro and haptics work as they do locally. Android and Mac: USB, the Puck or Bluetooth. iPhone, iPad, Apple TV: Bluetooth. | Android, Apple |
| **DualSense / DualShock passthrough (USB)** | On | Drives a USB DualSense or DualShock 4 directly, for adaptive triggers, lightbar and gyro. | Android |
| **Rumble on this phone**, **Gyro from this phone** | Off | The phone's own motor and gyro stand in for a clip-on pad's. | Android, iPhone |

### Keyboard, mouse and touch

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Touch input** | Trackpad | **Trackpad**, **Direct pointer** or **Touch passthrough**. | Linux, Windows, iPhone, iPad, Android |
| **Mouse input** | Capture (games) (Android: Desktop) | **Capture** locks the pointer for games; **Desktop** points absolutely. | Linux, Windows, Mac, Android |
| **Capture pointer for games** | On | Locks a hardware mouse for mouse-look in fullscreen. | iPad |
| **Capture system shortcuts** | On | While input is captured, Alt+Tab and the Windows key (⌘ shortcuts on a Mac) go to the host. **Ctrl+Alt+Shift+Q** or ⌘⎋ always releases capture. On Linux it needs KDE Plasma, GNOME or a wlroots compositor. | Linux, Windows, Mac |
| **Modifier keys** | Mac (⌥ Alt · ⌘ Super) | **Windows (⌘ Alt · ⌥ Super)** makes the key beside the space bar send Alt, like a PC keyboard. | Mac, iPhone, iPad |
| **Invert scroll direction** | Off | Reverses the scrolling sent to the host. | All but Apple TV |

### Quick actions

**Quick actions** edits the in-stream [quick-action dial](/docs/input#the-quick-action-dial) on the
dial itself: click or tap a button to change it, drag one onto another to swap them, and add key
shortcuts. It's on every app and in the console (which edits your defaults only). On Android,
**Back opens quick actions** (on) sets what Back does mid-stream.

**Virtual controller** (iPhone, iPad, Android) is under Quick actions:

| Setting | Default | What it does |
|---|---|---|
| **Layout** | Full | **Full**, **Sticks and shoulders** or **D-pad and face buttons** |
| **Opacity** | 45 % | 15–100 % |
| **Scale** | 100 % | 60–160 % |
| **Edit layout** | — | Move, resize (50–200 %) or hide each control. Wide and upright screens keep separate layouts. |

## Behavior

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Start in** | Host list | **Library** opens the default host's games; **Stream** connects to its desktop. With several paired hosts, set one with **Make default host** in its menu (Apple: **Default host** on the host page) or `punktfunk default-host`. | All |
| **Auto-wake on connect** | On | Wakes a sleeping saved host with [Wake-on-LAN](/docs/wake-on-lan) and waits. Turn it off for hosts over a VPN. | All. Console: **Wake hosts automatically**. |
| **Start streams in fullscreen** | On | F11 or Alt+Enter leaves fullscreen. | Linux, Windows, Mac (**Fullscreen while streaming**) |
| **Keep streaming in background** | Off | Audio and the connection stay live while you switch apps, for 10 minutes unless you pick another limit. | iPhone, iPad, Apple TV, Android phones (**Keep streaming in the background**) |
| **Safe windowed presentation** | On | Avoids a macOS display-driver crash in windowed streams, at a little latency. | Mac |

## Interface

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Controller-optimized UI** | On | Switches to the console when a controller is in use. | Android (not Android TV, which always uses it), Apple (**Gamepad-optimized browsing**) |
| **Show it** | With a controller | **Always** keeps the console without a pad, for a docked phone or tablet. | Android, Apple |
| **Background** | Violet | The console's backdrop colour: seven dark, six pale. **Eclipse** is true black for OLED screens. | Console; Apple TV **Settings** |
| **Reduce motion** | Off | Stops the console's moving backdrop. Apple follows the system setting instead. | Console on Linux, Windows, Android |
| **Reduce interface resolution** | On for Android TV, off on phones | Draws the console at 1080p and lets the screen scale it up, for slow 4K TV boxes. The stream isn't affected. | Android console |
| **Library view**, **Start in collections** | Shelf, off | How a host's library opens. | Console, Apple |
| **Follow the Omarchy theme** | On | The app and console follow `omarchy-theme-set`. **Hosts in the Omarchy menu** adds your hosts to the Omarchy menu. See [Omarchy](/docs/omarchy#this-box-as-a-client). | Linux on Omarchy. Console: **Follow system theme**. |

## Overlay

| Setting | Default | What it does | Where |
|---|---|---|---|
| **Statistics overlay** | Normal | **Off**, **Compact**, **Normal** or **Detailed**: the level a stream starts at. Change it live in-stream; see [Stats](/docs/stats). | All. Windows: **Stats overlay (HUD)**. Android: **Stats overlay**. |
| **Position** | Top right | The overlay's corner. | Apple |
| **Advanced statistics** | Off | Off shows the figures Moonlight's overlay also shows. On shows Punktfunk's capture-to-screen timings. | All |

## Settings that are facts about your device

These describe the device you hold, so a preset can't change them and they don't show while you edit
one: **Video decoder**, **GPU**, the **Speaker** and **Microphone** devices, **Forwarded
controller**, **Auto-wake on connect**, **Start in**, **Advanced statistics** and the
[Interface](#interface) rows.

**Share clipboard** isn't in Settings: it's per host, in the host's edit sheet. See
[Shared clipboard](/docs/clipboard).

## When the client and the host disagree

The host answers each request before your decoder and speakers are set up, so you always get what
it really sends.

| You ask for | The host |
|---|---|
| A resolution and refresh rate | Builds a display at exactly that mode. A host [streaming a real monitor](/docs/virtual-displays#stream-a-real-monitor-instead) keeps that monitor's mode and your device scales. A size the encoder can't take fails the connect. |
| A bitrate | Clamps it to 500 kbps – 8 Gbps. PyroWave ignores it. |
| A codec | Uses it if it can encode it, otherwise HEVC, AV1, H.264 in that order. |
| 10-bit HDR | Sends it only for HDR content on an encoder that can; otherwise 8-bit SDR. |
| 4:4:4 | Sends it only when every requirement is met; otherwise 4:2:0. |
| A channel count | Rounds it to 2, 6 or 8. |
| A gamepad type | Uses it; a type it can't create becomes an Xbox 360 pad. |
| A compositor | Treats it as a hint and picks its own when that one isn't there. |
