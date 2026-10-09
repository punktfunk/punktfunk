//! Client half of host power actions (`design/host-actions.md`): list what the paired
//! host offers this device, then invoke one by id.
//!
//! Same mTLS lane as [`crate::library`]: device identity, fingerprint pin, `mgmt_port`.
//! [`ActionInfo::permitted`] is the host's grant for this device; ungranted rows are
//! omitted rather than offered as a 403. Both calls work out of session so a host tile
//! can sleep the box without a stream.

use serde::Deserialize;

/// One row from `GET /api/v1/actions` for this device.
///
/// Unknown ids are expected: render [`Self::title`] verbatim so a later host can add
/// actions without a client release.
#[derive(Clone, Debug, Deserialize)]
pub struct ActionInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub group: String,
    /// Confirm first: the action drops host state (reboot, shutdown).
    #[serde(default)]
    pub danger: bool,
    /// Host can run it now. False for no-suspend, a foreign inhibitor, missing group.
    #[serde(default)]
    pub available: bool,
    /// Why `available` is false; shown on a disabled row, never used to hide it.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// Host-power grant for this device. False hides the row ([`Self::offerable`]).
    #[serde(default)]
    pub permitted: bool,
}

impl ActionInfo {
    /// Ungranted rows are omitted. Granted-but-unavailable rows stay, disabled, with [`Self::unavailable_reason`].
    pub fn offerable(&self) -> bool {
        self.permitted
    }

    /// Local wording for known ids; [`Self::title`] for anything else so a new host action still shows.
    pub fn label(&self) -> &str {
        match self.id.as_str() {
            "power.sleep" => "Sleep host",
            "power.reboot" => "Restart host",
            "power.shutdown" => "Shut down host",
            _ => &self.title,
        }
    }
}

#[cfg(desktop)]
#[derive(Deserialize, Default)]
struct ActionList {
    #[serde(default)]
    actions: Vec<ActionInfo>,
}

/// `GET /api/v1/actions`. Empty on any miss, never an error — same contract as [`crate::library::fetch_running`].
///
/// An older host has no such route. A missing menu row is cheaper than failing the host card.
#[cfg(desktop)]
pub fn fetch_actions(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Vec<ActionInfo> {
    crate::library::get_json::<ActionList>(addr, mgmt_port, identity, pin, "/api/v1/actions")
        .actions
}

/// `POST /api/v1/actions/{id}` with an empty body.
///
/// `Ok(())` is 202 Accepted: the host then ends every session and acts ~1 s later.
/// 4xx becomes [`crate::library::LibraryError::Http`]; other failures go through [`crate::library::classify`].
#[cfg(desktop)]
pub fn invoke(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    action_id: &str,
) -> Result<(), crate::library::LibraryError> {
    use crate::library::LibraryError;
    let agent = crate::library::agent(identity, pin)?;
    // Empty body: no request field reaches the privileged path. Do not percent-encode; ids are `[a-z.]`.
    let url = format!(
        "{}/api/v1/actions/{action_id}",
        crate::library::base_url(addr, mgmt_port)
    );
    match agent.post(&url).send_empty() {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => {
            Err(LibraryError::Http(code))
        }
        Err(e) => Err(crate::library::classify(e)),
    }
}

/// [`invoke`] with the outcome every shell shows. The cached rows go first: whatever the
/// host said about itself is about to be wrong. A 202 is the last word, so there is
/// nothing to poll.
#[cfg(desktop)]
pub fn run(
    host_name: &str,
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    fp_hex: &str,
    action_id: &str,
    label: &str,
) -> String {
    invalidate(fp_hex);
    let pin = crate::trust::parse_hex32(fp_hex);
    match invoke(addr, mgmt_port, identity, pin, action_id) {
        Ok(()) => {
            tracing::info!(host = %host_name, action = %action_id, "host action accepted");
            format!("{host_name}: {label} — on its way")
        }
        Err(e) => {
            tracing::warn!(host = %host_name, action = %action_id, error = %e, "host action refused");
            format!("{label} failed — {e}")
        }
    }
}

/// 300 s. Grant and suspend-capability change when an operator edits access, not
/// minute-to-minute; each refresh is a TLS handshake against an idle host.
#[cfg(desktop)]
pub const TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Settled before a menu draws: a row that appears under a cursor already moving toward
/// it can shut the machine down. One cache so the console, GTK, and Windows tiles agree.
#[cfg(desktop)]
static ACTIONS: crate::library::FpCache<ActionInfo> =
    crate::library::FpCache::new(TTL, "punktfunk-hostactions");

/// Offerable rows last stored for this fingerprint. Empty until [`refresh`] answers, and for no-route / no-grant hosts.
#[cfg(desktop)]
pub fn cached(fp_hex: &str) -> Vec<ActionInfo> {
    ACTIONS.get(fp_hex)
}

/// Ask the host for its rows unless [`TTL`] says the last answer still stands.
/// Idempotent; call it on any shell tick.
#[cfg(desktop)]
pub fn refresh(addr: &str, mgmt_port: u16, fp_hex: &str) {
    ACTIONS.refresh(
        addr,
        mgmt_port,
        fp_hex,
        fetch_actions,
        ActionInfo::offerable,
    );
}

/// Drop the cache after [`invoke`]: otherwise the menu still offers Sleep until [`TTL`] lapses on an already-asleep host.
#[cfg(desktop)]
pub fn invalidate(fp_hex: &str) {
    ACTIONS.invalidate(fp_hex);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_prefer_local_wording_and_fall_back_to_the_host() {
        let mk = |id: &str, title: &str| ActionInfo {
            id: id.into(),
            title: title.into(),
            group: "power".into(),
            danger: false,
            available: true,
            unavailable_reason: None,
            permitted: true,
        };
        assert_eq!(mk("power.sleep", "Sleep host").label(), "Sleep host");
        assert_eq!(mk("power.reboot", "whatever").label(), "Restart host");
        assert_eq!(
            mk("plugin:vpn:toggle", "Toggle the VPN").label(),
            "Toggle the VPN"
        );
    }

    #[test]
    fn permission_hides_but_unavailability_only_disables() {
        let mut a = ActionInfo {
            id: "power.sleep".into(),
            title: "Sleep host".into(),
            group: "power".into(),
            danger: false,
            available: false,
            unavailable_reason: Some("this machine does not support sleep".into()),
            permitted: true,
        };
        assert!(a.offerable(), "unavailable actions are shown with a reason");
        a.permitted = false;
        assert!(!a.offerable(), "an ungranted action is not offered at all");
    }
}
