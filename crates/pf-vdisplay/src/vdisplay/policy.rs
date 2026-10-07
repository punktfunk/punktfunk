//! Virtual-display management policy: create, keep-alive, and arrange.
//!
//! Pure config above the per-compositor [`VirtualDisplay`](super) backends.
//! [`DisplayPolicy`] and named [`Preset`]s persist to
//! `<config>/display-settings.json`; the web console edits them.
//!
//! Precedence matches GPU preference (console > env pin > default): a valid
//! `display-settings.json` wins; if it is absent,
//! [`DisplayPolicyStore::configured`] is `None` and call sites keep their
//! env/default path. The store is re-read on acquire/teardown, so a console
//! write applies on the next connect without a host restart.
//!
//! Evidence: `design/display-management.md`. Tests cover preset expansion,
//! [`DisplayPolicy::effective`], and [`KeepAlive`] linger. The store follows
//! `gpu.rs`: private dir, temp-write + atomic rename, in-memory rollback
//! on a failed write.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

/// Linger after the last client detaches. Tagged on `mode` so the web form
/// and OpenAPI stay `{"mode":"off"}` / `{"mode":"duration","seconds":N}` /
/// `{"mode":"forever"}`. On gamescope's bare spawn this also keeps the
/// nested session and its game.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum KeepAlive {
    Off,
    /// Linger `seconds` after the last session leaves; a reconnect inside
    /// the window reuses the display.
    Duration {
        /// Linger seconds, clamped to `0..=86400` on write. Longer is
        /// `forever` in practice; unclamped `u32` is ~136 years and a
        /// nonsense `expires_in_ms`.
        seconds: u32,
    },
    /// Until host shutdown or `POST /display/release` (force-releases
    /// `Pinned` like `Lingering`). The `gaming-rig` preset selects this.
    Forever,
}

impl Default for KeepAlive {
    fn default() -> Self {
        // 10 s covers a client's own reconnect (mode change, network blip)
        // without leaving a leftover virtual display if they walk away.
        KeepAlive::Duration { seconds: 10 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Linger {
    Immediate,
    For(Duration),
    /// Never auto-tear-down (`Pinned`).
    Forever,
}

impl KeepAlive {
    pub fn linger(self) -> Linger {
        match self {
            KeepAlive::Off => Linger::Immediate,
            KeepAlive::Duration { seconds } => Linger::For(Duration::from_secs(seconds as u64)),
            KeepAlive::Forever => Linger::Forever,
        }
    }
}

/// Host topology while managed virtual displays are up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// Resolved at acquire ([`super::effective_topology`]): exclusive on
    /// Windows and auto-detected Linux desktop; extend under an explicit
    /// `PUNKTFUNK_COMPOSITOR` pin.
    #[default]
    Auto,
    /// Add the virtual display(s); leave physical outputs alone.
    Extend,
    /// Group's primary virtual display becomes OS primary; physicals stay on.
    Primary,
    /// Only managed virtual displays stay enabled; physicals restore on teardown.
    Exclusive,
}

/// Admission when a new client asks for a different mode than the live
/// display. [`super::admission`] runs this before Welcome so `reject` is a
/// handshake error, not a half-built session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModeConflict {
    /// Own virtual display on the same desktop.
    #[default]
    Separate,
    /// Stop existing sessions, reconfigure, serve the new client.
    Steal,
    /// Admit the new client at the live display's mode.
    Join,
    Reject,
}

/// Stable identity so DEs persist per-display config (KDE scaling). Carried
/// as Windows EDID serial + IddCx connector index, KWin per-slot output
/// name, and the host-persisted Mutter scale map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Identity {
    Shared,
    #[default]
    PerClient,
    /// One identity per (client, resolution). Distinct scaling per mode
    /// burns slot ids.
    PerClientMode,
}

/// Arrangement in desktop space. Computed only in `layout::arrange`:
/// `/display/state` and (Linux, KWin only) position apply both consume it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutMode {
    /// Left-to-right in acquire order, top-aligned.
    #[default]
    AutoRow,
    /// Per-identity-slot offsets from [`Layout::positions`].
    Manual,
}

/// Desktop-space offset (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Layout {
    #[serde(default)]
    pub mode: LayoutMode,
    /// Canonical decimal identity-slot ids (`"1"`..`"15"`) — the exact
    /// string `arrange` looks up. [`DisplayPolicy::sanitized`] maps `"01"`
    /// → `"1"` and drops non-ids; a key that never matches is a pin the
    /// console still shows while every session auto-rows past it.
    #[serde(default)]
    pub positions: BTreeMap<String, Position>,
}

/// How a session that launches a game is served
/// (`design/gamemode-and-dedicated-sessions.md`). Top-level
/// [`DisplayPolicy`] field, not part of [`EffectivePolicy`], so a preset
/// never clobbers it. Linux-only in effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GameSession {
    /// Launch rides the box's current session (managed Steam, bare spawn,
    /// or the live desktop on KWin/Mutter/wlroots).
    #[default]
    Auto,
    /// Own headless gamescope at the client's mode, game only. Degrades to
    /// `auto` when gamescope is missing.
    Dedicated,
}

/// Where a library launch's windows open on the streamed head.
///
/// Default `own`: the player gets the game on an empty workspace instead of
/// the operator's desk. Honoured only by the backends that can place a launch
/// (`claim_workspace`); everywhere else a launch is always `current`.
///
/// [`DisplayPolicy`] field, not part of [`EffectivePolicy`]: a preset never
/// clobbers it. A library entry's own `on_window.workspace` outranks it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePlacement {
    /// An empty workspace on the streamed head, released with the launch.
    #[default]
    Own,
    /// Whatever the head already shows.
    Current,
}

/// Named bundle of the fields below. `Custom` uses the explicit fields;
/// any other preset ignores them and expands ([`DisplayPolicy::effective`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    #[default]
    Custom,
    Default,
    /// Headless/couch: displays + game survive disconnects; next client takes over.
    GamingRig,
    /// Physical desktop: never blank real monitors, never linger virtuals.
    SharedDesktop,
    /// One user at a time with fast reattach; a second user is refused.
    Hotdesk,
    /// Multi-monitor: manual arrangement, per-client identity, exclusive.
    Workstation,
}

/// File + mgmt GET/PUT shape. When [`preset`](Self::preset) is not
/// [`Preset::Custom`], explicit fields are ignored; [`effective`](Self::effective)
/// resolves both to [`EffectivePolicy`].
// Not `Eq`: a per-device overlay carries a scale, and a float has no total equality.
// Nothing compares policies for anything but "did this change", which `PartialEq` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DisplayPolicy {
    /// Schema version. Unknown versions load best-effort
    /// ([`DisplayPolicyStore::load_from`] warns) and write pins current.
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub preset: Preset,
    #[serde(default)]
    pub keep_alive: KeepAlive,
    #[serde(default)]
    pub topology: Topology,
    #[serde(default)]
    pub mode_conflict: ModeConflict,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default)]
    pub layout: Layout,
    /// Simultaneous live virtual displays. Clamped to `1..=16` (connector ceiling).
    #[serde(default = "default_max_displays")]
    pub max_displays: u32,
    /// Game-launch routing. Orthogonal to `preset`; `#[serde(default)]` is
    /// `Auto` so older `display-settings.json` files stay untouched.
    #[serde(default)]
    pub game_session: GameSession,
    /// Default for a launch whose library entry names no `on_window.workspace`.
    /// Orthogonal to `preset`; `#[serde(default)]` is `own`, so an older
    /// `display-settings.json` opts in with the rest.
    #[serde(default)]
    pub launch_workspace: WorkspacePlacement,
    /// Windows: DDC/CI panel off (VCP 0xD6) before Exclusive isolate, on at
    /// restore. Cuts standby auto-input-scan / DP link churn on a dark
    /// physical. Best-effort; no DDC/CI → skip. Orthogonal to `preset`; default off.
    #[serde(default)]
    pub ddc_power_off: bool,
    /// Windows: disable the PnP nodes of the physical monitors the isolate
    /// switched off, for the stream; re-enabled at teardown. Persistent so a
    /// re-HPD stays off. Inactive externals are [`standby_sink_neutralise`]'s.
    /// A crash journal re-enables leftovers. Orthogonal to `preset`; default
    /// on, and a v1 file migrates to on.
    #[serde(default = "yes")]
    pub pnp_disable_monitors: bool,
    /// Windows/AMD: pin connector EDID emulation while streaming
    /// (`pf_win_display::adl_emul`). Locked at first Exclusive isolate
    /// before physicals deactivate (awake sink answers the live-EDID read),
    /// unlocked at last-member teardown, crash-journaled. Inert without
    /// `atiadlxx.dll`. Orthogonal to `preset`; default off.
    #[serde(default)]
    pub edid_lock: bool,
    /// Stream this physical connector (`DP-1`, `HDMI-A-2`) instead of a
    /// virtual display; `None` is the virtual path. Host-wide, orthogonal
    /// to `preset`. `PUNKTFUNK_CAPTURE_MONITOR` overrides it so `host.env`
    /// can pin without a console write undoing it.
    #[serde(default)]
    pub capture_monitor: Option<String>,
    /// Connectors that stay lit while streaming, even under `exclusive`
    /// (`design/web-console-overhaul.md` §5.5).
    ///
    /// The shared-desktop case the all-or-nothing topology axis cannot express: the
    /// couch TV streams on the sole screen while the desk monitor stays usable. Only
    /// meaningful under `exclusive` — nothing else turns a monitor off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep_monitors: Vec<String>,
    /// Per-device deviations, keyed by pairing fingerprint — what identity
    /// slots, admission and the device list already key on (never an address:
    /// a dual-boot box keeps its fingerprint and changes its IP).
    ///
    /// Written only through `/display/clients/{fp}`; the host-wide PUT refuses
    /// a `clients` key outright, so a stale console cannot round-trip the
    /// whole map back over a change it never saw.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub clients: BTreeMap<String, ClientOverlay>,
}

