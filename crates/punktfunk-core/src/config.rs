//! Session configuration: role, FEC, shard/MTU knobs, and `Config`.

use crate::error::{PunktfunkError, Result};
use crate::packet::{MAX_DATAGRAM_BYTES, WIRE_OVERHEAD};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host = 0,
    Client = 1,
}

/// On-wire `fec_scheme` tag.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FecScheme {
    /// Classic RS, GameStream-compatible; 255 shards/block.
    Gf8 = 0,
    /// Leopard-RS; 65535 shards/block.
    Gf16 = 1,
}

impl FecScheme {
    pub fn from_u8(v: u8) -> Option<FecScheme> {
        match v {
            0 => Some(FecScheme::Gf8),
            1 => Some(FecScheme::Gf16),
            _ => None,
        }
    }

    pub fn max_total_shards(self) -> usize {
        match self {
            FecScheme::Gf8 => 255,
            FecScheme::Gf16 => u16::MAX as usize, // wire fields are u16
        }
    }
}

/// Size the host should produce on the virtual output.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Client compositor preference on [`Hello`](crate::quic::Hello); [`Welcome`](crate::quic::Welcome)
/// echoes the backend actually chosen. A concrete value is used only if that backend is
/// available now; otherwise the host auto-detects. Older peers omit the byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CompositorPref {
    #[default]
    Auto,
    Kwin,
    Wlroots,
    Mutter,
    /// Nested process; available wherever the binary is installed.
    Gamescope,
    Hyprland,
    /// The Windows virtual display, that host's only backend. A Welcome echo, never a choice.
    Windows,
}

impl CompositorPref {
    pub fn to_u8(self) -> u8 {
        match self {
            CompositorPref::Auto => 0,
            CompositorPref::Kwin => 1,
            CompositorPref::Wlroots => 2,
            CompositorPref::Mutter => 3,
            CompositorPref::Gamescope => 4,
            CompositorPref::Hyprland => 5,
            CompositorPref::Windows => 6,
        }
    }

    /// Unknown bytes decode to `Auto` so a future concrete value still lets the host decide.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => CompositorPref::Kwin,
            2 => CompositorPref::Wlroots,
            3 => CompositorPref::Mutter,
            4 => CompositorPref::Gamescope,
            5 => CompositorPref::Hyprland,
            6 => CompositorPref::Windows,
            _ => CompositorPref::Auto,
        }
    }

    /// CLI/config name. `None` on unknown so callers can error instead of silently becoming `Auto`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "detect" | "default" => CompositorPref::Auto,
            "kwin" | "kde" | "plasma" => CompositorPref::Kwin,
            "wlroots" | "sway" | "river" | "wlr" => CompositorPref::Wlroots,
            "mutter" | "gnome" => CompositorPref::Mutter,
            "gamescope" => CompositorPref::Gamescope,
            "hyprland" => CompositorPref::Hyprland,
            "windows" => CompositorPref::Windows,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CompositorPref::Auto => "auto",
            CompositorPref::Kwin => "kwin",
            CompositorPref::Wlroots => "wlroots",
            CompositorPref::Mutter => "mutter",
            CompositorPref::Gamescope => "gamescope",
            CompositorPref::Hyprland => "hyprland",
            CompositorPref::Windows => "windows",
        }
    }
}

/// Client gamepad preference on [`Hello`](crate::quic::Hello); [`Welcome`](crate::quic::Welcome)
/// echoes the backend actually chosen. `Auto` uses `PUNKTFUNK_GAMEPAD` else Xbox 360. UHID
/// backends (DualSense family, Switch Pro, Steam pads) fold if the host cannot build them.
/// Older peers omit the byte; unknown bytes decode to `Auto`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GamepadPref {
    /// `PUNKTFUNK_GAMEPAD`, else Xbox 360.
    #[default]
    Auto,
    /// Universal default: every game speaks XInput.
    Xbox360,
    /// Linux UHID DualSense (`hid-playstation`).
    DualSense,
    /// Linux: Xbox 360 uinput with One/Series VID/PID/name (XInput-identical).
    /// Windows: distinct HID `045E:02FD` through the UMDF minidriver.
    XboxOne,
    /// Linux UHID DualShock 4 (`hid-playstation`, kernel ≥ 6.2).
    DualShock4,
    /// Linux UHID classic Steam Controller (`28DE:1102`, `hid-steam`).
    /// Wire right stick drives the right pad; left-pad contact shadows the stick.
    SteamController,
    /// Steam Deck (`28DE:1205`): Linux `hid-steam` via UHID/usbip/gadget, or Windows UMDF.
    /// Steam Input re-grabs it with native glyphs when Steam is on the host.
    SteamDeck,
    /// DualSense Edge (`054C:0DF2`, `hid-playstation` ≥ 6.3 / Windows UMDF).
    /// Back paddles land on native slots instead of the fold/drop policy.
    DualSenseEdge,
    /// Linux UHID Switch Pro (`057E:2009`, `hid-nintendo` ≥ 5.16).
    SwitchPro,
    /// Wired Steam Controller 2 (`28DE:1302`). Host presents that identity and
    /// mirrors client [`RichInput::HidReport`](crate::quic::RichInput); Steam on
    /// the host consumes hidraw (`hid-steam` stops at the Deck). Linux UHID.
    SteamController2,
    /// Puck dongle (`28DE:1304`) carrying a captured SC2. Host presents the
    /// native seven-interface Puck topology, not a relabelled wired `1302`.
    SteamController2Puck,
    /// Xbox Elite Series 2. Linux: uinput `045E:0B00`, paddles on
    /// `BTN_TRIGGER_HAPPY5-8`. Windows: HID `045E:0B22` (Bluetooth, UMDF), whose
    /// report has no paddles; `xinputhid` may also claim the collection
    /// (`design/xbox-pad-windows-handoff.md`).
    XboxElite,
    /// 8BitDo Ultimate 2 Wireless in its own mode (`2DC8:6012`, Bluetooth identity): L4/R4 and
    /// both back paddles, gyro, read by SDL's and Steam's `8bitdo` driver. Linux UHID, Windows
    /// UMDF, as the three kinds below.
    EightBitDoUltimate2,
    /// 8BitDo Pro 2 in D-input (`2DC8:6003`): two back paddles, gyro.
    EightBitDoPro2,
    /// 8BitDo Pro 3 in D-input (`2DC8:6009`): L4/R4 and two back paddles, gyro.
    EightBitDoPro3,
    /// Wireless HORIPAD for Steam (`0F0D:01AB`): four rear buttons, QAM, gyro. No rumble.
    HoripadSteam,
    /// Joy-Con pair: two Bluetooth halves (`057E:2006` + `2007`) that SDL and Steam combine. SL/SR
    /// carry the paddles, each half its own gyro and motor. Linux UHID, Windows UMDF.
    JoyConPair,
    /// Switch 2 Pro Controller (`057E:2069`): GL/GR back buttons, gyro. SDL and Steam read it only
    /// through libusb, so Linux hosts attach it over usbip; Windows hosts fold it to the Switch Pro.
    Switch2Pro,
    /// Switch 2 GameCube controller (`057E:2073`): analog L/R with a click, Z, ZL, no gyro. As
    /// [`GamepadPref::Switch2Pro`].
    Switch2GameCube,
}

