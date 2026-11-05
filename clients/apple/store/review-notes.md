# App Review notes

## The core problem, stated plainly

Punktfunk is the client half of a two-part system: it streams from a host on the user's own PC.
Guideline 2.1(a) asks for a way to exercise every feature, and a reviewer has no such PC. Build
0.37.0 was rejected on exactly that (September 14, 2026), with no notes attached.

The app answers it with a **built-in demo host** (`Home/DemoMode.swift`, `punktfunk-ffi`'s `demo_host.rs`): a
real `punktfunk/1` host on `127.0.0.1` inside the app. It renders a live desktop that reacts to
every input, encodes it with VideoToolbox and streams it through the same connect, decode, audio,
HUD and input path a real PC uses.

No screen offers it, so users never see it. Adding a host at the address **`demo.punktfunk`**
saves a "Demo Host" card whose library holds four sample titles. The notes give that address the
way they would give a demo login. No external host, PIN or video is needed. Paste the notes below.

---

## Notes template — paste into App Store Connect

The App Review Information "Notes" field caps at **4000 characters**; `check-limits.py` counts
the block below. An over-long note is silently truncated from the end, where the privacy and
entitlement answers live.

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

1. Launch Punktfunk and choose Add Host (Apple TV: "Add Host" on the Hosts tab; with a game
   controller: the "Add Host" tile).
2. Enter the address:  demo.punktfunk   (leave name and port as they are), then Add Host.
3. A "Demo Host" card appears. Select it: the stream starts. Move the pointer with the Siri
   Remote touch surface, a mouse or touch; click, type or press controller buttons -- the host
   draws each input back and chimes.
4. Disconnect: Apple TV, hold Back/Menu on the Siri Remote; iPhone/iPad, the Disconnect button in
   the stream overlay; Mac, Stream > Disconnect (Ctrl+Alt+Shift+D).
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
- device.usb / device.bluetooth (macOS): the GameController framework reaches wired controllers
  through IOHIDLibUserClient and wireless ones through startWirelessControllerDiscovery. USB also
  drives DualSense rumble, which CoreHaptics will not. Without these, no controller input.
- network.server (macOS): the App Sandbox gates bind() itself. Our QUIC endpoint and UDP socket
  each bind a local port to receive host-to-client datagrams, and the demo host listens on
  127.0.0.1; without this, no video, audio or rumble arrives.
- UIBackgroundModes "audio" (iPhone/iPad): a session carries real, audible audio from the host,
  and this keeps it alive if the user steps away briefly. Backgrounded, video decoding stops, only
  the real audio keeps rendering, and a bounded timer disconnects automatically. We never play
  silence to stay alive, nor use the mode outside an audible session.

ACCOUNTS, PURCHASES, DATA

No account, no sign-in, no in-app purchase. The app collects no personal data: no analytics,
tracking, advertising or crash-reporting SDKs, and no connection to any server of ours. Device
identity is a keychain keypair used only to authenticate to the user's own host.

Privacy policy: https://unom.io/en/legal/privacy#punktfunk-apps
```

---

## Before you submit — checklist

- [ ] On a fresh install of each platform, add `demo.punktfunk` and stream. On Apple TV use the
      **Siri Remote alone**: "requires an accessory to navigate" is a tvOS rejection.
- [ ] From the Demo Host card, open the library and launch a title; the stream names it.
- [ ] Confirm the privacy policy URL still resolves.
- [ ] Paste the same notes into every platform's version: iOS, macOS and tvOS are reviewed
      separately, and a version without notes is rejected on its own.

## Separately worth checking: the privacy manifest

There is **no `PrivacyInfo.xcprivacy`** anywhere in `clients/apple`. The app does use
`UserDefaults` (`HostStore` reads the `group.io.unom.punktfunk` suite), and `UserDefaults` is one of
Apple's "required reason" APIs, which are expected to be declared in a privacy manifest. Apps
missing a declaration typically get an automated **ITMS-91053** notice on upload.

This is adjacent to the copy work rather than part of it, so nothing has been changed here — but it
is worth adding a manifest declaring `NSPrivacyAccessedAPICategoryUserDefaults` with reason code
`CA92.1` (access to an app group container) and `NSPrivacyTracking` set to `false`, before the next
submission. Confirm the current reason codes against Apple's documentation rather than taking the
code above on trust; the list has changed since it was introduced.
