//! Gamepad shared-memory layouts (host ↔ UMDF drivers `pf_xusb` / `pf_gamepad`).
//!
//! Sealed channel (`design/gamepad-channel-sealing.md`): the host creates the DATA section
//! ([`XusbShm`]/[`PadShm`]) unnamed (SYSTEM-only DACL) and duplicates its handle into WUDFHost;
//! only the tiny [`PadBootstrap`] mailbox stays named. `Pod` + `offset_of!` asserts pin the
//! historical `OFF_*` / `view.add(N)` layout. Layout only; the sections are host-created.

use alloc::string::String;
use bytemuck::{Pod, Zeroable};

/// XUSB section magic (loosely "PFXU").
pub const XUSB_MAGIC: u32 = 0x5558_4650;
/// Pad section magic (loosely "PFDS"). The two magics use opposite byte-order mnemonics;
/// only the u32 value is the contract.
pub const PAD_MAGIC: u32 = 0x5046_4453;

/// `device_type` DualSense. The section is zeroed, so `0` is the default; one driver serves
/// every identity.
pub const DEVTYPE_DUALSENSE: u8 = 0;
/// DualShock 4 (`VID_054C&PID_09CC`).
pub const DEVTYPE_DUALSHOCK4: u8 = 1;
/// DualSense Edge (`VID_054C&PID_0DF2`) — DualSense report codec plus the four back/Fn bits.
pub const DEVTYPE_DUALSENSE_EDGE: u8 = 2;
/// Steam Deck (`VID_28DE&PID_1205`). Steam Input promotes it on Windows when the synthesized
/// USB hardware ids carry `&MI_02` (wired controller interface).
pub const DEVTYPE_STEAMDECK: u8 = 3;
/// Xbox Wireless Controller (`VID_045E&PID_0B13` — Bluetooth Xbox is a real HID device;
/// wired `045E:028E`/`045E:02EA` are not). `pf-xusb` registers only `GUID_DEVINTERFACE_XUSB`
/// and has no HID collection, so Steam/WGI/GameInput never see it.
///
/// Its input report is [`crate::xbox::SERIES_INPUT_LEN`] bytes, not 64: hidclass sizes its
/// buffer from the descriptor and refuses an over-long source.
pub const DEVTYPE_XBOX: u8 = 4;
/// Xbox One S over Bluetooth (`VID_045E&PID_02FD`). No Share button: it serves
/// [`crate::xbox::NO_SHARE_RDESC`], cut from the Series descriptor, never hand-written.
pub const DEVTYPE_XBOX_ONE_S: u8 = 5;
/// Xbox Elite Wireless Controller Series 2 (`VID_045E&PID_0B22`), on the One S descriptor.
/// The four paddles are not in its report; `xinputhid` may claim the collection exclusively.
pub const DEVTYPE_XBOX_ELITE: u8 = 6;
/// Steam Controller 2 (Triton): wired identity `28DE:1302`. Raw-passthrough — host feeds
/// captured reports; the driver answers Steam's feature query-dance (see [`crate::triton`]).
pub const DEVTYPE_TRITON: u8 = 7;
/// Nintendo Switch Pro Controller, wired (`VID_057E&PID_2009`). The driver answers the
/// `0x80` / `0x01` handshake itself from [`crate::switch`]; the host publishes `0x30` state.
/// A driver package without the `pf_switchpro` model line never binds the devnode, so the
/// type needs no protocol bump.
pub const DEVTYPE_SWITCH_PRO: u8 = 8;
/// 8BitDo Ultimate 2 Wireless in its own HID mode (`VID_2DC8&PID_6012`), a Bluetooth identity:
/// no IMU clock, so SDL spaces its samples 1/120 s apart ([`report_period_us`]).
pub const DEVTYPE_8BITDO_ULTIMATE2: u8 = 9;
/// 8BitDo Pro 2 (`VID_2DC8&PID_6003`): feature `0x06` and a µs IMU clock ([`crate::eightbitdo`]).
pub const DEVTYPE_8BITDO_PRO2: u8 = 10;
/// 8BitDo Pro 3 (`VID_2DC8&PID_6009`), as the Pro 2.
pub const DEVTYPE_8BITDO_PRO3: u8 = 11;
/// Wireless HORIPAD for Steam, wired (`VID_0F0D&PID_01AB`) ([`crate::hori`]).
pub const DEVTYPE_HORIPAD_STEAM: u8 = 12;
/// Left Joy-Con over Bluetooth (`VID_057E&PID_2006`): the Pro Controller's protocol with device
/// type 1 ([`crate::switch`]). A pair is two devnodes, one per half, that SDL and Steam combine.
pub const DEVTYPE_JOYCON_LEFT: u8 = 13;
/// Right Joy-Con over Bluetooth (`VID_057E&PID_2007`), device type 2.
pub const DEVTYPE_JOYCON_RIGHT: u8 = 14;

/// Written into the section's `driver_proto` on attach. The section starts zeroed, so `0`
/// means no driver has attached. Bump on a gamepad-layout change.
///
/// v3: sealed DATA section + [`ChannelProof`]. The host learns the duplication target over
/// the device stack, not the mailbox's `driver_pid`. Mixed pairings fail closed both ways.
/// Evidence: `design/gamepad-channel-sealing.md`.
pub const GAMEPAD_PROTO_VERSION: u32 = 3;

/// Behaviour revision the driver stamps into [`PadShm::driver_rev`]. The protocol version
/// only moves when the layout breaks, so a driver with old behaviour still attaches; the host
/// compares this instead and flags an older driver. Bump it with any driver change a game or
/// the host depends on. `1`: devnode-index serials, Deck packet numbers, refused unknown ids.
/// `2`: Share on the Series Xbox pad. `3`: the Switch Pro identity. `4`: the 8BitDo and
/// HORIPAD identities. `5`: the Joy-Con halves.
pub const GAMEPAD_DRIVER_REV: u32 = 5;

// Channel proof: who to hand the DATA section to. Do not take the duplication target from
// the mailbox's `driver_pid` — LocalService can spawn a world-executable WUDFHost and publish
// that pid. Ask the devnode the host created (`SwDeviceCreate` instance id). `pf_xusb` answers
// via IOCTL; `pf_gamepad`/`pf_mouse` have no control device (hidclass owns the stack).

/// Proof magic ("PFCP"), and the `PFCP` prefix of the text form.
pub const PROOF_MAGIC: u32 = 0x5043_4650;

/// HID string index the minidrivers answer with [`ChannelProof`]. 16-bit on purpose: both
/// `IOCTL_HID_GET_INDEXED_STRING` and `IOCTL_HID_GET_STRING` pack `(language_id << 16) |
/// string_index`, so only the low word survives. `0x5046` ("PF") is outside USB's 1..=255
/// string-descriptor range. hidclass currently does not forward an arbitrary indexed-string
/// request to a UMDF HID minidriver; kept as the first ask. Working transports:
/// [`proof_is_serial_string`] (`pf_mouse`) and [`HID_FEATURE_REPORT_CHANNEL_PROOF`] (PS pads).
pub const HID_STRING_INDEX_CHANNEL_PROOF: u32 = 0x5046;

