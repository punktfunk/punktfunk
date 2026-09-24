---
title: Understanding the Stats Overlay
description: What every number in the stats overlay means, in the Standard and Advanced views, and how the Standard view lines up with Moonlight's.
---

Every client draws the same stats overlay from the same measurements, in one of two views:

- **Standard**, the default, shows the figures Moonlight's overlay shows, measured the same way
  (averages over the last second), so you can hold the two side by side.
- **Advanced** shows how long a frame takes from capture on the host to your screen, as a median
  and a slow-frame figure, and every stage in between.

Both are in the client's Settings under **Statistics**
([client settings](/docs/client-settings#overlay)): the overlay row picks the level a stream starts
at, **Advanced statistics** the view.

## Detail levels

Four levels, **Off → Compact → Normal → Detailed**, each showing everything the one before it
shows. Cycle them during a stream:

| Client | Cycle with |
|---|---|
| Linux · Windows · Steam Deck | **Ctrl+Alt+Shift+S**, or a three-finger tap on a touchscreen |
| macOS | **⌃⌥⇧S**, or **Stream → Cycle Statistics** |
| iPhone · iPad | a three-finger tap, or **⌃⌥⇧S** on a hardware keyboard |
| Android | a three-finger tap |
| Apple TV | hold **Play/Pause** on the Siri Remote |
| LG TV (webOS) | the **green** button on the remote |
| Browser (preview) | **Ctrl+Alt+Shift+S**; the quality dot shows or hides the overlay |

With a controller, **Select + X** cycles it on every client. The Linux, Windows, Android and Apple
apps also have **Statistics** on the [quick-action dial](/docs/input#the-quick-action-dial). A cycle
lasts for that stream; the next one starts at the level in Settings.

The overlay follows your display's scaling. To resize it on the Linux and Windows clients, see
`PUNKTFUNK_OSD_SCALE` in [Configuration](/docs/configuration#client-side-native-clients).

## Standard view

```
1920×1080@120 · HEVC 10-bit · native-vulkan · HDR
received 120 fps · decoded 120 · presented 119 · 24.3 Mb/s
host 3.1 ms · decode 2.1 ms · display 2.3 ms (avg)
lost 0.0% · skipped 0.0% · rtt 0.6 ms
```

**Compact** is one line: `120 fps · 24.3 Mb/s · decode 2.1 ms`, plus `lost` when frames are lost.
**Normal** is the four lines above. **Detailed** adds `host min/max`, the two halves of `display`
(`display queue` + `render`) where your device measures them, the encoder's `target` bitrate, the
`mic` rate while your microphone is on, and the `audio buffer`.

Every time is an average over the last second. A figure your device can't measure is left out,
not shown as zero.

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

Three differences remain, and none makes Punktfunk look better than it is:

- Punktfunk's `host` includes the paced send of the frame; Sunshine's stops just before it.
- Moonlight's headline FPS estimates what the host produced, lost frames included. `received`
  counts what arrived and reports loss separately.
- On Android, Moonlight measures nothing after the decoder. Punktfunk shows `display`, with the
  phone's own compositor wait left out.

The Advanced headline has no Moonlight counterpart. To hold one against Moonlight, add up
Moonlight's host processing latency, half its network latency, and its decoding time, frame queue
delay and rendering time. It stays approximate: those are averages, the Advanced figures medians.

## In both views

- **`player 2`** (or `players 2 · 4`) leads the overlay when the session holds a controller: the
  player number a local co-op game reads. See
  [couch co-op over JOIN](/docs/access-levels#couch-co-op-over-join).
- The first line names the codec, bit depth and decoder (in Advanced, at Detailed only), then
  `HDR`, or `HDR→SDR` when an HDR stream is tone-mapped for a screen that can't show it, and
  `4:4:4`, or `4:4:4→4:2:0` when you asked for full chroma and the host declined. At every level it
  ends with the name of the stream's [preset](/docs/presets-and-links).
- **`audio lossless 96 kHz / 24-bit`** (Normal and up) names the format of a lossless audio
  session, with `5.1` or `7.1` when it is surround.
- **`lost N`** counts frames instead of a percentage when the last second held under 30 frames.

## Advanced view

Every Advanced figure is the time between two points in a frame's life: **capture** on the host,
**received** (the whole frame, after error correction), **decoded**, and **displayed** (as close to
the panel as the platform lets an app measure).

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

**Compact** is `fps · end-to-end ms · Mb/s`, plus `lost` when frames are lost. **Normal** is the
first two lines, plus the `lost` and `bitrate lowered` line when either applies. **Detailed** is
everything.

- **`end-to-end`** is measured directly from capture to the point after the arrow. `p50` is the
  typical frame, `p95` the slow ones. It stops where your device can see: `capture→on-glass` on
  Linux, Windows and the Apple apps; `capture→displayed` on Android and in the browser (the page's
  draw call); `capture→received` on an LG TV and the Apple fallback presenter, whose decoder shows
  the picture by itself. A second in which no frame reported reaching the screen shows
  `capture→decoded`. A shorter chain measures less.
- **The `=` line** splits the headline into stages, each starting where the previous one ends:
  - `host`: capture to sent. Against a host that doesn't report its share, `host+network` stands
    in for both.
  - `network`: sent to received, including reassembly on your device.
  - `decode`: received to decoded.
  - `display`: decoded to on screen, split into `pace` (getting the frame to the screen) and
    `latch` (the screen taking it) where on-glass timing exists. A large `latch` is the refresh.
  - `presented N`: frames that reached the screen this second. Far below `fps` means your device
    drops frames; an `fps` shortfall with `presented` keeping up is upstream.

  The stages are medians, so they sum only roughly to the headline.
- **`host:`** splits the host's share into queue, encode, error coding and send wait (`xfer`), and
  the paced send.
- **`os present +X excluded`** (iOS, tvOS, Android) is the depth of the system's own present
  pipeline, which no app can pace under. It is left out of the headline and `display`; add it back
  before comparing with a Mac, Linux or Windows client.
- **`rtt`** is the connection's round trip. `network` should sit near half of it plus the time to
  send one frame; far above means the clocks disagree.
- **`clock offset suspect`** means frames came out with impossible negative latency: the clock
  correction is wrong and the headline can't be trusted.
- **`decode … (inside display — not additive)`** appears on the Vulkan Video decoder, where decode
  runs on the GPU during presentation, so it sits outside the `=` line.
- **`lost`** counts frames lost beyond error correction, **`skipped`** frames your device chose not
  to show (`⚠ N overflow` when the decoder fell behind), and **`FEC`** the pieces error correction
  rebuilt: loss you didn't see.
- **`bitrate lowered:`** names why Automatic bitrate last cut the rate (`packet loss`, `decode
  repairs`, `slow decoding`, `slow host encoding` or `network delay`), until it climbs again.
  `N loss repairs/min` counts how often, in the last minute, your device asked the host to repair
  the picture. A count that keeps climbing means the link isn't recovering.
- **`audio buffer`** is decoded audio queued ahead of your speakers; **`a/v`** places it against the
  picture, positive meaning audio plays behind. The client steers towards zero.
- **Device lines** only one platform measures: `present:` (Linux, Windows) names how frames reach
  the screen (`mailbox`, `fifo`, …) and `vrr yes/no` once measured; `integrity:` (Linux, Windows)
  reports decode damage; `judder` and `coalesced` (Android) report cadence; `link latency` and
  `client queue` (Apple) report the display link and receive backlog; an LG TV shows its CPU and
  memory use.

On Android, `⚠ panel 60 Hz, not 120` or `⚠ app capped 60 Hz by the system` appears at every level
when the screen runs below the stream. Check the phone's game frame-rate limit.

### Clocks, and the `(same-host clock)` tag

`end-to-end` and `host+network` span two machines. At connect the client measures the offset
between its clock and the host's and corrects for it. When it couldn't, the headline adds
**`(same-host clock)`**: the figures are then only right when client and host are one machine. A
browser measures no offset, so it always shows the tag.

## Scripts and the client's stdout

While the overlay isn't Off, the Linux and Windows clients' session process prints two lines per
second: `stats:` carries the Advanced Detailed text with lines joined by ` | `, whichever view is on
screen, and `stats-json:` carries every figure as JSON. Parse the JSON; the text is for people.

## Recording a capture for a bug report

The overlay only shows the last second. To record a whole run, use the **Performance** page in the
[web console](/docs/web-console):

1. Press **Start capture**. Arming it costs the stream nothing.
2. Reproduce the problem. The live graphs fill in as it runs.
3. Press **Stop & save**. The recording appears under **Recordings** and survives a host restart.

**Latency by stage** stacks the host's stages against a dashed line at one frame for the stream's
rate. A stage near the line is using most of its frame. The stages depend on how the host encodes:

| Path | Stages |
|---|---|
| Linux native | Queue, Capture, Submit, Encode, Send |
| Windows | Pool (in the display driver, from the frame's present until its encode starts), Encode, Hand-off (driver to host), Copy, Send. One **Driver** stage replaces the first three when the driver doesn't report them. |
| GameStream (Moonlight clients) | Capture, Encode, Packetize, Send, Send spread; on Windows, the Windows stages plus Send spread |

The other graphs show frame rate, bitrate against the encoder's target, drops, the round trip and,
on a native stream, the cost of sealing each frame. The header names the encoder backend and GPU.

**Download** saves a recording as a `.json` file to attach to a report; **Delete** removes it.
Recordings live on the host in `~/.config/punktfunk/captures/`
(`%ProgramData%\punktfunk\captures\` on Windows).
