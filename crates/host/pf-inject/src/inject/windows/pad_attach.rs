//! A pad's attach watcher and the one diagnosis it logs when no driver serves the pad: the
//! driver store, the devnode's PnP status, and where to look next.

use super::driver_store::driver_store_has;
use crate::channel_proof;
use std::time::{Duration, Instant};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_DevNode_Status, CM_Locate_DevNodeW, CM_DEVNODE_STATUS_FLAGS,
    CM_LOCATE_DEVNODE_NORMAL, CM_PROB, CR_SUCCESS, DN_DRIVER_LOADED, DN_HAS_PROBLEM, DN_STARTED,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_HardwareIds, DEVPROPTYPE, DEVPROP_TYPE_STRING_LIST,
};

/// PnP bind + driver stamp window before a diagnosis warn.
const ATTACH_GRACE: Duration = Duration::from_secs(3);

/// Per-pad attach watcher. Feed `driver_proto` every service tick; logs attach,
/// version mismatch, a pad that enumerated as another controller, or — after
/// [`ATTACH_GRACE`] of silence — one diagnosis. States never repeat a log line, so the
/// pump can call this at full rate.
pub(crate) struct DriverAttach {
    driver: &'static str,
    inf: &'static str,
    driver_log: &'static str,
    shm_name: String,
    /// `None` on the out-of-band fallback path.
    instance_id: Option<String>,
    /// The `device_type` the pad's hardware id names; `None` for the XUSB pad and the mouse.
    identity: Option<u8>,
    created: Instant,
    state: AttachState,
}

enum AttachState {
    Waiting,
    /// Diagnosis logged; still watching so a late attach gets its INFO line.
    Warned,
    Attached,
}

impl DriverAttach {
    pub(crate) fn new(
        driver: &'static str,
        inf: &'static str,
        driver_log: &'static str,
        shm_name: String,
        instance_id: Option<String>,
    ) -> DriverAttach {
        DriverAttach {
            driver,
            inf,
            driver_log,
            shm_name,
            instance_id,
            identity: pf_driver_proto::gamepad::devtype_from_hwids(driver),
            created: Instant::now(),
            state: AttachState::Waiting,
        }
    }

    /// For the XUSB pad and the mouse, whose sections carry no driver revision.
    pub(crate) fn observe(&mut self, driver_proto: u32) {
        self.observe_pad(driver_proto, 0);
    }

    /// `driver_rev` is read after `driver_proto` ([`crate::pad_shm_ring::driver_marks`]);
    /// only the attach tick uses it.
    pub(crate) fn observe_pad(&mut self, driver_proto: u32, driver_rev: u32) {
        match self.state {
            AttachState::Attached => {}
            AttachState::Waiting | AttachState::Warned if driver_proto != 0 => {
                let late = matches!(self.state, AttachState::Warned);
                tracing::info!(
                    driver = self.driver,
                    shm = %self.shm_name,
                    proto = driver_proto,
                    late,
                    "gamepad driver attached to the shared section"
                );
                if driver_proto != pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION {
                    tracing::warn!(
                        driver = self.driver,
                        driver_proto,
                        host_proto = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION,
                        "gamepad driver/host protocol mismatch — update the drivers: run the Punktfunk installer again"
                    );
                }
                self.check_pad(driver_rev);
                self.state = AttachState::Attached;
            }
            AttachState::Waiting if self.created.elapsed() >= ATTACH_GRACE => {
                if self.identity.is_some() {
                    crate::note_pad_driver(crate::PadDriverVerdict::NotAttached);
                }
                self.diagnose();
                self.state = AttachState::Warned;
            }
            _ => {}
        }
    }

    /// Judge a freshly attached pad's driver ([`crate::pad_attach_verdict`]) for the diagnostics
    /// row, and WARN on an old revision or on a pad that reports another controller's VID/PID
    /// than its hardware id names (an older driver that did not know the id).
    fn check_pad(&self, driver_rev: u32) {
        let Some(devtype) = self.identity else {
            return;
        };
        let got = self
            .instance_id
            .as_deref()
            .and_then(channel_proof::hid_vid_pid);
        let verdict = crate::pad_attach_verdict(devtype, driver_rev, got);
        let fix = "reinstall the host with its controller drivers";
        match &verdict {
            crate::PadDriverVerdict::WrongIdentity { want, got } => tracing::warn!(
                driver = self.driver,
                want = %format!("VID_{:04X}&PID_{:04X}", want.0, want.1),
                got = %format!("VID_{:04X}&PID_{:04X}", got.0, got.1),
                fix,
                "virtual pad enumerated as another controller; games see the wrong pad"
            ),
            crate::PadDriverVerdict::Stale {
                driver_rev,
                host_rev,
            } => tracing::warn!(
                driver = self.driver,
                driver_rev,
                host_rev,
                fix,
                "gamepad driver is older than this host; pads run with its old behaviour"
            ),
            _ => {}
        }
        crate::note_pad_driver(verdict);
    }