/// One paired device's deviations from the host policy
/// (`design/web-console-overhaul.md` §6.1).
///
/// Every field is optional and absent means **follow the host**. That is the
/// whole point: a copied policy would silently stop following host changes,
/// while an overlay only pins what the operator actually chose for this
/// device. The TV wants take-over and keep-forever; the tablet wants its own
/// screen and no linger — one host policy cannot serve both.
///
/// `max_displays` and `layout` are deliberately absent: they are properties of
/// the host's desktop, not of a device connecting to it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ClientOverlay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<KeepAlive>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<Topology>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_conflict: Option<ModeConflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_session: Option<GameSession>,
    /// Mirror this connector for this device only. Absent follows the host,
    /// which is also the only way back to a virtual screen — a device cannot
    /// opt OUT of a host-wide pin. Nobody has asked to; the reverse direction
    /// is what the field exists for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_monitor: Option<String>,
    /// Scale this device's screen is created at, when the desktop has not already
    /// remembered one for it (`display-management.md` §5.4). Mutter mints a fresh EDID
    /// serial per session, so its own `monitors.xml` never rematches — without this the
    /// operator re-sets the scale on every connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Largest mode this device is granted, `WIDTHxHEIGHT@HZ`. A phone asking for 4K120
    /// on a weak host degrades every other session; the operator caps it once and the
    /// client is told the smaller mode rather than silently given one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mode: Option<String>,
}

/// `WIDTHxHEIGHT@HZ` → the three numbers. Shared by the cap and its test.
pub fn parse_mode(spec: &str) -> Option<(u32, u32, u32)> {
    let (size, hz) = spec.trim().split_once('@')?;
    let (w, h) = size.split_once(['x', 'X'])?;
    Some((
        w.trim().parse().ok()?,
        h.trim().parse().ok()?,
        hz.trim().parse().ok()?,
    ))
}

impl ClientOverlay {
    /// Nothing pinned — the operator reset every field, so the record can go.
    pub fn is_empty(&self) -> bool {
        *self == ClientOverlay::default()
    }

    /// What follows this device onto a seat: its cap and its scale.
    pub fn seat_part(self) -> ClientOverlay {
        ClientOverlay {
            max_mode: self.max_mode,
            scale: self.scale,
            ..ClientOverlay::default()
        }
    }

    /// Same clamps a host-wide write gets: an overlay must not be able to
    /// smuggle a linger window a direct PUT would refuse.
    pub fn sanitized(mut self) -> Self {
        self.keep_alive = self.keep_alive.map(clamp_keep_alive);
        self.capture_monitor = self
            .capture_monitor
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // The range every desktop actually offers. A 0 or a negative would reach a
        // compositor as a scale and is not a smaller screen, it is a broken one.
        self.scale = self.scale.filter(|s| (0.25..=6.0).contains(s));
        // Stored only if it parses: an unreadable cap would silently grant everything,
        // which is the opposite of what the operator asked for.
        self.max_mode = self
            .max_mode
            .filter(|spec| parse_mode(spec).is_some_and(|(w, h, hz)| w > 0 && h > 0 && hz > 0));
        self
    }
}

/// Schema this host writes. A newer file loads best-effort with a warning; an
/// older one migrates in [`DisplayPolicyStore::parse`]. 2: monitor PnP disable
/// on by default.
const CURRENT_VERSION: u32 = 2;

/// Cap on `KeepAlive::Duration.seconds` (24 h). Longer is `forever`;
/// unclamped `u32` is a deadline the reaper never reaches. `Forever` stays
/// releasable via `POST /display/release`.
const MAX_KEEP_ALIVE_SECS: u32 = 24 * 60 * 60;

/// Highest identity-slot id (`identity` `MAX_ID`). Mirrored: the slot table
/// is private. A key above this can never match and is dropped on write.
const MAX_IDENTITY_SLOT: u32 = 15;

fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}
fn default_max_displays() -> u32 {
    4
}

impl Default for DisplayPolicy {
    fn default() -> Self {
        // Bit-for-bit the `default` preset, so an unconfigured host matches
        // the un-overridden call sites.
        DisplayPolicy {
            version: CURRENT_VERSION,
            preset: Preset::Custom,
            keep_alive: KeepAlive::default(),
            topology: Topology::Auto,
            mode_conflict: ModeConflict::default(),
            identity: Identity::default(),
            layout: Layout::default(),
            max_displays: 4,
            game_session: GameSession::default(),
            launch_workspace: WorkspacePlacement::default(),
            ddc_power_off: false,
            pnp_disable_monitors: true,
            edid_lock: false,
            capture_monitor: None,
            keep_monitors: Vec::new(),
            clients: BTreeMap::new(),
        }
    }
}

/// The six axes after preset expansion. What lifecycle/registry read, and
/// what mgmt echoes as in-force.
///
/// Every field is required on the wire. This type is also
/// [`CustomPresetInput::fields`] (`POST/PUT /display/presets`) and a
/// response member three times. `#[serde(default)]` would turn
/// `{"name":"Kiosk","fields":{}}` into a 201 storing six unchosen axes and
/// make all six optional in OpenAPI. Catalog tolerance for older entries
/// lives on the read path only: [`StoredEffectivePolicy`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EffectivePolicy {
    pub keep_alive: KeepAlive,
    pub topology: Topology,
    pub mode_conflict: ModeConflict,
    pub identity: Identity,
    pub layout: Layout,
    pub max_displays: u32,
}

/// Hex form of a peer fingerprint — the key `clients` is stored under, and what
/// the pairing store and the device list already use.
pub fn fp_hex(fp: Option<[u8; 32]>) -> Option<String> {
    fp.map(hex::encode)
}

impl DisplayPolicy {
    /// This device's overlay, if it has one.
    pub fn overlay_for(&self, fp: Option<&str>) -> Option<&ClientOverlay> {
        self.clients.get(fp?)
    }

    /// `host.effective() ⊕ clients[fp]`, field-wise.
    ///
    /// `layout` and `max_displays` stay host-wide — they describe the desktop
    /// every device shares, so letting one device move them would move them
    /// for all of them.
    pub fn effective_for(&self, fp: Option<&str>) -> EffectivePolicy {
        let mut e = self.effective();
        let Some(o) = self.overlay_for(fp) else {
            return e;
        };
        if let Some(v) = o.keep_alive {
            e.keep_alive = v;
        }
        if let Some(v) = o.topology {
            e.topology = v;
        }
        if let Some(v) = o.mode_conflict {
            e.mode_conflict = v;
        }
        if let Some(v) = o.identity {
            e.identity = v;
        }
        e
    }

    /// Game-launch routing for this device, else the host's.
    pub fn game_session_for(&self, fp: Option<&str>) -> GameSession {
        self.overlay_for(fp)
            .and_then(|o| o.game_session)
            .unwrap_or(self.game_session)
    }

    /// The connector this device mirrors, else the host's pin.
    pub fn capture_monitor_for(&self, fp: Option<&str>) -> Option<String> {
        self.overlay_for(fp)
            .and_then(|o| o.capture_monitor.clone())
            .or_else(|| self.capture_monitor.clone())
    }

    /// Scale to create this device's screen at, if the operator set one.
    pub fn scale_for(&self, fp: Option<&str>) -> Option<f64> {
        self.overlay_for(fp).and_then(|o| o.scale)
    }

    /// Clamp a requested mode to this device's cap.
    ///
    /// Returns the mode to grant. Each axis is capped on its own: a device capped at
    /// 2560x1440@60 asking for 3840x2160@120 gets 2560x1440@60, and one asking for
    /// 1920x1080@120 keeps its size and loses only the refresh it cannot have.
    pub fn cap_mode(&self, fp: Option<&str>, want: (u32, u32, u32)) -> (u32, u32, u32) {
        self.cap_mode_or(fp, want, None)
    }

    /// [`Self::cap_mode`], with `fallback` for a device without a cap of its own.
    pub fn cap_mode_or(
        &self,
        fp: Option<&str>,
        want: (u32, u32, u32),
        fallback: Option<&str>,
    ) -> (u32, u32, u32) {
        let Some(cap) = self
            .overlay_for(fp)
            .and_then(|o| o.max_mode.as_deref())
            .or(fallback)
            .and_then(parse_mode)
        else {
            return want;
        };
        (want.0.min(cap.0), want.1.min(cap.1), want.2.min(cap.2))
    }

    pub fn effective(&self) -> EffectivePolicy {
        if let Some(mut e) = preset_fields(self.preset) {
            // A preset fixes the six axes; workstation still honors an
            // explicit positions table (data, not behavior).
            if self.preset == Preset::Workstation && !self.layout.positions.is_empty() {
                e.layout.positions = self.layout.positions.clone();
            }
            e
        } else {
            EffectivePolicy {
                keep_alive: self.keep_alive,
                topology: self.topology,
                mode_conflict: self.mode_conflict,
                identity: self.identity,
                layout: self.layout.clone(),
                max_displays: self.max_displays,
            }
        }
    }

    /// Clamp on write and on load. `max_displays` to `1..=16` (connector
    /// ceiling), linger to `MAX_KEEP_ALIVE_SECS`, layout keys to slot ids.
    pub fn sanitized(mut self) -> Self {
        self.version = CURRENT_VERSION;
        self.max_displays = self.max_displays.clamp(1, 16);
        self.keep_alive = clamp_keep_alive(self.keep_alive);
        self.layout.positions = canonical_positions(std::mem::take(&mut self.layout.positions));
        // A cleared picker sends `""`; that is "no pin", not a monitor named empty.
        self.capture_monitor = self
            .capture_monitor
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // A connector named twice, or blank, is one the operator cannot have meant.
        self.keep_monitors = std::mem::take(&mut self.keep_monitors)
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        self.clients = std::mem::take(&mut self.clients)
            .into_iter()
            .map(|(fp, o)| (fp.trim().to_ascii_lowercase(), o.sanitized()))
            // An overlay that pins nothing is "follows host", which is what an
            // absent key already means. Keeping it would show a device as
            // configured in the console while changing nothing.
            .filter(|(fp, o)| !fp.is_empty() && !o.is_empty())
            .collect();
        self
    }
}

/// Shared by [`DisplayPolicy::sanitized`] and [`sanitize_preset_fields`] so
/// a custom preset cannot smuggle a window a direct PUT would refuse.
fn clamp_keep_alive(keep_alive: KeepAlive) -> KeepAlive {
    match keep_alive {
        KeepAlive::Duration { seconds } if seconds > MAX_KEEP_ALIVE_SECS => KeepAlive::Duration {
            seconds: MAX_KEEP_ALIVE_SECS,
        },
        other => other,
    }
}

