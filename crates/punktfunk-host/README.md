# punktfunk-host

The streaming host: it accepts a client, builds a virtual display at that client's exact `WxH@Hz`,
captures and encodes it on the GPU, and streams it out. Runs on Linux (the primary path) and Windows
x64.

Everything below the platform seam lives in sibling crates — [`punktfunk-core`](../punktfunk-core/)
for the wire format, `pf-vdisplay` for the virtual outputs, `pf-capture`, `pf-encode`,
`pf-encode-win`, `pf-inject`, `pf-zerocopy`. This crate is the session orchestration, the two
protocol planes, the management API and the game library around them. Each module's `//!` is its own
map; browse `src/` rather than trusting a list here.

Two protocols from one process: `punktfunk/1` (QUIC control plane, GF(2¹⁶) FEC + AES-GCM data plane)
is what a native client uses and always runs; `--gamestream` additionally serves stock Moonlight and
is opt-in, trusted-LAN only.

## Run it

`serve` runs inside your desktop session:

```sh
cargo run -rp punktfunk-host -- serve                 # native-only (the secure default)
cargo run -rp punktfunk-host -- serve --gamestream    # + Moonlight compatibility
```

Then pair from the web console on `https://<host-ip>:47992` or from a client app (the
management API itself is 47990, and keeps every admin action loopback-only). For anything but
development, install a package instead — [`packaging/`](../../packaging/) builds them and
[docs.punktfunk.unom.io/docs/install](https://docs.punktfunk.unom.io/docs/install) is the user-facing
guide.

`--help` lists every subcommand and flag, and is the only list that cannot go stale: `serve`, `ctl`
(operator control over the local mgmt API), `plugins`, `tray`, `openapi` (regenerates
[`api/openapi.json`](../../api/openapi.json)), `punktfunk1-host` (a standalone native listener for
measurement), `probe-compositor`, `list-monitors`, `spike`, plus `service` / `driver` / `web` on
Windows.

## Test

```sh
cargo test -p punktfunk-host        # Linux or Windows; several suites self-skip without a compositor
```

The host compiles on macOS (CI checks it) but has no capture or display backend there. Run these
on Linux or Windows.
