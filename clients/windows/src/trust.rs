//! Client identity, the known-hosts (pinned fingerprint) store, and app settings —
//! re-exported from `pf-client-core` so the shell and the spawned `punktfunk-session`
//! binary share ONE `Settings`/`KnownHosts` shape over the same files
//! (`%APPDATA%\punktfunk`, or a non-empty `PUNKTFUNK_CONFIG_DIR`:
//! `client-windows-settings.json` / `client-known-hosts.json`).
//!
//! The shell is not the settings file's only writer (a session's window resize, the console
//! UI and the CLI save it too), so its edits go through `Settings::update`, under the file's
//! lock. Files from older WinUI shells still load via a serde alias in core.

pub use pf_client_core::trust::{
    access_error_message, device_code, device_name, grouped, hex, learn_from_advert,
    load_or_create_identity, pair_and_save, pair_error_message, parse_hex32, probe_known,
    request_access_to_host, save_paired, HostEdit, KnownHosts, Settings, PROBE_INTERVAL,
    PROBE_TIMEOUT,
};
