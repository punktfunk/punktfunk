---
title: Audio
description: Fixes for streamed audio that stutters, sounds worse than on the host, or lags behind the picture.
---

Fixes for sound on the stream. Hearing yourself? See [Why do I hear myself](/docs/echo). Audio
quality and lossless audio are [host settings](/docs/configuration#settings-in-the-web-console).

## Linux

### Audio stutters, and only the audio (Linux)

Something linked the host's virtual output to another device, and that device clocks the
stream. The host log warns `our audio capture group is being clocked by another node` and names it
in `driver=`.

Remove the link, usually a loopback ("listen to this device") from the host's output to a real
one. A sound card reached over the network (VirtualHere, USB/IP) is the worst case, because its clock
doesn't survive the link: turn its profile off (KDE: **Audio** → the device → **Profile** → **Off**).

Without that warning, treat it like any other stutter:
[Stutter, drops, or high latency](/docs/troubleshooting-stream#stutter-drops-or-high-latency).

## Windows

### Streamed audio sounds worse than the host does

The output the host captures mixes below 48 kHz or in fewer channels than the stream. The host log
warns `capturing an endpoint that …` and names the device.

- Raise that device's format to 48000 Hz or more in Windows' sound settings.
- A Bluetooth headset in hands-free mode mixes at 16 kHz mono or less. Switch it to its stereo
  profile.

## Any host

### Audio lags behind the picture

The client's audio buffer trims itself when it drifts deeper, so steady lag is rare.

- Reconnect once. If the lag is gone, it had built up; if not, something else adds it.
- Check the `audio playback` line in the client's log for a rising `underruns` count: the buffer
  runs dry, which is a network or CPU problem. [Send the log to the host](/docs/report-an-issue)
  to read it.
- Use a wired connection or 5 GHz Wi-Fi. Less jitter means a shallower buffer.