/// Re-key manual layout pins to canonical slot ids; drop what can never match.
///
/// `layout::arrange` looks up `u32::to_string()`, so `"01"` / `"slot1"` /
/// `"99"` round-trip through GET and then silently no-op. Parse here so
/// `"01"` works and junk is logged at write time. A canonical key wins over
/// a duplicate spelling, independent of `BTreeMap` order.
fn canonical_positions(positions: BTreeMap<String, Position>) -> BTreeMap<String, Position> {
    use std::collections::btree_map::Entry;
    let mut out: BTreeMap<String, Position> = BTreeMap::new();
    for (key, pos) in positions {
        let id = key.parse::<u32>().ok().filter(|id| {
            // Slot table only hands out 1..=MAX_ID; a pin outside cannot exist.
            (1..=MAX_IDENTITY_SLOT).contains(id)
        });
        let Some(id) = id else {
            tracing::warn!(
                key = %key,
                "display layout pin keyed by something that is not an identity slot \
                 (1..={MAX_IDENTITY_SLOT}) — dropping it; it could never have been applied"
            );
            continue;
        };
        let canonical = id.to_string();
        let already_canonical = key == canonical;
        match out.entry(canonical) {
            Entry::Vacant(v) => {
                v.insert(pos);
            }
            Entry::Occupied(mut o) if already_canonical => {
                o.insert(pos);
            }
            Entry::Occupied(_) => {}
        }
    }
    out
}

impl DisplayPolicy {
    /// This policy with a **manual** layout at `positions`. `/display/layout`
    /// uses it so arranging stays orthogonal to every other axis.
    ///
    /// Consumes and returns the WHOLE policy rather than rebuilding one from
    /// [`EffectivePolicy`]. The old shape took the five orthogonal fields as
    /// arguments and named every field it kept, so each field added since had
    /// to be remembered at this one site or arranging silently cleared it —
    /// which is how saving a layout used to drop a capture pin. Now anything
    /// not named here survives, which is the safe direction.
    pub fn with_manual_layout(mut self, positions: BTreeMap<String, Position>) -> DisplayPolicy {
        // Expand BEFORE switching to `Custom`: on a named preset the struct's
        // own axes are whatever was last written, and `Custom` is what makes
        // the new layout take effect at all.
        let e = self.effective();
        self.version = CURRENT_VERSION;
        self.preset = Preset::Custom;
        self.keep_alive = e.keep_alive;
        self.topology = e.topology;
        self.mode_conflict = e.mode_conflict;
        self.identity = e.identity;
        self.max_displays = e.max_displays;
        self.layout = Layout {
            mode: LayoutMode::Manual,
            positions,
        };
        self
    }
}

/// Named-preset expansion; `None` for [`Preset::Custom`]. `presets_match_doc`
/// guards the table.
pub fn preset_fields(preset: Preset) -> Option<EffectivePolicy> {
    let base = |keep_alive, topology, mode_conflict, identity, layout_mode| EffectivePolicy {
        keep_alive,
        topology,
        mode_conflict,
        identity,
        layout: Layout {
            mode: layout_mode,
            positions: BTreeMap::new(),
        },
        max_displays: 4,
    };
    Some(match preset {
        Preset::Custom => return None,
        Preset::Default => base(
            KeepAlive::Duration { seconds: 10 },
            Topology::Auto,
            ModeConflict::Separate,
            Identity::PerClient,
            LayoutMode::AutoRow,
        ),
        Preset::GamingRig => base(
            KeepAlive::Forever,
            Topology::Exclusive,
            ModeConflict::Steal,
            Identity::PerClient,
            LayoutMode::AutoRow,
        ),
        Preset::SharedDesktop => base(
            KeepAlive::Off,
            Topology::Extend,
            ModeConflict::Separate,
            Identity::PerClient,
            LayoutMode::AutoRow,
        ),
        Preset::Hotdesk => base(
            KeepAlive::Duration { seconds: 300 },
            Topology::Exclusive,
            ModeConflict::Reject,
            Identity::PerClientMode,
            LayoutMode::AutoRow,
        ),
        Preset::Workstation => base(
            KeepAlive::Duration { seconds: 300 },
            Topology::Exclusive,
            ModeConflict::Separate,
            Identity::PerClient,
            LayoutMode::Manual,
        ),
    })
}

/// Loaded file (or `None` if absent). Same discipline as
/// `pf_gpu::GpuPrefStore`: private dir, temp-write + atomic rename,
/// in-memory rollback if the disk write fails.
pub struct DisplayPolicyStore {
    path: PathBuf,
    /// `Some` only after a valid file load/write — the gate that lets call
    /// sites override env/default behavior.
    cur: Mutex<Option<DisplayPolicy>>,
    /// Serializes the write: serialize → temp-write → rename → publish to
    /// `cur`. Without it two concurrent PUTs can rename in one order and
    /// publish in the other. Held *around* `cur` so `get` never waits on disk.
    write: Mutex<()>,
    /// A seat host's per-device overlays: the box's, never its own file's.
    box_overlays: Option<BoxOverlays>,
    /// A seat that follows the contract: [`seat_contract`] instead of any stored policy.
    contract: bool,
}

/// The box's `clients` map on a seat host. The box console writes it; the seat re-reads the
/// box's file when its mtime moves, so an overlay edited there applies to the seat's next
/// connect.
struct BoxOverlays {
    path: PathBuf,
    cache: Mutex<(
        Option<std::time::SystemTime>,
        BTreeMap<String, ClientOverlay>,
    )>,
}

impl BoxOverlays {
    fn current(&self) -> BTreeMap<String, ClientOverlay> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if stamp != cache.0 {
            cache.1 = std::fs::read(&self.path)
                .ok()
                .and_then(|bytes| DisplayPolicyStore::parse(&self.path, &bytes))
                .map(|p| p.clients)
                .unwrap_or_default();
            cache.0 = stamp;
        }
        cache.1.clone()
    }
}

