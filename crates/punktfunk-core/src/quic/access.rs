//! Per-client access grants: `u32` bitmask shared by the wire and the host
//! trust store (`design/per-client-access.md`).
//!
//! [`Welcome`](super::Welcome) and [`AccessUpdate`](super::AccessUpdate) carry
//! the same value as the registry — no translation. Reserved bits must be
//! zero: the management API rejects unknown bits; hosts never emit them.
//! An omitted mask is [`GRANT_ALL`].
//!
//! [`classify`] is the input-plane table: exhaustive [`InputKind`] →
//! [`GrantClass`], no wildcard. Clipboard, mic, launch, power, and manage-games
//! are plane and route gates, not `0xC8` events.
//!
//! Tests in this file pin the bit layout, presets, legacy-full read, and
//! the classifier table.

use crate::input::InputKind;

// Literals, not `1 << n` or ORs: cbindgen copies the spelling into the header, and Swift
// imports a `#define` only when it is a plain value.
/// DualSense `0xCC`, pad-audio, rumble, and virtual-pad creation (no bit, no uinput node).
pub const GRANT_GAMEPAD: u32 = 0x01;
/// Mouse, scroll, touch, and the pen plane.
pub const GRANT_POINTER: u32 = 0x02;
/// Key down/up and IME-committed text.
pub const GRANT_KEYBOARD: u32 = 0x04;
/// Clipboard coordinator. ANDed with the operator clipboard policy; never overrides it.
pub const GRANT_CLIPBOARD: u32 = 0x08;
/// Mic datagram plane and the per-session mic-service attach.
pub const GRANT_MIC: u32 = 0x10;
/// `Hello.launch` resolution.
pub const GRANT_LAUNCH: u32 = 0x20;
/// `power.*` (sleep/reboot/shutdown) on the mgmt cert lane (`design/host-actions.md`).
/// Not a datagram; [`classify`] is untouched. Machine power only — never plugin actions.
pub const GRANT_POWER: u32 = 0x40;
/// Pause and remove a title's download on the mgmt cert lane (`design/plugin-downloads.md`).
/// Starting one is [`GRANT_LAUNCH`]: launching a missing title installs it anyway.
pub const GRANT_MANAGE_GAMES: u32 = 0x80;

/// An omitted Welcome or registry mask reads as this: every bit above.
pub const GRANT_ALL: u32 = 0xFF;

/// Stored "Full control" before [`GRANT_POWER`]. [`normalize_legacy_full`] lifts it.
pub const GRANT_ALL_PRE_POWER: u32 = 0x3F;
/// Stored "Full control" before [`GRANT_MANAGE_GAMES`]. [`normalize_legacy_full`] lifts it.
pub const GRANT_ALL_PRE_MANAGE: u32 = 0x7F;

/// Exact [`GRANT_ALL_PRE_POWER`] or [`GRANT_ALL_PRE_MANAGE`] → [`GRANT_ALL`]. Other masks
/// pass through. Those stored Fulls already have `KEYBOARD`+`POINTER` (the desktop's power
/// menu, the store's own uninstall), so "everything except the new bit" is not expressible.
pub fn normalize_legacy_full(mask: u32) -> u32 {
    if mask == GRANT_ALL_PRE_POWER || mask == GRANT_ALL_PRE_MANAGE {
        GRANT_ALL
    } else {
        mask
    }
}

/// The management API rejects these; it never silently clears unknown bits
/// (that would grant less than the caller asked).
pub const GRANT_RESERVED: u32 = !GRANT_ALL;

/// UI preset "Full control".
pub const GRANT_PRESET_FULL: u32 = GRANT_ALL;
/// UI preset "Controller only". No `LAUNCH` — the owner picks what runs.
pub const GRANT_PRESET_CONTROLLER_ONLY: u32 = GRANT_GAMEPAD;
/// UI preset "View only" — the spectator sends nothing.
pub const GRANT_PRESET_VIEW_ONLY: u32 = 0;

/// [`classify`] covers `0xC8` events; Clipboard/Mic/Launch/Power/ManageGames name
/// the plane and route gates so drop counters share this vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantClass {
    Gamepad,
    Pointer,
    Keyboard,
    Clipboard,
    Mic,
    Launch,
    /// `power.*` on the mgmt cert lane — never an input event.
    Power,
    /// Download pause and removal on the mgmt cert lane — never an input event.
    ManageGames,
}

impl GrantClass {
    /// Every class, in bit order. Tables indexed by [`GrantClass::bit`] size from this.
    ///
    /// cbindgen:ignore
    pub const ALL: [GrantClass; 8] = [
        Self::Gamepad,
        Self::Pointer,
        Self::Keyboard,
        Self::Clipboard,
        Self::Mic,
        Self::Launch,
        Self::Power,
        Self::ManageGames,
    ];

