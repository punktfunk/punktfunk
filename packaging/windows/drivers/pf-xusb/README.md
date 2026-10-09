# pf-xusb — virtual Xbox 360 XUSB companion (UMDF2, classic XInput)

A **pure-user-mode** UMDF2 driver that makes a virtual Xbox 360 controller visible to classic
**`XInputGetState`** with **no kernel bus driver** (no ViGEmBus) — the HIDMaestro approach. It is the
Windows counterpart to ViGEm's X360 target, owned in-tree.

## Why this is not the HID driver

XInput does **not** use HID. `xinput1_4.dll` enumerates the **XUSB device-interface GUID**
`{EC87F1E3-C13B-4100-B5F7-8B84D54260CB}` (`SetupDiEnumDeviceInterfaces`), opens the Nth present
instance (= player slot 0–3) with `CreateFile`, and polls it with buffered IOCTLs. So this driver:

- is **not** a HID minidriver (no `MsHidUmdf`) — it's a plain UMDF2 function driver under `WUDFRd`,
  **System** setup class;
- registers the XUSB interface with `WdfDeviceCreateDeviceInterface(device, &XUSB_GUID, NULL)`;
- answers the XUSB IOCTLs (all `METHOD_BUFFERED`, delivered to user mode by the reflector) from
  controller state the host publishes into an **unnamed** shared DATA section reached over the
  **sealed pad channel** (punktfunk-planning: `gamepad-channel-sealing.md`): the host duplicates the section
  handle into this driver's WUDFHost, bootstrapped via the named `Global\pfxusb-boot-<index>`
  mailbox (`pf_driver_proto::gamepad::PadBootstrap`); a game's rumble (`SET_STATE`) is published
  back for the host to forward to the client.

The WAIT_* IOCTLs return `STATUS_INVALID_DEVICE_REQUEST`, which makes `xinput1_4` fall back to
synchronous `GET_STATE` polling — so no manual queue / timer is needed for classic XInput. (WGI/
GameInput admission additionally needs a `xinputhid` `UpperFilters` registry tripwire + the async
`WAIT_FOR_INPUT` pump — not implemented; classic XInput does not need it.)

## Verified wire formats (source: HIDMaestro `driver/companion.c`, nefarius/XInputHooker `XUSB.h`, ViGEm)

| IOCTL | Code | Reply |
| --- | --- | --- |
| `GET_INFORMATION` | `0x80006000` | 12 B: `[0]`=ver `0x0103`, `[2]`=count `0x01`, `[8]`=VID `045E`, `[10]`=PID `028E` — marks the slot **connected** |
| `GET_CAPABILITIES` | `0x8000E004` | 24 B (or 36 B V2 if `outLen>=36`): Type `0x03`/SubType `0x01`, motor max `0xFFFF` (advertise rumble) |
| `GET_STATE` | `0x8000E00C` | **29 B**: `[0]`ver `[2]`count `[5]`u32 packet# `[0x0B]`u16 wButtons `[0x0D]`LT `[0x0E]`RT `[0x0F..0x16]`4×i16 sticks |
| `SET_STATE` | `0x8000A010` | input 5 B `{00, led, large, small, subcmd}`: `subcmd 0x02`=rumble (large `[2]`, small `[3]`), `0x01`=player-LED |
| `GET_LED_STATE` | `0x8000E008` | `{0,0,0x06}` |
| `GET_BATTERY_INFORMATION` | `0x8000E018` | `{0,0x01,0x03,0}` |
| `WAIT_GUIDE_BUTTON` / `WAIT_FOR_INPUT` | `0x8000E014` / `0x8000E3AC` | `STATUS_INVALID_DEVICE_REQUEST` → GET_STATE fallback |

`wButtons` is the `XINPUT_GAMEPAD_*` bitmap (DPAD_UP `0x0001` … A `0x1000` B `0x2000` X `0x4000`
Y `0x8000`). `dwPacketNumber` (GET_STATE `[5]`) must increment whenever the payload changes.

## Shared-memory layout (unnamed DATA section, 64 B) — host writes state, driver writes rumble

`pf_driver_proto::gamepad::XusbShm` (the crate owns the offsets; both sides compile against it):
`magic u32 @0` (`"PFXU"` `0x55584650`) · `packet u32 @4` (host bumps → dwPacketNumber) · `wButtons u16
@8` · `LT @10` · `RT @11` · `LX/LY/RX/RY i16 @12/@14/@16/@18` · `rumble_seq u32 @24` (driver bumps) ·
`large @28` · `small @29` · health marks `@32/@36` · `pad_index u32 @40` (validated against the
devnode's Location index when the delivered handle is mapped).

## Validated live (2026-06-22, maintainer's RTX test box)

`XInputGetState(0)` returns **CONNECTED** with the pushed buttons/sticks and an incrementing
`dwPacketNumber`; `XInputSetState(0xC000, 0x4000)` reaches the driver as `00 00 c0 40 02` → host sees
`large=192 small=64`. Test tools (on that box): `xusbtest.exe` (creates the `pf_xusb`
devnode + cycling state via shm) and `xinputtest.exe` (`XInputGetState`/`SetState` harness).

## Build / sign / install (same recipe as the DualSense driver)

Built as a member of the in-tree [`packaging/windows/drivers/`](../) workspace — one
`cargo build --release` builds all three drivers; `build-gamepad-drivers.ps1` (one level up) wraps
the whole build/sign/stage flow in CI. The manual steps:

1. `cargo build --release` in the workspace (env `LIBCLANG_PATH`, `Version_Number=10.0.26100.0`) →
   `target\x86_64-pc-windows-msvc\release\pf_xusb.dll`.
2. Clear the FORCE_INTEGRITY PE bit (bit `0x80` at `e_lfanew+0x5e` of `pf_xusb.dll`).
3. `signtool sign /fd SHA256 /sha1 6A52984E54376C45A1C236B1A2C8A746C5AB6131 pf_xusb.dll`.
4. `Inf2Cat /driver:<pkg> /os:10_X64` → re-sign `pf_xusb.cat` with the same thumbprint.
5. `pnputil /add-driver pf_xusb.inf` (no `/install`; the host SwDeviceCreate's `pf_xusb` per session).

## Host integration (done)

`crates/host/punktfunk-host/src/inject/windows/gamepad_windows.rs` is the Windows `GamepadManager` (used by
`PadBackend::Xbox360`): it SwDeviceCreate's the `pf_xusb` companion, delivers the unnamed DATA
section over the sealed channel (`PadChannel`), writes
the XInput state from the client's gamepad frame (already XInput-convention) and forwards rumble. There
is **no ViGEmBus dependency** anymore. The driver is built + signed from source in CI
(`build-gamepad-drivers.ps1`) and installed by the host installer via
`punktfunk-host.exe driver install --gamepad`.

## Multi-pad

The host stamps each pad's index into the device Location (`pszDeviceLocation`); the driver reads it
via `WdfDeviceAllocAndQueryProperty(DevicePropertyLocationInformation)` in EvtDeviceAdd and polls its own
`pfxusb-boot-<index>` bootstrap mailbox (the delivered DATA section's `pad_index` is validated against it). `UmdfHostProcessSharing=ProcessSharingDisabled` (the INF) gives each pad its own
WUDFHost, so the per-pad `SHM_INDEX` static doesn't collide. Validated live: two pads → two distinct
XInput slots. (XInput assigns the player slot 0-3 by interface-enumeration order, independent of this
index — which only routes shared memory.)
