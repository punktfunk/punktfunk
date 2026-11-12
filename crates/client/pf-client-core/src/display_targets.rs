//! Which host screen this device streams, and how: the host's answer, the Screen section every
//! shell draws from it ([`Targets::rows`]), and the write of this device's pick ([`set_pick`]).
//!
//! Same mTLS lane as [`crate::host_actions`]: device identity, fingerprint pin, `mgmt_port`.
//! The host names the monitors; this app words the roles. The pick is the device's own row on
//! the host, so no client stores it. `clients/shared/screen-rows-vectors.json` pins the rows
//! for the Swift and Kotlin copies.

use serde::{Deserialize, Deserializer, Serialize};

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

/// How this device uses the host's screens when it streams its own: the host's monitors off,
/// on with apps opening here, or on with this one more screen beside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Only,
    Main,
    Second,
}

impl Role {
    /// The Screen section's order.
    pub const ALL: [Role; 3] = [Role::Only, Role::Main, Role::Second];

    /// The host's word, which is also the row's id.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Only => "only",
            Role::Main => "main",
            Role::Second => "second",
        }
    }

    /// The `topology` a pick of this role writes.
    pub fn topology(self) -> &'static str {
        match self {
            Role::Only => "exclusive",
            Role::Main => "primary",
            Role::Second => "extend",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Only => pf_l10n::client::screen_role_only(),
            Role::Main => pf_l10n::client::screen_role_main(),
            Role::Second => pf_l10n::client::screen_role_second(),
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Role::Only => pf_l10n::client::screen_role_only_hint(),
            Role::Main => pf_l10n::client::screen_role_main_hint(),
            Role::Second => pf_l10n::client::screen_role_second_hint(),
        }
    }
}

/// The answer's `role`: present only where this device's own screen can take a role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoleInfo {
    /// The role of this device's own pick; `None` follows [`Self::host_default`].
    pub yours: Option<Role>,
    pub host_default: Role,
    /// The roles this host can give, in any order.
    pub offered: Vec<Role>,
}

impl RoleInfo {
    /// The role a connect on the own screen gets now.
    pub fn current(&self) -> Role {
        self.yours.unwrap_or(self.host_default)
    }
}

/// `role` never fails the answer: a word this build doesn't know drops out of `offered`, reads
/// as `None` in `yours` and as [`Role::Only`] in `host_default`. Anything but an object is no role.
fn lenient_role<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RoleInfo>, D::Error> {
    use serde_json::Value;
    let v = Value::deserialize(d)?;
    let role = |v: &Value| Role::deserialize(v).ok();
    Ok(v.as_object().map(|o| RoleInfo {
        yours: o.get("yours").and_then(role),
        host_default: o.get("host_default").and_then(role).unwrap_or(Role::Only),
        offered: o
            .get("offered")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(role).collect())
            .unwrap_or_default(),
    }))
}

/// `GET /api/v1/display/targets` for this device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Targets {
    /// The host's settings file pins the screen: the rows are not offered.
    pub locked: bool,
    /// This device may pick. A host that omits it predates the grant and lets every device.
    pub may_choose: bool,
    pub host_default: DisplayTarget,
    /// This device's own row; `None` follows [`Self::host_default`].
    pub yours: Option<DisplayTarget>,
    pub targets: Vec<Target>,
    #[serde(deserialize_with = "lenient_role")]
    pub role: Option<RoleInfo>,
}

impl Default for Targets {
    fn default() -> Self {
        Targets {
            locked: false,
            may_choose: true,
            host_default: DisplayTarget::Own,
            yours: None,
            targets: Vec::new(),
            role: None,
        }
    }
}

/// The body of `PUT /display/clients/{fp}`: the screen, and for a role its topology.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pick {
    pub screen: DisplayTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topology: Option<String>,
}

/// What a row of the Screen section stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Role(Role),
    Monitor,
    /// The own-screen row of a host that sends no `role`.
    Own,
}

/// One row of the Screen section, worded. A shell draws `label`, `detail` as its value, and
/// `reason`, else `hint`, under it. `header`, when set, heads a group from this row on.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenRow {
    pub kind: RowKind,
    /// `only` · `main` · `second`, a connector, or `own`: stable across refreshes.
    pub id: String,
    pub label: String,
    pub hint: Option<&'static str>,
    /// A monitor's connector and mode; empty for the rest.
    pub detail: String,
    pub header: Option<&'static str>,
    /// What a connect gets now.
    pub checked: bool,
    /// False draws the row greyed: a head off or taken, or the host decides.
    pub pickable: bool,
    /// Why a monitor can't be streamed now.
    pub reason: Option<String>,
    /// What picking the row writes; `None` when it is checked or not pickable.
    pub pick: Option<Pick>,
}

