//! Resolve a library id or operator command into the per-OS line the host runs.
//!
//! Scanners enumerate only. This module owns the `kind` vocabulary, charset
//! validators, and resolvers so a plugin lift does not take launch with it.
//! A client sends an entry id; the host holds the [`LaunchSpec`]
//! (design/library-scanner-plugins.md D1).
//!
//! Linux (`posix.rs`): the host runs the resolved shell command (nested gamescope
//! or live session). Windows (`windows.rs`): `launch_title` uses the signed-in
//! user of the host's WTS session. `plat` is whichever this build is.

use super::*;

#[cfg(not(windows))]
mod desktop;
#[cfg(not(windows))]
pub use desktop::valid_desktop_id;
mod exec;
pub use exec::{spec_is_valid as exec_spec_is_valid, ExecRecipe};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as plat;
#[cfg(not(windows))]
mod posix;
pub use self::plat::*;
#[cfg(not(windows))]
use self::posix as plat;

/// Library enumeration hits every store's on-disk metadata, so handshake
/// resolves this once and threads it through the data plane.
pub struct LaunchTarget {
    pub game: crate::gamelease::GameRef,
    /// Launcher tile (design D4): no game-exit to detect; the lease stays untracked.
    /// See [`crate::gamelease::LeaseRequest::launcher`].
    pub launcher: bool,
    pub detect: DetectSpec,
    /// Linux: the host-run shell command. Windows: always `None`; spawn is by library id.
    pub command: Option<String>,
    /// Open this launch on an empty workspace of the streamed head, where the
    /// compositor can place it ([`crate::library::OnWindow::own_workspace`]).
    pub own_workspace: bool,
    /// The entry's own placement block, applied to the game's first window.
    /// `own_workspace` above is the workspace key already resolved against the
    /// host policy; the rest is read when that window appears.
    pub on_window: crate::library::OnWindow,
}

/// Map a store-qualified library id to a [`LaunchTarget`] from the host's library.
/// `None` = unknown id, or on Linux a title with no runnable recipe.
///
/// Shared by both planes. Linux runs the command (nested gamescope or live
/// session). Windows has no nest: `launch_title` resolves the process later.
pub fn resolve_launch(id: &str) -> Option<LaunchTarget> {
    let entry = all_games().into_iter().find(|g| g.id == id)?;
    let game = crate::gamelease::GameRef {
        id: Some(entry.id.clone()),
        store: Some(entry.store.clone()),
        title: entry.title.clone(),
    };
    plat::launch_target(entry, game)
}

/// Recipe for an `exec`-kind entry, built from the owning plugin's manifest. `None` for every
/// other kind, so both OS resolvers try this first.
///
/// Lives here rather than in `command_for` / `windows_launch_for` because it needs `provider`
/// (stamped from `PUT /library/provider/{provider}`): the manifest that may be used is the one
/// belonging to the plugin that published the entry.
fn exec_recipe(entry: &GameEntry) -> Option<ExecRecipe> {
    exec::recipe(entry)
}

// Per-kind launch values. Scanners supply the VALUE; the host builds the URI.
// Lives here so URI construction stays with launch (design D1). Unparseable
// or hostile → `None`, never a partial command.

/// Digits-only Steam appid (or 64-bit [`shortcut_gameid`]). Shared kind because
/// `rungameid` takes either. Both platform recipe maps use it.
pub(crate) fn valid_steam_appid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// 64-bit `rungameid` for a non-Steam shortcut: high dword = 32-bit appid,
/// low dword = marker `0x0200_0000`. The bare 32-bit appid does not launch it.
pub(crate) fn shortcut_gameid(appid: u32) -> u64 {
    ((appid as u64) << 32) | 0x0200_0000
}

/// `steam_ui` values (design D4). Closed set, validated inbound and outbound
/// so a third value cannot sit in the library and resolve to nothing at launch.
pub(crate) fn valid_steam_ui(value: &str) -> bool {
    matches!(value, "bigpicture" | "desktop")
}