impl GamepadPref {
    /// Whether `RichInput::Motion` on this backend can reach the game.
    ///
    /// Answers one backend. For a particular pad use [`pad_motion_reaches`]: the
    /// host builds each virtual device from that pad's `GamepadArrival`, so
    /// [`Welcome::gamepad`](crate::quic::Welcome::gamepad) is not this pad's
    /// answer unless it declared the session default.
    ///
    /// `Auto` is `true` on purpose: unknown (old host, or not yet resolved).
    /// Suppressing on unknown would kill gyro against old DualSense hosts.
    /// Exhaustive so a new backend must pick an answer.
    pub const fn has_motion(self) -> bool {
        match self {
            GamepadPref::Auto => true,
            // No Xbox pad has a gyro in its HID contract — Elite Series 2 included.
            GamepadPref::Xbox360 | GamepadPref::XboxOne | GamepadPref::XboxElite => false,
            GamepadPref::Switch2GameCube => false,
            GamepadPref::DualSense
            | GamepadPref::DualShock4
            | GamepadPref::DualSenseEdge
            | GamepadPref::SwitchPro
            | GamepadPref::SteamController
            | GamepadPref::SteamDeck
            | GamepadPref::SteamController2
            | GamepadPref::SteamController2Puck
            | GamepadPref::EightBitDoUltimate2
            | GamepadPref::EightBitDoPro2
            | GamepadPref::EightBitDoPro3
            | GamepadPref::HoripadSteam
            | GamepadPref::JoyConPair
            | GamepadPref::Switch2Pro => true,
        }
    }

    pub const fn to_u8(self) -> u8 {
        match self {
            GamepadPref::Auto => 0,
            GamepadPref::Xbox360 => 1,
            GamepadPref::DualSense => 2,
            GamepadPref::XboxOne => 3,
            GamepadPref::DualShock4 => 4,
            GamepadPref::SteamController => 5,
            GamepadPref::SteamDeck => 6,
            GamepadPref::DualSenseEdge => 7,
            GamepadPref::SwitchPro => 8,
            GamepadPref::SteamController2 => 9,
            GamepadPref::SteamController2Puck => 10,
            GamepadPref::XboxElite => 11,
            GamepadPref::EightBitDoUltimate2 => 12,
            GamepadPref::EightBitDoPro2 => 13,
            GamepadPref::EightBitDoPro3 => 14,
            GamepadPref::HoripadSteam => 15,
            GamepadPref::JoyConPair => 16,
            GamepadPref::Switch2Pro => 17,
            GamepadPref::Switch2GameCube => 18,
        }
    }