impl ScreenRow {
    fn role(role: Role, checked: bool) -> ScreenRow {
        ScreenRow {
            kind: RowKind::Role(role),
            id: role.as_str().into(),
            label: role.label().into(),
            hint: Some(role.hint()),
            detail: String::new(),
            header: None,
            checked,
            pickable: true,
            reason: None,
            pick: Some(Pick {
                screen: DisplayTarget::Own,
                topology: Some(role.topology().into()),
            }),
        }
    }

    fn target(t: &Target, current: &DisplayTarget) -> ScreenRow {
        let reason = t.reason();
        ScreenRow {
            kind: match t.id.as_str() {
                OWN => RowKind::Own,
                _ => RowKind::Monitor,
            },
            id: t.id.clone(),
            label: t.name().into(),
            hint: None,
            detail: t.detail(),
            header: None,
            checked: t.target() == *current,
            pickable: reason.is_none(),
            reason,
            pick: Some(Pick {
                screen: t.target(),
                topology: None,
            }),
        }
    }
}

impl Targets {
    /// The pick a connect gets now: this device's row, else the host's default.
    pub fn current(&self) -> &DisplayTarget {
        self.yours.as_ref().unwrap_or(&self.host_default)
    }

    /// Why nothing in [`Self::rows`] can be picked: the settings file, else the missing grant.
    pub fn note(&self) -> Option<&'static str> {
        if self.locked {
            Some(pf_l10n::client::screen_locked_hint())
        } else if !self.may_choose {
            Some(pf_l10n::client::screen_host_decides())
        } else {
            None
        }
    }

    /// The Screen section. With `role`: the offered roles as only, main, second, then the
    /// monitors under [`pf_l10n::client::screen_show_monitor`], the own row left out. Without:
    /// one row per target in the host's order. Checked is the monitor this device streams,
    /// else its role. Under a [`Self::note`] only the checked row stays, greyed.
    pub fn rows(&self) -> Vec<ScreenRow> {
        let current = self.current();
        let mut rows: Vec<ScreenRow> = match &self.role {
            Some(role) => {
                let own = *current == DisplayTarget::Own;
                let roles = Role::ALL
                    .into_iter()
                    .filter(|r| role.offered.contains(r))
                    .map(|r| ScreenRow::role(r, own && r == role.current()));
                let monitors = self.targets.iter().filter(|t| t.id != OWN).enumerate();
                let monitors = monitors.map(|(i, t)| ScreenRow {
                    header: (i == 0).then(pf_l10n::client::screen_show_monitor),
                    ..ScreenRow::target(t, current)
                });
                roles.chain(monitors).collect()
            }
            None => self
                .targets
                .iter()
                .map(|t| ScreenRow::target(t, current))
                .collect(),
        };
        let decides = self.note().is_some();
        if decides {
            rows.retain(|r| r.checked);
        }
        for r in &mut rows {
            if decides {
                (r.pickable, r.header) = (false, None);
            }
            if r.checked || !r.pickable {
                r.pick = None;
            }
        }
        rows
    }

    /// The card's line: "Streams LG TV" for a monitor, the role's name for a main or second
    /// screen. `None` for the only screen and an older host's own screen: the default is silent.
    pub fn card_line(&self) -> Option<String> {
        match self.current() {
            DisplayTarget::Monitor(c) => {
                let row = self.targets.iter().find(|t| t.id == *c);
                Some(pf_l10n::client::screen_streams(
                    row.map_or(c.as_str(), Target::name),
                ))
            }
            DisplayTarget::Own => match self.role.as_ref()?.current() {
                Role::Only => None,
                r => Some(r.label().into()),
            },
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

/// `PUT /api/v1/display/clients/{this device's fingerprint}` with a row's [`Pick`], then the
/// host's rows read back into [`cached`] before this returns, so a shell redraws from them at
/// once. `fp_hex` is the host's fingerprint: the pin and the cache key. `Err` is a phrase each
/// shell frames, like [`crate::library::LibraryError`]'s.
#[cfg(desktop)]
pub fn set_pick(
    addr: &str,
    mgmt_port: u16,
    stream_port: u16,
    identity: &(String, String),
    fp_hex: &str,
    pick: &Pick,
) -> Result<(), String> {
    use crate::library::{HostApi, LibraryError};
    let pin = crate::trust::parse_hex32(fp_hex);
    let write = || -> Result<(), LibraryError> {
        let device = punktfunk_core::quic::endpoint::fingerprint_of_pem(&identity.0)
            .map_err(|e| LibraryError::Unreachable(e.to_string()))?;
        let path = format!(
            "/api/v1/display/clients/{}",
            punktfunk_core::fp::hex(&device)
        );
        let body = serde_json::json!(pick).to_string();
        let https = crate::library::agent(identity, pin)?;
        let api = HostApi::new(addr, mgmt_port, stream_port, identity, pin);
        let json = Some(("application/json", body.as_bytes()));
        match api.call(&https, "PUT", &path, json)?.status {
            200..=299 => Ok(()),
            code => Err(LibraryError::Http(code)),
        }
    };
    let said = write().map_err(|e| e.to_string());
    // Whatever the answer, the host's rows may have moved: read them back now. A miss keeps
    // the old rows up.
    if let Some(rows) = fetch_targets(addr, mgmt_port, stream_port, identity, pin) {
        TARGETS.put(fp_hex, vec![rows]);
    }
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
        assert_eq!(t.card_line().as_deref(), Some("Streams LG TV"));
        assert!(
            t.may_choose,
            "a host without the grant lets every device choose"
        );
        assert_eq!(t.role, None);
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
                t.card_line().as_deref(),
                Some("Streams DP-1"),
                "an unlisted head names its connector"
            );
        }
        let empty: Targets = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Targets::default());
        assert_eq!(empty.card_line(), None);
    }

    #[test]
    fn a_target_serialises_as_the_host_spells_it() {
        let json = |t: &DisplayTarget| serde_json::to_value(t).unwrap();
        assert_eq!(json(&DisplayTarget::Own), serde_json::json!("own"));
        assert_eq!(
            json(&DisplayTarget::Monitor("HDMI-A-1".into())),
            serde_json::json!({"monitor": "HDMI-A-1"})
        );
        let pick = |topology: Option<&str>| Pick {
            screen: DisplayTarget::Own,
            topology: topology.map(String::from),
        };
        let body = |p: Pick| serde_json::json!(p).to_string();
        assert_eq!(body(pick(None)), r#"{"screen":"own"}"#);
        assert_eq!(
            body(pick(Some("extend"))),
            r#"{"screen":"own","topology":"extend"}"#
        );
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

    /// A vector text slot as this build words it: null, the host's string, or a message by key
    /// with its arguments by name.
    fn text(v: &serde_json::Value) -> Option<String> {
        if v.is_null() || v.is_string() {
            return v.as_str().map(String::from);
        }
        let key = v["key"].as_str().unwrap();
        let m = pf_l10n::get(key).unwrap_or_else(|| panic!("no message {key}"));
        let args: Vec<(String, String)> = v["args"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let args: Vec<(&str, &dyn std::fmt::Display)> = args
            .iter()
            .map(|(k, v)| (k.as_str(), v as &dyn std::fmt::Display))
            .collect();
        Some(m.format(&args))
    }

    /// `clients/shared/screen-rows-vectors.json`: each answer's rows, note and card line.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../../clients/shared/screen-rows-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).unwrap();
        for case in file["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let t: Targets = serde_json::from_value(case["targets"].clone())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let got: Vec<serde_json::Value> = t
                .rows()
                .iter()
                .map(|r| {
                    let kind = match r.kind {
                        RowKind::Role(_) => "role",
                        RowKind::Monitor => "monitor",
                        RowKind::Own => "own",
                    };
                    serde_json::json!({
                        "kind": kind, "id": r.id, "label": r.label, "hint": r.hint,
                        "detail": r.detail, "header": r.header, "checked": r.checked,
                        "pickable": r.pickable, "reason": r.reason, "put": r.pick,
                    })
                })
                .collect();
            let want: Vec<serde_json::Value> = case["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| {
                    let mut w = w.clone();
                    for slot in ["label", "hint", "header", "reason"] {
                        w[slot] = text(&w[slot]).into();
                    }
                    w
                })
                .collect();
            assert_eq!(got, want, "{name}");
            assert_eq!(t.note().map(String::from), text(&case["note"]), "{name}");
            assert_eq!(t.card_line(), text(&case["card"]), "{name}");
        }
    }
}
