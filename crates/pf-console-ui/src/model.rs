//! Shared console snapshot and command bus, sibling of [`crate::library::LibraryShared`].
//! Service threads (discovery, probing, pairing, waking, persistence) write snapshots;
//! the shell reads them per frame by generation stamp. Anything that touches the
//! network or disk rides a [`ConsoleCmd`] — the overlay never blocks.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Resolved catalog entry. The service thread opens the presets file; the shell never
/// does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresetChip {
    pub id: String,
    pub name: String,
    /// `#RRGGBB`.
    pub accent: Option<String>,
    /// Bitrate this preset pins, if it pins one; `None` inherits the global. Only the
    /// speed test reads it, to name the layer the tested host actually resolves bitrate
    /// from. A producer that predates the field leaves it `None`, which reads as
    /// "inherits" — the safe half, since that is where the console writes.
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
}

/// Home carousel row, fully resolved by the service thread. The shell renders it
/// verbatim.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostRow {
    /// Fingerprint when pinned, else `addr:port` — cursor identity across snapshot churn.
    pub key: String,
    /// The store record's stable id (`KnownHost::id`), what "Make default host" points at.
    /// `None` on a merely discovered row, and on any row a producer built before this
    /// field existed — `serde(default)` because Android's bridge is one of them.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Lowercase hex fingerprint; empty = not pinned.
    pub fp_hex: String,
    pub paired: bool,
    /// In the known-hosts store, not merely discovered.
    pub saved: bool,
    /// mDNS advert or last probe succeeded.
    pub online: bool,
    /// Management API port (mDNS TXT or store).
    pub mgmt_port: u16,
    /// Offline with a stored MAC: Wake & Connect is offered.
    pub can_wake: bool,
    /// Per-host clipboard share while streaming (`KnownHost::clipboard_sync`).
    #[serde(default)]
    pub clipboard_sync: bool,
    /// Last successful connect, UNIX seconds.
    pub last_used: Option<u64>,
    /// OS-identity chain: live advert preferred, else stored. Empty = unknown.
    pub os: String,
    /// Host-reported extras (`design/host-actions.md`): sleep, restart, shut down.
    /// Empty when the host is unreachable or the route does not exist.
    #[serde(default)]
    pub actions: Vec<HostAction>,
    /// Pinned-preset shortcut after the host's primary tile, sharing its live state.
    /// `None` = this row is the primary tile.
    pub pin: Option<PresetChip>,
    /// Default preset (`KnownHost::preset_id`). Always `None` on a pinned row — that
    /// preset is `pin`.
    pub bound_preset: Option<PresetChip>,
    /// What this host has up right now (`GET /api/v1/status`), as a title to show.
    /// Empty = nothing running, unpaired, unreachable, or a host too old to ask —
    /// every one of which the tile renders the same way: no line.
    ///
    /// Host state, never store state: `serde(default)` so a producer predating the
    /// field still parses, and it is never persisted, which is what stops a carousel
    /// coming back claiming a game is up because it was up last night.
    #[serde(default)]
    pub running: String,
    /// Library title id → preset id (`KnownHost::game_presets`), for the bind screen's
    /// checkmark. Ids, not chips: the shell only compares them, and a title's binding
    /// outranks `bound_preset` at launch, which the host resolves.
    #[serde(default)]
    pub game_presets: BTreeMap<String, String>,
}

#[cfg(test)]
impl HostRow {
    /// A paired, saved, online host at `10.0.0.9:9777` pinned as `key`. Tests override what
    /// they are about.
    pub(crate) fn fixture(key: &str, name: &str) -> HostRow {
        HostRow {
            key: key.into(),
            fp_hex: key.into(),
            name: name.into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            mgmt_port: 47990,
            paired: true,
            saved: true,
            online: true,
            ..Default::default()
        }
    }
}

impl HostRow {
    /// The host half of [`Self::key`]: commands, bindings and the store address the host,
    /// never a pinned card's composite key.
    pub fn host_key(&self) -> &str {
        self.key.split('\0').next().unwrap_or(&self.key)
    }
}

/// A pinned card's row key: the host's key, then the preset id past a NUL, which no
/// fingerprint or `addr:port` holds. Apple and Android build the same string, pinned by
/// `pinned_key` in `clients/shared/console-vectors.json`.
pub fn pinned_key(host: &str, preset: &str) -> String {
    format!("{host}\0{preset}")
}

