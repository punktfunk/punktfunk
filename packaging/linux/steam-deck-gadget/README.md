# Virtual Steam Deck via USB gadget — true Steam Input recognition

**Proven on a real Steam Deck (SteamOS 3.8.11), 2026-06-29.** A `raw_gadget` userspace emulator of a
real 3-interface USB Steam Deck (`28DE:1205`) — mouse = interface 0, keyboard = 1, **controller =
interface 2** — bound to a `dummy_hcd` loopback UDC, so the host's own Steam sees a genuine
interface-2 Deck and **promotes it through Steam Input** (XInput pad emission, glyphs, bindings).

## Why this exists (the interface-2 wall)

A virtual Deck created via **UHID** (the `inject/proto/steam_proto.rs` / `steam_controller.rs` path)
binds the kernel `hid-steam` driver, but **Steam Input will not manage it**: Steam filters the Deck's
controller to USB **interface 2**, and a UHID device has no USB interface number (`Interface: -1` in
Steam's `controller.txt`), so Steam enumerates it but never promotes it. A single-interface DualSense
is accepted at `-1` (no ambiguity), but the multi-interface Deck specifically needs interface 2. See
punktfunk-planning: `steam-controller-deck-support.md` §11.

A real multi-interface USB device with the controller on interface 2 requires a **USB gadget**.
SteamOS ships every piece (`CONFIG_USB_DUMMY_HCD=m`, `CONFIG_USB_RAW_GADGET=m`,
`CONFIG_USB_CONFIGFS_F_HID=y`), so this runs on a Deck with no module-building.

## What's here

- **`deck_raw_gadget.c`** — the working emulator. Presents the 3-interface Deck with descriptors
  captured verbatim from a physical Deck (incl. the real 38-byte controller report descriptor), and
  — crucially — answers **every** control transfer, including the HID feature reports (`f_hid` can't,
  so it produced a "zombie controller" in Steam). Streams the 64-byte state report on the interface-2
  interrupt-IN endpoint. Build static (the Deck has no compiler):
  ```sh
  gcc -O2 -static -pthread -o deck_raw_gadget deck_raw_gadget.c
  ```
  Run as root with `dummy_hcd` + `raw_gadget` loaded: `./deck_raw_gadget [seconds]`.
- **`configfs_gadget_up.sh` / `_down.sh`** — the simpler **configfs `f_hid`** variant. It proves the
  structure (interface 2 → `hid-steam` binds → Steam *opens* it + *reserves an XInput slot*) but
  `f_hid` cannot serve HID feature reports, so Steam can't read controller details and drops it as a
  zombie. Kept as the minimal reproducer of the interface-2 result.

## Result (raw_gadget, live)

```
hid-steam ... Steam Controller 'PFDECK000' connected
input: Steam Deck / Steam Deck Motion Sensors            (kernel gamepad + IMU evdevs)
controller.txt: Interface: 2 ... device opened for index 14 ... reserving XInput slot 1
input: Microsoft X-Box 360 pad 1                          (Steam Input's XInput output — promoted)
```
Stable (1 connect, 0 disconnects), no zombie. The kernel `"Steam Deck"` evdev is then grabbed by
Steam Input, which exposes its own X-Box 360 pad — exactly a real Deck's behaviour.

## Key implementation gotchas (all real, all cost time)

- `struct usb_endpoint_descriptor` (ch9.h) is **9 bytes** (audio `bRefresh`/`bSynchAddress`); the wire
  descriptor needs **7** — use a packed 7-byte struct in the config blob or the kernel mis-parses it.
- raw_gadget EP0: a **no-data OUT** control (`SET_CONFIGURATION`, `SET_INTERFACE`, `SET_IDLE`,
  `SET_PROTOCOL`) is completed with a zero-length **`EP0_READ`**, not `EP0_WRITE` (using write →
  `EBUSY`/`can't set config error -110`). IN controls (`GET_*`) use `EP0_WRITE`.
- Don't start the input streamer until after `SET_CONFIGURATION` is fully acked, or its blocking
  `EP_WRITE` starves the control path.
- `dummy_hcd` + `raw_gadget` must both be loaded and `/dev/raw-gadget` present before launch.

## Host backend (shipped — default on for SteamOS)

The C PoC's transport is ported to a Rust host gamepad backend:
`crates/host/punktfunk-host/src/inject/linux/steam_gadget.rs` (`SteamDeckGadget`), driven by the same
`steam_proto` serializer as the UHID `SteamDeckPad`. The Steam-Deck manager
(`inject/linux/steam_controller.rs`) selects per-pad between **UHID** (universal) and the **USB
gadget**: the gadget is the **default on SteamOS hosts** (`gadget_preferred()` → `ID=steamos`;
best-effort `modprobe dummy_hcd raw_gadget`, graceful fallback to UHID if `/dev/raw-gadget` is
unusable), and off elsewhere where UHID stays the default. `PUNKTFUNK_STEAM_GADGET=1`/`0` forces it.
A Deck-as-host with a *physical* Deck never uses it — `resolve_gamepad`'s conflict gate degrades
`SteamDeck` → DualSense first.

The Rust transport is **validated on the Deck** (a static musl test binary that `#[path]`-includes the
real module): it enumerates the 3-interface Deck, hid-steam binds it + reads our serial + creates the
`Steam Deck` + `Motion Sensors` evdevs — identical to the C PoC. A real USB-stack bug it caught: on
musl, `ioctl(fd, RUN)` with no third arg passes a garbage `value`, and raw_gadget's `RUN`/`CONFIGURE`/
`EP0_STALL` reject a non-zero `value` with `EINVAL` — so the no-arg ioctls must pass an explicit `0`.

## Feature contract (hardened — churn fixed)

Steam's `GetControllerInfo` reads HID **feature reports**; serving the real ones is what stops Steam
re-probing (which was destroying + recreating the gamepad evdev — the "churn"). The contract was
captured from a physical Deck (`get_deck_attrs.c`, hidraw `HIDIOCGFEATURE`; usbmon truncates to 32B):

- **`0x83` GET_ATTRIBUTES_VALUES** — `[83, 2d, 9× (attr-id, u32-LE)]`: product id `0x1205`, a unit
  serial (`0x0a`/`0x04` — we stamp a per-instance value so a gadget never collides with a real Deck),
  and capability attrs (`0x09=0x2e`, `0x0b=0x0fa0`, `0x02/0x0c/0x0d/0x0e=0`). **This blob is the fix.**
- **`0xAE` GET_STRING_ATTRIBUTE** — `[ae, len, attr, ascii]`: serial (attr 1), board serial (attr 0).
- Other commands (e.g. `0x87` settings) read back the last write (echo).

Result on the Deck (`feature_reply` in `steam_gadget.rs`): **1 connect / 0 disconnect / 1 gamepad
evdev** (was constant churn), and Steam *activates* the controller cleanly (no `GetControllerInfo
failed`, no zombie) and emits its **X-Box 360 pad**. usbmon on the gadget's bus confirms our state
reports (with the pressed button at byte 8) are delivered on the interrupt-IN and consumed by
hid-steam — so the input transport is proven end-to-end.

## Remaining

- **Glass confirmation of the XInput mapping** — Steam Input only maps the gadget's raw input onto its
  X-Box pad while a game using Steam Input is focused; confirm a button reaches a real game, then
  default the gadget on for SteamOS hosts (it's strictly better than the non-promoted UHID path).
- A `punktfunk-host` build for SteamOS to exercise the integrated path end-to-end with a live client.
