//! Client identity, the known-hosts (pinned fingerprint) store, and app settings —
//! re-exported from `pf-client-core` so the shell and the spawned `punktfunk-session`
//! binary share ONE `Settings`/`KnownHosts` shape over the same files
//! (`%APPDATA%\punktfunk`, or a non-empty `PUNKTFUNK_CONFIG_DIR`:
//! `client-windows-settings.json` / `client-known-hosts.json`).
//!
//! The shell is the settings file's only writer; the session only reads it. The shell's
//! former private `Settings` copy (≤ 0.8.4: `show_hud`, `engine`) is gone — old files
//! still load via a serde alias in core.

pub use pf_client_core::trust::{
    hex, learn_from_advert, load_or_create_identity, pair_error_message, parse_hex32, probe_known,
    KnownHost, KnownHosts, Settings,
};