impl DisplayPolicyStore {
    /// Missing file ⇒ unconfigured. Corrupt ⇒ per-axis salvage. Unreadable
    /// ⇒ unconfigured + warn (never fail host startup).
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => Self::parse(&path, &bytes),
            // Exists-but-unreadable (EACCES, EIO) is not unconfigured: folding
            // it into silent `None` reverted the console to defaults with no log.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %path.display(),
                    "display-settings.json exists but could not be read ({e}) — this host is \
                     running on BUILT-IN DEFAULTS, not on its configured policy");
                None
            }
        };
        DisplayPolicyStore {
            path,
            cur: Mutex::new(cur),
            write: Mutex::new(()),
            box_overlays: None,
            contract: false,
        }
    }

    /// Per-device overlays come from `box_file` instead of this store's own file.
    fn with_box_overlays(mut self, box_file: Option<PathBuf>) -> Self {
        self.box_overlays = box_file.map(|path| BoxOverlays {
            path,
            cache: Mutex::new((None, BTreeMap::new())),
        });
        self
    }

    fn with_contract(mut self, on: bool) -> Self {
        self.contract = on;
        self
    }

    /// The contract with the box's per-device cap and scale: those follow a device onto any
    /// desktop. The rest of an overlay is the owner's desktop's, and never reaches a seat.
    fn contract_policy(&self) -> DisplayPolicy {
        let mut policy = seat_contract();
        if let Some(b) = &self.box_overlays {
            policy.clients = b
                .current()
                .into_iter()
                .map(|(fp, o)| (fp, o.seat_part()))
                .filter(|(_, o)| !o.is_empty())
                .collect();
        }
        policy
    }

    fn with_box_clients(&self, mut policy: DisplayPolicy) -> DisplayPolicy {
        if let Some(b) = &self.box_overlays {
            policy.clients = b.current();
        }
        policy
    }

    /// Parse with salvage. Split from [`Self::load_from`] so recovery is unit-tested.
    ///
    /// Three layers: (1) strict parse for console-written files; (2) version
    /// check: a future document is announced, not silently treated as ours,
    /// and an older one migrates ([`CURRENT_VERSION`]);
    /// (3) per-axis salvage — a member that fails alone is dropped, the rest
    /// survive. Dropping one axis is smaller than reverting the whole file.
    ///
    /// Two things salvage must not do:
    ///
    /// * `preset` is the selector, not an axis. Its default is `Custom`, which
    ///   would hand governance to leftover explicit fields (`exclusive` +
    ///   `forever` + `steal`). An unreadable preset name refuses the file.
    /// * Salvaging *nothing* must not report configured. `configured()` is the
    ///   console-has-configured gate; `DisplayPolicy::default()` is `PerClient`
    ///   identity, while unconfigured Linux uses `Shared`. Judge "nothing" on
    ///   the result (dropped something and landed on default), not surviving
    ///   keys — serde ignores unknowns, and `version` names no axis.
    fn parse(path: &std::path::Path, bytes: &[u8]) -> Option<DisplayPolicy> {
        let mut value: serde_json::Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(),
                    "display-settings.json is not valid JSON ({e}) — this host is running on \
                     BUILT-IN DEFAULTS, not on its configured policy");
                return None;
            }
        };
        let claimed = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(CURRENT_VERSION as u64);
        if claimed > CURRENT_VERSION as u64 {
            tracing::warn!(path = %path.display(), claimed, current = CURRENT_VERSION,
                "display-settings.json claims a schema version this host does not know — reading it \
                 best-effort (unknown axes are ignored); the next write pins it back to the current \
                 version");
        }
        // v1 wrote the monitor PnP disable while it was off by default: turn it on once. An
        // operator who turns it off again writes a v2 file, which stays off.
        if claimed < 2 {
            if let Some(o) = value.as_object_mut() {
                o.insert("pnp_disable_monitors".into(), serde_json::Value::Bool(true));
            }
        }
        match serde_json::from_value::<DisplayPolicy>(value.clone()) {
            Ok(p) => Some(p.sanitized()),
            Err(e) => {
                let mut obj = match value {
                    serde_json::Value::Object(o) => o,
                    _ => {
                        tracing::warn!(path = %path.display(),
                            "display-settings.json is not a JSON object ({e}) — this host is running \
                             on BUILT-IN DEFAULTS, not on its configured policy");
                        return None;
                    }
                };
                // Probe one member at a time: a one-key document parses iff
                // that value is valid. No hand-maintained field list.
                let probe = |key: &str, member: &serde_json::Value| -> bool {
                    let one = serde_json::Value::Object(
                        std::iter::once((key.to_string(), member.clone())).collect(),
                    );
                    serde_json::from_value::<DisplayPolicy>(one).is_ok()
                };
                // Selector is all-or-nothing (see this function's doc).
                if let Some(preset) = obj.get("preset") {
                    if !probe("preset", preset) {
                        tracing::warn!(path = %path.display(), preset = %preset,
                            "display-settings.json names a preset this host does not know — the \
                             preset SELECTS the other settings, so falling back to its default \
                             would silently activate whatever explicit fields the file happens to \
                             carry; this host is running on BUILT-IN DEFAULTS instead");
                        return None;
                    }
                }
                let before = obj.len();
                obj.retain(|key, member| {
                    let ok = probe(key.as_str(), &*member);
                    if !ok {
                        tracing::warn!(path = %path.display(), field = %key,
                            "display-settings.json carries an unreadable value for this setting — \
                             falling back to its built-in default and keeping the rest of the policy");
                    }
                    ok
                });
                let dropped = before - obj.len();
                match serde_json::from_value::<DisplayPolicy>(serde_json::Value::Object(obj)) {
                    // Dropped everything → not configured. Test the result,
                    // not the key list (unknown members and `version` survive
                    // without teaching us an axis). `Some(default)` would
                    // flip Linux identity Shared → PerClient.
                    Ok(p) => {
                        let p = p.sanitized();
                        if dropped > 0 && p == DisplayPolicy::default() {
                            tracing::warn!(path = %path.display(),
                                "display-settings.json had no setting this host could read — this \
                                 host is running on BUILT-IN DEFAULTS, not on its configured policy");
                            return None;
                        }
                        Some(p)
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(),
                            "display-settings.json unreadable even per-setting ({e}) — this host is \
                             running on BUILT-IN DEFAULTS, not on its configured policy");
                        None
                    }
                }
            }
        }
    }

    /// Stored policy, or [`DisplayPolicy::default`] when unconfigured (mgmt GET). A seat that
    /// follows the contract gets the contract.
    pub fn get(&self) -> DisplayPolicy {
        if self.contract {
            return self.contract_policy();
        }
        self.with_box_clients(self.cur.lock().unwrap().clone().unwrap_or_default())
    }

    /// Console-configured policy, or `None` if no file. `None` ⇒ leave
    /// historical env/default behavior, which is what a contract seat keeps for linger,
    /// topology and identity.
    pub fn configured(&self) -> Option<DisplayPolicy> {
        if self.contract {
            return None;
        }
        let policy = self.cur.lock().unwrap().clone()?;
        Some(self.with_box_clients(policy))
    }

    /// A requested mode clamped to this device's cap, else on a contract seat to the box's
    /// highest mode per seat (`seat_max_mode`).
    pub fn cap_mode(&self, fp: Option<&str>, want: (u32, u32, u32)) -> (u32, u32, u32) {
        let seat_cap = self
            .contract
            .then(pf_host_config::seat_defaults::current)
            .flatten()
            .and_then(|d| d.max_mode);
        self.get().cap_mode_or(fp, want, seat_cap.as_deref())
    }

    pub fn configured_effective(&self) -> Option<EffectivePolicy> {
        self.configured().map(|p| p.effective())
    }

    pub fn ddc_power_off(&self) -> bool {
        self.get().ddc_power_off
    }

    /// PnP-disable the monitors the isolate switched off. Default on;
    /// `PUNKTFUNK_STANDBY_SINK_KEEP` vetoes it with every other PnP disable.
    pub fn pnp_disable_monitors(&self) -> bool {
        self.get().pnp_disable_monitors && self.standby_sink_neutralise()
    }

    pub fn edid_lock(&self) -> bool {
        self.get().edid_lock
    }

    /// Host default for a launch's workspace ([`DisplayPolicy::launch_workspace`]).
    pub fn launch_workspace(&self) -> WorkspacePlacement {
        self.get().launch_workspace
    }

    /// Neutralise connected-but-inactive external sinks for the stream.
    /// Default on ([`standby_sink_neutralise`]). Monitors the isolate switched
    /// off are [`Self::pnp_disable_monitors`]'s, not this one's.
    pub fn standby_sink_neutralise(&self) -> bool {
        standby_sink_neutralise(std::env::var("PUNKTFUNK_STANDBY_SINK_KEEP").ok().as_deref())
    }

    /// Persist + adopt. Memory changes only after the disk write; the
    /// whole transaction holds [`Self::write`].
    pub fn set(&self, policy: DisplayPolicy) -> Result<()> {
        self.refuse_on_contract()?;
        let _tx = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.store(policy)
    }

    /// `edit` the stored policy and persist it, the read inside the same
    /// transaction: a writer that read, edited and then [`Self::set`] lost
    /// whatever another writer saved in between. `edit` returning `false`
    /// writes nothing; so does the result.
    pub fn update(&self, edit: impl FnOnce(&mut DisplayPolicy) -> bool) -> Result<bool> {
        self.refuse_on_contract()?;
        let _tx = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut policy = self.get();
        if !edit(&mut policy) {
            return Ok(false);
        }
        self.store(policy)?;
        Ok(true)
    }

    /// Whether this store follows the seat contract, which nothing here can change.
    pub fn follows_contract(&self) -> bool {
        self.contract
    }

    fn refuse_on_contract(&self) -> Result<()> {
        if self.contract {
            anyhow::bail!("a seat's display follows the seat contract");
        }
        Ok(())
    }

    /// The write under [`Self::write`], which the caller holds.
    fn store(&self, policy: DisplayPolicy) -> Result<()> {
        let mut policy = policy.sanitized();
        // A seat's own file never carries the box's overlays it reads.
        if self.box_overlays.is_some() {
            policy.clients.clear();
        }
        pf_paths::replace_secret_file(&self.path, &serde_json::to_vec_pretty(&policy)?)?;
        *self.cur.lock().unwrap() = Some(policy);
        Ok(())
    }
}

/// Process-wide store, loaded once. Same global-accessor shape as
/// `pf_gpu::prefs`: display setup sits deep in capture/vdisplay with no
/// app state threaded through.
pub fn prefs() -> &'static DisplayPolicyStore {
    static STORE: OnceLock<DisplayPolicyStore> = OnceLock::new();
    STORE.get_or_init(|| {
        DisplayPolicyStore::load_from(pf_paths::config_dir().join("display-settings.json"))
            .with_box_overlays(
                pf_paths::seat::trust_dir().map(|dir| dir.join("display-settings.json")),
            )
            .with_contract(pf_paths::seat::follows_contract())
    })
}

/// A seat's desktop (`design/web-console-structure-2026-10.md` §2.1): one virtual screen, the
/// seat's own. It never mirrors, keeps lit or powers off a real monitor, never routes a game to
/// a session of its own, and a second device shares the occupant's screen. Linger, topology and
/// identity stay the unconfigured host's, as they always were on a seat.
pub fn seat_contract() -> DisplayPolicy {
    DisplayPolicy {
        mode_conflict: ModeConflict::Join,
        game_session: GameSession::Auto,
        ddc_power_off: false,
        pnp_disable_monitors: false,
        edid_lock: false,
        capture_monitor: None,
        keep_monitors: Vec::new(),
        ..DisplayPolicy::default()
    }
}

/// Operator-named bundle of the six axes plus game-session, stored in
/// `<config>/display-presets.json`. Applying one writes a `Custom`
/// [`DisplayPolicy`] via `PUT /display/settings`. Editing the catalog never
/// mutates the running policy; re-apply to adopt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CustomPreset {
    /// Host-assigned, stable for the life of the entry.
    pub id: String,
    pub name: String,
    pub fields: EffectivePolicy,
    /// Unlike a built-in preset, applying a custom preset sets this axis.
    #[serde(default)]
    pub game_session: GameSession,
}

/// Create/replace body. No `id` — the host owns it.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct CustomPresetInput {
    pub name: String,
    pub fields: EffectivePolicy,
    #[serde(default)]
    pub game_session: GameSession,
}

fn custom_presets_path() -> PathBuf {
    pf_paths::config_dir().join("display-presets.json")
}

/// Same bounds as [`DisplayPolicy::sanitized`], so a later apply cannot
/// smuggle an out-of-range window past the direct PUT.
fn sanitize_preset_fields(mut fields: EffectivePolicy) -> EffectivePolicy {
    fields.max_displays = fields.max_displays.clamp(1, 16);
    fields.keep_alive = clamp_keep_alive(fields.keep_alive);
    fields.layout.positions = canonical_positions(std::mem::take(&mut fields.layout.positions));
    fields
}

/// Recovered entries plus whether anything was lost. CRUD checks `lossy`
/// before overwrite so a save that would drop entries preserves the original.
struct CatalogRead {
    presets: Vec<CustomPreset>,
    lossy: bool,
}

/// Lenient catalog-read shape: every axis defaulted.
///
/// Persist needs this (older entries, hand-dropped keys). The mgmt API
/// must stay strict: `#[serde(default)]` on [`EffectivePolicy`] turned
/// `{"name":"Kiosk","fields":{}}` into a 201. Private, `Deserialize`-only,
/// reached only from [`parse_catalog`]. Gain a field when
/// [`EffectivePolicy`] does; the `From` is exhaustive so a forgotten axis
/// is a compile error.
#[derive(Deserialize)]
struct StoredEffectivePolicy {
    #[serde(default)]
    keep_alive: KeepAlive,
    #[serde(default)]
    topology: Topology,
    #[serde(default)]
    mode_conflict: ModeConflict,
    #[serde(default)]
    identity: Identity,
    #[serde(default)]
    layout: Layout,
    #[serde(default = "default_max_displays")]
    max_displays: u32,
}

impl From<StoredEffectivePolicy> for EffectivePolicy {
    fn from(s: StoredEffectivePolicy) -> Self {
        let StoredEffectivePolicy {
            keep_alive,
            topology,
            mode_conflict,
            identity,
            layout,
            max_displays,
        } = s;
        EffectivePolicy {
            keep_alive,
            topology,
            mode_conflict,
            identity,
            layout,
            max_displays,
        }
    }
}

/// Lenient catalog-read shape. `id`/`name` stay required — without them
/// it is not a preset; the entry-wise skip keeps the rest.
#[derive(Deserialize)]
struct StoredCustomPreset {
    id: String,
    name: String,
    fields: StoredEffectivePolicy,
    #[serde(default)]
    game_session: GameSession,
}

impl From<StoredCustomPreset> for CustomPreset {
    fn from(s: StoredCustomPreset) -> Self {
        CustomPreset {
            id: s.id,
            name: s.name,
            fields: s.fields.into(),
            game_session: s.game_session,
        }
    }
}

