# visionOS — App Store metadata

Apple Vision Pro is a destination of the iOS target, so it inherits the iPhone/iPad feature set:
microphone uplink, DualSense feedback, mouse and keyboard, HDR through EDR. What it adds, and what
the copy leans on: a stream window beside other apps and several windows streaming at once. The
app has no theater and docks into no environment, so the copy promises neither.

The description reuses the live iOS/tvOS/macOS template with the device swapped, plus a Vision Pro
block. Keywords avoid Apple trademarks ("Vision Pro", "visionOS").

---

## Promotional Text (DE) — max 170 characters

### Primary (151)

```
Dein Gaming-PC auf der Apple Vision Pro: im Fenster neben deinen Apps, mit bis zu 4K und 90 Hz. Niedrige Latenz, dein Controller, ohne Konto und Cloud.
```

## Promotional Text (EN) — max 170 characters

### Primary (143)

```
Your gaming PC on Apple Vision Pro: in a window beside your apps, at up to 4K and 90 Hz. Low latency, your controller, no account and no cloud.
```

---

## Description (DE) — max 4000 characters

```
Punktfunk ist die nächste Generation des lokalen Game-Streamings. Streame von deinem Linux-PC, Headless-Server oder Laptop auf deine Apple Vision Pro – mit niedriger Latenz und bereits eingebauter Unterstützung für virtuelle Displays. Das heißt, die Auflösung passt sich genau an das Gerät an, auf das du streamst. Windows-Hosts werden ebenfalls unterstützt. Sowohl unsere Client-Apps als auch Server/Hosts sind Open Source. Punktfunk unterscheidet sich von bestehenden Lösungen durch eine niedrigere Latenz, die wir durch unser eigenes, auf QUIC basierendes Protokoll erreichen.

Auf der Apple Vision Pro:
• Spiele in einem Fenster neben deinen anderen Apps
• Mehrere Fenster streamen gleichzeitig, die Eingabe folgt dem Fenster, das du zuletzt ausgewählt hast
• Der native Modus streamt in 4K mit 90 Hz

Weitere Features:
• Umfassender DualSense-Controller-Support (Adaptive Trigger, LEDs und Vibration werden unterstützt)
• Xbox-Controller-Support
• Mikrofon-Passthrough: Du kannst aktivieren, dass das Mikrofon von deinem Client an den Host durchgereicht wird
• Stark optimierter Client mit Metal-basiertem Rendering
• 1 Gbps+ ready: Es ist viel Arbeit in die Unterstützung hoher Bitraten geflossen, um hohe Auflösungen und Bildwiederholraten zu ermöglichen. Voraussetzung: Ein gutes Netzwerk/WLAN
• (Experimentell) Durchsuche die Spielbibliothek des Hosts auf dem Client und starte Spiele direkt
• Windows-Hosts: 10-Bit-Farben und HDR-Support
• Statistiken: Glass-to-Glass-Messung der Latenz
• Maus- und Tastatur-Support
```

---

## Description (EN) — max 4000 characters

```
Punktfunk is the next generation of local game streaming. Stream from your Linux PC, headless server or laptop to your Apple Vision Pro — with low latency and built-in support for virtual displays. That means the resolution matches exactly the device you are streaming to. Windows hosts are supported as well. Both our client apps and the server/host are open source. Punktfunk sets itself apart from existing solutions through lower latency, which we achieve with our own QUIC-based protocol.

On Apple Vision Pro:
• Play in a window beside your other apps
• Several windows stream at once, and input follows the window you last pinched
• Native mode streams in 4K at 90 Hz

More features:
• Comprehensive DualSense controller support (adaptive triggers, LEDs and rumble)
• Xbox controller support
• Microphone passthrough: optionally forward the microphone from your client to the host
• Heavily optimised client with Metal-based rendering
• 1 Gbps+ ready: a lot of work went into supporting high bitrates, so high resolutions and refresh rates are possible. Requires a good network/Wi-Fi
• (Experimental) Browse the host's game library on the client and launch games directly
• Windows hosts: 10-bit colour and HDR support
• Statistics: glass-to-glass latency measurement
• Mouse and keyboard support
```

