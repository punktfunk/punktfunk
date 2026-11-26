//! The machine the host runs on: its TLS identity and OS name, the mDNS advert and wake
//! addresses clients find it by, its power and sleep controls, and on macOS the children
//! beside it.

#[cfg(target_os = "macos")]
pub(crate) mod app_loop;
#[cfg(target_os = "macos")]
pub(crate) mod companions;
// Network-facing; same `forbid` as `mod mgmt`.
#[forbid(unsafe_code)]
pub(crate) mod discovery;
// Network-facing; same `forbid` as `mod mgmt`. Tests mutate process env (`set_var` is unsafe in 2024).
#[cfg_attr(not(test), forbid(unsafe_code))]
pub(crate) mod identity;
#[cfg(target_os = "macos")]
pub(crate) mod instance;
pub(crate) mod osinfo;
pub(crate) mod power;
pub(crate) mod sleep_inhibit;
pub(crate) mod sleep_watch;
#[forbid(unsafe_code)]
pub(crate) mod wol;