    /// Unknown bytes decode to `Auto` so a future concrete value still lets the host decide.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => GamepadPref::Xbox360,
            2 => GamepadPref::DualSense,
            3 => GamepadPref::XboxOne,
            4 => GamepadPref::DualShock4,
            5 => GamepadPref::SteamController,
            6 => GamepadPref::SteamDeck,
            7 => GamepadPref::DualSenseEdge,
            8 => GamepadPref::SwitchPro,
            9 => GamepadPref::SteamController2,
            10 => GamepadPref::SteamController2Puck,
            11 => GamepadPref::XboxElite,
            12 => GamepadPref::EightBitDoUltimate2,
            13 => GamepadPref::EightBitDoPro2,
            14 => GamepadPref::EightBitDoPro3,
            15 => GamepadPref::HoripadSteam,
            16 => GamepadPref::JoyConPair,
            17 => GamepadPref::Switch2Pro,
            18 => GamepadPref::Switch2GameCube,
            _ => GamepadPref::Auto,
        }
    }

    /// CLI/config name. `None` on unknown so callers can error instead of silently becoming `Auto`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "default" => GamepadPref::Auto,
            "xbox" | "xbox360" | "x360" | "uinput" => GamepadPref::Xbox360,
            "dualsense" | "ds" | "ps5" => GamepadPref::DualSense,
            "xboxone" | "xbox-one" | "xone" | "xbox1" | "series" | "xboxseries" => {
                GamepadPref::XboxOne
            }
            // DualSense Edge answers to "edge", never "elite".
            "xboxelite" | "xbox-elite" | "elite" | "xboxelite2" | "elite2" => {
                GamepadPref::XboxElite
            }
            "dualshock4" | "dualshock" | "ds4" | "ps4" => GamepadPref::DualShock4,
            "steamdeck" | "steam-deck" | "deck" => GamepadPref::SteamDeck,
            "steamcontroller" | "steam-controller" | "steamcon" => GamepadPref::SteamController,
            "dualsenseedge" | "dualsense-edge" | "edge" | "dsedge" => GamepadPref::DualSenseEdge,
            "switchpro" | "switch-pro" | "switch" | "procontroller" | "pro-controller" => {
                GamepadPref::SwitchPro
            }
            "steamcontroller2" | "steam-controller-2" | "steamcon2" | "sc2" | "ibex" => {
                GamepadPref::SteamController2
            }
            "steamcontroller2puck" | "steam-controller-2-puck" | "sc2puck" | "ibexpuck" => {
                GamepadPref::SteamController2Puck
            }
            "8bitdoultimate2" | "8bitdo-ultimate-2" | "ultimate2" => {
                GamepadPref::EightBitDoUltimate2
            }
            "8bitdopro2" | "8bitdo-pro-2" | "pro2" => GamepadPref::EightBitDoPro2,
            "8bitdopro3" | "8bitdo-pro-3" | "pro3" => GamepadPref::EightBitDoPro3,
            "horipadsteam" | "horipad-steam" | "hori" => GamepadPref::HoripadSteam,
            "joyconpair" | "joycon-pair" | "joycons" => GamepadPref::JoyConPair,
            "switch2pro" | "switch2-pro" | "procontroller2" => GamepadPref::Switch2Pro,
            "switch2gamecube" | "switch2-gamecube" | "gamecube" => GamepadPref::Switch2GameCube,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GamepadPref::Auto => "auto",
            GamepadPref::Xbox360 => "xbox360",
            GamepadPref::DualSense => "dualsense",
            GamepadPref::XboxOne => "xboxone",
            GamepadPref::DualShock4 => "dualshock4",
            GamepadPref::SteamController => "steamcontroller",
            GamepadPref::SteamDeck => "steamdeck",
            GamepadPref::DualSenseEdge => "dualsenseedge",
            GamepadPref::SwitchPro => "switchpro",
            GamepadPref::SteamController2 => "steamcontroller2",
            GamepadPref::SteamController2Puck => "steamcontroller2puck",
            GamepadPref::XboxElite => "xboxelite",
            GamepadPref::EightBitDoUltimate2 => "8bitdoultimate2",
            GamepadPref::EightBitDoPro2 => "8bitdopro2",
            GamepadPref::EightBitDoPro3 => "8bitdopro3",
            GamepadPref::HoripadSteam => "horipadsteam",
            GamepadPref::JoyConPair => "joyconpair",
            GamepadPref::Switch2Pro => "switch2pro",
            GamepadPref::Switch2GameCube => "switch2gamecube",
        }
    }
}

/// Whether motion for one pad can reach the game.
///
/// `declared` is the pad's [`InputKind::GamepadArrival`](crate::input::InputKind::GamepadArrival),
/// `asked` is the Hello session default, `resolved` is
/// [`Welcome::gamepad`](crate::quic::Welcome::gamepad).
///
/// The host builds each virtual device from that pad's arrival and uses the
/// session default only when the pad never declared. It also folds backends it
/// cannot build (Switch Pro on Windows, any UHID host without `/dev/uhid`) to
/// Xbox 360, dropping the motion plane — nothing local can predict that. The
/// echo is one sample of that fold for the Hello kind, so it is authoritative
/// when `declared == asked`.
///
/// Otherwise trust the declaration. Wrong the other way would silently kill a
/// working gyro; wasted datagrams are the cheaper miss.
pub const fn pad_motion_reaches(
    declared: GamepadPref,
    asked: GamepadPref,
    resolved: GamepadPref,
) -> bool {
    // PartialEq::eq is not const; u8 compare is.
    if declared.to_u8() == asked.to_u8() {
        resolved.has_motion()
    } else {
        declared.has_motion()
    }
}

/// Per-block FEC. Recovery is GameStream's `m = ceil(k * fec_percent / 100)`, floored at
/// `MIN_RECOVERY_SHARDS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FecConfig {
    pub scheme: FecScheme,
    /// Percent of data shards. 0 disables FEC.
    pub fec_percent: u8,
    /// Data shards per block; larger frames split. GF(2⁸) is 255 total, so ≤ ~200 for `Gf8`.
    pub max_data_per_block: u16,
}

/// Parity floor per block while FEC is on. A burst is N packets whatever the frame size,
/// and a percent gives a small frame one shard or none; Moonlight's
/// `minRequiredFecPackets` floor is the same 2. Not a `FecConfig` field: that struct is
/// on the wire in `Welcome`, and the receiver sizes blocks from the packet header anyway.
pub(crate) const MIN_RECOVERY_SHARDS: usize = 2;