/// One AUMID half: non-empty, charset that cannot break `shell:AppsFolder\…`.
/// Load-bearing for `xbox`, whose Identity arrives from a plugin.
pub(crate) fn aumid_part(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn valid_aumid(value: &str) -> bool {
    value
        .split_once('!')
        .is_some_and(|(pfn, app)| aumid_part(pfn) && aumid_part(app))
}

/// Playnite GUID, interpolated into an explorer.exe URI: 8-4-4-4-12 hex + dashes.
pub(crate) fn valid_playnite_id(value: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for want in groups {
        match parts.next() {
            Some(p) if p.len() == want && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Ubisoft Connect game id, interpolated into `uplay://launch/<id>/0`: digits only.
pub(crate) fn valid_uplay_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Amazon Games product id (`amzn1.adg.product.<uuid>`), interpolated into
/// `amazon-games://play/<id>`: alphanumerics, `.`, `_`, `-`.
pub(crate) fn valid_amazon_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Battle.net launch code (`WTCG`, `Pro`, `wow_classic`), handed to the client as
/// `--exec="launch <code>"`: word characters, case kept — the client matches exactly.
pub(crate) fn valid_battlenet_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `launcher_ui` values this OS can open (design D4). One kind; a value names
/// a UI (`heroic` vs `heroic-console`; Windows `playnite` is Fullscreen).
///
/// Unknown *kind* is an unlaunchable tile; unknown *value* is a 400 that
/// refuses the whole reconcile. Platform-gated so a plugin gets 400 inbound
/// instead of a tile that fails at launch. `command` is operator-only, so a
/// plugin cannot publish a launcher without this kind.
///
/// Vocabulary: is `value` a launcher this OS knows? A miss is a plugin bug;
/// installing software will not fix it, so reconcile refuses the payload.
pub(crate) fn known_launcher_ui(value: &str) -> bool {
    plat::LAUNCHER_UI_STORES.contains(&value)
}

/// Environment: can this box open `value` *now*? Separate from
/// [`known_launcher_ui`]: unknown value is a plugin bug (400); known-but-missing
/// is an ordinary fact. Conflating them 400'd whole libraries over one tile
/// ([`super::sanitize_launcher_entries`] drops the tile; games still sync).
pub(crate) fn resolvable_launcher_ui(value: &str) -> bool {
    known_launcher_ui(value) && plat::launcher_ui_installed(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_ui_is_a_closed_two_value_enum() {
        assert!(valid_steam_ui("bigpicture"));
        assert!(valid_steam_ui("desktop"));
        assert!(!valid_steam_ui("gamepadui"));
        assert!(!valid_steam_ui(""));
        assert!(!valid_steam_ui("bigpicture; rm -rf ~"));
    }
    /// `xbox` is plugin-facing: Identity over the wire, host completes the PFN.
    /// Charset is load-bearing here; `aumid` is host-derived.
    #[test]
    fn xbox_value_is_identity_bang_appid_and_charset_guarded() {
        assert!(valid_aumid("Microsoft.Foo!Game"));
        assert!(valid_aumid("A_b-c.d!App"));
        assert!(!valid_aumid("Microsoft.Foo"));
        assert!(!valid_aumid("!Game"));
        assert!(!valid_aumid("Microsoft.Foo!"));
        assert!(!valid_aumid(""));
        assert!(!valid_aumid("Foo\"!Game"));
        assert!(!valid_aumid("Foo!Game\" & calc"));
        assert!(!valid_aumid("Foo\\..\\Bar!Game"));
        assert!(!valid_aumid("Foo Bar!Game"));
    }
    #[test]
    fn steam_appid_validation_accepts_appids_and_shortcut_gameids() {
        assert!(valid_steam_appid("570"));
        assert!(valid_steam_appid(
            &shortcut_gameid(2_456_789_012).to_string()
        ));
        assert!(!valid_steam_appid(""));
        assert!(!valid_steam_appid("570; rm -rf ~"));
        assert!(!valid_steam_appid("-1"));
    }
    /// Launch vocabulary, not enumeration: the scanner supplies the 32-bit appid only.
    #[test]
    fn shortcut_gameid_composes_appid_and_marker() {
        let id = shortcut_gameid(0x8000_0000);
        assert_eq!(id >> 32, 0x8000_0000, "high dword is the shortcut appid");
        assert_eq!(id & 0xFFFF_FFFF, 0x0200_0000, "low dword is the marker");
    }
    #[test]
    fn store_ids_are_charset_guarded() {
        assert!(valid_uplay_id("5595"));
        assert!(!valid_uplay_id(""));
        assert!(!valid_uplay_id("5595/0"));
        assert!(valid_amazon_id(
            "amzn1.adg.product.1f2a3b4c-5d6e-7f80-9a1b-2c3d4e5f6a7b"
        ));
        assert!(!valid_amazon_id("amzn1.adg.product.x\" & calc"));
        assert!(!valid_amazon_id(""));
        assert!(valid_battlenet_code("WTCG"));
        assert!(valid_battlenet_code("wow_classic"));
        assert!(!valid_battlenet_code("Pro\" & calc"));
        assert!(!valid_battlenet_code(""));
    }

    #[test]
    fn launcher_ui_refuses_the_empty_and_the_hostile_value() {
        assert!(!known_launcher_ui(""));
        assert!(!known_launcher_ui("lutris; rm -rf ~"));
        assert!(!resolvable_launcher_ui(""));
        assert!(!resolvable_launcher_ui("lutris; rm -rf ~"));
    }
}