/// One host-offered action, resolved from `GET /api/v1/actions`
/// (`design/host-actions.md`). `label` is already chosen: this client's wording for a
/// known id, else the host's title, so a new host action renders without a console
/// release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostAction {
    /// Invoke argument (`power.sleep`).
    pub id: String,
    pub label: String,
    /// Confirm twice: the action drops host state (restart, shut down).
    pub danger: bool,
    /// Host can run it now. `false` still shows the row, disabled — do not hide it.
    pub available: bool,
    #[serde(default)]
    pub unavailable_reason: String,
}

/// Pairing ceremony state. One at a time: the ceremony is modal.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub enum PairPhase {
    #[default]
    Idle,
    /// SPAKE2 in flight; can run ~90 s if the PIN is retried.
    Busy,
    Failed(String),
    /// Paired and persisted. `key` is the host's refreshed row.
    Paired {
        key: String,
    },
}

/// A wake-and-wait in progress (one at a time). The service thread re-sends magic
/// packets and probes; the shell renders the card and acts on `online`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakeStatus {
    pub key: String,
    pub name: String,
    /// Seconds since the wake started.
    pub seconds: u32,
    pub timed_out: bool,
    /// Probe answered. The shell launches if `then_connect`.
    pub online: bool,
    /// Connect once awake, versus a bare wake.
    pub then_connect: bool,
}

/// A network speed test in progress (one at a time). The service thread connects, asks the
/// host to burst, and reports; the shell renders the takeover and applies the answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeedStatus {
    /// [`HostRow::key`] of the tested host — the shell re-reads the row to name the layer
    /// Apply writes to, rather than trusting a copy taken when the test started.
    pub key: String,
    pub name: String,
    pub phase: SpeedPhase,
    /// The burst's live throughput as it arrived: seconds since measuring began, kbps.
    #[serde(default)]
    pub trace: Vec<(f32, u32)>,
    #[serde(skip)]
    pub trace_start: Option<std::time::Instant>,
}

impl SpeedStatus {
    pub fn new(key: String, name: String) -> SpeedStatus {
        SpeedStatus {
            key,
            name,
            phase: SpeedPhase::Connecting,
            trace: Vec::new(),
            trace_start: None,
        }
    }
}

/// Where a speed test is: it connects, it measures, then it has an answer or a reason.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SpeedPhase {
    Connecting,
    Measuring,
    /// A mid-burst report of the live throughput. The status stays `Measuring` and the
    /// value joins its trace, stamped on arrival: drivers poll at their own pace.
    Progress {
        kbps: u32,
    },
    Failed(String),
    /// `throughput_kbps` is what the link carries; `wall` says the ramp found its limit
    /// rather than a floor. `recommended_kbps` keeps headroom under it for FEC and for the
    /// loss a real stream meets — [`pf_client_core::speed::recommended_kbps`], so every client
    /// recommends the same kilobit.
    Done {
        throughput_kbps: u32,
        wall: bool,
        /// The round under the ceiling; `None` toward a host without a ramp, which gets no
        /// loss line — a blast's loss is the blast's.
        clean: Option<CleanRound>,
        recommended_kbps: u32,
        /// What the network check found; empty from a driver that only measured speed.
        #[serde(default)]
        findings: Vec<FindingRow>,
    },
}

/// One round at a rate the link holds: the loss figure a speed test shows.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CleanRound {
    pub rate_kbps: u32,
    pub loss_pct: f32,
    /// Spread of the inter-arrival gap, µs.
    pub jitter_us: u32,
}

/// One finding of the network check, by id ([`punktfunk_core::client::health::FindingId`]
/// as a byte); the words are [`crate::shell`]'s. `profile` is the delivery profile that
/// helps (`1` capped, `2` smooth), when one does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FindingRow {
    pub id: u8,
    pub severity: u8,
    pub numbers: [u32; 3],
    pub profile: Option<u8>,
}

#[derive(Default)]
struct ConsoleState {
    hosts: Vec<HostRow>,
    hosts_gen: u64,
    pair: PairPhase,
    wake: Option<WakeStatus>,
    speed: Option<SpeedStatus>,
    /// One-shot toast. The shell `take`s it on the next sync — unlike [`PairPhase`]
    /// there is no modal state, so a take-once string is the whole protocol.
    notice: Option<String>,
    /// What the host bundles, for the Licences screen. Kept once sent.
    licenses: Option<Arc<Vec<LicenseSection>>>,
    /// The latest controller reading while the input test is on.
    pad_test: Option<PadTestState>,
    /// Keyboards, mice and the like: listed on the Controllers tab, never sent as a pad.
    other_devices: Vec<OtherDevice>,
}