/// Parity shards a percent buys for a block of `data_shards`. The adaptive-FEC
/// target is sized against this, so there is one definition of it.
pub(crate) fn recovery_shards(data_shards: usize, fec_percent: u8) -> usize {
    if fec_percent == 0 || data_shards == 0 {
        return 0;
    }
    (data_shards * fec_percent as usize)
        .div_ceil(100)
        .max(MIN_RECOVERY_SHARDS)
}

impl FecConfig {
    pub fn recovery_for(&self, data_shards: usize) -> usize {
        recovery_shards(data_shards, self.fec_percent)
    }
}

/// Header and tag still fit in [`MAX_DATAGRAM_BYTES`].
pub const fn max_shard_payload() -> usize {
    MAX_DATAGRAM_BYTES - WIRE_OVERHEAD
}

/// Bytes of a 1500-MTU datagram the default shards leave empty. Growing into them
/// changes every host's `Welcome::shard_payload`.
const MTU1500_UNUSED: usize = 18;

/// Default shard payload whose sealed IPv4/UDP datagram fits a 1500-byte MTU:
/// `1500 − 20 − 8 − WIRE_OVERHEAD − MTU1500_UNUSED` = 1408. Past the 1426 fit the
/// kernel IP-fragments every video datagram; either fragment lost drops the datagram.
pub const fn mtu1500_shard_payload() -> usize {
    let p = 1500 - 20 - 8 - WIRE_OVERHEAD - MTU1500_UNUSED;
    p - p % 2 // FEC requires even shards
}

/// IPv6 sibling of [`mtu1500_shard_payload`]: `1500 − 40 − 8 − WIRE_OVERHEAD −
/// MTU1500_UNUSED` = 1388. IPv6 routers never fragment; an oversized datagram is
/// ICMPv6 Packet-Too-Big or a silent blackhole, not a degrade.
pub const fn mtu1500_shard_payload_v6() -> usize {
    let p = 1500 - 40 - 8 - WIRE_OVERHEAD - MTU1500_UNUSED;
    p - p % 2 // FEC requires even shards
}

/// MTU-safe shard payload for `peer`. Genuine IPv6 uses the v6 size; IPv4 and
/// IPv4-mapped IPv6 (`::ffff:…`, dual-stack `[::]` reporting a v4 client) use v4 —
/// those ride IPv4 on the wire. Hosts send this as `Welcome::shard_payload`.
pub fn mtu1500_shard_payload_for(peer: core::net::IpAddr) -> usize {
    match peer {
        core::net::IpAddr::V4(_) => mtu1500_shard_payload(),
        core::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => mtu1500_shard_payload(),
        core::net::IpAddr::V6(_) => mtu1500_shard_payload_v6(),
    }
}

/// Floor for a negotiated `shard_payload`. Below this the path cannot carry QUIC
/// either (QUIC's minimum UDP payload is 1200), so shrinking further buys nothing.
/// 512 is even and well under every real path.
pub const MIN_SHARD_PAYLOAD: usize = 512;

pub const fn sealed_datagram_bytes(shard_payload: usize) -> usize {
    shard_payload + WIRE_OVERHEAD
}

/// The 1500-MTU IPv4 UDP payload, 1472, which the sealed [`mtu1500_shard_payload`]
/// fits. Also the QUIC MTU-discovery probe ceiling: settled-at means the path carries
/// a 1500-byte wire; settled-below means it may not. quinn's stock 1452 ceiling
/// cannot make that discrimination.
pub const fn video_datagram_udp_ceiling() -> usize {
    1500 - 20 - 8
}

/// Largest even shard payload that fits `udp_budget` (what QUIC MTU discovery
/// measures). Clamped to [`mtu1500_shard_payload_for`] so a generous budget never
/// grows past the family 1500 default; floored at [`MIN_SHARD_PAYLOAD`].
pub fn shard_payload_for_udp_budget(udp_budget: usize, peer: core::net::IpAddr) -> usize {
    let p = udp_budget.saturating_sub(WIRE_OVERHEAD);
    let p = p - p % 2; // FEC requires even shards
    p.clamp(MIN_SHARD_PAYLOAD, mtu1500_shard_payload_for(peer))
}

/// IP+UDP headers between on-wire MTU and UDP payload: 20+8 IPv4 (and mapped), 40+8 IPv6.
fn ip_udp_overhead(peer: core::net::IpAddr) -> usize {
    match peer {
        core::net::IpAddr::V4(_) => 28,
        core::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => 28,
        core::net::IpAddr::V6(_) => 48,
    }
}

/// [`shard_payload_for_udp_budget`] for an operator on-wire IP MTU (`ip link` / `netsh`):
/// subtract family IP+UDP headers first.
pub fn shard_payload_for_wire_mtu(wire_mtu: usize, peer: core::net::IpAddr) -> usize {
    shard_payload_for_udp_budget(wire_mtu.saturating_sub(ip_udp_overhead(peer)), peer)
}

/// Operator jumbo opt-in (`design/shard-payload-reneg.md`): target on-wire IP MTU,
/// or `None` = never probe or grow above the 1500 default. `PUNKTFUNK_WIRE_MTU`
/// above 1500 is that number; `PUNKTFUNK_JUMBO=1` is the 9000 profile. Sessions
/// still start at the family default; growth is ACK-gated toward a client that
/// advertised [`max_shard_payload`] headroom.
pub fn jumbo_wire_mtu() -> Option<usize> {
    if let Ok(v) = std::env::var("PUNKTFUNK_WIRE_MTU") {
        if let Ok(mtu) = v.trim().parse::<usize>() {
            if mtu > 1500 {
                return Some(mtu);
            }
        }
    }
    match std::env::var("PUNKTFUNK_JUMBO") {
        Ok(v) if v.trim() == "1" => Some(9000),
        _ => None,
    }
}

