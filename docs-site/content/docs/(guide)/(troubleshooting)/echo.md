---
title: Why do I hear myself
description: Stop an echo while streaming — device speakers, Windows mic monitoring, host speakers, voice chat on the host and virtual mixers.
---

Hearing your own voice a beat late is a loop in one of five places. Check them in order; the first
two cover most cases.

## Your device's speakers

Your device's microphone picks up the stream playing from its speakers and sends it back.

Use headphones. **Echo cancellation** in [client settings](/docs/client-settings#audio) is on by
default and removes some of it; how much depends on the device. To stop talking for a moment,
[mute your microphone](/docs/input#muting-your-microphone) without leaving the stream.

## "Listen to this device" and app monitoring (Windows hosts)

Windows or an app plays the Punktfunk microphone out of the host's speakers, and the stream sends
it back.

- Windows: **Sound settings** → **More sound settings** → **Recording**, double-click the Punktfunk
  microphone (usually **Steam Streaming Microphone**), and on the **Listen** tab untick **Listen to
  this device**.
- Apps: turn off microphone monitoring, such as Discord's **Mic Test** or OBS's **Monitor audio**
  on a mic source.

## The host's own speakers

With **Host → Settings → Where audio plays** set to **Device and host**, your device's microphone
hears the host's speakers when you stream from the same room. Set it back to **Device only**, or
turn the host down.

## Voice chat running on the host

Friends streaming in hear themselves when a voice app runs on the host, because the stream carries
its playback. Set **Host → Settings → Voice chat** to **On the host**:
[Voice chat while they play](/docs/friends-over-the-internet#voice-chat-while-they-play).

## Virtual mixers (VoiceMeeter and friends)

A VoiceMeeter strip that hears the Punktfunk microphone and feeds the streamed output makes a loop.
Take the microphone off every strip that reaches that output.

## What the host log shows

While your microphone is in use, the host logs `mic uplink health` every 30 seconds: `depth_ms`
against `target_ms` for buffering, `gaps` and `concealed` for loss, `cadence_ms` for delivery. It
doesn't find an echo, but include it in a report when your voice also sounds choppy. The startup
log names the devices the host picked for the microphone and for audio capture.
