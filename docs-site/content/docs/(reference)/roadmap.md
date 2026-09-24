---
title: "Roadmap"
description: "Where Punktfunk is heading: what is being worked on, what comes next, and what is not planned."
---

What the project is working on and what comes after it. Nothing under **Working on now** or
**Next** has shipped. For what works today, see the [Support matrix](/docs/support-matrix); for
what changed, the [release notes](https://git.unom.io/unom/punktfunk/releases).

## Working on now

- **Windows host hardening.** The AMD (AMF) and Intel (QSV) encoders see far less use than NVENC,
  so their loss recovery and 10-bit paths get the most field fixes.
- **Latency under load.** Frame pacing on every client, holding the frame rate when the link
  degrades, and overlapping encode and send within a frame at high resolutions.
- **Finishing the clipboard.** The Linux client's side of the bridge, and file transfer, which no
  client offers yet. See [Shared clipboard](/docs/clipboard).
- **Console parity with the apps.** A speed test and a bitrate setting in the web console.
- **Closing unverified cells.** The matrix's
  [What is not verified](/docs/support-matrix#what-is-not-verified) list.

## Next

- **Reach beyond the LAN.** NAT traversal (ICE/STUN/TURN) with a self-hostable relay, and QUIC
  connection migration so a client roams between Wi-Fi and cellular without dropping the stream.
  Until then, reaching a host from outside takes a VPN, a tunnel or a forwarded port — see
  [Friends over the internet](/docs/friends-over-the-internet).
- **Per-user sessions.** A connecting client picks an identity that maps to an account on the host,
  and that person lands in their own signed-in desktop.
- **Remote work.** The host's monitors as separate client windows, the client's camera as a webcam
  on the host, and approving a new device from an already-paired device's app.
- **Picture and sound.** End-to-end variable refresh, latency measured to the client's screen
  rather than to receipt, and head-tracked spatial audio from game audio objects.

## Not planned, or blocked upstream

- **HDR on Mutter, KWin and wlroots virtual displays.** Those compositors' virtual outputs are
  SDR-only upstream. gamescope and the GNOME 50+ monitor mirror carry HDR — see [HDR](/docs/hdr).
- **Hosting on macOS, iOS, tvOS or Android.** They are client-only platforms.
- **HEVC 4:4:4 on AMD.** AMD's encoder can't produce it. Intel's VAAPI path has no 4:4:4 yet
  either. [PyroWave](/docs/pyrowave) carries full chroma on both.
- **DualSense speaker and haptics over Bluetooth.** The controller exposes no audio over Bluetooth,
  so the client needs it on USB. Rumble, adaptive triggers and the lightbar work either way.