/// Jumbo sibling of [`shard_payload_for_wire_mtu`]: clamped to [`max_shard_payload`]
/// (receive ceiling), not the family 1500 default.
pub fn jumbo_shard_payload_for(wire_mtu: usize, peer: core::net::IpAddr) -> usize {
    let p = wire_mtu
        .saturating_sub(ip_udp_overhead(peer))
        .saturating_sub(WIRE_OVERHEAD);
    let p = p - p % 2; // FEC requires even shards
    p.clamp(MIN_SHARD_PAYLOAD, max_shard_payload())
}

/// Inputs to construct a [`Session`](crate::session::Session). Keys are not among them:
/// they come from the connection's exporter ([`crate::session::MediaV2`]).
#[derive(Clone, Debug)]
pub struct Config {
    pub role: Role,
    pub fec: FecConfig,
    /// Even, and ≤ [`max_shard_payload`].
    pub shard_payload: usize,
    /// Reassembler cap; bounds memory against hostile/corrupt headers.
    pub max_frame_bytes: usize,
    /// Test hook: drop one of every N loopback packets. 0 = lossless.
    pub loopback_drop_period: u32,
}

impl Config {
    /// Keeps receive-side allocations bounded.
    pub fn validate(&self) -> Result<()> {
        if self.shard_payload == 0 || self.shard_payload % 2 != 0 {
            return Err(PunktfunkError::InvalidArg(
                "shard_payload must be even and > 0",
            ));
        }
        if self.shard_payload > max_shard_payload() {
            return Err(PunktfunkError::InvalidArg(
                "shard_payload too large to fit a datagram (header + crypto overhead)",
            ));
        }
        // Client only: Welcome.shard_payload sets the reassembler floor. A hostile
        // 2-byte Welcome would otherwise be accepted for the whole session. Hosts
        // derive locally (helpers already clamp); a hand-set host below the floor stays legal.
        if self.role == Role::Client && self.shard_payload < MIN_SHARD_PAYLOAD {
            return Err(PunktfunkError::InvalidArg(
                "negotiated shard_payload below MIN_SHARD_PAYLOAD",
            ));
        }
        if self.fec.max_data_per_block == 0 {
            return Err(PunktfunkError::InvalidArg("max_data_per_block must be > 0"));
        }
        // Per-block total must fit the field ceiling and the u16 wire fields.
        let k = self.fec.max_data_per_block as usize;
        let total = k + self.fec.recovery_for(k);
        if total > self.fec.scheme.max_total_shards() {
            return Err(PunktfunkError::InvalidArg(
                "max_data_per_block + recovery exceeds the FEC scheme's shard ceiling",
            ));
        }
        if self.max_frame_bytes == 0 {
            return Err(PunktfunkError::InvalidArg("max_frame_bytes must be > 0"));
        }
        // Block count is a u16 on the wire.
        let total_data = self.max_frame_bytes.div_ceil(self.shard_payload).max(1);
        let max_blocks = total_data.div_ceil(k).max(1);
        if max_blocks > u16::MAX as usize {
            return Err(PunktfunkError::InvalidArg(
                "max_frame_bytes too large for this shard/block configuration (block count overflows u16)",
            ));
        }
        Ok(())
    }