// Do not retry `WdfDeviceCreateDeviceInterface` for `pf_gamepad`/`pf_mouse`: hidclass owns
// `IRP_MJ_CREATE` on a devnode it is the FDO for, so `CreateFile` returns ERROR_GEN_FAILURE.

/// `CTL_CODE(0x8000, 0x0FE0, METHOD_BUFFERED, FILE_ANY_ACCESS)`: function code no xusb22 IOCTL
/// uses; `FILE_ANY_ACCESS` so the host can ask over a `CreateFile` handle opened with no access
/// rights (the same way it must open a HID collection).
pub const IOCTL_PF_GET_CHANNEL_PROOF: u32 = 0x8000_3F80;

/// Whether a driver serves its channel proof as its HID serial-number string.
/// `true` for `pf_mouse` only — its serial (`PFMOUSE00`) is inert. Pad serials are what SDL
/// and Steam dedup on; Steam mangles a pad's displayed name over serial format alone.
pub const fn proof_is_serial_string(pad_kind_is_mouse: bool) -> bool {
    pad_kind_is_mouse
}

/// Feature report the PS pad identities (DualSense / DualShock 4 / Edge) answer the proof on.
/// `0x85` is already declared as Feature in all three captured descriptors, so this needs no
/// report-descriptor change — Steam/SDL fingerprint VID/PID, layout, serial, product string.
pub const HID_FEATURE_REPORT_CHANNEL_PROOF: u8 = 0x85;

/// Steam Deck private proof command. The Deck descriptor declares one unnumbered feature
/// report; Steam drives it as `0x83`/`0xAE`. Two bytes, not one, so a Steam command byte we
/// have not catalogued cannot be mistaken for it. No descriptor change.
pub const DECK_PROOF_CMD: [u8; 2] = [0xF9, 0x50];

/// Driver's answer over the device stack: who it is, which pad, which WUDFHost pid.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct ChannelProof {
    pub magic: u32,
    pub proto: u32,
    /// Pad index from the devnode Location — cross-checked so a mis-resolved devnode cannot
    /// cross-wire two pads.
    pub pad_index: u32,
    /// `GetCurrentProcessId()` of the driver's WUDFHost: the duplication target.
    pub wudf_pid: u32,
}

impl ChannelProof {
    pub fn new(pad_index: u32, wudf_pid: u32) -> ChannelProof {
        ChannelProof {
            magic: PROOF_MAGIC,
            proto: GAMEPAD_PROTO_VERSION,
            pad_index,
            wudf_pid,
        }
    }

    /// Validate against the pad the host is delivering. `Err` is the operator-facing reason;
    /// every rejection is a refusal to deliver — do not fall back to an untrusted pid.
    pub fn check(&self, expect_pad_index: u32) -> Result<u32, &'static str> {
        if self.magic != PROOF_MAGIC {
            return Err(
                "the devnode's answer is not a punktfunk channel proof (bad magic) — \
                        some other driver is bound to this device",
            );
        }
        if self.proto != GAMEPAD_PROTO_VERSION {
            return Err(
                "the driver bound to this devnode speaks a different gamepad protocol \
                        — update the host and the drivers together",
            );
        }
        if self.pad_index != expect_pad_index {
            return Err(
                "the devnode answered for a DIFFERENT pad index — the interface lookup \
                        resolved the wrong device",
            );
        }
        if self.wudf_pid == 0 {
            return Err("the driver reported pid 0");
        }
        Ok(self.wudf_pid)
    }

    /// 16 wire bytes of the `pf_xusb` IOCTL answer. Driver crates need no `bytemuck`; both
    /// sides go through one length-checked pair with [`from_bytes`](Self::from_bytes).
    pub fn to_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(bytemuck::bytes_of(&self));
        out
    }

    /// Parse [`to_bytes`](Self::to_bytes). `None` on a short read — never zero-extend into a pid.
    ///
    /// `pod_read_unaligned`, not `from_bytes`: the feature-report form offsets the proof by one
    /// byte (report id at 0), so the slice is not 4-aligned. Device I/O buffers have no alignment.
    pub fn from_bytes(b: &[u8]) -> Option<ChannelProof> {
        (b.len() >= 16).then(|| bytemuck::pod_read_unaligned::<ChannelProof>(&b[..16]))
    }

    /// HID feature report of exactly `len` bytes: `[report_id, proof(16), 0…]`. Byte 0 is the
    /// report id; the driver pads to the descriptor length. `None` if `len` cannot hold id+proof.
    pub fn to_feature_report(self, report_id: u8, len: usize) -> Option<alloc::vec::Vec<u8>> {
        if len < 17 {
            return None;
        }
        let mut out = alloc::vec![0u8; len];
        out[0] = report_id;
        out[1..17].copy_from_slice(&self.to_bytes());
        Some(out)
    }

    /// Parse [`to_feature_report`](Self::to_feature_report); skips the leading report id.
    pub fn from_feature_report(b: &[u8]) -> Option<ChannelProof> {
        Self::from_bytes(b.get(1..)?)
    }

    /// HID indexed-string form: `PFCP:<proto>:<pad_index>:<wudf_pid>`.
    /// `HidD_GetIndexedString` is a string channel.
    pub fn to_hid_string(self) -> String {
        alloc::format!("PFCP:{}:{}:{}", self.proto, self.pad_index, self.wudf_pid)
    }

    /// Parse [`to_hid_string`](Self::to_hid_string). `None` on any deviation — refuse delivery
    /// rather than guess a pid.
    pub fn from_hid_string(s: &str) -> Option<ChannelProof> {
        let rest = s.strip_prefix("PFCP:")?;
        let mut it = rest.split(':');
        let proto = it.next()?.parse::<u32>().ok()?;
        let pad_index = it.next()?.parse::<u32>().ok()?;
        let wudf_pid = it.next()?.parse::<u32>().ok()?;
        if it.next().is_some() {
            return None; // trailing field: not a shape we mint
        }
        Some(ChannelProof {
            magic: PROOF_MAGIC,
            proto,
            pad_index,
            wudf_pid,
        })
    }
}

/// Bootstrap-mailbox magic (`"PFBT"` LE). The host stamps it last (after `host_proto`) so a
/// driver only trusts a fully-initialized mailbox.
pub const BOOT_MAGIC: u32 = 0x5442_4650;

/// `Global\pfxusb-boot-<index>` — Xbox 360 pad bootstrap mailbox ([`PadBootstrap`]).
pub fn xusb_boot_name(index: u8) -> String {
    alloc::format!("Global\\pfxusb-boot-{index}")
}
/// `Global\pfds-boot-<index>` — DualSense / DualShock 4 bootstrap mailbox ([`PadBootstrap`]).
pub fn pad_boot_name(index: u8) -> String {
    alloc::format!("Global\\pfds-boot-{index}")
}

