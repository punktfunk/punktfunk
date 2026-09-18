---
title: Understanding the Stats Overlay
description: What every number in the Punktfunk stats overlay means, in the Standard and Advanced views, and how the Standard view lines up with Moonlight's.
---

Every Punktfunk client has an in-stream stats overlay, and every client builds it from the same
measurements with the same labels. It speaks one of two vocabularies:

- **Standard**, the default, shows the figures Moonlight's overlay shows, measured the same way
  (averages over the last second), so you can hold the two side by side.
- **Advanced** shows Punktfunk's own view: how long a frame takes from capture on the host to your
  screen, as a median and a slow-frame figure, and every stage in between.

Turn on **Advanced statistics** in each client's [Settings](/docs/client-settings#overlay) to switch.
The setting belongs to the device, so a settings preset never changes it.

## Detail levels

Both views have four levels — **Off → Compact → Normal → Detailed**. Each level shows everything the
one below it shows. Settings picks the level a stream starts at; cycle it live in-stream:

| Platform | Cycle with |
|---|---|
| Linux · Windows · Steam Deck | **Ctrl+Alt+Shift+S**, a **three-finger tap** on a touchscreen, or the quick-action ring |
| macOS / iPad (pointer or trackpad) | **⌃⌥⇧S** or a **three-finger tap** |
| Android · iPhone | a **three-finger tap** |
| Apple TV | **hold Play/Pause** on the Siri Remote |
| Any client with a controller in hand | **Select + X** |
| LG TV (webOS) | the **green** button on the remote |
| Browser (preview) | **Ctrl+Alt+Shift+S**; the quality dot shows or hides the overlay |

