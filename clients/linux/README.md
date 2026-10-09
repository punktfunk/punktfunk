# punktfunk — Linux client

`punktfunk-client` is the relm4/GTK4/libadwaita **shell**: hosts, pairing and trust, settings, the
desktop library page. It does not stream. Every session runs in the sibling
[`punktfunk-session`](../session/) Vulkan binary, which the shell spawns — `--connect` and
`--browse` exec it directly, so the Decky wrapper keeps working unchanged.

Rust end to end, no C ABI. The UI-agnostic plumbing — session pump, the native decode ladder,
PipeWire audio, SDL3 gamepads and keymap, trust store, mDNS discovery, library client,
Wake-on-LAN — is `crates/client/pf-client-core`, shared with the session binary.

Installing it is [the docs site](https://docs.punktfunk.unom.io/docs/install-client)'s job;
building the packages is [`packaging/`](../../packaging/)'s.

## Build & run

Needs GTK ≥ 4.16, libadwaita ≥ 1.5, PipeWire and SDL3 (with hidapi) development packages, plus a C
compiler — the CPU decode rung builds OpenH264 from source. No *decoder* development package is
needed: libva and the Vulkan loader are opened at runtime, so hardware decode is a fact about the
box you run on, not the one you build on.

```sh
cargo run -p punktfunk-client-linux                             # the app
cargo run -p punktfunk-client-linux -- --connect HOST[:PORT]    # straight to a stream
cargo run -p punktfunk-client-linux -- --browse HOST            # the gamepad library launcher
```

Headless paths stay in the shell: `--pair - --connect host[:port]` (PIN on stdin), `--wake`, and
`--library host[:mgmt_port]`.

## Layout

`src/` splits by screen — `ui_hosts.rs`, `ui_library.rs`, `ui_trust.rs`, `ui_settings.rs` — around
`app.rs` (the relm4 AppModel: window, trust gate, session-child lifecycle), `cli.rs` (the headless
paths and the exec handoff) and `spawn.rs` (the session child's stdout contract → `AppMsg`).
`tools/screenshots.sh` captures the store screenshots, with an Xvfb fallback.