    /// One-shot WARN: driver-store presence, devnode PnP problem, where to look next.
    ///
    /// Spawns a thread and returns. The caller is the pad service thread (input + rumble);
    /// `pnputil` can block for tens of seconds and a deadline wait would stall every
    /// unattached pad on that same wait.
    fn diagnose(&self) {
        let (driver, inf, driver_log) = (self.driver, self.inf, self.driver_log);
        let shm_name = self.shm_name.clone();
        let instance_id = self.instance_id.clone();
        let pad = self.identity.is_some();
        std::thread::Builder::new()
            .name("pf-driver-diagnose".into())
            .spawn(move || diagnose_blocking(driver, inf, driver_log, &shm_name, instance_id, pad))
            .ok();
    }
}

/// Split out of [`DriverAttach::diagnose`] so the blocking wait is visible as blocking.
fn diagnose_blocking(
    driver: &'static str,
    inf: &'static str,
    driver_log: &'static str,
    shm_name: &str,
    instance_id: Option<String>,
    pad: bool,
) {
    let store = match driver_store_has(inf) {
        Some(true) => "driver package present in the driver store",
        Some(false) => "driver package NOT in the driver store — run the Punktfunk installer again",
        None => "driver store could not be queried (pnputil failed or still enumerating)",
    };
    let devnode = match &instance_id {
        Some(id) => devnode_status_line(id, pad),
        None => "no per-session devnode (SwDeviceCreate failed earlier — see the warning above)"
            .to_string(),
    };
    tracing::warn!(
        driver,
        shm = %shm_name,
        grace_secs = ATTACH_GRACE.as_secs(),
        store,
        devnode = %devnode,
        driver_log,
        "gamepad driver has not attached to the shared section — the virtual pad exists but no \
         driver is serving it (games will not see it); an old (pre-sealed-channel) driver also \
         reads as not-attached: update by running the Punktfunk installer again \
         (driver_log is only written by debug driver builds, or with the PFXUSB_DEBUG_LOG / \
         PFGAMEPAD_DEBUG_LOG / PFMOUSE_DEBUG_LOG system env var set + the device restarted)"
    );
}

pub(super) fn devnode_status_line(instance_id: &str, pad: bool) -> String {
    let wide = HSTRING::from(instance_id);
    let mut devinst = 0u32;
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 instance id; `devinst` receives the handle.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst,
            PCWSTR(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        return format!(
            "devnode {instance_id} not found (CM_Locate_DevNodeW CR={})",
            cr.0
        );
    }
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    // SAFETY: devinst is the devnode located above; the two out-params receive status + problem.
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if cr != CR_SUCCESS {
        return format!("devnode {instance_id}: status query failed (CR={})", cr.0);
    }
    if status.0 & DN_HAS_PROBLEM.0 != 0 {
        let refused =
            pad && hardware_ids(devinst).is_some_and(|ids| crate::pad_refused(problem.0, &ids));
        let hint = if refused {
            "the gamepad driver refused it: no pf_* hardware id names this pad; the host and \
             driver disagree on the controller list — reinstall both"
        } else {
            cm_problem_hint(problem.0)
        };
        return format!(
            "devnode {instance_id} has PnP problem code {} ({hint}) [status 0x{:08x}]",
            problem.0, status.0
        );
    }
    format!(
        "devnode {instance_id} status 0x{:08x} (driver_loaded={} started={})",
        status.0,
        status.0 & DN_DRIVER_LOADED.0 != 0,
        status.0 & DN_STARTED.0 != 0,
    )
}

/// A devnode's hardware ids, lowercase and `;`-terminated, the form the driver matches on.
fn hardware_ids(devinst: u32) -> Option<String> {
    let mut ty = DEVPROPTYPE(0);
    let mut buf = [0u16; 512];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: `devinst` is a located devnode; the key is a static const; `buf` holds `size`
    // bytes and `ty` / `size` are valid out-params.
    let cr = unsafe {
        CM_Get_DevNode_PropertyW(
            devinst,
            &DEVPKEY_Device_HardwareIds,
            &mut ty,
            Some(buf.as_mut_ptr().cast()),
            &mut size,
            0,
        )
    };
    if cr != CR_SUCCESS || ty != DEVPROP_TYPE_STRING_LIST {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    Some(
        buf[..len]
            .split(|&c| c == 0)
            .filter(|id| !id.is_empty())
            .map(|id| String::from_utf16_lossy(id).to_ascii_lowercase() + ";")
            .collect(),
    )
}

fn cm_problem_hint(problem: u32) -> &'static str {
    match problem {
        1 => "not configured — no driver bound; install the drivers",
        10 => "device did not start — driver bound but the start failed; check the driver log",
        18 => "reinstall required — re-run driver install",
        24 => "device not present/working — PnP could not start the virtual devnode",
        28 => "drivers not installed — the pf driver package is missing from the store or its certificate is not trusted",
        31 => "driver did not load — the package was found but the load failed",
        39 => "driver corrupt or missing — reinstall the drivers",
        43 => "reported failure after start — check the driver log",
        52 => "driver signature rejected — certificate not in Root/TrustedPublisher, or blocked by Memory Integrity",
        _ => "see Device Manager for this code",
    }
}