/// Per-pad bootstrap mailbox (32 B, named `Global\pf…-boot-<index>`, SY+LS DACL) — the only
/// named object on the gamepad channel. UMDF HID minidrivers have no control device (hidclass
/// owns the stack), so this is the late-bound handshake: host stamps `host_proto` then `magic`;
/// driver writes `driver_proto`/`driver_pid`; host asks the **devnode** ([`ChannelProof`]) who
/// the driver is, duplicates the unnamed DATA section, then writes `data_handle`/`handle_pid`
/// and bumps `handle_seq` last. `driver_pid` is advisory; the mailbox does not choose the
/// duplication target. Evidence: `design/gamepad-channel-sealing.md`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct PadBootstrap {
    /// [`BOOT_MAGIC`], host-stamped last at creation.
    pub magic: u32,
    /// Host's [`GAMEPAD_PROTO_VERSION`]. A driver whose version differs must not publish its
    /// pid (fail closed); it still writes `driver_proto` so the host can log the mismatch.
    pub host_proto: u32,
    /// Driver's WUDFHost pid (`0` = none yet). Advisory liveness hint — not the duplication
    /// target; that comes from [`ChannelProof`].
    pub driver_pid: u32,
    /// Driver's [`GAMEPAD_PROTO_VERSION`] (diagnostics only).
    pub driver_proto: u32,
    /// DATA-section handle VALUE duplicated into `handle_pid`'s table; valid only in that process.
    pub data_handle: u64,
    /// Pid `data_handle` was duplicated for — a driver whose pid differs ignores the delivery.
    pub handle_pid: u32,
    /// Host-global monotonic, never 0. Bumped AFTER `data_handle`/`handle_pid` — new-delivery trigger.
    pub handle_seq: u32,
}

/// Virtual Xbox 360 (XInput) shared section (64 B). Host writes XInput state; driver answers
/// `XInputGetState`. Driver writes `XInputSetState` into `rumble_*` (bumping `rumble_seq`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct XusbShm {
    pub magic: u32,
    /// XInput `dwPacketNumber` — bumped on every state change.
    pub packet: u32,
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
    pub _reserved0: u32,
    /// Bumped on a new force-feedback packet.
    pub rumble_seq: u32,
    pub rumble_large: u8,
    pub rumble_small: u8,
    pub _pad0: [u8; 2],
    /// [`GAMEPAD_PROTO_VERSION`] while attached. `0` = no driver — the host health check keys off it.
    pub driver_proto: u32,
    /// Bumped on every serviced XInput IOCTL. Only advances while something polls the slot, so a
    /// static value is not an error.
    pub driver_heartbeat: u32,
    /// Pad index (host-stamped before the magic). The driver checks it against
    /// `pszDeviceLocation` so a cross-pad delivery is rejected. Carved from v1 reserved space.
    pub pad_index: u32,
    pub _reserved1: [u8; 20],
}

/// Pre-ring [`PadShm`] size. Every field old binaries know sits below this offset; the ring
/// keeps bytes `0..256` identical. Pagefile-backed sections are page-granular, so either
/// generation's view maps against either generation's section — a driver must still fall
/// back to this size if the full-size map is refused (`ChannelConfig::min_data_size`).
pub const PAD_SHM_LEGACY_SIZE: usize = 256;

/// v2.1 output-report ring depth — hardcoded `%` in every pre-v2.2 driver, and the drain
/// length whenever [`PadShm::out_ring_len`] reads 0. Eight slots at a ~4 ms poll overflow
/// under a sustained >2 kHz writer (DS5 compat-vibration re-sends per audio quantum).
pub const OUT_RING_LEN: u32 = 8;
pub const OUT_RING_LEN_USIZE: usize = OUT_RING_LEN as usize;

/// v2.2 ring depth: every slot that fits the one-page section ([`PAD_SHM_SIZE`] = 4096).
/// 56 slots at ~4 ms poll ≈ 14 kHz. Used only when both sides negotiated it (`out_ring_ver
/// >= 2` and the driver echoed the length in [`PadShm::out_ring_len`]).
pub const OUT_RING_LEN_V22: u32 = 56;
pub const OUT_RING_LEN_V22_USIZE: usize = OUT_RING_LEN_V22 as usize;

/// v2.1 [`PadShm`] size. A v2.1 driver maps this much and gates 8-slot ring writes on
/// `mapped_len() >= 1024`; v2.2 keeps bytes `0..1024` identical.
pub const PAD_SHM_V21_SIZE: usize = 1024;

/// Full [`PadShm`] size — exactly one page. Hard ceiling: pagefile-backed sections round up
/// to page granularity, which is what lets every generation map its own size against any
/// other generation's section. Growing past 4096 needs a new negotiation.
pub const PAD_SHM_SIZE: usize = 4096;

/// One slot of the lossless output-report ring: report bytes as the game wrote them (id
/// first), with the exact length. The legacy latest-report slot's fixed 64-byte copy can
/// carry a stale tail from a previous longer report.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct OutSlot {
    /// Valid bytes in `data` (`0..=64`). `0` = never written.
    pub len: u32,
    pub data: [u8; 64],
}

