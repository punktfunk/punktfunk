//! Which host screen this device streams (`design/screen-pick.md`): the rows the host offers
//! it, and the write of its own pick.
//!
//! Same mTLS lane as [`crate::host_actions`]: device identity, fingerprint pin, `mgmt_port`.
//! The host resolves every row for the caller; a client renders them and never builds a
//! label. The pick is the device's own row on the host, so no client stores it.

use serde::{Deserialize, Serialize};

/// The id `GET /display/targets` gives the own-screen row.
const OWN: &str = "own";

/// What a session streams: the device's own virtual screen, or a host monitor by connector.
/// JSON `"own"` or `{"monitor": "HDMI-A-1"}`, the host's spelling.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayTarget {
    #[default]
    Own,
    Monitor(String),
}

/// One row of the picker. Every field defaults, so a host that adds or drops one never
/// hides the section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Target {
    /// `own`, else the connector.
    pub id: String,
    /// The host's words: a monitor's description, or its English for the own row.
    pub label: String,
    pub connector: Option<String>,
    /// `3840x2160@60`, as the host sends it.
    pub mode: Option<String>,
    pub primary: bool,
    /// False when the compositor reports the head off.
    pub enabled: bool,
    /// The pairing name of the device streaming this head now.
    pub in_use_by: Option<String>,
}

impl Default for Target {
    fn default() -> Self {
        Target {
            id: String::new(),
            label: String::new(),
            connector: None,
            mode: None,
            primary: false,
            enabled: true,
            in_use_by: None,
        }
    }
}

impl Target {
    /// What picking this row writes.
    pub fn target(&self) -> DisplayTarget {
        match self.id.as_str() {
            OWN => DisplayTarget::Own,
            id => DisplayTarget::Monitor(id.to_string()),
        }
    }

    /// The row's word: this app's wording for the own row, the host's label for a monitor.
    pub fn name(&self) -> &str {
        match (self.id.as_str(), self.label.as_str()) {
            (OWN, _) => pf_l10n::client::screen_own(),
            (id, "") => id,
            (_, label) => label,
        }
    }

    /// Connector and mode, for the row's value; the connector drops out when it is the label.
    pub fn detail(&self) -> String {
        let connector = self.connector.as_deref().filter(|c| *c != self.label);
        [connector, self.mode.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    }

    /// Why the row can't be picked now, shown under it; `None` for a pickable row.
    pub fn reason(&self) -> Option<String> {
        match &self.in_use_by {
            Some(device) => Some(pf_l10n::client::screen_in_use(device)),
            None if !self.enabled => Some(pf_l10n::client::screen_off().into()),
            None => None,
        }
    }
}

/// `GET /api/v1/display/targets` for this device.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Targets {
    /// The host's settings file pins the screen: the rows are not offered.
    pub locked: bool,
    pub host_default: DisplayTarget,
    /// This device's own row; `None` follows [`Self::host_default`].
    pub yours: Option<DisplayTarget>,
    pub targets: Vec<Target>,
}

impl Targets {
    /// The pick a connect gets now: this device's row, else the host's default.
    pub fn current(&self) -> &DisplayTarget {
        self.yours.as_ref().unwrap_or(&self.host_default)
    }

    /// The card's line: "Streams LG TV" for a monitor, the own row's word for its own screen.
    pub fn summary(&self) -> String {
        match self.current() {
            DisplayTarget::Own => pf_l10n::client::screen_own().into(),
            DisplayTarget::Monitor(c) => {
                let row = self.targets.iter().find(|t| t.id == *c);
                pf_l10n::client::screen_streams(row.map_or(c.as_str(), Target::name))
            }
        }
    }
}