---

## Keywords — max 100 characters

### DE (94)

```
Game-Streaming,Lokal,Open-Source,Gaming,Remote Play,Spatial,Controller,PC,Linux,Windows,4K,HDR
```

### EN (94)

```
game streaming,local,open source,remote play,gaming,spatial,controller,pc,linux,windows,4k,hdr
```

---

## Screenshots — 3840 × 2160, one to ten

No rendered device here: Apple asks for the app in its surroundings and takes a Simulator capture
at this size. Five captures, in listing order. Each carries the headline and subhead of the
frame of that name on the other platforms, in German and English; the app's own strings are
English in both.

| Frame | Scene | Shows |
|-------|-------|-------|
| `pixel` | `01-stream` | The stream window and its ornament |
| `anywhere` | `02-hosts` | The Hosts tab |
| `library` | `15f-library-filter` | The Library tab |
| `gamepad` | `06-console` | The gamepad UI |
| `settings` | `05-settings` | Display settings |

Capture them with `BUILD_VISIONOS=1 bash scripts/build-xcframework.sh`, then
`OUT="$TMPDIR/shots" clients/apple/tools/screenshots.sh visionos`, with `PUNKTFUNK_SHOT_HERO`
naming the store images' game frame on the internal disk. The type is set over them, and the
upload runs, where the other platforms' store images are composed. No fenced block in this
section: `check-limits.py` reads each one as a field.

---

## Review notes template — visionOS

The steps of `review-notes.md`, rewritten for look-and-pinch and the stream window's controls.

```
WHAT THIS APP IS

Punktfunk is a low-latency game- and desktop-streaming client. It streams from a "host" the user
installs on their own gaming PC (Linux, or Windows 11 22H2+), over their own network. The host is
separate open-source software we publish at https://git.unom.io/unom/punktfunk; it is not sold,
and this app has no purchases.

HOW TO REVIEW IT: THE BUILT-IN DEMO HOST

No PC or account is needed. The app contains a demo host: a real host running inside the app
that renders a live desktop, encodes it and streams it through the same connect, decode, audio
and input path a real PC uses. Like a demo login, it is reached with an address we give you:

1. Launch Punktfunk and choose Add Host.
2. Enter the address:  demo.punktfunk   (leave name and port as they are), then Add Host.
3. A "Demo Host" card appears. Select it: the stream starts in the window. Pinch and drag to
   move the pointer, pinch to click, type on a keyboard or press controller buttons -- the host
   draws each input back and chimes.
4. The controls below the stream window: Quick Actions opens the in-stream menu; New Window
   opens a second window that can stream at the same time.
   Disconnect: the Disconnect button in the stream overlay, or End stream in Quick Actions.
5. Browse the Demo Host's library (four sample titles) and launch one: it streams under that
   title. Its host page shows connection, presets and pairing.
6. Settings covers decoder, bitrate, HDR, audio, controllers and presets.

Remove "Demo Host" from its host page to hide it again.

WHY THE APP ASKS FOR WHAT IT ASKS FOR

- Local Network: finds hosts via Bonjour (_punktfunk._udp) and connects to them -- the app's
  entire purpose. The demo needs no permission.
- Microphone (optional, off by default): audio goes to the user's own paired host, appearing
  there as a virtual microphone for voice chat. Never recorded, never sent to us.
- networking.multicast: sends the Wake-on-LAN magic packet, which must go to a broadcast address:
  a sleeping PC has no ARP entry, so unicast cannot reach it. Used for nothing else.
- UIBackgroundModes "audio": a session carries real, audible audio from the host, and this keeps
  it alive if the user steps away briefly. Backgrounded, video decoding stops, only the real
  audio keeps rendering, and a bounded timer disconnects automatically. We never play silence to
  stay alive, nor use the mode outside an audible session.

ACCOUNTS, PURCHASES, DATA

No account, no sign-in, no in-app purchase. The app collects no personal data: no analytics,
tracking, advertising or crash-reporting SDKs, and no connection to any server of ours. Device
identity is a keychain keypair used only to authenticate to the user's own host.

Privacy policy: https://unom.io/en/legal/privacy#punktfunk-apps
```