/// DualSense / DualShock 4 shared section ([`PAD_SHM_SIZE`] = 4096). Bytes `0..256` are the
/// v2 layout ([`PAD_SHM_LEGACY_SIZE`]); `0..1024` are v2.1 ([`PAD_SHM_V21_SIZE`]). Host writes
/// `input`; driver publishes output into the legacy `output` slot (every host generation reads
/// it) and, when `out_ring_ver` is stamped, into the lossless `out_ring`. The single slot
/// coalesces — a rumble-stop overwritten inside one poll is gone (`design/rumble-root-fix.md`).
///
/// Tail extension, not a [`GAMEPAD_PROTO_VERSION`] bump: bootstrap fails closed on a version
/// mismatch (no pad at all), the wrong failure for a feedback-quality fix. An old host never
/// stamps `out_ring_ver`, so a new driver stays on the legacy slot.
///
/// Ring-length: each side declares, the shorter wins. Host stamps `out_ring_ver = 2`; the
/// driver picks (`>= 2` + full map → 56, `1` → 8, `0` → no ring) and echoes into
/// `out_ring_len` before every `ring_head` bump. Drain keys off the echo (`0` = 8). Store
/// order slot-bytes → echo → head-bump: an Acquire-observed head bump always has that length.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct PadShm {
    pub magic: u32,
    pub _reserved0: u32,
    /// Host-written HID input (≤ 64 B). Spans `magic`+pad .. `out_seq`.
    pub input: [u8; 64],
    /// Bumped when the driver publishes a new `output` report.
    pub out_seq: u32,
    /// Driver-written output: rumble / lightbar / player-LEDs / adaptive triggers.
    pub output: [u8; 64],
    /// HID identity — [`DEVTYPE_DUALSENSE`] / [`DEVTYPE_DUALSHOCK4`].
    pub device_type: u8,
    pub _pad0: [u8; 3],
    /// [`GAMEPAD_PROTO_VERSION`] while mapped. `0` = no driver — the host health check keys off it.
    pub driver_proto: u32,
    /// Bumped by the driver's ~125 Hz timer each tick — advances whenever loaded, game or not.
    pub driver_heartbeat: u32,
    /// Pad index (host-stamped before the magic) — see [`XusbShm::pad_index`].
    pub pad_index: u32,
    /// Host-stamped `1` ⇔ this section carries `out_ring` and the host drains it. Zeroed
    /// section + old host never writes it, so `0` tells a new driver to stay legacy-only.
    pub out_ring_ver: u32,
    /// Driver-bumped AFTER writing `out_ring[ring_head % len]`. Overflow is `head - tail >= len`:
    /// at `len` the next write is already landing in the reader's oldest slot.
    /// Same publish-then-bump order as `out_seq` (host Acquire load).
    pub ring_head: u32,
    /// Ring length the driver's slot math is using, (re-)stamped before every `ring_head`
    /// bump. `0` = pre-v2.2 driver = [`OUT_RING_LEN`].
    pub out_ring_len: u32,
    /// Seqlock over [`PadShm::input`]: odd while mid-copy, even when the slot holds a whole
    /// report. Host: bump odd, Release-fence, write 64 bytes, Release-store even. Driver
    /// samples before and after and retries on a write in flight. An old host leaves this 0
    /// (constant even), so a new driver's re-check always passes. Inside the v2 legacy region.
    pub input_gen: u32,
    /// [`GAMEPAD_DRIVER_REV`], stamped beside `driver_proto`. `0` = a driver older than the
    /// field. Inside the v2 legacy region, so every map reaches it.
    pub driver_rev: u32,
    pub _reserved1: [u8; 80],
    /// Lossless output ring. [`OUT_RING_LEN`] under v2.1, [`OUT_RING_LEN_V22`] under v2.2
    /// (slots 8.. overlay what v2.1 called `_reserved2`, which no shipped binary touched).
    pub out_ring: [OutSlot; OUT_RING_LEN_V22_USIZE],
    pub _reserved2: [u8; 32],
}

// Offsets are the wire contract the shipped drivers already read by hand. A failing assert
// means the struct no longer matches the historical `OFF_*` / `view.add(N)` layout.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<XusbShm>() == 64);
    assert!(offset_of!(XusbShm, magic) == 0);
    assert!(offset_of!(XusbShm, packet) == 4);
    assert!(offset_of!(XusbShm, buttons) == 8);
    assert!(offset_of!(XusbShm, left_trigger) == 10);
    assert!(offset_of!(XusbShm, right_trigger) == 11);
    assert!(offset_of!(XusbShm, thumb_lx) == 12);
    assert!(offset_of!(XusbShm, thumb_ly) == 14);
    assert!(offset_of!(XusbShm, thumb_rx) == 16);
    assert!(offset_of!(XusbShm, thumb_ry) == 18);
    assert!(offset_of!(XusbShm, rumble_seq) == 24);
    assert!(offset_of!(XusbShm, rumble_large) == 28);
    assert!(offset_of!(XusbShm, rumble_small) == 29);
    assert!(offset_of!(XusbShm, driver_proto) == 32);
    assert!(offset_of!(XusbShm, driver_heartbeat) == 36);
    assert!(offset_of!(XusbShm, pad_index) == 40);

    assert!(size_of::<PadShm>() == PAD_SHM_SIZE);
    assert!(offset_of!(PadShm, magic) == 0);
    assert!(offset_of!(PadShm, input) == 8);
    assert!(offset_of!(PadShm, out_seq) == 72);
    assert!(offset_of!(PadShm, output) == 76);
    assert!(offset_of!(PadShm, device_type) == 140);
    assert!(offset_of!(PadShm, driver_proto) == 144);
    assert!(offset_of!(PadShm, driver_heartbeat) == 148);
    assert!(offset_of!(PadShm, pad_index) == 152);
    // Ring extension — everything below PAD_SHM_LEGACY_SIZE is the v2 layout verbatim.
    assert!(offset_of!(PadShm, out_ring_ver) == 156);
    assert!(offset_of!(PadShm, ring_head) == 160);
    assert!(offset_of!(PadShm, out_ring) == PAD_SHM_LEGACY_SIZE);
    assert!(size_of::<OutSlot>() == 68);
    // Echo field in v2.1 reserved space; slot k stays at 256 + k*68; struct is one page.
    assert!(offset_of!(PadShm, out_ring_len) == 164);
    // Input seqlock: 4-aligned (atomic accessors check it) and inside the v2 legacy region.
    assert!(offset_of!(PadShm, input_gen) == 168);
    assert!(offset_of!(PadShm, input_gen) % 4 == 0);
    assert!(offset_of!(PadShm, input_gen) < PAD_SHM_LEGACY_SIZE);
    assert!(offset_of!(PadShm, driver_rev) == 172);
    assert!(PAD_SHM_LEGACY_SIZE + OUT_RING_LEN_USIZE * size_of::<OutSlot>() <= PAD_SHM_V21_SIZE);
    assert!(PAD_SHM_SIZE == 4096);

    assert!(size_of::<ChannelProof>() == 16);
    assert!(offset_of!(ChannelProof, magic) == 0);
    assert!(offset_of!(ChannelProof, proto) == 4);
    assert!(offset_of!(ChannelProof, pad_index) == 8);
    assert!(offset_of!(ChannelProof, wudf_pid) == 12);

    assert!(size_of::<PadBootstrap>() == 32);
    assert!(offset_of!(PadBootstrap, magic) == 0);
    assert!(offset_of!(PadBootstrap, host_proto) == 4);
    assert!(offset_of!(PadBootstrap, driver_pid) == 8);
    assert!(offset_of!(PadBootstrap, driver_proto) == 12);
    assert!(offset_of!(PadBootstrap, data_handle) == 16);
    assert!(offset_of!(PadBootstrap, handle_pid) == 24);
    assert!(offset_of!(PadBootstrap, handle_seq) == 28);
};

/// Vendor Feature report `0x85`, 63 bytes: the sealed channel's proof transport. Every global it
/// uses is restated, so it cannot shift a report above it.
#[rustfmt::skip]
pub const PROOF_ITEMS: [u8; 18] = [
    0x06, 0x00, 0xFF, 0x85, 0x85, 0x09, 0x2D, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95,
    0x3F, 0xB1, 0x02,
];