/// Parse **entry-wise**. Pure so recovery is unit-tested.
///
/// Whole-document `from_slice::<Vec<CustomPreset>>` made one bad entry
/// empty the catalog; the next create then renamed that empty vector over
/// the file. A bad entry costs itself; `lossy` tells the caller the disk
/// still holds more than we understood.
fn parse_catalog(bytes: &[u8]) -> CatalogRead {
    let entries: Vec<serde_json::Value> = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e,
                "display-presets.json is not a JSON array of presets — ignoring the custom-preset \
                 catalog; it is preserved as display-presets.json.bad if anything overwrites it");
            return CatalogRead {
                presets: Vec::new(),
                lossy: true,
            };
        }
    };
    let mut presets = Vec::with_capacity(entries.len());
    let mut lossy = false;
    for (i, entry) in entries.into_iter().enumerate() {
        // Keep id/name for the log even when the body is unreadable —
        // "entry 3" is useless next to a console list of names.
        let named = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unnamed>")
            .to_string();
        match serde_json::from_value::<StoredCustomPreset>(entry) {
            Ok(p) => {
                let mut p = CustomPreset::from(p);
                p.fields = sanitize_preset_fields(p.fields);
                presets.push(p);
            }
            Err(e) => {
                lossy = true;
                tracing::warn!(index = i, name = %named, error = %e,
                    "display-presets.json entry is unreadable — skipping just this preset, the rest \
                     of the catalog is kept");
            }
        }
    }
    CatalogRead { presets, lossy }
}

