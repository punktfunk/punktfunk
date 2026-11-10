# punktfunk — Windows client

The WinUI 3 shell: discover, pair, configure, launch. Pure Rust — the UI is WinUI 3 driven through
[windows-reactor](https://github.com/microsoft/windows-rs), and it links `punktfunk-core` directly.

Decode, present and input are **not** here. They live in the spawned
[`punktfunk-session`](../session/) binary; this crate is the shell around it. What a user can do
with the app is [the docs site](https://docs.punktfunk.unom.io/docs/install-client)'s job.
The session presents through Vulkan, or through Direct3D 11 where the GPU's Vulkan driver can't
import D3D11VA pictures (Snapdragon X). It picks once at start; `PUNKTFUNK_PRESENT_BACKEND=vulkan`
or `d3d11` forces one, and `punktfunk-session --probe-decode` prints the pick.

Ships x64 and ARM64, three ways from one layout: a signed installer (the default — a per-user
setup.exe whose stable install path Steam can launch, so the overlay and Big Picture work), a
portable zip, and a signed MSIX kept for Microsoft Store compatibility. All three are built from
[`packaging/`](packaging/).

## Build

Windows-only; the crate builds as a stub elsewhere so the workspace stays green. You need the MSVC
toolchain and CMake (SDL3 builds from source) and nothing else — decode is native, so there is no
`FFMPEG_DIR`. At start the client resolves the installed Windows App Runtime 2.6.4 or newer.

```sh
cargo build -p punktfunk-client-windows --target x86_64-pc-windows-msvc

# Cargo names the exe punktfunk-client-windows.exe; the packages ship it as punktfunk-client.exe.
punktfunk-client --discover                                     # list hosts on the LAN
punktfunk-client --headless --connect host[:port] [--pin HEX]   # connect, count frames, print stats
punktfunk-client --headless --speed-test --connect host[:port]  # probe burst → recommended bitrate
```

> `CARGO_HOME` must be an ASCII path — non-ASCII characters break SDL3's MSVC precompiled-header
> build.

## Manual smoke checklist

The windows-reactor pin is a moving target and WinUI regressions rarely show up in `cargo check`.
Walk this after a reactor bump or a change to the render/state architecture:

- **Hosts** — discovery populates tiles; tile hover fill; "…" → Forget and Rename; add-host modal
  connects; the WOL wait screen cancels.
- **Settings** — every section renders; combos still show their selection after a section switch
  *and* a scope switch (the historic blank-combo reconciler bug); preset create / rename / delete;
  colour swatches repaint; the Overridden marker appears on edit and clears on Reset; the GPU combo
  lists adapters.
- **Pair** — PIN entry pairs, and the typed PIN reaches the Connect click (the `use_ref` mirror path).
- **Session** — connect spawns the session; HUD stats tick; Ctrl+Alt+Shift+Q releases the pointer;
  the shell window restores on exit.
- **Shell** — speed test completes; library grid loads; a `punktfunk://` deep link routes and a
  second instance hands off and exits; the window icon appears.