/// `rdesc` with [`PROOF_ITEMS`] before its closing `End Collection`: what the Windows driver
/// serves for an identity whose own descriptor has no feature `0x85`. Without it hidclass
/// refuses the host's channel proof and the pad serves neutral forever. `N` is `rdesc.len() + 18`.
pub const fn with_proof<const N: usize>(rdesc: &[u8]) -> [u8; N] {
    assert!(N == rdesc.len() + PROOF_ITEMS.len());
    let body = rdesc.len() - 1;
    assert!(rdesc[body] == 0xC0);
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = if i < body {
            rdesc[i]
        } else if i < body + PROOF_ITEMS.len() {
            PROOF_ITEMS[i - body]
        } else {
            0xC0
        };
        i += 1;
    }
    out
}

/// How often a real pad on USB sends an input report: `DualSense`, DualShock 4 and the Deck
/// (its 4 ms connection interval) alike.
///
/// A game polls the stream, not the state: Sony's libScePad hands a frame no sample when no
/// report arrived since its last read, so a pad slower than the game's frame rate reads as a
/// held input released and pressed again.
pub const REPORT_PERIOD_US: u64 = 4_000;

/// The input-report period the driver serves `device_type` at (the Triton serves changes).
///
/// The Pro Controller pushes `0x30` every 8 ms over Bluetooth and 15 ms over USB, three IMU
/// samples each; a Joy-Con every 15 ms. Both sit inside `hid-nintendo`'s 8–17 ms window, and SDL
/// measures the sample spacing from the stream.
pub const fn report_period_us(device_type: u8) -> u64 {
    match device_type {
        DEVTYPE_SWITCH_PRO => 8_000,
        DEVTYPE_JOYCON_LEFT | DEVTYPE_JOYCON_RIGHT => 15_000,
        // SDL's fixed step for this pad over Bluetooth; it carries no clock to correct it.
        DEVTYPE_8BITDO_ULTIMATE2 => 8_333,
        _ => REPORT_PERIOD_US,
    }
}

/// Whether a report is due at `now_us`, given the slot `due_us` it was scheduled for.
///
/// `Some(next)` means serve now and schedule the next slot one `period_us` on. A tick more
/// than a period late restarts the schedule from now rather than catching up, since a burst
/// of back-dated reports is exactly the cadence a game must not see.
pub fn serve_due(now_us: u64, due_us: u64, period_us: u64) -> Option<u64> {
    if now_us < due_us {
        return None;
    }
    let from = if now_us - due_us >= period_us {
        now_us
    } else {
        due_us
    };
    Some(from + period_us)
}

/// Write the pad's own clocks into a report about to be served.
///
/// A USB `DualSense` advances four every report: the 8-bit counter (byte 7), a 32-bit packet
/// sequence (12–15), `sensor_timestamp` (28–31) and a second 32-bit timer (49–52) about 2 ms
/// after it. Motion code integrates gyro over `sensor_timestamp`, so it has to advance by the
/// real time between the reports a game receives. A Deck frame carries `unPacketNum` (4–7),
/// and Valve's `controller_structs.h` tells readers to skip a repeated one. The host publishes
/// at its client's rate and the driver serves at the hardware's, so the driver owns every
/// clock. A Switch Pro `0x30` or `0x21` carries an 8-bit timer (byte 1). `serial` is this
/// report's index, `elapsed_us` the time since the first report; every field wraps as hardware
/// does. The Joy-Con halves carry the Pro's timer. Returns `false`, and leaves the report
/// alone, for an identity that has no such fields.
pub fn stamp_report_clock(
    device_type: u8,
    report: &mut [u8; 64],
    serial: u32,
    elapsed_us: u64,
) -> bool {
    match device_type {
        DEVTYPE_DUALSENSE | DEVTYPE_DUALSENSE_EDGE => {
            report[7] = serial as u8;
            report[12..16].copy_from_slice(&serial.to_le_bytes());
            // 1/3 µs ticks (hid-playstation's DIV_ROUND_CLOSEST(delta, 3)).
            let ticks = elapsed_us * 3;
            report[28..32].copy_from_slice(&(ticks as u32).to_le_bytes());
            // A real pad stamps this ~5 900 ticks (≈2 ms) after the sensor sample.
            report[49..53].copy_from_slice(&((ticks + 5_900) as u32).to_le_bytes());
            true
        }
        DEVTYPE_DUALSHOCK4 => {
            // The counter is the top six bits; the low two are PS and touchpad click.
            report[7] = (report[7] & 0x03) | (((serial as u8) & 0x3F) << 2);
            // 16/3 µs ticks, mirrored into the one touch frame's own timestamp byte.
            let ts = (elapsed_us * 3 / 16) as u16;
            report[10..12].copy_from_slice(&ts.to_le_bytes());
            report[34] = ts as u8;
            true
        }
        DEVTYPE_STEAMDECK => {
            report[4..8].copy_from_slice(&serial.to_le_bytes());
            true
        }
        // A `0x81` handshake ack has no timer; its byte 1 is the echoed command.
        DEVTYPE_SWITCH_PRO | DEVTYPE_JOYCON_LEFT | DEVTYPE_JOYCON_RIGHT if report[0] != 0x81 => {
            report[1] = serial as u8;
            true
        }
        // The µs IMU clock the Pro models declare in feature `0x06`.
        DEVTYPE_8BITDO_PRO2 | DEVTYPE_8BITDO_PRO3 => {
            report[27..31].copy_from_slice(&(elapsed_us as u32).to_le_bytes());
            true
        }
        // A u16 µs clock SDL reads and then replaces with its own fixed step.
        DEVTYPE_HORIPAD_STEAM => {
            report[10..12].copy_from_slice(&(elapsed_us as u16).to_le_bytes());
            true
        }
        _ => false,
    }
}

/// Low octet of a PlayStation pad's MAC: byte 1 of its pairing reply (feature 0x09 / 0x12)
/// and the last two hex digits of its USB serial. Each identity has its own base, so no
/// index of one identity lands on another's octet.
pub const fn ps_mac_low(device_type: u8, index: u8) -> u8 {
    let base: u8 = match device_type {
        DEVTYPE_DUALSHOCK4 => 0x01,
        DEVTYPE_DUALSENSE_EDGE => 0x94,
        _ => 0x74,
    };
    base.wrapping_add(index)
}