/// `Ok` for absent (empty) and for readable-possibly-lossy. `Err` only when
/// the file exists and the OS refused it — writing back what we could read
/// (nothing) would erase an unreachable catalog.
fn read_catalog() -> Result<CatalogRead> {
    match std::fs::read(custom_presets_path()) {
        Ok(bytes) => Ok(parse_catalog(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CatalogRead {
            presets: Vec::new(),
            lossy: false,
        }),
        Err(e) => Err(e.into()),
    }
}

/// Copy aside to `display-presets.json.bad` before a non-round-trip save.
/// Copy, not rename: a crash still leaves a catalog at the live path.
/// Best-effort — failing to preserve must not fail the operator write.
fn quarantine_catalog() {
    let path = custom_presets_path();
    let bad = path.with_extension("json.bad");
    match std::fs::copy(&path, &bad) {
        Ok(_) => tracing::warn!(path = %bad.display(),
            "the custom-preset catalog held entries this host could not read; the original was \
             copied aside before being rewritten"),
        Err(e) => tracing::warn!(error = %e, path = %bad.display(),
            "unreadable custom-preset catalog not preserved before the rewrite"),
    }
}

/// Serializes catalog read → mutate → save. Without it concurrent add +
/// delete each write back what they loaded and one edit vanishes.
static CATALOG_LOCK: Mutex<()> = Mutex::new(());

/// Persist the catalog ([`pf_paths::replace_secret_file`]). Callers hold [`CATALOG_LOCK`].
fn save_custom_presets(presets: &[CustomPreset]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(presets)?;
    pf_paths::replace_secret_file(&custom_presets_path(), &bytes)?;
    Ok(())
}

/// Load saved custom presets. Absent/unreadable ⇒ empty, non-fatal; the
/// file is left untouched.
pub fn load_custom_presets() -> Vec<CustomPreset> {
    match read_catalog() {
        Ok(c) => c.presets,
        Err(e) => {
            tracing::warn!(error = %e,
                "display-presets.json exists but could not be read — the console will show no custom \
                 presets; the file itself is untouched");
            Vec::new()
        }
    }
}

/// Neutralise a connected-but-inactive external sink while streaming,
/// unless `PUNKTFUNK_STANDBY_SINK_KEEP` is set (any value but `0`/`off`/empty).
///
/// Default-on: standby HPD/DDC on an unused TV stalls the virtual head.
/// Operator displays are out of scope — this selector only sees external
/// physicals in no topology (`monitor_devnode::disable_connected_inactive`).
pub fn standby_sink_neutralise(opt_out: Option<&str>) -> bool {
    !matches!(opt_out, Some(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("off"))
}

/// 12 hex chars from name + wall-clock nanos + `nonce` (host `library`
/// custom-entry scheme). Name+nanos is not unique: two creates in one tick
/// hash identically, then `update_custom_preset` edits whichever landed first.
fn preset_id(name: &str, nanos: u128, nonce: u64) -> String {
    hex::encode(&Sha256::digest(format!("{name}:{nanos}:{nonce}").as_bytes())[..6])
}

/// First id not already in `presets`, re-rolling nonce against one clock
/// read. 48 bits is collision-free only if something checks; re-reading
/// the clock per attempt is untestable luck.
fn free_preset_id_at(presets: &[CustomPreset], name: &str, nanos: u128) -> String {
    (0u64..)
        .map(|nonce| preset_id(name, nanos, nonce))
        .find(|id| presets.iter().all(|p| &p.id != id))
        .expect("the nonce space is unbounded, so some id is always free")
}

fn free_preset_id(presets: &[CustomPreset], name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    free_preset_id_at(presets, name, nanos)
}

pub fn add_custom_preset(input: CustomPresetInput) -> Result<CustomPreset> {
    let _tx = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let catalog = read_catalog()?;
    let mut presets = catalog.presets;
    let preset = CustomPreset {
        id: free_preset_id(&presets, &input.name),
        name: input.name,
        fields: sanitize_preset_fields(input.fields),
        game_session: input.game_session,
    };
    presets.push(preset.clone());
    if catalog.lossy {
        quarantine_catalog();
    }
    save_custom_presets(&presets)?;
    Ok(preset)
}

/// Replace fields; id preserved. `None` ⇒ no such id.
pub fn update_custom_preset(id: &str, input: CustomPresetInput) -> Result<Option<CustomPreset>> {
    let _tx = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let catalog = read_catalog()?;
    let mut presets = catalog.presets;
    let Some(slot) = presets.iter_mut().find(|p| p.id == id) else {
        return Ok(None);
    };
    slot.name = input.name;
    slot.fields = sanitize_preset_fields(input.fields);
    slot.game_session = input.game_session;
    let updated = slot.clone();
    if catalog.lossy {
        quarantine_catalog();
    }
    save_custom_presets(&presets)?;
    Ok(Some(updated))
}

/// Delete. `false` ⇒ no such id.
pub fn delete_custom_preset(id: &str) -> Result<bool> {
    let _tx = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let catalog = read_catalog()?;
    let mut presets = catalog.presets;
    let before = presets.len();
    presets.retain(|p| p.id != id);
    if presets.len() == before {
        return Ok(false);
    }
    if catalog.lossy {
        quarantine_catalog();
    }
    save_custom_presets(&presets)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_preset_serde_roundtrips_and_defaults_game_session() {
        let preset = CustomPreset {
            id: "abc123".into(),
            name: "My Rig".into(),
            fields: preset_fields(Preset::GamingRig).unwrap(),
            game_session: GameSession::Dedicated,
        };
        let json = serde_json::to_string(&preset).unwrap();
        assert_eq!(serde_json::from_str::<CustomPreset>(&json).unwrap(), preset);

        // Catalog written before `game_session` still loads (defaults to `Auto`).
        let legacy: CustomPreset = serde_json::from_value(serde_json::json!({
            "id": "x",
            "name": "Legacy",
            "fields": serde_json::to_value(preset_fields(Preset::Default).unwrap()).unwrap(),
        }))
        .unwrap();
        assert_eq!(legacy.game_session, GameSession::Auto);
    }

    #[test]
    fn sanitize_preset_fields_clamps_max_displays() {
        let mut f = preset_fields(Preset::Default).unwrap();
        f.max_displays = 999;
        assert_eq!(sanitize_preset_fields(f.clone()).max_displays, 16);
        f.max_displays = 0;
        assert_eq!(sanitize_preset_fields(f).max_displays, 1);
    }

    #[test]
    fn keep_alive_serializes_tagged_on_mode() {
        assert_eq!(
            serde_json::to_value(KeepAlive::Duration { seconds: 300 }).unwrap(),
            serde_json::json!({ "mode": "duration", "seconds": 300 })
        );
        assert_eq!(
            serde_json::to_value(KeepAlive::Off).unwrap(),
            serde_json::json!({ "mode": "off" })
        );
        assert_eq!(
            serde_json::to_value(KeepAlive::Forever).unwrap(),
            serde_json::json!({ "mode": "forever" })
        );
        for k in [
            KeepAlive::Off,
            KeepAlive::Duration { seconds: 42 },
            KeepAlive::Forever,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            assert_eq!(serde_json::from_str::<KeepAlive>(&s).unwrap(), k);
        }
    }

    #[test]
    fn keep_alive_linger_resolution() {
        assert_eq!(KeepAlive::Off.linger(), Linger::Immediate);
        assert_eq!(
            KeepAlive::Duration { seconds: 30 }.linger(),
            Linger::For(Duration::from_secs(30))
        );
        assert_eq!(KeepAlive::Forever.linger(), Linger::Forever);
    }

    #[test]
    fn default_policy_is_todays_behavior() {
        let e = DisplayPolicy::default().effective();
        assert_eq!(e.keep_alive, KeepAlive::Duration { seconds: 10 });
        assert_eq!(e.topology, Topology::Auto);
        assert_eq!(e.mode_conflict, ModeConflict::Separate);
        assert_eq!(e.identity, Identity::PerClient);
        assert_eq!(e.layout.mode, LayoutMode::AutoRow);
    }

    #[test]
    fn custom_uses_explicit_fields_presets_override_them() {
        let p = DisplayPolicy {
            preset: Preset::Custom,
            keep_alive: KeepAlive::Off,
            topology: Topology::Extend,
            ..DisplayPolicy::default()
        };
        assert_eq!(p.effective().keep_alive, KeepAlive::Off);
        assert_eq!(p.effective().topology, Topology::Extend);

        let p = DisplayPolicy {
            preset: Preset::GamingRig,
            keep_alive: KeepAlive::Off,
            topology: Topology::Extend,
            ..DisplayPolicy::default()
        };
        let e = p.effective();
        assert_eq!(e.keep_alive, KeepAlive::Forever);
        assert_eq!(e.topology, Topology::Exclusive);
        assert_eq!(e.mode_conflict, ModeConflict::Steal);
    }

    #[test]
    fn workstation_preset_keeps_manual_layout_positions() {
        let mut positions = BTreeMap::new();
        positions.insert("1".to_string(), Position { x: 2560, y: 0 });
        let p = DisplayPolicy {
            preset: Preset::Workstation,
            layout: Layout {
                mode: LayoutMode::AutoRow, // workstation forces Manual regardless
                positions,
            },
            ..DisplayPolicy::default()
        };
        let e = p.effective();
        assert_eq!(e.layout.mode, LayoutMode::Manual);
        assert_eq!(
            e.layout.positions.get("1"),
            Some(&Position { x: 2560, y: 0 })
        );
    }

    #[test]
    fn every_preset_expands() {
        for preset in [
            Preset::Default,
            Preset::GamingRig,
            Preset::SharedDesktop,
            Preset::Hotdesk,
            Preset::Workstation,
        ] {
            assert!(preset_fields(preset).is_some(), "{preset:?} must expand");
        }
        assert!(preset_fields(Preset::Custom).is_none());
    }

    #[test]
    fn sanitize_clamps_max_displays_and_pins_version() {
        let p = DisplayPolicy {
            version: 99,
            max_displays: 0,
            ..DisplayPolicy::default()
        }
        .sanitized();
        assert_eq!(p.version, CURRENT_VERSION);
        assert_eq!(p.max_displays, 1);
        let p = DisplayPolicy {
            max_displays: 999,
            ..DisplayPolicy::default()
        }
        .sanitized();
        assert_eq!(p.max_displays, 16);
    }

    #[test]
    fn with_manual_layout_preserves_behavior_and_sets_positions() {
        let stored = DisplayPolicy {
            preset: Preset::Workstation,
            game_session: GameSession::Dedicated,
            ddc_power_off: true,
            pnp_disable_monitors: true,
            edid_lock: true,
            capture_monitor: Some("DP-2".into()),
            ..DisplayPolicy::default()
        };
        let eff = stored.effective();
        let mut positions = BTreeMap::new();
        positions.insert("1".to_string(), Position { x: 0, y: 0 });
        positions.insert("7".to_string(), Position { x: 2560, y: 0 });
        let p = stored.with_manual_layout(positions);
        // Arranging must not clear orthogonal pins (game-session, capture, …).
        assert_eq!(p.game_session, GameSession::Dedicated);
        assert!(p.ddc_power_off);
        assert!(p.pnp_disable_monitors);
        assert!(p.edid_lock);
        assert_eq!(p.capture_monitor.as_deref(), Some("DP-2"));
        assert_eq!(p.preset, Preset::Custom);
        assert_eq!(p.keep_alive, eff.keep_alive);
        assert_eq!(p.topology, eff.topology);
        assert_eq!(p.mode_conflict, eff.mode_conflict);
        assert_eq!(p.identity, eff.identity);
        assert_eq!(p.max_displays, eff.max_displays);
        let e2 = p.effective();
        assert_eq!(e2.layout.mode, LayoutMode::Manual);
        let want = Position { x: 2560, y: 0 };
        assert_eq!(e2.layout.positions.get("7"), Some(&want));
    }

    /// File contract, not PUT. Older hosts' documents load with missing
    /// axes at defaults. PUT `/display/settings` is a whole-object replace,
    /// so omitting `capture_monitor` currently clears the pin. A merge DTO
    /// belongs in the mgmt handler; this type stays as asserted here.
    #[test]
    fn serde_defaults_fill_a_partial_document() {
        let p: DisplayPolicy =
            serde_json::from_str(r#"{ "preset": "custom", "max_displays": 2 }"#).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.keep_alive, KeepAlive::default());
        assert_eq!(p.topology, Topology::Auto);
        assert_eq!(p.version, 1);
        // Files from before these axes: DDC off, monitor PnP disable on.
        assert!(!p.ddc_power_off);
        assert!(p.pnp_disable_monitors);
    }

    /// A v1 file stored the monitor PnP disable off by default; loading turns it on once.
    /// A v2 file that says off stays off.
    #[test]
    fn v1_file_migrates_pnp_disable_on() {
        let path = std::path::Path::new("display-settings.json");
        let v1 = br#"{ "version": 1, "preset": "custom", "pnp_disable_monitors": false }"#;
        let p = DisplayPolicyStore::parse(path, v1).unwrap();
        assert!(p.pnp_disable_monitors);
        assert_eq!(p.version, CURRENT_VERSION);
        let v2 = br#"{ "version": 2, "preset": "custom", "pnp_disable_monitors": false }"#;
        assert!(
            !DisplayPolicyStore::parse(path, v2)
                .unwrap()
                .pnp_disable_monitors
        );
    }

    /// A seat's lookups see the box's device overlays; its own file never stores them.
    #[test]
    fn a_seat_takes_the_box_overlays_and_keeps_them_out_of_its_own_file() {
        let dir = std::env::temp_dir().join(format!("pf-disp-seat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let box_file = dir.join("box.json");
        let mut boxed = DisplayPolicy::default();
        boxed.clients.insert(
            "aa".into(),
            ClientOverlay {
                mode_conflict: Some(ModeConflict::Join),
                ..ClientOverlay::default()
            },
        );
        std::fs::write(&box_file, serde_json::to_vec(&boxed).unwrap()).unwrap();

        let own = dir.join("seat.json");
        let seat = DisplayPolicyStore::load_from(own.clone()).with_box_overlays(Some(box_file));
        // Unconfigured, the seat still answers the box's overlay: admission reads `get()`.
        assert!(seat.configured().is_none());
        assert_eq!(
            seat.get().effective_for(Some("aa")).mode_conflict,
            ModeConflict::Join
        );
        seat.set(DisplayPolicy::default()).unwrap();
        assert!(seat.configured().unwrap().overlay_for(Some("aa")).is_some());
        let stored: DisplayPolicy = serde_json::from_slice(&std::fs::read(&own).unwrap()).unwrap();
        assert!(
            stored.clients.is_empty(),
            "the box's overlays stay the box's"
        );
    }

    /// A contract seat never reads its own file, takes only a device's cap and scale from the
    /// box, refuses every write, and stays unconfigured for linger, topology and identity.
    #[test]
    fn a_contract_seat_ignores_its_file_and_the_owners_policy() {
        let dir = std::env::temp_dir().join(format!("pf-disp-contract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let box_file = dir.join("box.json");
        let mut boxed = DisplayPolicy::default();
        boxed.clients.insert(
            "aa".into(),
            ClientOverlay {
                capture_monitor: Some("DP-1".into()),
                topology: Some(Topology::Exclusive),
                max_mode: Some("1920x1080@60".into()),
                ..ClientOverlay::default()
            },
        );
        std::fs::write(&box_file, serde_json::to_vec(&boxed).unwrap()).unwrap();
        let own = dir.join("seat.json");
        let leftover = DisplayPolicy {
            capture_monitor: Some("HDMI-1".into()),
            ddc_power_off: true,
            ..DisplayPolicy::default()
        };
        std::fs::write(&own, serde_json::to_vec(&leftover).unwrap()).unwrap();

        let seat = DisplayPolicyStore::load_from(own)
            .with_box_overlays(Some(box_file))
            .with_contract(true);
        let p = seat.get();
        assert_eq!(p.capture_monitor_for(Some("aa")), None);
        assert!(!p.ddc_power_off && !p.pnp_disable_monitors && p.keep_monitors.is_empty());
        assert_eq!(
            p.effective_for(Some("aa")).mode_conflict,
            ModeConflict::Join
        );
        assert_eq!(p.cap_mode(Some("aa"), (3840, 2160, 120)), (1920, 1080, 60));
        assert!(seat.configured().is_none());
        assert!(seat.set(DisplayPolicy::default()).is_err());
        assert!(seat.update(|_| true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A device's own cap outranks the seat's; a device without one takes the seat's.
    #[test]
    fn a_seat_cap_fills_in_for_a_device_without_one() {
        let mut p = DisplayPolicy::default();
        p.clients.insert(
            "pad".into(),
            ClientOverlay {
                max_mode: Some("1280x720@60".into()),
                ..ClientOverlay::default()
            },
        );
        let seat = Some("2560x1440@120");
        assert_eq!(
            p.cap_mode_or(Some("pad"), (3840, 2160, 120), seat),
            (1280, 720, 60)
        );
        assert_eq!(
            p.cap_mode_or(Some("tv"), (3840, 2160, 144), seat),
            (2560, 1440, 120)
        );
        assert_eq!(
            p.cap_mode_or(Some("tv"), (3840, 2160, 144), None),
            (3840, 2160, 144)
        );
    }

    #[test]
    fn store_roundtrips_and_gates_on_file_presence() {
        let dir = std::env::temp_dir().join(format!("pf-disp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("display-settings.json");
        let _ = std::fs::remove_file(&path);

        let store = DisplayPolicyStore::load_from(path.clone());
        assert!(store.configured().is_none());
        assert_eq!(store.get(), DisplayPolicy::default());

        let want = DisplayPolicy {
            preset: Preset::SharedDesktop,
            ..DisplayPolicy::default()
        };
        store.set(want.clone()).unwrap();
        assert_eq!(
            store.configured().as_ref().map(|p| p.preset),
            Some(Preset::SharedDesktop)
        );
        assert_eq!(
            store.configured_effective().unwrap().keep_alive,
            KeepAlive::Off
        );

        let reopened = DisplayPolicyStore::load_from(path.clone());
        assert_eq!(reopened.configured().unwrap().preset, Preset::SharedDesktop);

        // An update edits what is stored, keeping what another writer saved.
        assert!(store
            .update(|p| {
                p.edid_lock = true;
                true
            })
            .unwrap());
        assert!(!store.update(|_| false).unwrap(), "no edit, no write");
        let reopened = DisplayPolicyStore::load_from(path.clone());
        let stored = reopened.configured().unwrap();
        assert!(stored.edid_lock);
        assert_eq!(stored.preset, Preset::SharedDesktop);

        let _ = std::fs::remove_file(&path);
    }

    /// The mgmt PUT merge must enumerate every axis. Bump this count only
    /// in the same change that wires the new field.
    #[test]
    fn every_policy_axis_is_accounted_for() {
        let v = serde_json::to_value(DisplayPolicy::default()).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.len(),
            14,
            "a display-policy axis was added or removed: {keys:?} — wire it into the mgmt PUT's \
             per-axis merge (and into `EffectivePolicy` if it is a behavior axis) before bumping this"
        );
    }

    #[test]
    fn sanitize_clamps_an_absurd_linger_to_a_day() {
        // Unclamped `u32` is a deadline the reaper never reaches.
        let p = DisplayPolicy {
            keep_alive: KeepAlive::Duration { seconds: u32::MAX },
            ..DisplayPolicy::default()
        }
        .sanitized();
        assert_eq!(
            p.keep_alive,
            KeepAlive::Duration {
                seconds: MAX_KEEP_ALIVE_SECS
            }
        );
        for k in [
            KeepAlive::Duration { seconds: 300 },
            KeepAlive::Off,
            KeepAlive::Forever,
        ] {
            let p = DisplayPolicy {
                keep_alive: k,
                ..DisplayPolicy::default()
            }
            .sanitized();
            assert_eq!(p.keep_alive, k);
        }
        // Same bound on the catalog, which would otherwise smuggle past PUT.
        let mut f = preset_fields(Preset::Default).unwrap();
        f.keep_alive = KeepAlive::Duration { seconds: u32::MAX };
        assert_eq!(
            sanitize_preset_fields(f).keep_alive,
            KeepAlive::Duration {
                seconds: MAX_KEEP_ALIVE_SECS
            }
        );
    }

    #[test]
    fn sanitize_canonicalizes_layout_keys_and_drops_unusable_ones() {
        let mut positions = BTreeMap::new();
        positions.insert("01".to_string(), Position { x: 10, y: 0 }); // zero-padded
        positions.insert("2".to_string(), Position { x: 20, y: 0 });
        positions.insert("slot3".to_string(), Position { x: 30, y: 0 }); // not a slot id
        positions.insert(" 4".to_string(), Position { x: 40, y: 0 }); // whitespace
        positions.insert("99".to_string(), Position { x: 50, y: 0 }); // no such slot
        positions.insert("0".to_string(), Position { x: 60, y: 0 }); // slots start at 1
        let p = DisplayPolicy {
            layout: Layout {
                mode: LayoutMode::Manual,
                positions,
            },
            ..DisplayPolicy::default()
        }
        .sanitized();
        let got = &p.layout.positions;
        assert_eq!(got.len(), 2, "only the two real slot pins survive: {got:?}");
        // `"01"` must resolve: `arrange` looks up `u32::to_string()`.
        assert_eq!(got.get("1"), Some(&Position { x: 10, y: 0 }));
        assert_eq!(got.get("2"), Some(&Position { x: 20, y: 0 }));
    }

    #[test]
    fn canonical_positions_prefers_the_canonical_spelling_over_a_duplicate() {
        // Two spellings of slot 1: must not depend on BTreeMap order.
        let mut positions = BTreeMap::new();
        positions.insert("01".to_string(), Position { x: 10, y: 0 });
        positions.insert("1".to_string(), Position { x: 11, y: 0 });
        let out = canonical_positions(positions);
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("1"), Some(&Position { x: 11, y: 0 }));
    }

    #[test]
    fn a_readable_file_is_sanitized_on_load_not_only_on_write() {
        // Load sanitizes like a console PUT, so a hand-edit cannot skip clamps.
        let doc = br#"{ "version": 1, "max_displays": 999,
                       "keep_alive": { "mode": "duration", "seconds": 4294967295 },
                       "layout": { "mode": "manual", "positions": { "01": { "x": 5, "y": 6 } } } }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, 16);
        assert_eq!(
            p.keep_alive,
            KeepAlive::Duration {
                seconds: MAX_KEEP_ALIVE_SECS
            }
        );
        assert_eq!(p.layout.positions.get("1"), Some(&Position { x: 5, y: 6 }));
    }

    #[test]
    fn one_unreadable_axis_does_not_discard_the_whole_policy() {
        // Unknown enum variant: drop that axis, keep the rest (pin, preset, linger).
        let doc = br#"{ "version": 1, "preset": "hotdesk", "topology": "hologram",
                        "max_displays": 3, "capture_monitor": "DP-2" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc)
            .expect("a single bad axis must not discard the document");
        assert_eq!(p.topology, Topology::Auto, "the bad axis falls to default");
        assert_eq!(p.preset, Preset::Hotdesk, "…and everything else survives");
        assert_eq!(p.max_displays, 3);
        assert_eq!(p.capture_monitor.as_deref(), Some("DP-2"));
    }

    #[test]
    fn a_mistyped_scalar_falls_back_to_its_own_default_not_zero() {
        // `max_displays` serde-defaults to 4, not 0; a typo must not admit zero displays.
        let doc = br#"{ "max_displays": "four", "preset": "workstation" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, default_max_displays());
        assert_eq!(p.preset, Preset::Workstation);
    }

    #[test]
    fn a_document_that_is_not_a_policy_at_all_is_unconfigured() {
        // Truncated/non-object → unconfigured. Half-reading would be worse.
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"{ not json").is_none());
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"[1, 2, 3]").is_none());
        // Empty object is a policy: every axis defaults.
        assert_eq!(
            DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"{}"),
            Some(DisplayPolicy::default())
        );
    }

    #[test]
    fn a_future_schema_version_is_still_read() {
        // Unknown version still loads; rejecting it would revert on downgrade.
        let doc = br#"{ "version": 7, "preset": "gaming-rig", "future_axis": { "a": 1 } }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.preset, Preset::GamingRig);
        assert_eq!(
            p.version, CURRENT_VERSION,
            "sanitized back to what we write"
        );
    }

    #[test]
    fn a_missing_file_and_a_present_one_are_different_states() {
        let dir = std::env::temp_dir().join(format!("pf-disp-io-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("display-settings.json");
        let _ = std::fs::remove_file(&path);
        assert!(DisplayPolicyStore::load_from(path.clone())
            .configured()
            .is_none());
        std::fs::write(&path, br#"{"preset":"hotdesk"}"#).unwrap();
        assert_eq!(
            DisplayPolicyStore::load_from(path.clone())
                .configured()
                .unwrap()
                .preset,
            Preset::Hotdesk
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn one_bad_catalog_entry_costs_only_itself() {
        // One bad entry must not empty the catalog (the next create would then overwrite it).
        let doc = br#"[
            { "id": "a", "name": "Keep", "fields": { "keep_alive": { "mode": "forever" },
              "topology": "exclusive", "mode_conflict": "steal", "identity": "per-client",
              "layout": { "mode": "auto-row", "positions": {} }, "max_displays": 2 } },
            { "id": "b", "name": "Broken", "fields": { "topology": "hologram" } },
            { "id": "c", "name": "Also kept", "fields": {} }
        ]"#;
        let read = parse_catalog(doc);
        assert!(read.lossy, "the caller must know an entry was dropped");
        let names: Vec<&str> = read.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Keep", "Also kept"]);
        // Omitted fields default (real `max_displays` default, not 0).
        let c = &read.presets[1];
        assert_eq!(c.fields.max_displays, default_max_displays());
        assert_eq!(c.fields.keep_alive, KeepAlive::default());
        assert_eq!(c.fields.topology, Topology::Auto);
        assert_eq!(c.game_session, GameSession::Auto);
        assert_eq!(read.presets[0].fields.max_displays, 2);
    }

    #[test]
    fn a_clean_catalog_is_not_lossy_and_a_broken_one_is() {
        let clean = parse_catalog(b"[]");
        assert!(clean.presets.is_empty() && !clean.lossy);
        // Not an array: nothing recovered, `lossy` so CRUD preserves the file.
        let junk = parse_catalog(br#"{"presets":[]}"#);
        assert!(junk.presets.is_empty() && junk.lossy);
    }

    #[test]
    fn preset_ids_never_collide_with_the_catalog() {
        let entry = |id: &str| CustomPreset {
            id: id.to_string(),
            name: "Same name".into(),
            fields: preset_fields(Preset::Default).unwrap(),
            game_session: GameSession::Auto,
        };
        // Same name, same nanosecond: second create must take the next nonce.
        let first = free_preset_id_at(&[], "Same name", 42);
        assert_eq!(first, preset_id("Same name", 42, 0));
        let second = free_preset_id_at(&[entry(&first)], "Same name", 42);
        assert_eq!(second, preset_id("Same name", 42, 1));
        assert_ne!(first, second);
        assert_eq!(first.len(), 12, "12 hex chars, unchanged id shape");
    }

    /// Request contract: `#[serde(default)]` on [`EffectivePolicy`] would
    /// turn `{"fields":{}}` into a 201 and drop axes from OpenAPI `required`.
    /// Catalog leniency is [`StoredEffectivePolicy`].
    #[test]
    fn the_wire_shape_of_an_effective_policy_requires_every_axis() {
        assert!(serde_json::from_str::<EffectivePolicy>("{}").is_err());
        // Omitting one axis is a mistake, not a default.
        let almost = r#"{ "keep_alive": { "mode": "forever" }, "topology": "exclusive",
                          "mode_conflict": "steal", "identity": "per-client",
                          "layout": { "mode": "auto-row", "positions": {} } }"#;
        assert!(serde_json::from_str::<EffectivePolicy>(almost).is_err());
        let mut full: serde_json::Value = serde_json::from_str(almost).unwrap();
        full["max_displays"] = serde_json::json!(2);
        assert!(serde_json::from_value::<EffectivePolicy>(full).is_ok());
        assert!(
            serde_json::from_str::<CustomPresetInput>(r#"{ "name": "Kiosk", "fields": {} }"#)
                .is_err()
        );
    }

    /// File contract is the opposite: an older catalog entry still loads.
    #[test]
    fn a_catalog_entry_may_omit_an_axis_even_though_the_wire_shape_may_not() {
        let doc = br#"[{ "id": "c", "name": "Old", "fields": { "topology": "exclusive" } }]"#;
        let read = parse_catalog(doc);
        assert!(!read.lossy, "an omitted axis is not a lost entry");
        assert_eq!(read.presets.len(), 1);
        let f = &read.presets[0].fields;
        assert_eq!(f.topology, Topology::Exclusive);
        assert_eq!(f.max_displays, default_max_displays(), "not 0");
        assert_eq!(f.keep_alive, KeepAlive::default());
    }

    #[test]
    fn an_unreadable_preset_name_refuses_the_whole_document() {
        // `preset` selects the other axes; salvaging it to `Custom` would
        // activate leftover explicit fields nobody chose.
        let doc = br#"{ "version": 1, "preset": "kiosk",
                        "keep_alive": { "mode": "forever" }, "topology": "exclusive",
                        "mode_conflict": "steal", "identity": "per-client",
                        "layout": { "mode": "auto-row", "positions": {} }, "max_displays": 4 }"#;
        assert!(
            DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none(),
            "a preset name this build cannot read means we do not know what the file asks for"
        );
        // A known preset still salvages its neighbours.
        let doc = br#"{ "preset": "hotdesk", "topology": "hologram" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.preset, Preset::Hotdesk);
        assert_eq!(p.topology, Topology::Auto);
    }

    #[test]
    fn a_document_we_understood_nothing_of_is_unconfigured_not_default() {
        // `None` is the historical-default gate. `Some(default)` is PerClient
        // identity; unconfigured Linux is Shared — that rename drops KDE config.
        let doc = br#"{ "topology": "hologram", "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        // `version` and unknown keys are not axes; they cannot mark configured.
        let doc = br#"{ "version": 1, "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        let doc = br#"{ "note": "mine", "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        // One real axis is a configuration.
        let doc = br#"{ "identity": "perclient", "max_displays": 2 }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.identity, Identity::default());
    }

    #[test]
    fn standby_sink_neutralise_is_on_unless_explicitly_kept() {
        // Unset, empty, and the two "off" spellings all mean: neutralise.
        assert!(standby_sink_neutralise(None));
        assert!(standby_sink_neutralise(Some("")));
        assert!(standby_sink_neutralise(Some("0")));
        assert!(standby_sink_neutralise(Some("off")));
        assert!(standby_sink_neutralise(Some("OFF")));
        // Anything else is the operator asking to keep the sink.
        assert!(!standby_sink_neutralise(Some("1")));
        assert!(!standby_sink_neutralise(Some("keep")));
    }

    /// Overlay resolution, as a table (`design/web-console-overhaul.md` §6.1).
    ///
    /// The shape this pins: an ABSENT field follows the host, so a host-side
    /// change still reaches a device that pinned something else. A copied
    /// policy would silently stop following, which is why the overlay is
    /// field-wise and not a whole `DisplayPolicy`.
    mod client_overlay {
        use super::*;

        const TV: &str = "aa11";
        const PAD: &str = "bb22";

        /// A host on `default`, with the TV pinned to take-over + keep-forever.
        fn host() -> DisplayPolicy {
            let mut p = DisplayPolicy {
                preset: Preset::Default,
                ..DisplayPolicy::default()
            };
            p.clients.insert(
                TV.into(),
                ClientOverlay {
                    keep_alive: Some(KeepAlive::Forever),
                    mode_conflict: Some(ModeConflict::Steal),
                    ..ClientOverlay::default()
                },
            );
            p
        }

        #[test]
        fn an_overlaid_field_wins_and_the_rest_still_follow_the_host() {
            let p = host();
            let tv = p.effective_for(Some(TV));
            let base = p.effective();
            assert_eq!(tv.keep_alive, KeepAlive::Forever);
            assert_eq!(tv.mode_conflict, ModeConflict::Steal);
            // Untouched axes are the host's, not the type's defaults.
            assert_eq!(tv.topology, base.topology);
            assert_eq!(tv.identity, base.identity);
        }

        #[test]
        fn a_device_with_no_overlay_is_exactly_the_host_policy() {
            let p = host();
            assert_eq!(p.effective_for(Some(PAD)), p.effective());
            assert_eq!(p.effective_for(None), p.effective());
            assert_eq!(p.effective_for(Some("unknown")), p.effective());
        }

        /// The reason for a field-wise overlay rather than a copied policy:
        /// changing the host must still move every axis the device did not pin.
        #[test]
        fn a_host_change_still_reaches_an_overlaid_device() {
            let mut p = host();
            p.preset = Preset::Workstation;
            let tv = p.effective_for(Some(TV));
            assert_eq!(tv.topology, p.effective().topology, "followed the host");
            assert_eq!(tv.keep_alive, KeepAlive::Forever, "kept its own pin");
        }

        /// These describe the host's desktop, not a device on it. Asserted on a
        /// `Custom` host because a named preset fixes both axes itself.
        #[test]
        fn layout_and_max_displays_stay_host_wide() {
            let mut p = host();
            p.preset = Preset::Custom;
            p.max_displays = 7;
            p.layout.mode = LayoutMode::Manual;
            let tv = p.effective_for(Some(TV));
            assert_eq!(tv.max_displays, 7);
            assert_eq!(tv.layout.mode, LayoutMode::Manual);
        }

        #[test]
        fn the_orthogonal_axes_overlay_too() {
            let mut p = host();
            p.game_session = GameSession::Auto;
            p.capture_monitor = Some("DP-1".into());
            p.clients.insert(
                PAD.into(),
                ClientOverlay {
                    game_session: Some(GameSession::Dedicated),
                    capture_monitor: Some("HDMI-1".into()),
                    ..ClientOverlay::default()
                },
            );
            assert_eq!(p.game_session_for(Some(PAD)), GameSession::Dedicated);
            assert_eq!(p.capture_monitor_for(Some(PAD)).as_deref(), Some("HDMI-1"));
            // The TV pinned neither, so it still mirrors the host's monitor.
            assert_eq!(p.game_session_for(Some(TV)), GameSession::Auto);
            assert_eq!(p.capture_monitor_for(Some(TV)).as_deref(), Some("DP-1"));
        }

        /// An overlay must not smuggle a window a host-wide PUT would clamp.
        #[test]
        fn sanitize_clamps_an_overlay_the_way_it_clamps_the_host() {
            let mut p = DisplayPolicy::default();
            p.clients.insert(
                TV.into(),
                ClientOverlay {
                    keep_alive: Some(KeepAlive::Duration { seconds: 999_999 }),
                    capture_monitor: Some("  ".into()),
                    ..ClientOverlay::default()
                },
            );
            let p = p.sanitized();
            let o = &p.clients[TV];
            assert_eq!(
                o.keep_alive,
                Some(KeepAlive::Duration {
                    seconds: MAX_KEEP_ALIVE_SECS
                })
            );
            assert_eq!(o.capture_monitor, None, "a blank pin is no pin");
        }

        /// "Pins nothing" is what an absent key already means; keeping the
        /// record would show the device as configured in the console.
        #[test]
        fn sanitize_drops_an_overlay_that_pins_nothing() {
            let mut p = DisplayPolicy::default();
            p.clients.insert(TV.into(), ClientOverlay::default());
            p.clients.insert(
                "  ".into(),
                ClientOverlay {
                    keep_alive: Some(KeepAlive::Forever),
                    ..ClientOverlay::default()
                },
            );
            assert!(p.sanitized().clients.is_empty());
        }

        /// Fingerprints are hex and reach us from two spellings of the same
        /// device (a console echo, a hand-edited file).
        #[test]
        fn sanitize_lowercases_the_key_so_one_device_is_one_record() {
            let mut p = DisplayPolicy::default();
            p.clients.insert(
                "AA11".into(),
                ClientOverlay {
                    keep_alive: Some(KeepAlive::Forever),
                    ..ClientOverlay::default()
                },
            );
            let p = p.sanitized();
            assert!(p.clients.contains_key(TV));
            assert_eq!(p.effective_for(Some(TV)).keep_alive, KeepAlive::Forever);
        }

        /// Teardown is where "keep forever" has to land, and it runs on the linger
        /// thread with no session in scope — it reaches the device through the identity
        /// slot's recorded owner. Asserted here on the resolution the registry performs.
        #[test]
        fn a_kept_display_resolves_its_own_owners_keep_alive() {
            let mut p = host();
            p.preset = Preset::Custom;
            p.keep_alive = KeepAlive::Off;
            // The TV pinned Forever; the tablet pinned nothing.
            assert_eq!(p.effective_for(Some(TV)).keep_alive, KeepAlive::Forever);
            assert_eq!(p.effective_for(Some(PAD)).keep_alive, KeepAlive::Off);
            // A display with no recorded owner (shared / anonymous) follows the host.
            assert_eq!(p.effective_for(None).keep_alive, KeepAlive::Off);
        }

        /// A cap is per axis, so a device that asks for less than the cap in one
        /// dimension keeps what it asked for there.
        #[test]
        fn a_mode_cap_clamps_each_axis_on_its_own() {
            let mut p = DisplayPolicy::default();
            p.clients.insert(
                PAD.into(),
                ClientOverlay {
                    max_mode: Some("2560x1440@60".into()),
                    ..ClientOverlay::default()
                },
            );
            assert_eq!(p.cap_mode(Some(PAD), (3840, 2160, 120)), (2560, 1440, 60));
            // Asked for less than the cap: nothing is taken away.
            assert_eq!(p.cap_mode(Some(PAD), (1920, 1080, 60)), (1920, 1080, 60));
            // Only the refresh is over the cap.
            assert_eq!(p.cap_mode(Some(PAD), (1920, 1080, 240)), (1920, 1080, 60));
            // Uncapped devices are untouched.
            assert_eq!(p.cap_mode(Some(TV), (3840, 2160, 120)), (3840, 2160, 120));
            assert_eq!(p.cap_mode(None, (3840, 2160, 120)), (3840, 2160, 120));
        }

        /// An unreadable cap must not silently grant everything, nor silently grant
        /// nothing — it is refused at the door and the device stays uncapped.
        #[test]
        fn an_unparsable_cap_is_not_stored() {
            let mut p = DisplayPolicy::default();
            for spec in ["", "big", "1920x1080", "0x0@60"] {
                p.clients.insert(
                    PAD.into(),
                    ClientOverlay {
                        max_mode: Some(spec.into()),
                        ..ClientOverlay::default()
                    },
                );
                let s = p.clone().sanitized();
                assert!(
                    s.clients
                        .get(PAD)
                        .and_then(|o| o.max_mode.as_ref())
                        .is_none(),
                    "{spec:?} should not survive"
                );
            }
        }

        /// A scale is a number a compositor will act on; 0 or negative is not a smaller
        /// screen, it is a broken one.
        #[test]
        fn only_a_usable_scale_is_stored() {
            let overlay = |scale| ClientOverlay {
                scale: Some(scale),
                ..ClientOverlay::default()
            };
            let stored = |scale| {
                let mut p = DisplayPolicy::default();
                p.clients.insert(PAD.into(), overlay(scale));
                p.sanitized().clients.get(PAD).and_then(|o| o.scale)
            };
            assert_eq!(stored(1.5), Some(1.5));
            assert_eq!(stored(0.0), None);
            assert_eq!(stored(-2.0), None);
            assert_eq!(stored(99.0), None);
        }

        /// The shared-desktop case: one monitor stays lit through an exclusive stream.
        #[test]
        fn kept_monitors_are_deduplicated_and_trimmed() {
            let p = DisplayPolicy {
                keep_monitors: vec!["DP-1".into(), " DP-1 ".into(), "".into(), "HDMI-A-2".into()],
                ..DisplayPolicy::default()
            }
            .sanitized();
            assert_eq!(p.keep_monitors, vec!["DP-1", "HDMI-A-2"]);
        }

        /// Arranging must not clear what it does not mention — the trap the
        /// old six-argument rebuild kept falling into.
        #[test]
        fn arranging_a_layout_keeps_every_overlay() {
            let p = host().with_manual_layout(BTreeMap::new());
            assert_eq!(p.layout.mode, LayoutMode::Manual);
            assert_eq!(p.effective_for(Some(TV)).mode_conflict, ModeConflict::Steal);
        }
    }
}
