//! The machine the host runs on: its TLS identity and OS name, the mDNS advert and wake
//! addresses clients find it by, and its power and sleep controls.

// Network-facing; same `forbid` as `mod mgmt`.
#[forbid(unsafe_code)]
pub(crate) mod discovery;
// Network-facing; same `forbid` as `mod mgmt`. Tests mutate process env (`set_var` is unsafe in 2024).
#[cfg_attr(not(test), forbid(unsafe_code))]
pub(crate) mod identity;
pub(crate) mod osinfo;
pub(crate) mod power;
pub(crate) mod sleep_inhibit;
#[forbid(unsafe_code)]
pub(crate) mod wol;