    /// GF(2⁸), 15% FEC, 1024-byte shards, 64 MiB frame cap. A session's [`crate::quic::Welcome`]
    /// overrides the FEC and shard size it negotiated.
    pub fn defaults(role: Role) -> Self {
        Config {
            role,
            fec: FecConfig {
                scheme: FecScheme::Gf8,
                fec_percent: 15,
                max_data_per_block: 200,
            },
            shard_payload: 1024,
            max_frame_bytes: 64 * 1024 * 1024,
            loopback_drop_period: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Client `shard_payload` comes from Welcome and is the reassembler floor;
    /// a sub-floor value must fail rather than lower that floor.
    #[test]
    fn rejects_negotiated_shard_payload_below_the_floor() {
        let mut c = Config::defaults(Role::Client);
        c.shard_payload = 2;
        assert!(c.validate().is_err());
        c.shard_payload = MIN_SHARD_PAYLOAD - 2;
        assert!(c.validate().is_err());
        c.shard_payload = MIN_SHARD_PAYLOAD;
        assert!(c.validate().is_ok());
        assert_eq!(
            crate::packet::ReassemblerLimits::from_config(&c).min_shard_bytes,
            MIN_SHARD_PAYLOAD
        );
    }

    #[test]
    fn rejects_oversized_shard_payload() {
        let mut c = Config::defaults(Role::Host);
        c.shard_payload = max_shard_payload() + 2; // even, but won't fit a datagram
        assert!(c.validate().is_err());
    }

    /// Pin 1500-MTU IPv4 math: the sealed default and [`MTU1500_UNUSED`] fill 1472
    /// (`1500 − 20 − 8`) exactly.
    #[test]
    fn mtu1500_shard_payload_never_fragments() {
        let p = mtu1500_shard_payload();
        assert_eq!(p % 2, 0, "FEC requires even shards");
        assert!(p <= max_shard_payload());
        assert_eq!(p, 1408);
        assert_eq!(sealed_datagram_bytes(p) + MTU1500_UNUSED, 1472);
    }

    /// Pin IPv6 math: the sealed default and [`MTU1500_UNUSED`] fill 1452
    /// (`1500 − 40 − 8`) exactly. v6 routers do not fragment, so overshoot blackholes.
    #[test]
    fn mtu1500_shard_payload_v6_never_blackholes() {
        let p = mtu1500_shard_payload_v6();
        assert_eq!(p % 2, 0, "FEC requires even shards");
        assert!(p <= max_shard_payload());
        assert_eq!(p, 1388);
        assert_eq!(sealed_datagram_bytes(p) + MTU1500_UNUSED, 1452);
    }

    /// QUIC MTU discovery probes to the IPv4 UDP payload, which the sealed default fits.
    #[test]
    fn video_datagram_ceiling_fits_the_sealed_default() {
        assert_eq!(video_datagram_udp_ceiling(), 1472);
        assert!(sealed_datagram_bytes(mtu1500_shard_payload()) <= video_datagram_udp_ceiling());
    }

    /// A sealed datagram is its shard, the v2 header and the AEAD tag.
    #[test]
    fn sealed_datagram_is_shard_plus_header_and_tag() {
        assert_eq!(WIRE_OVERHEAD, 46);
        assert_eq!(sealed_datagram_bytes(1000), 1046);
        assert_eq!(max_shard_payload() + WIRE_OVERHEAD, MAX_DATAGRAM_BYTES);
    }

    /// Even, fits the budget, clamped to the family default and [`MIN_SHARD_PAYLOAD`].
    #[test]
    fn shard_payload_for_udp_budget_math() {
        use core::net::IpAddr;
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        let v6: IpAddr = "fd00::50".parse().unwrap();
        assert_eq!(
            shard_payload_for_udp_budget(video_datagram_udp_ceiling(), v4),
            mtu1500_shard_payload()
        );
        // 1280-byte tunnel MTU: sealed result must fit, stay even.
        let p = shard_payload_for_udp_budget(1280, v4);
        assert_eq!(p % 2, 0);
        assert!(sealed_datagram_bytes(p) <= 1280);
        assert!(sealed_datagram_bytes(p + 2) > 1280, "not maximal");
        // Odd budgets round down to even shards.
        assert_eq!(shard_payload_for_udp_budget(1281, v4) % 2, 0);
        // Generous budget must not grow past the family default.
        assert_eq!(
            shard_payload_for_udp_budget(9000, v4),
            mtu1500_shard_payload()
        );
        assert_eq!(
            shard_payload_for_udp_budget(9000, v6),
            mtu1500_shard_payload_v6()
        );
        assert_eq!(shard_payload_for_udp_budget(100, v4), MIN_SHARD_PAYLOAD);
    }

    /// Subtracts the right IP+UDP header per family; 1500 reproduces the defaults.
    #[test]
    fn shard_payload_for_wire_mtu_math() {
        use core::net::IpAddr;
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        let v6: IpAddr = "fd00::50".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.50".parse().unwrap();
        assert_eq!(
            shard_payload_for_wire_mtu(1500, v4),
            mtu1500_shard_payload()
        );
        assert_eq!(
            shard_payload_for_wire_mtu(1500, mapped),
            mtu1500_shard_payload()
        );
        assert_eq!(
            shard_payload_for_wire_mtu(1500, v6),
            mtu1500_shard_payload_v6()
        );
        // 1280 wire − 28 − 46 = 1206 (v4); − 48 − 46 = 1186 (v6).
        assert_eq!(shard_payload_for_wire_mtu(1280, v4), 1206);
        assert_eq!(shard_payload_for_wire_mtu(1280, v6), 1186);
    }

    /// Jumbo grow-target: even, sealed fits the wire, clamped to [`max_shard_payload`].
    #[test]
    fn jumbo_shard_payload_math() {
        use core::net::IpAddr;
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        let v6: IpAddr = "fd00::50".parse().unwrap();
        // 9000 − 28 − 46 = 8926 (v4); 9000 − 48 − 46 = 8906 (v6).
        assert_eq!(jumbo_shard_payload_for(9000, v4), 8926);
        assert_eq!(sealed_datagram_bytes(8926), 8972);
        assert!(sealed_datagram_bytes(8926) <= MAX_DATAGRAM_BYTES);
        assert_eq!(jumbo_shard_payload_for(9000, v6), 8906);
        // Oversize clamps to the receive ceiling; degenerate floors.
        assert_eq!(jumbo_shard_payload_for(64_000, v4), max_shard_payload());
        let p = jumbo_shard_payload_for(4000, v4);
        assert_eq!(p % 2, 0);
        assert!(sealed_datagram_bytes(p) <= 4000 - 28);
        assert_eq!(jumbo_shard_payload_for(100, v4), MIN_SHARD_PAYLOAD);
    }

    /// Genuine v6 gets the v6 size; v4 and IPv4-mapped v6 (`[::]` reporting a v4 client) keep v4.
    #[test]
    fn shard_payload_follows_peer_family() {
        use core::net::IpAddr;
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        let v6: IpAddr = "fd00::50".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.50".parse().unwrap();
        assert_eq!(mtu1500_shard_payload_for(v4), mtu1500_shard_payload());
        assert_eq!(mtu1500_shard_payload_for(mapped), mtu1500_shard_payload());
        assert_eq!(mtu1500_shard_payload_for(v6), mtu1500_shard_payload_v6());
    }

    #[test]
    fn rejects_block_exceeding_scheme_ceiling() {
        let mut c = Config::defaults(Role::Host); // Gf8, ceiling 255
        c.fec.max_data_per_block = 250;
        c.fec.fec_percent = 15; // 250 + ceil(250*15/100) = 288 > 255
        assert!(c.validate().is_err());
    }

    #[test]
    fn gamepad_pref_steam_roundtrip() {
        use GamepadPref::*;
        // Unknown byte still degrades to Auto.
        for (p, b) in [(SteamController, 5u8), (SteamDeck, 6)] {
            assert_eq!(p.to_u8(), b);
            assert_eq!(GamepadPref::from_u8(b), p);
        }
        assert_eq!(GamepadPref::from_u8(99), Auto);
        assert_eq!(GamepadPref::from_name("steamdeck"), Some(SteamDeck));
        assert_eq!(GamepadPref::from_name("deck"), Some(SteamDeck));
        assert_eq!(
            GamepadPref::from_name("steamcontroller"),
            Some(SteamController)
        );
        assert_eq!(SteamDeck.as_str(), "steamdeck");
        assert_eq!(
            GamepadPref::from_name(SteamController.as_str()),
            Some(SteamController)
        );
    }

    #[test]
    fn compositor_pref_wire_and_names() {
        for p in [
            CompositorPref::Auto,
            CompositorPref::Kwin,
            CompositorPref::Wlroots,
            CompositorPref::Mutter,
            CompositorPref::Gamescope,
            CompositorPref::Hyprland,
            CompositorPref::Windows,
        ] {
            assert_eq!(CompositorPref::from_u8(p.to_u8()), p);
            assert_eq!(CompositorPref::from_name(p.as_str()), Some(p));
        }
        assert_eq!(CompositorPref::from_name("KDE"), Some(CompositorPref::Kwin));
        assert_eq!(
            CompositorPref::from_name("sway"),
            Some(CompositorPref::Wlroots)
        );
        assert_eq!(CompositorPref::from_name("nope"), None);
        // Unknown wire byte degrades to Auto.
        assert_eq!(CompositorPref::from_u8(200), CompositorPref::Auto);
    }

    /// False negative kills working gyro; false positive streams ~250 Hz into a void.
    #[test]
    fn pads_without_a_gyro_lack_a_motion_plane() {
        for p in [
            GamepadPref::Xbox360,
            GamepadPref::XboxOne,
            GamepadPref::XboxElite,
            GamepadPref::Switch2GameCube,
        ] {
            assert!(
                !p.has_motion(),
                "{} should have no motion plane",
                p.as_str()
            );
        }
        for p in [
            GamepadPref::DualSense,
            GamepadPref::DualShock4,
            GamepadPref::DualSenseEdge,
            GamepadPref::SwitchPro,
            GamepadPref::SteamController,
            GamepadPref::SteamDeck,
            GamepadPref::SteamController2,
            GamepadPref::SteamController2Puck,
            GamepadPref::EightBitDoUltimate2,
            GamepadPref::EightBitDoPro2,
            GamepadPref::EightBitDoPro3,
            GamepadPref::HoripadSteam,
            GamepadPref::JoyConPair,
            GamepadPref::Switch2Pro,
        ] {
            assert!(p.has_motion(), "{} should carry motion", p.as_str());
        }
        // Unknown must not suppress: an old host may have resolved DualSense.
        assert!(GamepadPref::Auto.has_motion());
    }

    /// Per-pad, not per-session: which of declared / asked / resolved decides each row.
    #[test]
    fn motion_reach_is_answered_per_pad_not_per_session() {
        use GamepadPref::*;
        // Mixed pads under Auto: Hello/echo is Xbox, but this pad declared DualSense.
        // A session-level check would kill a gyro that works.
        assert!(pad_motion_reaches(DualSense, Xbox360, Xbox360));
        // The pad that declared Xbox still has no motion plane.
        assert!(!pad_motion_reaches(Xbox360, Xbox360, Xbox360));

        // Generic pad under Auto: detection lands on Xbox 360, no motion plane.
        assert!(!pad_motion_reaches(Xbox360, Xbox360, Xbox360));

        // Switch Pro on Windows folds to Xbox 360. declared == asked, so the echo catches it.
        assert!(!pad_motion_reaches(SwitchPro, SwitchPro, Xbox360));
        assert!(pad_motion_reaches(SwitchPro, SwitchPro, SwitchPro));

        // DualSense on a host with no usable /dev/uhid folds the same way.
        assert!(!pad_motion_reaches(DualSense, DualSense, Xbox360));

        // Hello asked Auto; a later pad is judged on its own declaration.
        assert!(pad_motion_reaches(DualSense, Auto, Xbox360));
        assert!(!pad_motion_reaches(Xbox360, Auto, DualSense));

        // Old host echoes Auto; must not suppress (it may have resolved DualSense).
        assert!(pad_motion_reaches(DualSense, DualSense, Auto));
        assert!(!pad_motion_reaches(Xbox360, DualSense, Auto));
    }

    #[test]
    fn gamepad_pref_wire_and_names() {
        for p in [
            GamepadPref::Auto,
            GamepadPref::Xbox360,
            GamepadPref::DualSense,
            GamepadPref::XboxOne,
            GamepadPref::DualShock4,
            GamepadPref::SteamController,
            GamepadPref::SteamDeck,
            GamepadPref::DualSenseEdge,
            GamepadPref::SwitchPro,
            GamepadPref::SteamController2,
            GamepadPref::SteamController2Puck,
            GamepadPref::XboxElite,
            GamepadPref::EightBitDoUltimate2,
            GamepadPref::EightBitDoPro2,
            GamepadPref::EightBitDoPro3,
            GamepadPref::HoripadSteam,
            GamepadPref::JoyConPair,
            GamepadPref::Switch2Pro,
            GamepadPref::Switch2GameCube,
        ] {
            assert_eq!(GamepadPref::from_u8(p.to_u8()), p);
            assert_eq!(GamepadPref::from_name(p.as_str()), Some(p));
        }
        // Bytes 0..=18 are assigned and pinned; older peers may know only a prefix.
        for (v, p) in [
            (0, GamepadPref::Auto),
            (1, GamepadPref::Xbox360),
            (2, GamepadPref::DualSense),
            (3, GamepadPref::XboxOne),
            (4, GamepadPref::DualShock4),
            (5, GamepadPref::SteamController),
            (6, GamepadPref::SteamDeck),
            (7, GamepadPref::DualSenseEdge),
            (8, GamepadPref::SwitchPro),
            (9, GamepadPref::SteamController2),
            (10, GamepadPref::SteamController2Puck),
            (11, GamepadPref::XboxElite),
            (12, GamepadPref::EightBitDoUltimate2),
            (13, GamepadPref::EightBitDoPro2),
            (14, GamepadPref::EightBitDoPro3),
            (15, GamepadPref::HoripadSteam),
            (16, GamepadPref::JoyConPair),
            (17, GamepadPref::Switch2Pro),
            (18, GamepadPref::Switch2GameCube),
        ] {
            assert_eq!(p.to_u8(), v);
            assert_eq!(GamepadPref::from_u8(v), p);
        }
        // Next unassigned byte degrades to Auto; assigning it later must update this.
        assert_eq!(GamepadPref::from_u8(19), GamepadPref::Auto);
        assert_eq!(GamepadPref::from_name("PS5"), Some(GamepadPref::DualSense));
        assert_eq!(GamepadPref::from_name("x360"), Some(GamepadPref::Xbox360));
        assert_eq!(GamepadPref::from_name("ps4"), Some(GamepadPref::DualShock4));
        assert_eq!(GamepadPref::from_name("DS4"), Some(GamepadPref::DualShock4));
        assert_eq!(
            GamepadPref::from_name("edge"),
            Some(GamepadPref::DualSenseEdge)
        );
        assert_eq!(
            GamepadPref::from_name("Switch-Pro"),
            Some(GamepadPref::SwitchPro)
        );
        assert_eq!(
            GamepadPref::from_name("ibex"),
            Some(GamepadPref::SteamController2)
        );
        assert_eq!(
            GamepadPref::from_name("sc2"),
            Some(GamepadPref::SteamController2)
        );
        assert_eq!(
            GamepadPref::from_name("sc2puck"),
            Some(GamepadPref::SteamController2Puck)
        );
        assert_eq!(
            GamepadPref::from_name("xbox-one"),
            Some(GamepadPref::XboxOne)
        );
        assert_eq!(GamepadPref::from_name("series"), Some(GamepadPref::XboxOne));
        // "edge" is DualSense Edge, never Elite.
        assert_eq!(
            GamepadPref::from_name("Elite"),
            Some(GamepadPref::XboxElite)
        );
        assert_eq!(
            GamepadPref::from_name("xbox-elite"),
            Some(GamepadPref::XboxElite)
        );
        assert_eq!(
            GamepadPref::from_name("elite2"),
            Some(GamepadPref::XboxElite)
        );
        assert_eq!(
            GamepadPref::from_name("edge"),
            Some(GamepadPref::DualSenseEdge)
        );
        assert_eq!(GamepadPref::from_name("nope"), None);
        // Unknown wire byte degrades to Auto.
        assert_eq!(GamepadPref::from_u8(200), GamepadPref::Auto);
    }

    /// The cross-language contract; the Swift `GamepadType` replays the same file.
    #[test]
    fn gamepad_kind_vectors() {
        let raw = include_str!("../../../clients/shared/gamepad-kind-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let kinds = file["kinds"].as_array().expect("kinds");
        for (i, row) in kinds.iter().enumerate() {
            let value = row["value"].as_u64().unwrap();
            assert_eq!(value, i as u64, "rows run 0, 1, 2… with no gap");
            let p = GamepadPref::from_u8(value as u8);
            assert_eq!(u64::from(p.to_u8()), value, "{row}");
            assert_eq!(p.as_str(), row["name"].as_str().unwrap(), "{row}");
            assert_eq!(GamepadPref::from_name(p.as_str()), Some(p), "{row}");
            assert_eq!(
                p.has_motion(),
                row["has_motion"].as_bool().unwrap(),
                "{row}"
            );
            for alias in row["aliases"].as_array().unwrap() {
                let alias = alias.as_str().unwrap();
                assert_eq!(GamepadPref::from_name(alias), Some(p), "{alias}");
                let loud = format!(" {} ", alias.to_ascii_uppercase());
                assert_eq!(GamepadPref::from_name(&loud), Some(p), "{loud}");
            }
        }
        // Every byte past the last row is unassigned, so a new kind needs a row.
        for v in kinds.len()..=255 {
            assert_eq!(GamepadPref::from_u8(v as u8), GamepadPref::Auto, "byte {v}");
        }
        for name in file["rejected_names"].as_array().expect("rejected_names") {
            assert_eq!(
                GamepadPref::from_name(name.as_str().unwrap()),
                None,
                "{name}"
            );
        }
        let kind = |v: &serde_json::Value| GamepadPref::from_name(v.as_str().unwrap()).unwrap();
        for row in file["motion_reaches"].as_array().expect("motion_reaches") {
            let got = pad_motion_reaches(
                kind(&row["declared"]),
                kind(&row["asked"]),
                kind(&row["resolved"]),
            );
            assert_eq!(got, row["want"].as_bool().unwrap(), "{}", row["why"]);
        }
    }
}
