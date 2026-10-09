//! Uninstall-time sweep of MEDIA-class audio DEVNODEs this host minted.
//!
//! `punktfunk-host driver uninstall --audio` (Inno `[UninstallRun]`). DEVNODEs
//! persist across restarts by design ([`minted`](super::minted),
//! [`pad_endpoint`](super::pad_endpoint)); uninstall must still remove them.
//! Match owner markers, never names — our instances share VALVE streaming-audio
//! HWIDs and display names with Steam.
//!
//! `Device Parameters` markers:
//! * [`pad_endpoint::PAD_INDEX_VALUE`](super::pad_endpoint::PAD_INDEX_VALUE) — per-pad speakers
//! * [`minted::ROLE_MARKER`](super::minted::ROLE_MARKER) — Speakers/Microphone substrate
//! * [`audio_probe::PROBE_MARKER`](super::audio_probe::PROBE_MARKER) — leftover probe DEVNODEs
//!
//! Unmarked `ROOT\MEDIA\*` with [`MINTED_HWIDS`] is a host that died before the
//! marker write. Steam's own devices use the same HWIDs under
//! `ROOT\SteamStreamingSpeakers\*` / `ROOT\SteamStreamingMicrophone\*`.

use super::devnode_api as da;
use super::{audio_control, audio_probe, minted, pad_endpoint as pe};
use anyhow::Result;
use windows::Win32::Devices::DeviceAndDriverInstallation::SPDRP_HARDWAREID;

/// `Device Parameters` REG_DWORD names. Presence of the name is "ours"; the value is family-specific.
pub const OWNER_MARKERS: [&str; 3] = [
    pe::PAD_INDEX_VALUE,
    minted::ROLE_MARKER,
    audio_probe::PROBE_MARKER,
];

/// Steam streaming HWIDs we mint with. Second half of the abandoned-devnode test in [`owned_devnodes`].
const MINTED_HWIDS: [&str; 2] = [
    "ROOT\\SteamStreamingSpeakers",
    "ROOT\\SteamStreamingMicrophone",
];

/// Sweep counts. `endpoint_records` is best-effort; see [`delete_endpoint_record`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Removed {
    pub devnodes: usize,
    pub devnode_failures: usize,
    pub endpoint_records: usize,
}

/// Unpark defaults, then sweep minted DEVNODEs.
///
/// Stuck devices are counted, never fatal: a non-zero exit aborts the uninstaller.
pub fn purge() -> Result<Removed> {
    // Restore first: the parked default may still point at a device this sweep is about to delete.
    // Windows would re-pick by its own ranking, not the operator's original device.
    if audio_control::unpark_default_for_uninstall() {
        println!("restored the default audio device(s) this host had parked");
    }
    // Voice apps pinned to a sink this sweep deletes would lose their output.
    super::voice_route::recover_orphaned();

    let mut out = Removed::default();
    for inst in owned_devnodes()? {
        // Resolve MMDevices records before the DEVNODE goes. After removal the `{1}.<instance id>`
        // link is gone, and name-matching is exactly what this module refuses to do.
        let records: Vec<(&str, String)> = [
            (pe::MMDEV_RENDER_PATH, pe::find_endpoint_for_devnode(&inst)),
            (
                pe::MMDEV_CAPTURE_PATH,
                pe::find_capture_endpoint_for_devnode(&inst),
            ),
        ]
        .into_iter()
        .filter_map(|(path, found)| Some((path, found.ok().flatten()?)))
        .collect();

        if !remove_devnode(&inst) {
            out.devnode_failures += 1;
            // Still present; its MMDevices record still belongs to a live endpoint.
            continue;
        }
        out.devnodes += 1;
        for (path, endpoint) in records {
            if delete_endpoint_record(path, &endpoint) {
                out.endpoint_records += 1;
            }
        }
    }
    Ok(out)
}

/// MEDIA-class DEVNODEs with an [`OWNER_MARKERS`] value, including phantoms.
/// Enumerated without `DIGCF_PRESENT` (see [`da::media_class_devs`]).
fn owned_devnodes() -> Result<Vec<String>> {
    let set = da::media_class_devs()?;
    let mut out = Vec::new();
    for did in set.iter() {
        let Some(inst) = da::instance_id(&set, &did) else {
            continue;
        };
        if !is_removable_instance(&inst) {
            continue;
        }
        if OWNER_MARKERS
            .iter()
            .any(|m| da::read_devparam_dword(&set, &did, m).is_some())
        {
            out.push(inst);
            continue;
        }
        // Unmarked `ROOT\MEDIA\*` with a minting HWID: the host died before the marker write.
        // Prefix is required — Steam's devices share these HWIDs under `ROOT\SteamStreaming*\*`.
        if is_abandoned_mint(
            &inst,
            &da::devnode_multi_sz_prop(&set, &did, SPDRP_HARDWAREID),
        ) {
            out.push(inst);
        }
    }
    Ok(out)
}