/// One reading of the controller under test. `held` names buttons by Xbox position: `A` `B`
/// `X` `Y` `LB` `RB` `LT` `RT` `Back` `Start` `Guide` `LS` `RS` `Up` `Down` `Left` `Right`.
/// `axes` are `LX` `LY` `RX` `RY` (−1…1, +y down) and `LT` `RT` (0…1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PadTestState {
    #[serde(default)]
    pub held: Vec<String>,
    #[serde(default)]
    pub axes: Vec<(String, f32)>,
}

/// An input device that is not a controller: `kind` is `keyboard`, `mouse` or `other`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OtherDevice {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub detail: String,
}

/// One block of a host's bundled licences: a heading, then its text as the file has it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LicenseSection {
    pub heading: String,
    pub text: String,
}

/// Service threads write; the shell polls per frame. Cheap locks; no GPU data.
#[derive(Clone, Default)]
pub struct ConsoleShared(Arc<Mutex<ConsoleState>>);

impl ConsoleShared {
    pub fn set_hosts(&self, hosts: Vec<HostRow>) {
        let mut s = self.0.lock().unwrap();
        if s.hosts != hosts {
            s.hosts = hosts;
            s.hosts_gen += 1;
        }
    }

    pub(crate) fn hosts_gen(&self) -> u64 {
        self.0.lock().unwrap().hosts_gen
    }

    pub(crate) fn hosts_snapshot(&self) -> (Vec<HostRow>, u64) {
        let s = self.0.lock().unwrap();
        (s.hosts.clone(), s.hosts_gen)
    }

    pub fn set_pad_test(&self, state: PadTestState) {
        self.0.lock().unwrap().pad_test = Some(state);
    }

    pub(crate) fn take_pad_test(&self) -> Option<PadTestState> {
        self.0.lock().unwrap().pad_test.take()
    }

    pub fn set_other_devices(&self, devices: Vec<OtherDevice>) {
        self.0.lock().unwrap().other_devices = devices;
    }

    pub(crate) fn other_devices(&self) -> Vec<OtherDevice> {
        self.0.lock().unwrap().other_devices.clone()
    }

    pub fn set_licenses(&self, sections: Vec<LicenseSection>) {
        self.0.lock().unwrap().licenses = Some(Arc::new(sections));
    }

    pub(crate) fn licenses(&self) -> Option<Arc<Vec<LicenseSection>>> {
        self.0.lock().unwrap().licenses.clone()
    }

    pub fn set_pair(&self, phase: PairPhase) {
        self.0.lock().unwrap().pair = phase;
    }

    pub(crate) fn pair(&self) -> PairPhase {
        self.0.lock().unwrap().pair.clone()
    }

    pub fn set_wake(&self, wake: Option<WakeStatus>) {
        self.0.lock().unwrap().wake = wake;
    }

    pub(crate) fn wake(&self) -> Option<WakeStatus> {
        self.0.lock().unwrap().wake.clone()
    }

    /// `None` closes the takeover. The shell also clears it when the player dismisses, so
    /// a service thread that reports a late phase must not resurrect a closed test — see
    /// [`Self::advance_speed`].
    pub fn set_speed(&self, speed: Option<SpeedStatus>) {
        self.0.lock().unwrap().speed = speed;
    }

    /// Report a new phase for the test on `key`. A no-op once the shell has cleared the slot,
    /// and a no-op for a different host: the burst outlives a dismiss, so its result must
    /// neither reopen the takeover nor land under the name of a test started since. A
    /// `Progress` after the answer is a straggler and changes nothing.
    pub fn advance_speed(&self, key: &str, phase: SpeedPhase) {
        let mut s = self.0.lock().unwrap();
        let Some(sp) = s.speed.as_mut().filter(|sp| sp.key == key) else {
            return;
        };
        match phase {
            SpeedPhase::Progress { kbps } => {
                if matches!(sp.phase, SpeedPhase::Failed(_) | SpeedPhase::Done { .. }) {
                    return;
                }
                let start = *sp.trace_start.get_or_insert_with(std::time::Instant::now);
                sp.trace.push((start.elapsed().as_secs_f32(), kbps));
                sp.phase = SpeedPhase::Measuring;
            }
            phase => {
                if phase == SpeedPhase::Measuring {
                    sp.trace_start.get_or_insert_with(std::time::Instant::now);
                }
                sp.phase = phase;
            }
        }
    }

