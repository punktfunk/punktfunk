---
title: Embed the core (C ABI)
description: Build a client for a platform without a Punktfunk app by linking punktfunk-core through its C ABI.
---

Link `punktfunk-core` into your own client and it speaks the whole protocol for you: the QUIC
handshake, pairing, the encrypted UDP data plane, FEC and loss recovery, clock sync. You decode
video, present it, play audio and read input. The contract is
[`include/punktfunk_core.h`](https://git.unom.io/unom/punktfunk/src/branch/main/include/punktfunk_core.h):
every symbol carries a doc comment, and this page is the map to it. A Rust client depends on the
crate with `features = ["quic"]` instead, as the Android client does.

## Build and link

```sh
cargo build -p punktfunk-core --features quic --release
```

One build writes all three library kinds to `target/release/`: `libpunktfunk_core.a` (static),
`libpunktfunk_core.so` / `.dylib` / `punktfunk_core.dll` (dynamic). The build also regenerates the
header from [`abi.rs`](https://git.unom.io/unom/punktfunk/src/branch/main/crates/punktfunk-core/src/abi.rs)
with cbindgen; the header is checked in, and CI fails when it is stale.

Compile your code with `-DPUNKTFUNK_FEATURE_QUIC`. The whole client API (`punktfunk_connect*`,
`punktfunk_connection_*`, `punktfunk_pair`, `punktfunk_probe`) sits behind that define. Without it
the header declares only the raw transport (`punktfunk_session_*`), which the host and tests use.

```sh
cc -std=c11 -DPUNKTFUNK_FEATURE_QUIC -I include -c myclient.c
cc myclient.o -L target/release -lpunktfunk_core -o myclient          # dynamic
NATIVE=$(cargo rustc -p punktfunk-core --features quic --release --lib --crate-type staticlib \
  -- --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)
cc myclient.o target/release/libpunktfunk_core.a $NATIVE -o myclient     # static
```

To cross-compile, `rustup target add <triple>`, set `CC_<triple>` and
`CARGO_TARGET_<TRIPLE>_LINKER` to your SDK's compiler, and add `--target <triple>`. The QUIC build
compiles aws-lc (TLS) and libopus from C, so the target needs a working C compiler. Apple builds go
through `scripts/build-xcframework.sh`, which packs the core into `PunktfunkCore.xcframework` with
`PUNKTFUNK_FEATURE_QUIC` already defined. Toolchain setup:
[Build from source](/docs/developers/build-from-source).

## Check the version first

```c
if (punktfunk_abi_version() != PUNKTFUNK_ABI_VERSION) abort(); /* library ≠ header */
```

Do this before any other call: structs without a size prefix (`PunktfunkHidOutput`,
`PunktfunkAudioPcm`) rely on it to stop an older binary passing a slot that is too small. Structs
that start with `struct_size` (`PunktfunkConnectOpts`, `PunktfunkRichInputEx`, `PunktfunkConfig`)
are zero-initialised, then `struct_size = sizeof(T)`; the core rejects a size below its minimum.
`PUNKTFUNK_WIRE_VERSION` is the protocol, which the core checks against the host itself. ABI
changes per release: [`CHANGELOG.md`](https://git.unom.io/unom/punktfunk/src/branch/main/CHANGELOG.md).

## Rules for every call

- **Status.** Fallible calls return `PunktfunkStatus`: `PUNKTFUNK_STATUS_OK` is 0, errors are
  negative. In pull loops, `NO_FRAME` means nothing arrived before the timeout and `CLOSED` means
  the session ended. `-20` to `-32` (`PUNKTFUNK_STATUS_REJECTED_*`) is the host refusing you; show
  each as its own sentence. A Rust panic never crosses the boundary; it returns `PANIC` (`-99`).
- **One thread per plane.** Video, audio, rumble, HID output, HDR metadata, host timing, cursor,
  clipboard and pad audio are separate queues. Pull each from at most one thread; different planes
  may run concurrently.
- **Borrowed buffers.** `next_au`, `next_audio`, `next_audio_pcm`, `next_cursor_shape` and
  `next_clipboard` hand back pointers into core memory that stay valid until your next pull on the
  same plane. Decode or copy before you pull again.
- **Senders and setters** (`send_*`, `set_*`, `request_*`) enqueue and return, from any thread.
  `punktfunk_connect*`, `punktfunk_pair`, `punktfunk_probe` and `punktfunk_wake_on_lan` block on
  the network: keep them off your UI thread.

## Lifecycle

1. **Logs (optional).** `punktfunk_set_log_callback(3, cb, user)` routes the core's log lines to
   your sink; `3` is info and up. The callback runs on core threads.
2. **Identity, once per install.** `punktfunk_generate_identity` writes a certificate and key as
   PEM (4096-byte buffers are enough). Store both securely and pass them to every pair and connect.
   Hosts require pairing unless started with `--open`, so an anonymous connect gets refused.
3. **Admission.** Either connect with the identity and let the operator click **Approve** in the
   console (the connect waits up to `timeout_ms`; the host holds the request for 3 minutes), or
   call `punktfunk_pair` with the PIN the host shows (`CRYPTO` means a wrong PIN). See
   [Pairing](/docs/pairing). Store the host fingerprint you get back and pass it as `pin_sha256` on
   every later connect.
4. **Connect** with `punktfunk_connect_opts` (below), read the resolved session, and start one
   thread per plane you use. On NULL, `status_out` says why.
5. **Tear down.** Stop your pull threads, call `punktfunk_connection_disconnect_quit` only when the
   user chose to stop (the host then drops the display instead of holding it for a reconnect), then
   `punktfunk_connection_close`, which joins the core's threads and frees the handle.

Discovery isn't part of the C ABI. Browse mDNS for `_punktfunk._udp` with the platform's API, or
let the user type an address. `punktfunk_probe` checks a known address; compare its
`observed_sha256_out` with the stored fingerprint, since any machine on that address answers.
`punktfunk_wake_on_lan` wakes a host from the MACs in its mDNS `mac` TXT record.

## Connect

Fill `PunktfunkConnectOpts`; zero means auto or unspecified for every field. Advertise only codecs
and caps you can decode and present, because the host upgrades to 10-bit, HDR, 4:4:4 or AV1 when
you set the bit. `video_codecs = 0` means HEVC only.

| Field | Meaning |
| --- | --- |
| `host`, `port` | Required. The host's native port ([Ports](/docs/ports)). |
| `width`, `height`, `refresh_hz` | The mode the host creates its virtual display at. |
| `video_codecs`, `preferred_codec` | `PUNKTFUNK_CODEC_*` bits you decode; one bit to prefer. |
| `video_caps` | `PUNKTFUNK_VIDEO_CAP_10BIT` / `_HDR` / `_444`. |
| `audio_channels` | 2, 6 or 8. |
| `audio_rate_hz`, `audio_bits` | Leave 0/0 for Opus. Any non-zero pair asks for lossless PCM. |
| `gamepad`, `compositor` | `PUNKTFUNK_GAMEPAD_*` / `PUNKTFUNK_COMPOSITOR_*`; 0 lets the host pick. |
| `bitrate_kbps` | 0 = host default. |
| `client_caps` | `PUNKTFUNK_CLIENT_CAP_CURSOR` only if you draw the cursor yourself. |
| `pin_sha256`, `client_cert_pem`, `client_key_pem`, `device_name` | Trust and identity (above). |
| `launch_id` | `steam:<appid>` or `custom:<id>` to start a library title. |
| `timeout_ms` | Bounds the whole connect, including a wait for approval. |

The positional `punktfunk_connect` to `punktfunk_connect_ex12` stay for existing callers; new
options land only in the struct. `video_fit` is the one option only `punktfunk_connect_ex12` takes.

After connect, build your decoder and presenter from what the host chose, never from your request:
`punktfunk_connection_codec`, `_color_info` (CICP; transfer 16 or 18 is HDR), `_chroma_format`,
`_mode`, `_audio_channels`, `_audio_sample_rate`, `_audio_bits`, `_gamepad`, `_host_caps`.

## Video

`punktfunk_connection_next_au` delivers complete access units in order, with parameter sets in
band. The first is an IDR; after that the stream is P-frames only, so loss recovery is yours:

- Call `punktfunk_connection_note_frame_index` for every AU. On a gap it asks the host for a
  reference-frame repair and writes `gap_out`.
- Poll `punktfunk_connection_frames_dropped`. When it climbs, call
  `punktfunk_connection_request_keyframe`, at most once per ~100 ms. The decoder conceals missing
  references without an error, so this counter is the signal.
- To freeze on the last good picture until the stream recovers, use the `punktfunk_reanchor_gate_*`
  helpers; they read `PUNKTFUNK_USER_FLAG_RECOVERY_ANCHOR` / `_RECOVERY_POINT` from `frame.flags`.

`punktfunk_connection_request_mode` switches resolution or refresh mid-session. When the host
accepts, the next AU is an IDR: rebuild the decoder from it.

## Audio

Use one of the two per connection. `punktfunk_connection_next_audio_pcm` decodes in the core to
interleaved f32 (`FL FR FC LFE RL RR SL SR`); open your device at `_audio_sample_rate`.
`punktfunk_connection_next_audio` hands you raw packets, for a client with a multistream Opus decoder.

## Input

Fill a `PunktfunkInputEvent` and call `punktfunk_connection_send_input`. Fields depend on `kind`:

| Kind | Fields |
| --- | --- |
| `MOUSE_MOVE_ABS`, `TOUCH_DOWN`/`_MOVE` | `x`/`y` in pixels, `flags = (width << 16) \| height`. A zero `flags` is dropped. |
| `GAMEPAD_BUTTON` | `code` = `PUNKTFUNK_BTN_*`, `x` ≠ 0 pressed, `flags` = pad index. |
| `GAMEPAD_AXIS` | `code` = `PUNKTFUNK_AXIS_*`, `flags` = pad. Sticks are i16 with +y up; triggers 0–255. |
| `MOUSE_SCROLL` | `x` = signed delta, 120 per detent; `code` 0 vertical, 1 horizontal. |

Check `punktfunk_connection_host_caps` before the capability-gated paths: `send_pen` needs
`PUNKTFUNK_HOST_CAP_PEN`, `TEXT_INPUT` events need `PUNKTFUNK_HOST_CAP_TEXT_INPUT`, and
`GAMEPAD_STATE` snapshots need `PUNKTFUNK_HOST_CAP_GAMEPAD_STATE`. Other uplinks: `send_mic` (Opus
you encode), `send_rich_input2` (touchpads, motion), `send_hid_report` (raw reports for a
`PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2` pad).

## Feedback planes

| Pull | What you do with it |
| --- | --- |
| `next_rumble_cmd2` | Apply the motor levels; the core handles lease expiry. `next_rumble`, `_rumble2` and `_rumble_cmd` read the same plane, so use exactly one. |
| `next_hidout` | Replay lightbar, player LEDs, adaptive triggers or raw HID on the physical pad. |
| `next_hdr_meta` | Hand ST.2086 and content light level to your HDR display path. |
| `next_host_timing` | Split host and network latency in a stats view. |
| `next_cursor_shape` / `_state` | Draw the pointer. Only with `PUNKTFUNK_CLIENT_CAP_CURSOR`. |

After `CLOSED`, `punktfunk_connection_end_reason` says whether the session ended normally, and
`_end_reject` / `_end_reject_said` give the host's refusal and its sentence.

## Minimal example

Connects, pulls 600 access units, and quits.

```c
#include <stdio.h>
#include "punktfunk_core.h"

int main(int argc, char **argv) {
    if (argc < 2 || punktfunk_abi_version() != PUNKTFUNK_ABI_VERSION) return 1;
    static char cert[4096], key[4096]; /* generate once, then load from storage */
    if (punktfunk_generate_identity(cert, sizeof cert, key, sizeof key)) return 1;

    PunktfunkConnectOpts o = {0};
    o.struct_size = sizeof o;
    o.host = argv[1]; o.port = 9777;
    o.width = 1920; o.height = 1080; o.refresh_hz = 60;
    o.video_codecs = PUNKTFUNK_CODEC_HEVC | PUNKTFUNK_CODEC_H264;
    o.client_cert_pem = cert; o.client_key_pem = key; o.device_name = "My Client";
    o.timeout_ms = 180000; /* long enough to click Approve in the console */

    uint8_t host_fp[32]; int32_t status = 0;
    PunktfunkConnection *c = punktfunk_connect_opts(&o, host_fp, &status);
    if (!c) { fprintf(stderr, "connect: status %d\n", status); return 1; }

    uint8_t codec = 0; punktfunk_connection_codec(c, &codec);
    PunktfunkFrame f; uint64_t seen = 0, drops = 0;
    for (int n = 0; n < 600;) {
        PunktfunkStatus rc = punktfunk_connection_next_au(c, &f, 20);
        if (rc == PUNKTFUNK_STATUS_CLOSED) break;
        if (rc != PUNKTFUNK_STATUS_OK) continue;
        punktfunk_connection_note_frame_index(c, f.frame_index, NULL);
        /* feed f.data / f.len to a decoder for `codec` */
        punktfunk_connection_frames_dropped(c, &drops);
        if (drops > seen) { punktfunk_connection_request_keyframe(c); seen = drops; }
        n++;
    }
    punktfunk_connection_disconnect_quit(c);
    punktfunk_connection_close(c);
    return 0;
}
```

A real client throttles the keyframe request, persists the identity and `host_fp`, and runs audio
and feedback on their own threads. For a complete client over the same API, read
[`crates/pf-client-core/src/session.rs`](https://git.unom.io/unom/punktfunk/src/branch/main/crates/pf-client-core/src/session.rs).
`bash crates/punktfunk-core/tests/c/run.sh` proves the static library links from C on your machine.