/// USB serial string of pad `index` presented as `device_type`. SDL and Steam dedup pads by
/// it, so no two (identity, index) pairs may share one. A PlayStation serial is the pairing
/// MAC, most significant octet first; each Xbox model has its own base octet. A Switch Pro or
/// Joy-Con serial is its device-info MAC ([`crate::switch::mac`]).
pub fn pad_serial(device_type: u8, index: u8) -> String {
    let low = ps_mac_low(device_type, index);
    let xbox = |base: u8| alloc::format!("F4B0FC2A6C{:02X}", base.wrapping_add(index));
    match device_type {
        DEVTYPE_DUALSHOCK4 => alloc::format!("DEADBEEF00{low:02X}"),
        DEVTYPE_STEAMDECK => alloc::format!("FVPF{:08X}", 0x5046_0000u32 | index as u32),
        DEVTYPE_XBOX => xbox(0x10),
        DEVTYPE_XBOX_ONE_S => xbox(0x30),
        DEVTYPE_XBOX_ELITE => xbox(0x50),
        DEVTYPE_TRITON => {
            let mut s = [0u8; 13];
            crate::triton::serial(index, &mut s);
            String::from_utf8_lossy(&s).into_owned()
        }
        DEVTYPE_SWITCH_PRO | DEVTYPE_JOYCON_LEFT | DEVTYPE_JOYCON_RIGHT => {
            crate::switch::mac(device_type, index)
                .iter()
                .map(|b| alloc::format!("{b:02X}"))
                .collect()
        }
        DEVTYPE_8BITDO_ULTIMATE2 | DEVTYPE_8BITDO_PRO2 | DEVTYPE_8BITDO_PRO3 => {
            crate::eightbitdo::mac(device_type, index)
                .iter()
                .map(|b| alloc::format!("{b:02X}"))
                .collect()
        }
        DEVTYPE_HORIPAD_STEAM => crate::hori::serial(index)
            .iter()
            .map(|b| alloc::format!("{b:02X}"))
            .collect(),
        _ => alloc::format!("35533AD6E7{low:02X}"),
    }
}

/// The `pf_*` hardware-id tokens the host puts first on a pad devnode, and the `device_type`
/// each names. A token that prefixes another comes after it (`pf_dualsense` after
/// `pf_dualsenseedge`), so the first match is the right one. Windows Server has no
/// `xinputhid`, so every Xbox kind binds `pf_xbox_nofilter`; the section fixes the PID.
pub const HWID_DEVTYPES: [(&str, u8); 16] = [
    ("pf_switchpro", DEVTYPE_SWITCH_PRO),
    ("pf_joycon_left", DEVTYPE_JOYCON_LEFT),
    ("pf_joycon_right", DEVTYPE_JOYCON_RIGHT),
    ("pf_8bitdo_ultimate2", DEVTYPE_8BITDO_ULTIMATE2),
    ("pf_8bitdo_pro2", DEVTYPE_8BITDO_PRO2),
    ("pf_8bitdo_pro3", DEVTYPE_8BITDO_PRO3),
    ("pf_horipad_steam", DEVTYPE_HORIPAD_STEAM),
    ("pf_xbox_nofilter", DEVTYPE_XBOX),
    ("pf_xboxwireless", DEVTYPE_XBOX),
    ("pf_xboxones", DEVTYPE_XBOX_ONE_S),
    ("pf_xboxelite", DEVTYPE_XBOX_ELITE),
    ("pf_triton", DEVTYPE_TRITON),
    ("pf_steamdeck", DEVTYPE_STEAMDECK),
    ("pf_dualsenseedge", DEVTYPE_DUALSENSE_EDGE),
    ("pf_dualshock4", DEVTYPE_DUALSHOCK4),
    ("pf_dualsense", DEVTYPE_DUALSENSE),
];

/// The identity a devnode's lowercase hardware-id list names. `None` when no `pf_*` token is
/// in it: the driver refuses that devnode rather than guess one.
pub fn devtype_from_hwids(ids: &str) -> Option<u8> {
    HWID_DEVTYPES
        .iter()
        .find(|(token, _)| ids.contains(token))
        .map(|&(_, devtype)| devtype)
}

/// USB VID/PID the driver reports in each identity's HID attributes. SDL, Steam and Windows
/// key their stock mappings off them. `None` for a `device_type` this build does not know.
pub const fn identity_vid_pid(device_type: u8) -> Option<(u16, u16)> {
    Some(match device_type {
        DEVTYPE_DUALSENSE => (0x054C, 0x0CE6),
        DEVTYPE_DUALSHOCK4 => (0x054C, 0x09CC),
        DEVTYPE_DUALSENSE_EDGE => (0x054C, 0x0DF2),
        DEVTYPE_STEAMDECK => (0x28DE, 0x1205),
        DEVTYPE_XBOX => (0x045E, 0x0B13),
        DEVTYPE_XBOX_ONE_S => (0x045E, 0x02FD),
        DEVTYPE_XBOX_ELITE => (0x045E, 0x0B22),
        DEVTYPE_TRITON => (0x28DE, 0x1302),
        DEVTYPE_SWITCH_PRO => (0x057E, 0x2009),
        DEVTYPE_8BITDO_ULTIMATE2 => (0x2DC8, 0x6012),
        DEVTYPE_8BITDO_PRO2 => (0x2DC8, 0x6003),
        DEVTYPE_8BITDO_PRO3 => (0x2DC8, 0x6009),
        DEVTYPE_HORIPAD_STEAM => (0x0F0D, 0x01AB),
        DEVTYPE_JOYCON_LEFT => (0x057E, 0x2006),
        DEVTYPE_JOYCON_RIGHT => (0x057E, 0x2007),
        _ => return None,
    })
}