    pub(crate) fn speed(&self) -> Option<SpeedStatus> {
        self.0.lock().unwrap().speed.clone()
    }

    /// One-shot toast. A newer notice replaces an unshown older one.
    pub fn set_notice(&self, text: String) {
        self.0.lock().unwrap().notice = Some(text);
    }

    pub(crate) fn take_notice(&self) -> Option<String> {
        self.0.lock().unwrap().notice.take()
    }
}

/// Overlay→binary work. Every variant blocks on network or disk; never on the
/// render path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConsoleCmd {
    FetchLibrary {
        addr: String,
        mgmt: u16,
        fp_hex: String,
    },
    /// Re-read running titles (`GET /api/v1/status`) without touching the catalog.
    /// Not [`Self::FetchLibrary`]: that sets `Loading` and would replace the shelf
    /// with a spinner.
    RefreshRunning {
        addr: String,
        mgmt: u16,
        fp_hex: String,
    },
    /// SPAKE2 PIN ceremony; on success persist the pin and refresh hosts.
    Pair {
        addr: String,
        port: u16,
        pin: String,
        device_name: String,
    },
    /// Upload the log ring to this paired host's management API. Same transport as
    /// [`Self::FetchLibrary`]; the result is a notice toast. For platforms whose own
    /// logs are unreachable.
    SendLogs {
        addr: String,
        mgmt: u16,
        fp_hex: String,
        host_name: String,
    },
    /// Measure the path to this host over the real data plane: connect, ask it to burst,
    /// report goodput and loss. Progress arrives back as [`ConsoleShared::advance_speed`],
    /// not as a notice — the takeover narrates it and holds the Apply button.
    ///
    /// A second connect, not the running stream's: the console offers this out of session,
    /// and a burst down a live stream is what the host's keyframe-at-probe-end guard exists
    /// to survive rather than something to invite.
    SpeedTest {
        key: String,
        addr: String,
        port: u16,
        fp_hex: String,
        host_name: String,
    },
    /// Remember the delivery profile a network check offered for this host (`1` capped,
    /// `2` smooth, `0` none); the next connect asks for it. Per host, never global.
    SetHostDelivery {
        key: String,
        profile: u8,
    },
    /// Save a manually entered host, unpaired, and refresh the rows.
    SaveHost {
        name: String,
        addr: String,
        port: u16,
    },
    /// Rename or re-address a saved host. Fingerprint, pins, and MACs stay — this
    /// edits the row, it does not replace it.
    UpdateHost {
        key: String,
        name: String,
        addr: String,
        port: u16,
    },
    /// Drop a saved host. The next connect to that address has no pin, pairing, or
    /// pinned cards.
    ForgetHost {
        key: String,
    },
    Wake {
        key: String,
        then_connect: bool,
    },
    /// Stop the wake loop and clear its status.
    CancelWake,
    Probe,
    /// Pin or unpin a preset card on a saved host (`KnownHost::pinned_presets`).
    /// `key` is the host row. Presentation only: does not touch the default binding
    /// or the preset. Idempotent.
    SetPin {
        key: String,
        preset_id: String,
        pin: bool,
    },
    /// Bind or clear a preset. `game` names a library title
    /// (`KnownHost::game_presets`); `None` binds the host's own default
    /// (`KnownHost::preset_id`). [`Self::SetPin`] is presentation; this is the
    /// binding. `preset_id: None` clears. Idempotent.
    BindPreset {
        key: String,
        #[serde(default)]
        game: Option<String>,
        preset_id: Option<String>,
    },
    /// Per-host clipboard share while streaming (`KnownHost::clipboard_sync`).
    /// Never global.
    SetClipboard {
        key: String,
        on: bool,
    },
    /// Open a platform-owned overlay (`design/android-skia-console-port.md`).
    /// `id` is [`crate::platform::PlatformScreen::id`]. The host draws it and holds
    /// input; the console never sees the pixels. Desktop raises none.
    OpenPlatformScreen {
        id: String,
    },
    /// Forget a saved host's identity and keep the record: its pin and paired flag clear, so
    /// the next connect asks for a PIN again. `key` as in [`Self::ForgetHost`].
    UnpairHost {
        key: String,
    },
    /// Create or replace one preset. `overrides` is a [`SettingsOverlay`] in the shared
    /// presets file's spelling; the host persists it and pushes its catalog back.
    ///
    /// [`SettingsOverlay`]: pf_client_core::presets::SettingsOverlay
    SavePreset {
        id: String,
        name: String,
        overrides: serde_json::Value,
    },
    /// Remove one preset; a host bound or pinned to it falls back as a dangling id does.
    DeletePreset {
        id: String,
    },
    /// The input test is on screen (`true`) or gone. While on, the host sends
    /// [`PadTestState`]s and keeps the pad out of menu moves, so every button can be tried.
    PadTest {
        on: bool,
    },
    /// The Licences screen opened: send this host's [`LicenseSection`]s.
    LoadLicenses,
    /// The answer to a [`crate::screens::prompt::Prompt`]: the row picked, or `None` for
    /// Back. Only a host that raised the prompt receives one.
    PromptAnswer {
        id: String,
        choice: Option<usize>,
    },
    /// Platform-only pad work. `action` is [`crate::screens::players::PadAction::id`];
    /// `pad_key` indexes [`crate::screens::Ctx::pads`] and is empty when the pad list
    /// cannot name the device. One command, not one per button: the host's answer is
    /// always "do it, report as a notice", and a command per grant would span three crates.
    PadAction {
        action: String,
        pad_key: String,
    },
    /// Invoke a host action (`design/host-actions.md`). Same lane as [`Self::SendLogs`].
    /// Parameterised by `action_id` like [`Self::PadAction`]: a command per verb would
    /// span three crates. Outcome is a notice toast.
    HostAction {
        addr: String,
        mgmt: u16,
        fp_hex: String,
        host_name: String,
        /// Stable id (`power.sleep`).
        action_id: String,
        /// Resolved label for the toast — the service thread must not re-derive wording
        /// the screen already settled.
        label: String,
    },
    /// End a title this device launched (`POST /api/v1/game/end`). The outcome is a
    /// notice ([`pf_client_core::library::GameEnd::notice`]), then a running refresh.
    EndGame {
        addr: String,
        mgmt: u16,
        fp_hex: String,
        app_id: String,
        title: String,
    },
}