A cycle lasts for that stream; the next stream starts at the level in Settings again.
**Ctrl+Alt+Shift+S** is one of a small set of shortcuts a stream reserves; the others are in
[Getting your input back](/docs/input#getting-your-input-back).

The overlay follows your display's scaling. To nudge it on a desktop client, set
`PUNKTFUNK_OSD_SCALE` in the **client's** environment (0.5×–4×) — see [Configuration →
Client-side](/docs/configuration#client-side-native-clients).

## Standard view

```
1920×1080@120 · HEVC 10-bit · native-vulkan · HDR
received 120 fps · decoded 120 · presented 119 · 24.3 Mb/s
host 3.1 ms · decode 2.1 ms · display 2.3 ms (avg)
lost 0.0% · skipped 0.0% · rtt 0.6 ms
```

**Compact** is one line: `120 fps · 24.3 Mb/s · decode 2.1 ms`, plus `lost N%` when frames are lost.
**Normal** is the four lines above. **Detailed** adds the host's fastest and slowest frame, the two
halves of `display` where your device measures them, the encoder's target bitrate, and the audio
buffer.

Every time is an average over the last second, like Moonlight's. A figure your device cannot measure
is left out rather than shown as zero.

A session holding a controller gets one more line above all of these — `player 2`, or `players 2 · 4`
with more than one pad — naming the controller slots the host gave it. That is the player number a
local co-op game reads, and the host's operator can place it; see
[couch co-op over JOIN](/docs/access-levels#couch-co-op-over-join). Every tier shows it, and a
session with no pad shows nothing.

| Standard | What it measures | Moonlight's line |
|---|---|---|
| `received N fps` | Frames that arrived from the network | Incoming frame rate from network |
| `decoded N` | Frames the decoder produced | Decoding frame rate |
| `presented N` | Frames that reached the screen | Rendering frame rate |
| `Mb/s` | The video you received, without error-correction overhead | (bitrate, where shown) |
| `host X ms` | Capture to sent, reported by the host for every frame | Host processing latency (average) |
| `decode X ms` | Received to decoded, on your device | Average decoding time |
| `display X ms` | Decoded to on screen: the wait for a refresh, drawing, and vsync | Average frame queue delay + average rendering time |
| `lost N%` | Frames the network lost beyond what error correction could rebuild | Frames dropped by your network connection |
| `skipped N%` | Frames your device chose not to show because a newer one had arrived | Frames dropped due to network jitter |
| `rtt X ms` | Round trip of the connection | Average network latency |

Three differences remain, and none of them makes Punktfunk look better than it is:

- Punktfunk's `host` includes the paced send of the frame; Sunshine's stops just before it.
- Moonlight's headline FPS estimates what the host produced, lost frames included. `received` counts
  what arrived and reports loss separately; the two agree while nothing is lost.
- On Android, Moonlight measures nothing after the decoder. Punktfunk shows `display` anyway, with
  the phone's own compositor wait left out (see [Advanced view](#advanced-view)).

## Advanced view

Every Advanced figure is the time between two of four points in a frame's life:

1. **capture** — the host grabs the frame. Stamped on the host's clock and carried with the frame.
2. **received** — your device has the whole frame from the network, after any error correction.
3. **decoded** — the video decoder has produced the picture.
4. **displayed** — the picture reaches the screen, as close to the light leaving the panel as the
   platform lets an app measure.

```
1920×1080@120 · 120 fps · 24.3 Mb/s · target 30 Mb/s (auto) · HEVC 10-bit · native-vulkan · HDR
end-to-end 14.2 ms p50 · 19.8 p95 · capture→on-glass
= host 3.1 + network 6.7 + decode 2.1 + display 2.3 (pace 0.6 + latch 1.7) · presented 119
host: queue 0.6 · encode 1.8 · xfer 0.2 · pace 0.5 ms
rtt 0.6 ms
audio buffer 28 ms · a/v +4 ms
lost 3 (2.4%) · skipped 1 · FEC 12
present: mailbox · vrr yes
```

**Compact** is `fps · end-to-end ms · Mb/s`. **Normal** is the first two lines, the resolved audio
format on a lossless session, and `lost` when frames are lost. **Detailed** is everything.

- **The headline.** `end-to-end` is measured directly from capture to the point named after the
  arrow. `p50` is the typical frame; `p95` is the slow frames. It stops where your device can see:
  `capture→on-glass` or `capture→displayed` on a device that stamps the screen,
  `capture→decoded` for a second in which no frame reported reaching the screen, and
  `capture→received` on a device whose decoder shows the picture by itself (an LG TV, and the
  Apple clients' fallback presenter). A shorter chain is smaller because it measures less.
- **The equation.** The stages tile the headline: each starts where the previous one ends.
  - `host` — capture to sent: the host's own share, reported for every frame.
  - `network` — sent to received: the flight across the network, plus reassembly on your device.
    Against a host that does not report its share, `host+network` stands in for both.
  - `decode` — received to decoded, on your device.
  - `display` — decoded to on screen. Where the driver reports true on-glass timing it splits into
    `pace` (Punktfunk getting the frame to the screen) and `latch` (the screen taking it). A large
    `latch` is the refresh cycle, not the stream.
  - `presented N` — frames that reached the screen this second. Far below `fps` means frames are
    being dropped on your device; an `fps` shortfall with `presented` keeping up is upstream.

  The stages are medians, so they sum only roughly to the headline, which is measured on its own.
- **`host:`** splits the host's share into queue, encode, the error coding and send wait (`xfer`),
  and the paced send.
- **`os present +X excluded`** (iOS, tvOS, Android) — the depth of the operating system's own present
  pipeline, which no app can pace under. It is left out of the headline and of `display`, and shown
  here so you can add it back before comparing with a Mac, Linux or Windows client.
- **`rtt`** — the connection's round trip. `network` should sit near half of it plus the time to
  send one frame; far above means the clocks disagree.
- **`clock offset suspect`** — frames came out with an impossible negative latency, so the clock
  correction between host and client is wrong and the headline cannot be trusted.
- **`decode … (1 sample, inside display — not additive)`** — on the Vulkan Video decoder the decode
  runs on the GPU while the frame is being presented, so it is shown apart from the equation.
- **Counters.** `lost` counts frames the network lost beyond error correction, `skipped` frames your
  device chose not to show (`⚠ N overflow` when the decoder fell behind), and `FEC` the pieces error
  correction rebuilt: loss you did not see.
- **Audio.** `audio buffer` is decoded audio queued ahead of your speakers; `a/v` is where that
  places it against the picture — positive means audio plays behind the picture. The client steers
  towards zero without dropping below the depth your link's jitter needs.
- **Device lines.** Some lines only one platform can measure: `present:` names how frames reach the
  screen on Linux and Windows (`mailbox`, `fifo`, …, `vrr yes/no` once measured, queue counters when
  they move); `integrity:` reports decode damage on the hardware decoders; `judder` and `coalesced`
  report presentation cadence on Android; `link latency` and `client queue` report Apple's display
  link and receive backlog; an LG TV shows its CPU and memory use.

### Clocks, and the `(same-host clock)` tag

`end-to-end` and `host+network` span two machines. At connect, the client measures the offset between
its clock and the host's and corrects for it. When that was not possible, the headline adds
**`(same-host clock)`**: the figures are then only right when client and host are the same machine.

### What each platform can measure

| client | headline | why |
|---|---|---|
| Windows, Linux | `capture→on-glass` | the present instant is available; published raw |
| macOS | `capture→on-glass` | the system's on-glass time for the flip; published raw |
| iOS, tvOS | `capture→on-glass` | available, with the OS present floor left out |
| Android | `capture→displayed` | the platform's render timestamp, with the OS present floor left out |
| LG TV (webOS) | `capture→received` | the TV's decoder presents on its own |
| Browser (preview) | `capture→displayed` | the page's draw call, not the screen; always `(same-host clock)`, since a browser measures no clock offset |
| macOS/iOS fallback presenter | `capture→received` | the system video layer hides decode and present timing |

## Comparing with Moonlight / Sunshine

The Standard view is the comparison: each of its lines has a Moonlight line beside it (see the table
above).

The Advanced headline has **no Moonlight counterpart**. Moonlight's overlay shows separate client
segments and, on a Sunshine host, the host's share; nothing in it measures capture to screen, or the
network flight of a frame. To hold an Advanced headline against Moonlight, rebuild an approximation:

```
Moonlight ≈ host processing latency (avg)
          + ½ × average network latency
          + average decoding time
          + average frame queue delay
          + average rendering time
```

It is still approximate: Moonlight's figures are averages, the Advanced figures are medians, and
half a round trip stands in for a one-way flight Moonlight does not measure.

## Scripts and the desktop's stdout

The desktop session prints two lines once per second while the overlay is on: `stats:` carries the
Advanced Detailed text with lines joined by ` | `, whichever view is on screen, and `stats-json:`
carries every figure as JSON. Parse the JSON; the text is for people reading a log.

## Recording a capture for a bug report

The overlay only ever shows the last second. To capture a whole run, use the host's recorder — the
**Performance** page in the [web console](/docs/web-console):

1. Press **Start capture**. Sampling happens at the host's existing aggregation boundary (about
   every 1–2 s), so arming it costs the stream nothing.
2. Reproduce the problem. The live graphs fill in as it runs.
3. Press **Stop & save**. The recording appears in the list below, and survives a host restart.

The latency graph stacks the host's stages in milliseconds against a dashed line at one frame for the
stream's rate, with the host's whole share drawn over the stack. A stage near the line is using most
of its frame. The stages depend on how the host encodes:

| Path | Stages |
|---|---|
| Linux native | queue, capture, submit, encode, send |
| Windows | pool (in the display driver, from the frame's present until its encode starts), encode, hand-off (driver to host), copy, send |
| GameStream (Moonlight clients) | capture, encode, packetize, send, send spread; on Windows, the Windows stages plus send spread |

An older Windows recording shows one **driver** span in place of pool, encode and hand-off.

The other graphs show new against repeated frames per second next to the stream's rate, the video
bitrate next to the encoder's target, frame and send drops, and the round trip to the client. A
native stream adds what sealing each frame costs: error correction, encryption and the socket
sends, in microseconds. A counter the host cannot see is left out. The header names the **encoder backend and the GPU**:
without them a stage split can't be read.

**Download** saves a recording as a `.json` file you can attach to a report; **Delete** removes it.
On disk they live on the host in `~/.config/punktfunk/captures/` (`%ProgramData%\punktfunk\captures\`
on Windows) until you delete them.