/// Abandoned-mint test, extracted so it is checkable without a live devinfo set.
fn is_abandoned_mint(instance_id: &str, hwids: &[String]) -> bool {
    instance_id
        .to_ascii_uppercase()
        .starts_with("ROOT\\MEDIA\\")
        && MINTED_HWIDS
            .iter()
            .any(|want| hwids.iter().any(|h| h.eq_ignore_ascii_case(want)))
}

#[cfg(test)]
mod abandoned_tests {
    use super::is_abandoned_mint;

    fn hw(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn adopts_our_own_unmarked_devnodes() {
        assert!(is_abandoned_mint(
            r"ROOT\MEDIA\0004",
            &hw(r"ROOT\SteamStreamingMicrophone")
        ));
        assert!(is_abandoned_mint(
            r"ROOT\MEDIA\0002",
            &hw(r"ROOT\SteamStreamingSpeakers")
        ));
        // PnP casing is not guaranteed.
        assert!(is_abandoned_mint(
            r"root\media\0009",
            &hw(r"root\steamstreamingspeakers")
        ));
    }

    #[test]
    fn never_matches_valves_own_devices() {
        assert!(!is_abandoned_mint(
            r"ROOT\STEAMSTREAMINGMICROPHONE\0000",
            &hw(r"ROOT\SteamStreamingMicrophone")
        ));
        assert!(!is_abandoned_mint(
            r"ROOT\STEAMSTREAMINGSPEAKERS\0000",
            &hw(r"ROOT\SteamStreamingSpeakers")
        ));
    }

    #[test]
    fn never_matches_other_vendors_or_real_hardware() {
        // VB-Cable also mints `ROOT\MEDIA\*`; HWID is the discriminator.
        assert!(!is_abandoned_mint(r"ROOT\MEDIA\0000", &hw("VBAudioVACWDM")));
        assert!(!is_abandoned_mint(
            r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0897",
            &hw(r"ROOT\SteamStreamingSpeakers")
        ));
        assert!(!is_abandoned_mint(r"ROOT\MEDIA\0001", &[]));
    }
}

/// ROOT-enumerated (software-created) only.
///
/// A marker name colliding under a real sound card's `Device Parameters` must not take hardware.
fn is_removable_instance(instance_id: &str) -> bool {
    instance_id.to_ascii_uppercase().starts_with("ROOT\\")
}

/// [`pf_paths::remove_device`], reported on the uninstaller's console.
fn remove_devnode(instance_id: &str) -> bool {
    match pf_paths::remove_device(instance_id) {
        Ok(()) => {
            println!("removed audio devnode {instance_id}");
            true
        }
        Err(e) => {
            eprintln!("warning: couldn't remove {instance_id}: {e:#}");
            false
        }
    }
}

/// Delete one MMDevices `{guid}` subkey. Best-effort and quiet: SYSTEM-owned keys grant
/// Administrators read-only, and the uninstaller is elevated as a user. A leftover is
/// inert (NOTPRESENT without a DEVNODE). Do not seize SYSTEM ownership to finish the cosmetic.
fn delete_endpoint_record(reg_path: &str, endpoint_id: &str) -> bool {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS};
    use winreg::RegKey;

    let Ok(guid) = pe::endpoint_guid_part(endpoint_id) else {
        return false;
    };
    let Ok(store) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(reg_path, KEY_ALL_ACCESS)
    else {
        return false;
    };
    store.delete_subkey_all(guid).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_enumerated_devnodes_are_ours() {
        assert!(is_removable_instance(r"ROOT\MEDIA\0003"));
        // PnP casing is not guaranteed.
        assert!(is_removable_instance(r"root\media\0004"));
        // Real hardware, even if a marker-shaped value landed under it.
        assert!(!is_removable_instance(
            r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0900\4&1c4a4e5&0&0001"
        ));
        assert!(!is_removable_instance(r"USB\VID_046D&PID_0A38\ABCDEF"));
        // `ROOT` as a later path component is not ROOT-enumerated.
        assert!(!is_removable_instance(r"SWD\ROOT\MEDIA\0003"));
    }

    #[test]
    fn every_minted_family_is_swept() {
        // A new minted family that omits itself here ships as a leftover after uninstall.
        assert!(OWNER_MARKERS.contains(&"PunktfunkPadIndex"));
        assert!(OWNER_MARKERS.contains(&"PunktfunkAudioRole"));
        assert!(OWNER_MARKERS.contains(&"PunktfunkAudioProbe"));
    }
}