/// Overlay→binary command queue. Same locking as the shared models. Drain cadence
/// is not latency-critical: every effect arrives via a model snapshot.
#[derive(Clone, Default)]
pub struct ConsoleBus(Arc<Mutex<VecDeque<ConsoleCmd>>>);

impl ConsoleBus {
    /// Queue a command. The binary may seed one too (direct-entry library fetch) —
    /// same lane, same handler.
    pub fn send(&self, cmd: ConsoleCmd) {
        self.0.lock().unwrap().push_back(cmd);
    }

    pub fn drain(&self) -> Vec<ConsoleCmd> {
        self.0.lock().unwrap().drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tower() -> HostRow {
        HostRow {
            addr: "10.0.0.2".into(),
            online: false,
            ..HostRow::fixture("aa", "Tower")
        }
    }

    #[test]
    fn pinned_keys_match_the_shared_vectors() {
        let raw = include_str!("../../../clients/shared/console-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).unwrap();
        let cases = file["pinned_key"].as_array().expect("pinned_key cases");
        assert!(!cases.is_empty());
        for c in cases {
            let s = |k: &str| c[k].as_str().unwrap_or_else(|| panic!("{k} missing"));
            let key = pinned_key(s("host"), s("preset"));
            assert_eq!(key, s("key"));
            let card = HostRow { key, ..tower() };
            assert_eq!(card.host_key(), s("host"));
        }
        assert_eq!(
            tower().host_key(),
            "aa",
            "a primary row's key is its host key"
        );
    }

    #[test]
    fn hosts_generation_bumps_only_on_change() {
        let shared = ConsoleShared::default();
        let row = tower();
        shared.set_hosts(vec![row.clone()]);
        let g1 = shared.hosts_gen();
        shared.set_hosts(vec![row.clone()]);
        assert_eq!(shared.hosts_gen(), g1, "identical snapshot doesn't churn");
        shared.set_hosts(vec![HostRow {
            online: true,
            ..row
        }]);
        assert_eq!(shared.hosts_gen(), g1 + 1);
    }

    #[test]
    fn bus_drains_in_order() {
        let bus = ConsoleBus::default();
        bus.send(ConsoleCmd::Probe);
        bus.send(ConsoleCmd::CancelWake);
        assert_eq!(bus.drain(), vec![ConsoleCmd::Probe, ConsoleCmd::CancelWake]);
        assert!(bus.drain().is_empty());
    }
}