    pub fn bit(self) -> u32 {
        match self {
            Self::Gamepad => GRANT_GAMEPAD,
            Self::Pointer => GRANT_POINTER,
            Self::Keyboard => GRANT_KEYBOARD,
            Self::Clipboard => GRANT_CLIPBOARD,
            Self::Mic => GRANT_MIC,
            Self::Launch => GRANT_LAUNCH,
            Self::Power => GRANT_POWER,
            Self::ManageGames => GRANT_MANAGE_GAMES,
        }
    }
}

/// Grant class for one `0xC8` input event.
///
/// Exhaustive, no wildcard: a new [`InputKind`] is a compile error until
/// classified. Do not add `_ =>`. Mic (`0xCA`), DualSense (`0xCC`), and pen
/// are plane-gated before decode (Mic / Gamepad / Pointer by construction).
pub fn classify(kind: InputKind) -> GrantClass {
    match kind {
        InputKind::KeyDown | InputKind::KeyUp | InputKind::TextInput => GrantClass::Keyboard,
        InputKind::MouseMove
        | InputKind::MouseMoveAbs
        | InputKind::MouseButtonDown
        | InputKind::MouseButtonUp
        | InputKind::MouseScroll
        | InputKind::Scroll
        | InputKind::TouchDown
        | InputKind::TouchMove
        | InputKind::TouchUp => GrantClass::Pointer,
        InputKind::GamepadButton
        | InputKind::GamepadAxis
        | InputKind::GamepadState
        | InputKind::GamepadRemove
        | InputKind::GamepadArrival => GrantClass::Gamepad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_disjoint_and_all_covers_exactly_them() {
        let bits = [
            GRANT_GAMEPAD,
            GRANT_POINTER,
            GRANT_KEYBOARD,
            GRANT_CLIPBOARD,
            GRANT_MIC,
            GRANT_LAUNCH,
            GRANT_POWER,
            GRANT_MANAGE_GAMES,
        ];
        let mut acc = 0u32;
        for b in bits {
            assert_eq!(b.count_ones(), 1);
            assert_eq!(acc & b, 0, "overlapping grant bits");
            acc |= b;
        }
        assert_eq!(acc, GRANT_ALL);
        let classes = GrantClass::ALL.iter().fold(0, |acc, c| acc | c.bit());
        assert_eq!(
            classes, GRANT_ALL,
            "GrantClass::ALL must name every grant bit"
        );
        for (i, c) in GrantClass::ALL.iter().enumerate() {
            assert_eq!(
                c.bit().trailing_zeros() as usize,
                i,
                "{c:?} out of bit order"
            );
        }
        assert_eq!(GRANT_ALL & GRANT_RESERVED, 0);
        assert_eq!(GRANT_ALL | GRANT_RESERVED, u32::MAX);
    }

    #[test]
    fn presets_match_the_design() {
        assert_eq!(GRANT_PRESET_FULL, GRANT_ALL);
        assert_eq!(GRANT_PRESET_FULL & GRANT_POWER, GRANT_POWER);
        assert_eq!(GRANT_PRESET_FULL & GRANT_MANAGE_GAMES, GRANT_MANAGE_GAMES);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY & GRANT_MANAGE_GAMES, 0);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY, GRANT_GAMEPAD);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY & GRANT_LAUNCH, 0);
        assert_eq!(GRANT_PRESET_VIEW_ONLY, 0);
    }

    #[test]
    fn legacy_full_reads_as_the_current_full() {
        assert_eq!(
            GRANT_ALL_PRE_POWER,
            GRANT_ALL & !GRANT_POWER & !GRANT_MANAGE_GAMES
        );
        assert_eq!(GRANT_ALL_PRE_MANAGE, GRANT_ALL & !GRANT_MANAGE_GAMES);
        assert_eq!(normalize_legacy_full(GRANT_ALL_PRE_POWER), GRANT_ALL);
        assert_eq!(normalize_legacy_full(GRANT_ALL_PRE_MANAGE), GRANT_ALL);
        assert_eq!(normalize_legacy_full(GRANT_ALL), GRANT_ALL);
        assert_eq!(normalize_legacy_full(GRANT_GAMEPAD), GRANT_GAMEPAD);
        assert_eq!(normalize_legacy_full(0), 0);
        let limited = GRANT_ALL_PRE_POWER & !GRANT_KEYBOARD;
        assert_eq!(normalize_legacy_full(limited), limited);
    }

    #[test]
    fn every_input_kind_classifies_per_the_design_table() {
        use GrantClass::*;
        // from_u8, not the enum: a kind in the decoder but not classify still fails here.
        let mut seen = 0;
        for v in 0..=u8::MAX {
            let Some(kind) = InputKind::from_u8(v) else {
                continue;
            };
            seen += 1;
            let want = match kind {
                InputKind::KeyDown | InputKind::KeyUp | InputKind::TextInput => Keyboard,
                InputKind::GamepadButton
                | InputKind::GamepadAxis
                | InputKind::GamepadState
                | InputKind::GamepadRemove
                | InputKind::GamepadArrival => Gamepad,
                _ => Pointer,
            };
            assert_eq!(classify(kind), want, "kind {kind:?}");
        }
        assert_eq!(
            seen, 17,
            "InputKind wire vocabulary grew — classify the new kind"
        );
    }

    /// The bit table, the legacy-full read, and the mask → preset level each client derives:
    /// normalize, drop unknown bits, then match the three presets or fall to `custom`.
    fn grant_vectors() -> String {
        let bits = [
            ("GAMEPAD", GRANT_GAMEPAD),
            ("POINTER", GRANT_POINTER),
            ("KEYBOARD", GRANT_KEYBOARD),
            ("CLIPBOARD", GRANT_CLIPBOARD),
            ("MIC", GRANT_MIC),
            ("LAUNCH", GRANT_LAUNCH),
            ("POWER", GRANT_POWER),
            ("MANAGE_GAMES", GRANT_MANAGE_GAMES),
        ];
        let masks = [
            0,
            GRANT_GAMEPAD,
            GRANT_ALL,
            GRANT_ALL_PRE_POWER,
            GRANT_ALL_PRE_POWER & !GRANT_KEYBOARD,
            GRANT_ALL & !GRANT_LAUNCH,
            GRANT_GAMEPAD | GRANT_CLIPBOARD,
            GRANT_POWER,
            GRANT_ALL_PRE_MANAGE,
            GRANT_ALL_PRE_MANAGE & !GRANT_KEYBOARD,
            GRANT_MANAGE_GAMES | GRANT_GAMEPAD,
            0x100,
            0x100 | GRANT_GAMEPAD,
            0x100 | GRANT_ALL,
            0x100 | GRANT_ALL_PRE_POWER,
            0x100 | GRANT_ALL_PRE_MANAGE,
        ];
        let level = |mask: u32| match normalize_legacy_full(mask) & GRANT_ALL {
            GRANT_PRESET_FULL => "full",
            GRANT_PRESET_CONTROLLER_ONLY => "controller",
            GRANT_PRESET_VIEW_ONLY => "view",
            _ => "custom",
        };
        let about = "Generated from punktfunk_core::quic::access by grant_vectors_are_checked_in \
            (UPDATE_VECTORS=1 rewrites it). pf-client-core, the web console, Kotlin and Swift \
            replay it.";
        let mut out = format!("{{\n  \"$comment\": \"{about}\",\n  \"bits\": {{\n");
        for (i, (name, bit)) in bits.iter().enumerate() {
            let comma = if i + 1 < bits.len() { "," } else { "" };
            out += &format!("    \"{name}\": {bit}{comma}\n");
        }
        out += &format!(
            "  }},\n  \"all\": {GRANT_ALL},\n  \"all_pre_power\": {GRANT_ALL_PRE_POWER},\n  \
             \"all_pre_manage\": {GRANT_ALL_PRE_MANAGE},\n  \"masks\": [\n"
        );
        for (i, &mask) in masks.iter().enumerate() {
            let comma = if i + 1 < masks.len() { "," } else { "" };
            let normalized = normalize_legacy_full(mask);
            let level = level(mask);
            out += &format!(
                "    {{\"mask\": {mask}, \"normalized\": {normalized}, \"level\": \"{level}\"}}\
                 {comma}\n"
            );
        }
        out + "  ]\n}\n"
    }

    #[test]
    fn grant_vectors_are_checked_in() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/grant-vectors.json");
        let fresh = grant_vectors();
        if std::env::var_os("UPDATE_VECTORS").is_some() {
            std::fs::write(path, &fresh).unwrap();
        }
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            on_disk == fresh,
            "{path} is stale: rerun with UPDATE_VECTORS=1"
        );
    }

    #[test]
    fn class_bits_round_onto_the_grant_consts() {
        assert_eq!(GrantClass::Gamepad.bit(), GRANT_GAMEPAD);
        assert_eq!(GrantClass::Pointer.bit(), GRANT_POINTER);
        assert_eq!(GrantClass::Keyboard.bit(), GRANT_KEYBOARD);
        assert_eq!(GrantClass::Clipboard.bit(), GRANT_CLIPBOARD);
        assert_eq!(GrantClass::Mic.bit(), GRANT_MIC);
        assert_eq!(GrantClass::Launch.bit(), GRANT_LAUNCH);
        assert_eq!(GrantClass::Power.bit(), GRANT_POWER);
        assert_eq!(GrantClass::ManageGames.bit(), GRANT_MANAGE_GAMES);
    }
}