/// `GET /api/v1/display/targets`. `None` on any miss, never an error — same contract as
/// [`crate::host_actions::fetch_actions`]: a host without the route has no Screen section.
#[cfg(desktop)]
pub fn fetch_targets(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Option<Targets> {
    let path = "/api/v1/display/targets";
    crate::library::get_json::<Option<Targets>>(addr, mgmt_port, stream_port, identity, pin, path)
}

/// `PUT /api/v1/display/clients/{this device's fingerprint}` with `{"screen": target}`, then
/// a fresh fetch. `fp_hex` is the host's fingerprint: the pin and the cache key. `Err` is a
/// phrase each shell frames, like [`crate::library::LibraryError`]'s.
#[cfg(desktop)]
pub fn set_target(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    fp_hex: &str,
    target: &DisplayTarget,
) -> Result<(), String> {
    use crate::library::{HostApi, LibraryError};
    let pin = crate::trust::parse_hex32(fp_hex);
    let put = || -> Result<(), LibraryError> {
        let device = punktfunk_core::quic::endpoint::fingerprint_of_pem(&identity.0)
            .map_err(|e| LibraryError::Unreachable(e.to_string()))?;
        let path = format!(
            "/api/v1/display/clients/{}",
            punktfunk_core::fp::hex(&device)
        );
        let body = serde_json::json!({ "screen": target }).to_string();
        let https = crate::library::agent(identity, pin)?;
        let api = HostApi::new(addr, mgmt_port, stream_port, identity, pin);
        let json = Some(("application/json", body.as_bytes()));
        match api.call(&https, "PUT", &path, json)?.status {
            200..=299 => Ok(()),
            code => Err(LibraryError::Http(code)),
        }
    };
    let said = put().map_err(|e| e.to_string());
    // Whatever the answer, the host's rows may have moved: ask again, keeping the old rows up.
    TARGETS.expire(fp_hex);
    refresh(addr, mgmt_port, stream_port, fp_hex);
    said
}

/// 20 s. `in_use_by` moves as other devices start and end streams; each refresh is a TLS
/// handshake, so this is the rate limit, not the display cadence.
#[cfg(desktop)]
pub const TTL: std::time::Duration = std::time::Duration::from_secs(20);

/// One answer per host fingerprint, so the console, GTK and Windows cards agree.
#[cfg(desktop)]
static TARGETS: crate::library::FpCache<Targets> =
    crate::library::FpCache::new(TTL, "punktfunk-screens");

/// What this host last answered. `None` until [`refresh`] lands, and for a host without the route.
#[cfg(desktop)]
pub fn cached(fp_hex: &str) -> Option<Targets> {
    TARGETS.get(fp_hex).into_iter().next()
}

/// Ask the host for its rows unless [`TTL`] says the last answer still stands. Idempotent;
/// call it on any shell tick.
#[cfg(desktop)]
pub fn refresh(addr: &str, mgmt_port: u16, stream_port: u16, fp_hex: &str) {
    TARGETS.refresh(
        addr,
        (mgmt_port, stream_port),
        fp_hex,
        |addr, mgmt, stream, identity, pin| {
            fetch_targets(addr, mgmt, stream, identity, pin)
                .into_iter()
                .collect()
        },
        |_| true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"{ "locked": false, "host_default": "own", "yours": {"monitor": "HDMI-A-1"},
      "targets": [
        { "id": "own", "label": "Own screen" },
        { "id": "HDMI-A-1", "label": "LG TV", "connector": "HDMI-A-1", "mode": "3840x2160@60",
          "primary": true, "enabled": true, "in_use_by": null },
        { "id": "DP-2", "label": "DELL U2723QE", "connector": "DP-2", "mode": "3840x2160@60",
          "primary": false, "enabled": false, "in_use_by": null } ] }"#;

    #[test]
    fn the_plans_body_parses() {
        let t: Targets = serde_json::from_str(PLAN).unwrap();
        assert!(!t.locked);
        assert_eq!(t.host_default, DisplayTarget::Own);
        let tv = DisplayTarget::Monitor("HDMI-A-1".into());
        assert_eq!(t.yours.as_ref(), Some(&tv));
        assert_eq!(t.current(), &tv);
        let own = &t.targets[0];
        assert_eq!(own.target(), DisplayTarget::Own);
        assert!(own.enabled, "an own row without the field is pickable");
        assert_eq!(own.name(), "Own screen");
        assert_eq!(own.reason(), None);
        assert_eq!(t.targets[1].name(), "LG TV");
        assert_eq!(t.targets[1].detail(), "HDMI-A-1 \u{b7} 3840x2160@60");
        assert_eq!(t.targets[2].reason().as_deref(), Some("Off"));
        assert_eq!(t.summary(), "Streams LG TV");
    }

    #[test]
    fn yours_absent_or_null_follows_the_host() {
        for body in [
            r#"{"host_default": {"monitor": "DP-1"}, "targets": []}"#,
            r#"{"host_default": {"monitor": "DP-1"}, "yours": null}"#,
        ] {
            let t: Targets = serde_json::from_str(body).unwrap();
            assert_eq!(t.yours, None);
            assert_eq!(t.current(), &DisplayTarget::Monitor("DP-1".into()));
            assert_eq!(
                t.summary(),
                "Streams DP-1",
                "an unlisted head names its connector"
            );
        }
        let empty: Targets = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.summary(), "Own screen");
    }

    #[test]
    fn a_target_serialises_as_the_host_spells_it() {
        let json = |t: &DisplayTarget| serde_json::to_value(t).unwrap();
        assert_eq!(json(&DisplayTarget::Own), serde_json::json!("own"));
        assert_eq!(
            json(&DisplayTarget::Monitor("HDMI-A-1".into())),
            serde_json::json!({"monitor": "HDMI-A-1"})
        );
        let body = serde_json::json!({ "screen": DisplayTarget::Own });
        assert_eq!(body.to_string(), r#"{"screen":"own"}"#);
    }

    #[test]
    fn a_taken_head_says_who_has_it() {
        let row = Target {
            id: "HDMI-A-1".into(),
            label: "HDMI-A-1".into(),
            connector: Some("HDMI-A-1".into()),
            mode: Some("1920x1080@60".into()),
            in_use_by: Some("Ben's Apple TV".into()),
            ..Target::default()
        };
        assert_eq!(row.reason().as_deref(), Some("Streamed by Ben's Apple TV"));
        assert_eq!(
            row.detail(),
            "1920x1080@60",
            "the label already names the connector"
        );
    }
}
