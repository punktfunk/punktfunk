# punktfunk — probe (reference client)

`punktfunk-probe` is the headless reference client for `punktfunk/2`: it connects, exercises a
plane, and reports numbers. Nothing to watch on — use a real client for that. Because it links the
same `punktfunk-core` as every other client, it doubles as the worked example of driving the
protocol end to end.

It receives a real stream, writes a playable elementary stream whose extension tracks the
*negotiated* codec, and reports per-frame capture→received percentiles from the host's own capture
stamp. Against a synthetic host it byte-checks deterministic test frames instead.

```sh
cargo run -p punktfunk-probe -- --discover
cargo run -p punktfunk-probe -- --mode 1280x720x120 --connect HOST:PORT --out /tmp/a.h265
cargo run -p punktfunk-probe -- --connect HOST:PORT --pair -          # PIN on stdin
cargo run -p punktfunk-probe -- --connect HOST:PORT --pin <64-hex> --input-test
```

The scripted plane exercisers are the reason to reach for it: `--input-test` (mouse/keyboard),
`--mic-test` (a 440 Hz Opus tone up to the host mic), `--touch-test`, `--type TEXT` (committed
text, as a soft keyboard sends it), and `--rich-input-test` (DualSense touchpad and motion,
logging the HID-output feedback that comes back). Negotiation knobs
— `--mode`, `--remode` for a mid-stream change, `--bitrate`, `--codec`, `--audio-channels`,
`--speed-test` — cover the rest.

The full flag reference is the module doc-comment at the top of [`src/main.rs`](src/main.rs), which
cannot drift from the parser the way a table here would. Probe against a persistent listener with
`punktfunk-host punktfunk1-host`.