/// NTSTATUS the driver fails `EvtDeviceAdd` with when no `pf_*` hardware id names the pad
/// (`STATUS_DEVICE_CONFIGURATION_ERROR`). UMDF does not pass it on: the devnode reports
/// `STATUS_DEVICE_DATA_ERROR`, so the host judges the devnode's hardware ids instead.
pub const STATUS_NO_PAD_IDENTITY: u32 = 0xC000_0182;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dualsense, dualshock4};

    /// A pad served at the hardware cadence advances its counter by one and its timestamps by the
    /// real period per report.
    #[test]
    fn sony_clock_advances_like_hardware() {
        let mut ds = [0xAAu8; 64];
        assert!(stamp_report_clock(DEVTYPE_DUALSENSE, &mut ds, 5, 8_000));
        assert_eq!(ds[7], 5);
        let le = |r: &[u8; 64], at: usize| u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
        assert_eq!(le(&ds, 12), 5, "packet sequence");
        assert_eq!(le(&ds, 28), 24_000);
        assert_eq!(le(&ds, 49), 29_900, "second timer");
        assert_eq!(
            ds[27], 0xAA,
            "the gyro/accel bytes before the timestamp stay the host's"
        );
        assert_eq!(ds[53], 0xAA, "battery byte stays the host's");

        // Every clock advances on every report, as on hardware.
        let mut next = ds;
        stamp_report_clock(DEVTYPE_DUALSENSE, &mut next, 6, 12_000);
        for at in [12, 28, 49] {
            assert!(le(&next, at) > le(&ds, at), "field at {at} did not advance");
        }

        let mut edge = [0u8; 64];
        assert!(stamp_report_clock(
            DEVTYPE_DUALSENSE_EDGE,
            &mut edge,
            256 + 3,
            4_000
        ));
        assert_eq!(edge[7], 3, "the counter wraps at a byte");

        let mut ds4 = [0u8; 64];
        ds4[7] = 0x03; // PS + touchpad click held
        assert!(stamp_report_clock(
            DEVTYPE_DUALSHOCK4,
            &mut ds4,
            64 + 2,
            16_000
        ));
        assert_eq!(
            ds4[7],
            0x03 | (2 << 2),
            "counter wraps at six bits, buttons survive"
        );
        assert_eq!(u16::from_le_bytes([ds4[10], ds4[11]]), 3_000);
        assert_eq!(ds4[34], 3_000u16 as u8);

        let mut xbox = [0x11u8; 64];
        assert!(!stamp_report_clock(DEVTYPE_XBOX, &mut xbox, 1, 4_000));
        assert_eq!(xbox, [0x11u8; 64]);
    }

    /// The driver re-serves a held Deck frame every 4 ms. Readers skip a frame whose
    /// `unPacketNum` (bytes 4..8) repeats, so every served frame needs its own.
    #[test]
    fn deck_packet_number_advances_per_served_report() {
        let mut held = [0x5Au8; 64];
        held[..4].copy_from_slice(&[0x01, 0x00, 0x09, 0x40]);
        let (mut a, mut b) = (held, held);
        assert!(stamp_report_clock(DEVTYPE_STEAMDECK, &mut a, 41, 4_000));
        assert!(stamp_report_clock(DEVTYPE_STEAMDECK, &mut b, 42, 8_000));
        assert_eq!(a[4..8], 41u32.to_le_bytes());
        assert_eq!(b[4..8], 42u32.to_le_bytes());
        assert_eq!(a[..4], held[..4], "the header stays the host's");
        assert_eq!(a[8..], held[8..], "controls stay the host's");
    }

    /// SDL and Steam merge two pads that report one serial. Every identity at every host pad
    /// index (`MAX_PADS` = 16) must be distinct; a DualSense in slot n+1 once matched an Edge in
    /// slot n.
    #[test]
    fn no_two_pads_share_a_serial() {
        let mut seen = std::collections::HashMap::new();
        for devtype in DEVTYPE_DUALSENSE..=DEVTYPE_JOYCON_RIGHT {
            for index in 0..16u8 {
                let serial = pad_serial(devtype, index);
                if let Some(prev) = seen.insert(serial.clone(), (devtype, index)) {
                    panic!("{serial} is both {prev:?} and {:?}", (devtype, index));
                }
            }
        }
        // The pairing MAC's low octet is the serial's last two hex digits.
        for devtype in [
            DEVTYPE_DUALSENSE,
            DEVTYPE_DUALSHOCK4,
            DEVTYPE_DUALSENSE_EDGE,
        ] {
            let serial = pad_serial(devtype, 3);
            let low = alloc::format!("{:02X}", ps_mac_low(devtype, 3));
            assert!(serial.ends_with(&low), "{serial} vs {low}");
        }
        assert_eq!(pad_serial(DEVTYPE_DUALSENSE, 0), "35533AD6E774");
        assert_eq!(pad_serial(DEVTYPE_DUALSHOCK4, 0), "DEADBEEF0001");
        assert_eq!(pad_serial(DEVTYPE_STEAMDECK, 2), "FVPF50460002");
        assert_eq!(pad_serial(DEVTYPE_TRITON, 12), "FVPF130212D03");
    }

    /// The driver settles its identity from the hardware ids before hidclass asks, and refuses
    /// a devnode none of them names. A token tested before a longer one it prefixes would hand
    /// the longer one's pad the wrong report descriptor.
    #[test]
    fn hardware_ids_name_exactly_one_identity() {
        for (i, (token, devtype)) in HWID_DEVTYPES.iter().enumerate() {
            for (later, _) in &HWID_DEVTYPES[i + 1..] {
                assert!(!later.starts_with(token), "{token} shadows {later}");
            }
            assert_eq!(devtype_from_hwids(token), Some(*devtype), "{token}");
            // The devnode carries synthesized USB ids after ours, as the host creates it.
            let ids = alloc::format!("{token};usb\\vid_054c&pid_0ce6&rev_0100;usb\\class_03");
            assert_eq!(devtype_from_hwids(&ids), Some(*devtype), "{ids}");
            assert!(
                identity_vid_pid(*devtype).is_some(),
                "{token} has no VID/PID"
            );
        }
        assert_eq!(
            devtype_from_hwids("root\\pf_dualsense"),
            Some(DEVTYPE_DUALSENSE)
        );
        assert_eq!(devtype_from_hwids("usb\\vid_054c&pid_0ce6"), None);
        assert_eq!(devtype_from_hwids(""), None, "a failed property query");
        assert_eq!(identity_vid_pid(DEVTYPE_JOYCON_RIGHT + 1), None);
        assert_eq!(
            identity_vid_pid(DEVTYPE_DUALSENSE_EDGE),
            Some((0x054C, 0x0DF2))
        );
        assert_eq!(identity_vid_pid(DEVTYPE_XBOX_ELITE), Some((0x045E, 0x0B22)));
    }

    /// Serves land one period apart on a fine timer, and a coarse timer restarts the schedule
    /// instead of bursting reports to catch up.
    #[test]
    fn serves_at_the_hardware_period() {
        let p = REPORT_PERIOD_US;
        assert_eq!(serve_due(0, 0, p), Some(p));
        assert_eq!(
            serve_due(2_000, p, p),
            None,
            "a 2 ms tick between slots serves nothing"
        );
        assert_eq!(
            serve_due(p + 100, p, p),
            Some(2 * p),
            "a slightly late tick keeps the grid"
        );
        assert_eq!(
            serve_due(p + 15_600, p, p),
            Some(p + 15_600 + p),
            "a coarse tick restarts it"
        );
        let sw = report_period_us(DEVTYPE_SWITCH_PRO);
        assert_eq!(sw, 8_000);
        assert_eq!(
            serve_due(sw - 2_000, sw, sw),
            None,
            "a Switch slot waits 8 ms"
        );
        assert_eq!(report_period_us(DEVTYPE_DUALSENSE), p);
        assert_eq!(
            report_period_us(DEVTYPE_8BITDO_ULTIMATE2),
            8_333,
            "SDL's Bluetooth step"
        );
    }

    /// The Pro models' µs clock and the HORIPAD's u16 one advance with real time.
    #[test]
    fn eightbitdo_and_hori_clocks() {
        let mut r = [0u8; 64];
        assert!(stamp_report_clock(DEVTYPE_8BITDO_PRO2, &mut r, 0, 70_000));
        assert_eq!(u32::from_le_bytes(r[27..31].try_into().unwrap()), 70_000);
        let mut u2 = [0u8; 64];
        assert!(!stamp_report_clock(
            DEVTYPE_8BITDO_ULTIMATE2,
            &mut u2,
            0,
            70_000
        ));
        let mut h = [0u8; 64];
        assert!(stamp_report_clock(DEVTYPE_HORIPAD_STEAM, &mut h, 0, 70_000));
        assert_eq!(u16::from_le_bytes([h[10], h[11]]), 70_000u32 as u16);
    }

    #[test]
    fn gamepad_names_and_magics_are_stable() {
        assert_eq!(xusb_boot_name(0), "Global\\pfxusb-boot-0");
        assert_eq!(pad_boot_name(2), "Global\\pfds-boot-2");
        // Lock the exact u32 magics the shipped host/drivers use.
        assert_eq!(XUSB_MAGIC, 0x5558_4650);
        assert_eq!(PAD_MAGIC, 0x5046_4453);
        // "PFBT" little-endian.
        assert_eq!(BOOT_MAGIC.to_le_bytes(), *b"PFBT");
    }

    #[test]
    fn pad_bootstrap_roundtrips_through_bytes() {
        let b = PadBootstrap {
            magic: BOOT_MAGIC,
            host_proto: GAMEPAD_PROTO_VERSION,
            driver_pid: 1234,
            driver_proto: GAMEPAD_PROTO_VERSION,
            data_handle: 0x0000_0000_0000_2a4c,
            handle_pid: 1234,
            handle_seq: 7,
        };
        let bytes = bytemuck::bytes_of(&b);
        assert_eq!(bytes.len(), 32);
        assert_eq!(*bytemuck::from_bytes::<PadBootstrap>(bytes), b);
        // Handle value rides 8-aligned at offset 16; seq trails at 28 (written last).
        assert_eq!(bytes[16..24], 0x2a4cu64.to_le_bytes());
        assert_eq!(bytes[28..32], 7u32.to_le_bytes());
    }

    /// Both wire forms (IOCTL struct and HID indexed-string) round-trip; malformed shapes refuse
    /// rather than half-parse into a pid.
    #[test]
    fn channel_proof_round_trips_in_both_wire_forms() {
        let proof = ChannelProof::new(2, 4242);
        assert_eq!(proof.magic, PROOF_MAGIC);
        assert_eq!(PROOF_MAGIC, u32::from_le_bytes(*b"PFCP"));
        assert_eq!(proof.proto, GAMEPAD_PROTO_VERSION);

        let bytes = bytemuck::bytes_of(&proof);
        assert_eq!(bytes.len(), 16);
        assert_eq!(*bytemuck::from_bytes::<ChannelProof>(bytes), proof);

        let s = proof.to_hid_string();
        assert_eq!(s, alloc::format!("PFCP:{GAMEPAD_PROTO_VERSION}:2:4242"));
        assert_eq!(ChannelProof::from_hid_string(&s), Some(proof));

        // Every malformed shape parses to None.
        for bad in [
            "",
            "PFCP",
            "PFCP:",
            "PFCP:3:0",        // truncated read
            "PFCP:3:0:4242:9", // trailing field we never mint
            "PFCP:3:0:-1",     // not a u32
            "PFCP:3:0:0x10",   // not decimal
            "PFCP:3:0: 4242",  // whitespace is not trimmed away into a valid pid
            "NOPE:3:0:4242",   // another driver answered this string index
            "pfcp:3:0:4242",   // prefix is case-sensitive
        ] {
            assert_eq!(
                ChannelProof::from_hid_string(bad),
                None,
                "malformed proof {bad:?} must not parse"
            );
        }
    }

    /// Pin each `check` refusal: foreign driver, version skew, wrong-devnode (would cross-wire pads).
    #[test]
    fn channel_proof_check_refuses_everything_it_should() {
        assert_eq!(ChannelProof::new(0, 1234).check(0), Ok(1234));
        assert_eq!(ChannelProof::new(3, 1234).check(3), Ok(1234));

        // Right shape, wrong pad: the interface lookup resolved another pad's devnode.
        assert!(ChannelProof::new(1, 1234).check(0).is_err());
        let mut foreign = ChannelProof::new(0, 1234);
        foreign.magic = 0xDEAD_BEEF;
        assert!(foreign.check(0).is_err());
        // Version skew must fail closed, not "probably compatible".
        let mut old = ChannelProof::new(0, 1234);
        old.proto = GAMEPAD_PROTO_VERSION - 1;
        assert!(old.check(0).is_err());
        // pid 0 is never a duplication target.
        assert!(ChannelProof::new(0, 0).check(0).is_err());
    }

    /// v2 driver answers no proof and a v2 host never asks — version must have moved.
    #[test]
    fn gamepad_proto_is_at_the_channel_proof_version() {
        assert_eq!(GAMEPAD_PROTO_VERSION, 3);
    }

    /// Feature-report framing: report id in byte 0, proof in 1..17, zero pad; short reads refuse.
    #[test]
    fn channel_proof_feature_report_round_trips_and_refuses_short_reads() {
        let proof = ChannelProof::new(1, 4242);
        let rep = proof
            .to_feature_report(HID_FEATURE_REPORT_CHANNEL_PROOF, 64)
            .expect("64 bytes is plenty");
        assert_eq!(rep.len(), 64);
        assert_eq!(
            rep[0], 0x85,
            "byte 0 is the report id, as every HID feature reply is"
        );
        assert!(rep[17..].iter().all(|&b| b == 0), "tail is zero padding");
        assert_eq!(ChannelProof::from_feature_report(&rep), Some(proof));

        assert!(proof.to_feature_report(0x85, 17).is_some());
        assert!(proof.to_feature_report(0x85, 16).is_none());
        // A truncated read must not be zero-extended into a pid.
        assert_eq!(ChannelProof::from_feature_report(&rep[..16]), None);
        assert_eq!(ChannelProof::from_feature_report(&[]), None);

        // Two bytes so a stray Steam command cannot collide.
        assert_eq!(DECK_PROOF_CMD.len(), 2);
        assert!(!DECK_PROOF_CMD.starts_with(&[0x83]) && !DECK_PROOF_CMD.starts_with(&[0xAE]));
        assert!(!DECK_PROOF_CMD.starts_with(&[0xEB]) && !DECK_PROOF_CMD.starts_with(&[0x8F]));
    }

    /// A PlayStation pad's USB serial is its pairing MAC, most significant octet first, so the
    /// serial and hid-playstation's `uniq` name the same pad for every identity and index.
    #[test]
    fn pairing_mac_is_the_serial() {
        let mac = |r: &[u8]| -> String {
            r[1..7]
                .iter()
                .rev()
                .map(|b| alloc::format!("{b:02X}"))
                .collect()
        };
        for index in 0..16u8 {
            for devtype in [DEVTYPE_DUALSENSE, DEVTYPE_DUALSENSE_EDGE] {
                let r = dualsense::pairing_reply(devtype, index);
                assert_eq!(r[0], 0x09);
                assert_eq!(mac(&r), pad_serial(devtype, index), "{devtype} pad {index}");
            }
            let r = dualshock4::pairing_reply(index);
            assert_eq!(r[0], 0x12);
            assert_eq!(
                mac(&r),
                pad_serial(DEVTYPE_DUALSHOCK4, index),
                "DS4 pad {index}"
            );
        }
        assert_eq!(
            dualsense::pairing_reply(DEVTYPE_DUALSENSE, 0),
            dualsense::FEATURE_PAIRING
        );
        assert_eq!(dualshock4::pairing_reply(0), dualshock4::FEATURE_PAIRING);
    }
}
