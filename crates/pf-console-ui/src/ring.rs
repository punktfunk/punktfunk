//! In-stream quick-action ring (`design/touch-client-overlay.md`).
//!
//! Six slots plus a centre "More" that opens the sheet. A twist (`RingInput::Turn`)
//! drives the opening; `⌃⌥⇧O` opens it at the window centre. No icon font: a
//! button carries a short text label, a shortcut its keycap chord.
//!
//! Contract: nothing is drawn while closed; every pointer event is consumed while
//! open (the scrim). Commands leave via [`Ring::take_command`]; host actions via
//! [`Ring::take_cmds`].

use crate::anim::{approach, springs, Spring};
use crate::input::Key;
use crate::model::ConsoleCmd;
use crate::pointer::{Pointer, PointerKind};
use crate::theme::{fill, glow_ring, rim_light, ring_scrim, soft_shadow, stroke, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::host_actions::ActionInfo;
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{chord_chip, key_vk, OverlayConfig, RingPlatform, SlotId};
use pf_client_core::ring::{RingCommand, RingFacts, RingInput};
use punktfunk_core::input::gamepad as wire;
use skia_safe::{Canvas, Color4f, Point, RRect, Rect};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Design units at 100 %; `FrameCtx::scale` multiplies every metric.
const RADIUS: f32 = pf_client_core::ring::RING_RADIUS;
const SLOT_D: f32 = pf_client_core::ring::SLOT_DIAMETER;
/// 13 pt text inside an 8 pt pill. The editor sizes its stage by this so the label is not clipped.
pub(crate) const LABEL_H: f32 = 13.0 + 2.0 * 8.0;
const CENTRE_D: f32 = pf_client_core::ring::CENTRE_DIAMETER;
const IDLE_CLOSE: Duration = Duration::from_secs(8);
const HINT_LIFE: Duration = Duration::from_secs(2);
/// Slot k lags the previous one by this fraction of the twist so the ring unwinds visibly.
const SLOT_LAG: f32 = 0.06;
const RES_PRESETS: [(&str, u32, u32); 3] = [
    ("1440p", 2560, 1440),
    ("1080p", 1920, 1080),
    ("720p", 1280, 720),
];
const HZ_PRESETS: [u32; 2] = [120, 60];

struct Spec {
    id: String,
    label: String,
    short: String,
    enabled: bool,
    reason: String,
    /// Destructive: two presses.
    armed: bool,
    /// Leave the ring open so the new state is visible.
    toggle: bool,
    state: String,
}

#[derive(Clone, PartialEq)]
enum SheetRow {
    Slot(SlotId),
    Resolution,
    Refresh,
    ScrollInvert,
}

/// Editor: a press picks, Y lifts, A drops, a pointer carries. Centre is inert;
/// nothing fires or idle-closes.
#[derive(Default)]
struct Editing {
    /// Pad-lifted disc (Y), waiting for the slot A drops it on.
    lifted: Option<usize>,
    drag: Option<Drag>,
}

struct Drag {
    slot: usize,
    x0: f64,
    y0: f64,
    dx: f32,
    dy: f32,
}

impl Drag {
    /// A carry past this is a drag; under it the release is the pick.
    const SLOP: f32 = 10.0;

    fn moved(&self) -> bool {
        self.dx.hypot(self.dy) > Self::SLOP
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditEvent {
    Pick(usize),
    Swap(usize, usize),
}

/// Editor preview of the three power actions as offered. Dim-as-unavailable would
/// describe a host this screen has not bound.
fn preview_host_label(id: &str) -> &str {
    match id {
        "power.sleep" => "Sleep host",
        "power.reboot" => "Restart host",
        "power.shutdown" => "Shut down host",
        other => other,
    }
}

/// The in-stream quick-action dial. A host feeds it pad or key events and facts, draws it over the
/// stream once per frame, and drains its commands; it never touches the session itself.
pub struct Ring {
    progress: f32,
    committed: bool,
    clockwise: bool,
    centre: (f32, f32),
    /// Drawn opening: twist progress until commit, then a spring toward 1.
    shown: f32,
    sheet: bool,
    armed: Option<String>,
    hint: Option<String>,
    hint_at: Instant,
    last_touch: Instant,
    /// Slot 0…5, or 6 for the centre. `None` until a pad or key moves it.
    highlight: Option<usize>,
    /// Stick past the deadzone owns the highlight: four-way moves are ignored until
    /// it lets go.
    stick: bool,
    /// Tick count hashed into `damage` while animating, so a spring the overlay
    /// would otherwise freeze on its first frame keeps redrawing.
    frame: u64,
    spring: Spring,
    /// Closed for input, still winding in.
    closing: bool,
    /// Per-slot highlight 0…1 (centre is 6), eased so the glow travels instead of snapping.
    hot: [f32; 7],
    sheet_rise: f64,
    /// Hit rects from last draw: slots 0…5, centre 6.
    geom: Vec<Rect>,
    sheet_rect: Rect,
    list: MenuList,
    facts: RingFacts,
    cfg: OverlayConfig,
    pending: VecDeque<RingCommand>,
    cmds: Vec<ConsoleCmd>,
    editing: Option<Editing>,
    edits: VecDeque<EditEvent>,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub fn new() -> Ring {
        Ring {
            progress: 0.0,
            committed: false,
            clockwise: true,
            centre: (0.0, 0.0),
            shown: 0.0,
            sheet: false,
            armed: None,
            hint: None,
            hint_at: Instant::now(),
            last_touch: Instant::now(),
            highlight: None,
            stick: false,
            frame: 0,
            spring: Spring::rest(0.0),
            closing: false,
            hot: [0.0; 7],
            sheet_rise: 0.0,
            geom: vec![Rect::new_empty(); 7],
            sheet_rect: Rect::new_empty(),
            list: MenuList::new(),
            facts: RingFacts::default(),
            cfg: OverlayConfig::platform_default(RingPlatform::Desktop),
            pending: VecDeque::new(),
            cmds: Vec::new(),
            editing: None,
            edits: VecDeque::new(),
        }
    }

    pub fn open(&self) -> bool {
        self.committed || self.progress > 0.0
    }

    /// Open, or closed and still winding in — what `render` and `damage` go by.
    /// Input uses [`Ring::open`]: a closing ring takes nothing.
    fn visible(&self) -> bool {
        self.open() || self.closing
    }

    pub(crate) fn stick_engaged(&self) -> bool {
        self.stick
    }

    /// Adopt stick-engaged when a screen hands focus back while the stick is still
    /// held, so four-way repeats step nothing until `Sector(None)`.
    pub(crate) fn adopt_stick(&mut self, engaged: bool) {
        self.stick = engaged;
    }

    /// Open as the editor at `(x, y)`: no idle-close, slot 0 highlighted so a pad has a start.
    pub(crate) fn edit_at(&mut self, x: f32, y: f32) {
        self.editing = Some(Editing::default());
        self.centre = (x, y);
        self.commit();
        self.highlight = Some(0);
    }

    pub(crate) fn recentre(&mut self, x: f32, y: f32) {
        self.centre = (x, y);
    }

    pub(crate) fn highlight(&self) -> Option<usize> {
        self.highlight
    }

    pub(crate) fn set_highlight(&mut self, k: usize) {
        self.highlight = Some(k.min(5));
    }

    /// True while a disc is lifted or carried — the screen must keep focus on the ring.
    pub(crate) fn carrying(&self) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|e| e.lifted.is_some() || e.drag.is_some())
    }

    pub(crate) fn take_edit(&mut self) -> Option<EditEvent> {
        self.edits.pop_front()
    }

    /// Overlay config is re-parsed only when the blob changes.
    pub fn set_facts(&mut self, facts: &RingFacts) {
        if facts.overlay_actions != self.facts.overlay_actions {
            self.cfg = OverlayConfig::parse(&facts.overlay_actions, RingPlatform::Desktop);
        }
        self.facts = facts.clone();
    }

    // In-stream surface (`skia_overlay`), so a build without that overlay sees these unused.
    // `allow` rather than `cfg`, which would cascade into the parameter types' imports.
    pub fn input(&mut self, input: RingInput) {
        match input {
            RingInput::Turn {
                progress,
                clockwise,
                x,
                y,
            } => {
                if self.committed {
                    return;
                }
                self.progress = progress;
                self.clockwise = clockwise;
                self.centre = (x, y);
            }
            RingInput::Commit => self.commit(),
            RingInput::Cancel => self.close(),
            RingInput::Toggle { x, y } => {
                if self.committed {
                    self.close();
                } else {
                    self.centre = (x, y);
                    self.commit();
                }
            }
        }
    }

    fn commit(&mut self) {
        self.committed = true;
        self.closing = false;
        self.progress = 1.0;
        // The centre is where Confirm already goes with nothing picked, so light it: an
        // opening ring that highlights nothing tells the player their first Enter has no
        // target, when in fact it opens the sheet. `edit_at` moves it to a disc after this.
        // Lit on arrival rather than eased in — it is the starting state, not a move.
        self.highlight = Some(6);
        self.hot[6] = 1.0;
        self.touch();
    }

    fn close(&mut self) {
        self.committed = false;
        self.progress = 0.0;
        // Not a snap: `render` winds `shown` down and clears `closing` on land.
        // Reduce motion closes at once.
        self.closing = self.shown > 0.0 && !crate::theme::reduce_motion();
        if !self.closing {
            self.shown = 0.0;
        }
        self.sheet_rise = 0.0;
        self.sheet = false;
        self.armed = None;
        self.hint = None;
        self.highlight = None;
        self.stick = false;
    }

    fn touch(&mut self) {
        self.last_touch = Instant::now();
    }

    fn say(&mut self, text: String) {
        self.hint = Some(text);
        self.hint_at = Instant::now();
    }

    /// Idle-close after `IDLE_CLOSE` unless the sheet or editor is up; drop arm/hint
    /// after `HINT_LIFE`. Once per frame, before `damage` is read.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        if self.committed
            && !self.sheet
            && self.editing.is_none()
            && self.last_touch.elapsed() > IDLE_CLOSE
        {
            self.close();
        }
        if (self.armed.is_some() || self.hint.is_some()) && self.hint_at.elapsed() > HINT_LIFE {
            self.armed = None;
            self.hint = None;
        }
    }

    pub fn take_command(&mut self) -> Option<RingCommand> {
        self.pending.pop_front()
    }

    pub fn take_cmds(&mut self) -> Vec<ConsoleCmd> {
        std::mem::take(&mut self.cmds)
    }

    /// Overlay damage key: redraw only when this changes.
    pub fn damage(&self) -> u64 {
        if !self.visible() {
            return 0;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ((self.shown * 200.0) as u32).hash(&mut h);
        self.committed.hash(&mut h);
        self.clockwise.hash(&mut h);
        ((self.centre.0 / 4.0) as i32, (self.centre.1 / 4.0) as i32).hash(&mut h);
        self.sheet.hash(&mut h);
        self.armed.hash(&mut h);
        self.hint.hash(&mut h);
        self.highlight.hash(&mut h);
        self.list.cursor.hash(&mut h);
        self.facts.touch_mode.hash(&mut h);
        self.facts.invert_scroll.hash(&mut h);
        self.facts.stats_tier.hash(&mut h);
        self.facts.mic_muted.hash(&mut h);
        self.facts.pad_mouse_on.hash(&mut h);
        self.facts.audio_mute.hash(&mut h);
        self.facts.pad_mouse_target.hash(&mut h);
        self.facts.pointer_granted.hash(&mut h);
        self.facts.mode.hash(&mut h);
        // Hash `frame` while animating so the overlay keeps drawing until springs land.
        if self.animating() {
            self.frame.hash(&mut h);
        }
        h.finish().max(1)
    }

    /// True while a spring, ease, or entrance is short of its target. Read before
    /// the next frame: this is the state the last render left.
    fn animating(&self) -> bool {
        if self.closing {
            return true;
        }
        if self.committed && (self.shown != 1.0 || self.spring.vel != 0.0) {
            return true;
        }
        if self.sheet && (self.sheet_rise < 1.0 || self.list.animating()) {
            return true;
        }
        (0..7).any(|k| self.hot[k] != self.hot_target(k))
    }

    fn hot_target(&self, k: usize) -> f32 {
        let lifted = self.editing.as_ref().and_then(|e| e.lifted);
        if self.highlight == Some(k) || lifted == Some(k) {
            1.0
        } else {
            0.0
        }
    }

    /// Desktop shell cache; empty on Android and on a GL host, where the editor previews via
    /// [`preview_host_label`]. Gated on `desktop`, not the OS: webOS is Linux too, and there
    /// the fetch that fills this cache does not exist.
    fn actions(&self) -> Vec<ActionInfo> {
        #[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
        {
            pf_client_core::host_actions::cached(&self.facts.fp_hex)
        }
        #[cfg(not(all(feature = "desktop", any(target_os = "linux", windows))))]
        {
            Vec::new()
        }
    }

    fn spec(&self, slot: &SlotId) -> Spec {
        let plain = |id: &str, label: &str, short: &str| Spec {
            id: id.into(),
            label: label.into(),
            short: short.into(),
            enabled: true,
            reason: String::new(),
            armed: false,
            toggle: false,
            state: String::new(),
        };
        let f = &self.facts;
        match slot {
            SlotId::EndStream => Spec {
                armed: true,
                ..plain("end_stream", "End stream", "End")
            },
            SlotId::DisconnectLinger => plain(
                "disconnect_linger",
                "Disconnect, keep the game running",
                "Leave",
            ),
            SlotId::TouchMode => Spec {
                toggle: true,
                state: match f.touch_mode.as_str() {
                    "pointer" => "Direct pointer",
                    "touch" => "Touch passthrough",
                    "off" => "Off",
                    _ => "Trackpad",
                }
                .into(),
                ..plain(
                    "touch_mode",
                    "Touch mode",
                    match f.touch_mode.as_str() {
                        "pointer" => "Point",
                        "touch" => "Pass",
                        "off" => "Off",
                        _ => "Track",
                    },
                )
            },
            SlotId::Keyboard => plain("keyboard", "Keyboard", "Keys"),
            SlotId::Stats => Spec {
                toggle: true,
                state: f.stats_tier.clone(),
                ..plain("stats", "Statistics", "Stats")
            },
            SlotId::Mic => Spec {
                enabled: f.mic_available,
                reason: "No microphone is running this session".into(),
                toggle: true,
                state: if f.mic_muted { "Muted" } else { "On" }.into(),
                ..plain(
                    "mic",
                    "Microphone",
                    if f.mic_muted { "Mic ✕" } else { "Mic" },
                )
            },
            SlotId::Pad => Spec {
                enabled: false,
                reason: "The virtual controller is for phones and tablets".into(),
                ..plain("pad", "Virtual controller", "Pad")
            },
            SlotId::SendText => Spec {
                enabled: false,
                reason: "Not on this client yet — use the keyboard".into(),
                ..plain("send_text", "Send text", "Text")
            },
            SlotId::Guide => plain("guide", "Guide button", "Guide"),
            SlotId::Qam => plain("qam", "Quick access menu", "QAM"),
            SlotId::PadMouse => Spec {
                enabled: f.pointer_granted && f.pad_mouse_target != 0,
                reason: if f.pointer_granted {
                    "No controller is connected"
                } else {
                    "This host only allows controller input"
                }
                .into(),
                toggle: true,
                state: if f.pad_mouse_on { "On" } else { "Off" }.into(),
                ..plain(
                    "pad_mouse",
                    "Controller mouse",
                    if f.pad_mouse_on { "Mouse ✓" } else { "Mouse" },
                )
            },
            SlotId::StreamMute => Spec {
                toggle: true,
                // The state describes the mute: `Off` while audible, else the shared
                // sentence naming whose mute it is.
                state: punktfunk_core::audio::audio_mute_label(f.audio_mute)
                    .unwrap_or("Off")
                    .into(),
                ..plain(
                    "stream_mute",
                    "Mute this stream",
                    if f.audio_mute & punktfunk_core::audio::AUDIO_MUTE_LOCAL != 0 {
                        "Mute ✕"
                    } else {
                        "Mute"
                    },
                )
            },
            SlotId::Host(id) if self.editing.is_some() => Spec {
                armed: true,
                ..plain(&format!("host:{id}"), preview_host_label(id), "Power")
            },
            SlotId::Host(id) => {
                let act = self.actions().into_iter().find(|a| &a.id == id);
                Spec {
                    enabled: act.as_ref().is_some_and(|a| a.available),
                    reason: act
                        .as_ref()
                        .and_then(|a| a.unavailable_reason.clone())
                        .unwrap_or_else(|| "This host doesn't offer it".into()),
                    armed: act.as_ref().is_none_or(|a| a.danger),
                    ..plain(
                        &format!("host:{id}"),
                        act.as_ref().map_or(id.as_str(), |a| a.label()),
                        "Power",
                    )
                }
            }
            SlotId::Shortcut(id) => {
                let s = self.cfg.shortcut(id);
                let keys: Vec<String> = s.map(|s| s.keys.clone()).unwrap_or_default();
                let chip = chord_chip(&keys);
                Spec {
                    enabled: !keys.is_empty() && keys.iter().all(|k| key_vk(k).is_some()),
                    reason: "A key in this chord is unknown".into(),
                    ..plain(
                        &format!("shortcut:{id}"),
                        s.filter(|s| !s.label.is_empty())
                            .map_or(chip.as_str(), |s| s.label.as_str()),
                        &chip,
                    )
                }
            }
        }
    }

    /// Dim: say why. Destructive: arm, fire on the second press. Toggles stay
    /// open; commands close.
    fn fire(&mut self, slot: &SlotId) {
        self.touch();
        let s = self.spec(slot);
        if !s.enabled {
            self.armed = None;
            self.say(s.reason);
            return;
        }
        if s.armed && self.armed.as_deref() != Some(&s.id) {
            self.armed = Some(s.id.clone());
            self.say(format!("{}? Press again", s.label));
            return;
        }
        self.armed = None;
        self.hint = None;
        match slot {
            SlotId::EndStream => {
                self.close();
                self.pending.push_back(RingCommand::EndStream);
            }
            SlotId::DisconnectLinger => {
                self.close();
                self.pending.push_back(RingCommand::DisconnectLinger);
            }
            SlotId::TouchMode => self.pending.push_back(RingCommand::CycleTouchMode),
            SlotId::Keyboard => {
                self.close();
                self.pending.push_back(RingCommand::Keyboard);
            }
            SlotId::Stats => self.pending.push_back(RingCommand::CycleStats),
            SlotId::Mic => self.pending.push_back(RingCommand::ToggleMic),
            SlotId::PadMouse => self.pending.push_back(RingCommand::TogglePadMouse),
            SlotId::StreamMute => self.pending.push_back(RingCommand::ToggleStreamMute),
            SlotId::Pad | SlotId::SendText => {}
            // The host's own overlay is about to take the screen: close first, like End stream.
            SlotId::Guide | SlotId::Qam => {
                let bit = if *slot == SlotId::Guide {
                    wire::BTN_GUIDE
                } else {
                    wire::BTN_MISC1
                };
                self.close();
                self.pending.push_back(RingCommand::TapButton(bit));
            }
            SlotId::Host(id) => {
                let f = &self.facts;
                self.cmds.push(ConsoleCmd::HostAction {
                    addr: f.addr.clone(),
                    mgmt: f.mgmt_port,
                    fp_hex: f.fp_hex.clone(),
                    host_name: f.host_name.clone(),
                    action_id: id.clone(),
                    label: s.label.clone(),
                });
                self.close();
            }
            SlotId::Shortcut(id) => {
                if let Some(sc) = self.cfg.shortcut(id) {
                    let keys = sc.keys.clone();
                    self.close();
                    self.pending.push_back(RingCommand::Shortcut(keys));
                }
            }
        }
        if s.toggle {
            // Label is the state before the command; next frame's facts correct it.
            self.say(format!("{}: {}", s.label, s.state));
        }
    }

    fn sheet_rows(&self) -> Vec<SheetRow> {
        let mut rows = vec![
            SheetRow::Slot(SlotId::EndStream),
            SheetRow::Slot(SlotId::DisconnectLinger),
            SheetRow::Resolution,
            SheetRow::Refresh,
            SheetRow::Slot(SlotId::TouchMode),
            SheetRow::ScrollInvert,
            SheetRow::Slot(SlotId::Keyboard),
            SheetRow::Slot(SlotId::Guide),
            SheetRow::Slot(SlotId::Qam),
            SheetRow::Slot(SlotId::PadMouse),
            SheetRow::Slot(SlotId::Stats),
            SheetRow::Slot(SlotId::Mic),
            SheetRow::Slot(SlotId::StreamMute),
        ];
        rows.extend(
            self.actions()
                .into_iter()
                .filter(ActionInfo::offerable)
                .map(|a| SheetRow::Slot(SlotId::Host(a.id))),
        );
        rows.extend(
            self.cfg
                .shortcuts
                .iter()
                .map(|s| SheetRow::Slot(SlotId::Shortcut(s.id.clone()))),
        );
        rows
    }

    fn res_label(&self) -> String {
        let (w, h, _) = self.facts.mode;
        let (nw, nh, _) = self.facts.native_mode;
        if (w, h) == (nw, nh) {
            format!("Native ({w}×{h})")
        } else {
            RES_PRESETS
                .iter()
                .find(|p| (p.1, p.2) == (w, h))
                .map_or_else(|| format!("{w}×{h}"), |p| p.0.into())
        }
    }

    fn sheet_row_spec(&self, row: &SheetRow) -> RowSpec {
        match row {
            SheetRow::Resolution => RowSpec::field("Resolution", self.res_label(), ""),
            SheetRow::Refresh => RowSpec::field("Refresh", format!("{} Hz", self.facts.mode.2), ""),
            SheetRow::ScrollInvert => {
                let value = if !self.facts.pointer_granted {
                    "Pointer input is not allowed"
                } else if self.facts.invert_scroll {
                    "On"
                } else {
                    "Off"
                };
                let mut row = RowSpec::field("Invert scroll direction", value.into(), "");
                row.enabled = self.facts.pointer_granted;
                row
            }
            SheetRow::Slot(slot) => {
                let s = self.spec(slot);
                let value = if !s.enabled {
                    s.reason.clone()
                } else if self.armed.as_deref() == Some(&s.id) {
                    "press again".into()
                } else {
                    s.state.clone()
                };
                let mut r = RowSpec::field(s.label, value, "");
                r.enabled = s.enabled;
                r
            }
        }
    }

    /// Cycle resolution/refresh presets; native is first.
    fn adjust(&mut self, row: &SheetRow, dir: i32) {
        self.touch();
        let (w, h, hz) = self.facts.mode;
        let (nw, nh, nhz) = self.facts.native_mode;
        match row {
            SheetRow::Resolution => {
                let mut options: Vec<(u32, u32)> = vec![(nw, nh)];
                options.extend(RES_PRESETS.iter().map(|p| (p.1, p.2)));
                let i = options.iter().position(|o| *o == (w, h)).unwrap_or(0) as i32;
                let n = options.len() as i32;
                let (rw, rh) = options[((i + dir).rem_euclid(n)) as usize];
                self.pending.push_back(RingCommand::RequestMode {
                    width: rw,
                    height: rh,
                    refresh_hz: hz,
                });
            }
            SheetRow::Refresh => {
                let mut options = vec![nhz];
                options.extend(HZ_PRESETS);
                options.dedup();
                let i = options.iter().position(|o| *o == hz).unwrap_or(0) as i32;
                let n = options.len() as i32;
                let rhz = options[((i + dir).rem_euclid(n)) as usize];
                self.pending.push_back(RingCommand::RequestMode {
                    width: w,
                    height: h,
                    refresh_hz: rhz,
                });
            }
            SheetRow::ScrollInvert => {
                if self.facts.pointer_granted {
                    self.pending.push_back(RingCommand::ToggleScrollInvert);
                }
            }
            SheetRow::Slot(_) => {}
        }
    }

    fn sheet_msg(&mut self, msg: ListMsg, rows: &[SheetRow]) {
        let Some(row) = rows.get(self.list.cursor).cloned() else {
            return;
        };
        match msg {
            ListMsg::None => {}
            ListMsg::Adjust(d) => self.adjust(&row, d),
            ListMsg::Activate => match &row {
                SheetRow::Slot(slot) => self.fire(slot),
                SheetRow::Resolution | SheetRow::Refresh | SheetRow::ScrollInvert => {
                    self.adjust(&row, 1)
                }
            },
        }
    }

    /// Consumed while open (the scrim). Editor: only hits on a disc or an in-flight
    /// carry, so widgets under the ring keep their pointer.
    pub(crate) fn pointer(&mut self, p: Pointer) -> bool {
        if !self.open() {
            return false;
        }
        if self.editing.is_some() {
            return self.edit_pointer(p);
        }
        match p.kind {
            // Not the ring's: a drag falls back to Scroll ticks, a slow tap stays a tap.
            PointerKind::PanStart { .. }
            | PointerKind::Pan { .. }
            | PointerKind::Fling { .. }
            | PointerKind::LongPress => return false,
            PointerKind::Back => {
                if self.sheet {
                    self.sheet = false;
                } else {
                    self.close();
                }
            }
            PointerKind::Press if self.sheet => {
                if p.hits(self.sheet_rect) {
                    self.touch();
                    let rows = self.sheet_rows();
                    let (msg, _) = self.list.pointer(p, rows.len());
                    self.sheet_msg(msg, &rows);
                } else {
                    self.sheet = false;
                }
            }
            PointerKind::Press => match p.pick(&self.geom) {
                Some(6) => {
                    self.touch();
                    self.sheet = true;
                    self.list = MenuList::new();
                }
                Some(k) => {
                    if let Some(slot) = self.cfg.ring[k].clone() {
                        self.fire(&slot);
                    }
                }
                None => self.close(),
            },
            PointerKind::Scroll { .. } if self.sheet => {
                let rows = self.sheet_rows();
                let (msg, _) = self.list.pointer(p, rows.len());
                self.sheet_msg(msg, &rows);
            }
            _ => {}
        }
        true
    }

    /// Press starts a carry; release under slop is pick, over another slot is swap.
    /// Off the discs: the screen's.
    fn edit_pointer(&mut self, p: Pointer) -> bool {
        let geom = self.geom.clone();
        let Some(ed) = self.editing.as_mut() else {
            return false;
        };
        match p.kind {
            PointerKind::Press => match p.pick(&geom) {
                Some(k) if k < 6 => {
                    ed.drag = Some(Drag {
                        slot: k,
                        x0: p.x,
                        y0: p.y,
                        dx: 0.0,
                        dy: 0.0,
                    });
                    ed.lifted = None;
                    self.highlight = Some(k);
                    self.touch();
                    true
                }
                _ => false,
            },
            PointerKind::Move => match ed.drag.as_mut() {
                Some(d) => {
                    d.dx = (p.x - d.x0) as f32;
                    d.dy = (p.y - d.y0) as f32;
                    true
                }
                None => false,
            },
            PointerKind::Release => {
                let Some(d) = ed.drag.take() else {
                    return false;
                };
                // Carried disc's rect has followed the pointer, so look past it.
                let target = (0..6).find(|&j| j != d.slot && p.hits(geom[j]));
                match target {
                    Some(j) if d.moved() => self.edits.push_back(EditEvent::Swap(d.slot, j)),
                    _ if !d.moved() => self.edits.push_back(EditEvent::Pick(d.slot)),
                    _ => {}
                }
                true
            }
            PointerKind::Cancel => {
                ed.drag = None;
                false
            }
            PointerKind::Scroll { .. }
            | PointerKind::Back
            | PointerKind::PanStart { .. }
            | PointerKind::Pan { .. }
            | PointerKind::Fling { .. }
            | PointerKind::LongPress => false,
        }
    }

    /// Pad vocabulary on keys. Always consumed while open, including unknown keys.
    ///
    /// Tab is the keyboard's own step, and the only one that visits every stop: the six
    /// discs then the centre. The arrows speak the pad's language, where the centre is a
    /// released stick rather than a direction.
    pub fn key(&mut self, key: Key) -> bool {
        if !self.open() {
            return false;
        }
        if key == Key::Tab && !self.sheet && self.editing.is_none() {
            self.touch();
            self.highlight = Some((self.highlight.unwrap_or(6) + 1) % 7);
            return true;
        }
        let ev = match key {
            Key::Escape => MenuEvent::Back,
            Key::Return | Key::Space => MenuEvent::Confirm,
            Key::Up => MenuEvent::Move(MenuDir::Up),
            Key::Down => MenuEvent::Move(MenuDir::Down),
            Key::Left => MenuEvent::Move(MenuDir::Left),
            Key::Right => MenuEvent::Move(MenuDir::Right),
            // In the sheet it is the list's next row; the ring arm above took it otherwise.
            Key::Tab => MenuEvent::Move(MenuDir::Down),
            Key::Y => MenuEvent::Secondary,
            _ => return true,
        };
        self.menu(ev);
        true
    }

    /// Pad while open: Right/Left step the ring, Up is 12 o'clock, Down is 6, Y is
    /// centre. A fires (centre opens the sheet); B closes (sheet first). In the sheet
    /// the list takes moves; Left/Right adjust a resolution row.
    pub fn menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        if !self.open() {
            return None;
        }
        self.touch();
        if self.editing.is_some() {
            return self.edit_menu(ev);
        }
        if self.sheet {
            if ev == MenuEvent::Back {
                self.sheet = false;
                return Some(MenuPulse::Confirm);
            }
            let rows = self.sheet_rows();
            let (msg, pulse) = self.list.menu(ev, rows.len());
            self.sheet_msg(msg, &rows);
            return pulse;
        }
        let h = self.highlight.unwrap_or(6);
        match ev {
            // Stick angle is the slot; neutral is the centre.
            MenuEvent::Sector(sector) => {
                self.stick = sector.is_some();
                let next = sector.map_or(6, |k| usize::from(k) % 6);
                if self.highlight == Some(next) {
                    return None;
                }
                self.highlight = Some(next);
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(_) if self.stick => None,
            MenuEvent::Move(MenuDir::Right) => {
                self.highlight = Some(if h >= 6 { 0 } else { (h + 1) % 6 });
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Left) => {
                self.highlight = Some(if h >= 6 { 5 } else { (h + 5) % 6 });
                Some(MenuPulse::Move)
            }
            // Up is 12 o'clock, down is 6 — and once there, on into the centre, which is
            // inside the ring. Without that a keyboard reaches the sheet only through Y,
            // a letter nothing on screen names.
            MenuEvent::Move(MenuDir::Up) => {
                self.highlight = Some(if h == 0 { 6 } else { 0 });
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Down) => {
                self.highlight = Some(if h == 3 { 6 } else { 3 });
                Some(MenuPulse::Move)
            }
            MenuEvent::Secondary => {
                self.highlight = Some(6);
                Some(MenuPulse::Move)
            }
            MenuEvent::Confirm => {
                if h >= 6 {
                    self.sheet = true;
                    self.list = MenuList::new();
                } else if let Some(slot) = self.cfg.ring[h].clone() {
                    self.fire(&slot);
                } else {
                    return Some(MenuPulse::Boundary);
                }
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                self.close();
                Some(MenuPulse::Confirm)
            }
            _ => None,
        }
    }

    /// Editor pad: Right/Left step, Up/Down jump to 12 and 6. A picks or drops a
    /// lift; Y lifts (again puts down). B with a lift puts it down; else B is the
    /// screen's. Centre is inert.
    fn edit_menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        let h = self.highlight.unwrap_or(0).min(5);
        let ed = self.editing.as_mut()?;
        match ev {
            // Neutral leaves the highlight: the centre is inert here.
            MenuEvent::Sector(sector) => {
                self.stick = sector.is_some();
                let next = usize::from(sector?) % 6;
                if self.highlight == Some(next) {
                    return None;
                }
                self.highlight = Some(next);
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(_) if self.stick => None,
            MenuEvent::Move(MenuDir::Right) => {
                self.highlight = Some((h + 1) % 6);
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Left) => {
                self.highlight = Some((h + 5) % 6);
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Up) => {
                self.highlight = Some(0);
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Down) => {
                self.highlight = Some(3);
                Some(MenuPulse::Move)
            }
            MenuEvent::Secondary => {
                ed.lifted = if ed.lifted == Some(h) { None } else { Some(h) };
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Confirm => {
                match ed.lifted.take() {
                    Some(j) if j != h => self.edits.push_back(EditEvent::Swap(j, h)),
                    Some(_) => {}
                    None => self.edits.push_back(EditEvent::Pick(h)),
                }
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back if ed.lifted.is_some() => {
                ed.lifted = None;
                Some(MenuPulse::Confirm)
            }
            _ => None,
        }
    }

    /// `scale` is the chrome scale.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        scale: f32,
        fonts: &Fonts,
        dt: f64,
    ) {
        if !self.visible() {
            return;
        }
        // Twist drives opening; after commit the spring overshoots then seats. Reduce
        // motion: fade in place, no spring/spiral. Closing winds `shown` down.
        let reduce = crate::theme::reduce_motion();
        if self.closing {
            self.shown = approach(f64::from(self.shown), 0.0, dt, 0.045) as f32;
            self.spring = Spring::rest(f64::from(self.shown));
            if self.shown < 0.02 {
                self.shown = 0.0;
                self.closing = false;
                self.geom.iter_mut().for_each(|g| *g = Rect::new_empty());
                self.sheet_rect = Rect::new_empty();
                return;
            }
        } else if self.committed {
            if reduce {
                self.shown = 1.0;
                self.spring = Spring::rest(1.0);
            } else {
                self.spring.step_spec(1.0, springs::RING, dt);
                self.spring.settle(1.0, 0.002, 0.02);
                self.shown = self.spring.pos as f32;
            }
        } else {
            self.shown = self.progress;
            self.spring = Spring::rest(f64::from(self.progress));
        }
        for k in 0..7 {
            let target = self.hot_target(k);
            self.hot[k] = if reduce {
                target
            } else {
                approach(f64::from(self.hot[k]), f64::from(target), dt, 0.07) as f32
            };
            if (self.hot[k] - target).abs() < 0.001 {
                self.hot[k] = target;
            }
        }
        self.sheet_rise = if !self.sheet {
            0.0
        } else if reduce {
            1.0
        } else {
            let r = approach(self.sheet_rise, 1.0, dt, 0.09);
            if r > 0.995 {
                1.0
            } else {
                r
            }
        };
        let shown = self.shown;
        // Opacity never exceeds 1 — the spring's overshoot is travel and size, not alpha.
        let vis = shown.min(1.0);
        let radius = RADIUS * scale;
        let slot_d = SLOT_D * scale;
        let margin = radius + slot_d / 2.0 + 16.0 * scale;
        let cx = self
            .centre
            .0
            .clamp(margin, (width as f32 - margin).max(margin));
        let cy = self
            .centre
            .1
            .clamp(margin, (height as f32 - margin).max(margin));
        let white = |a: f32| Color4f::new(1.0, 1.0, 1.0, a);

        // Scrim while open so nothing reaches the stream. Editor: none — its screen
        // owns the backdrop and the widgets under the ring.
        let editing = self.editing.is_some();
        if !editing {
            canvas.draw_rect(
                Rect::from_wh(width as f32, height as f32),
                &ring_scrim(cx, cy, radius * 2.8, vis),
            );
        }
        let lifted = self.editing.as_ref().and_then(|e| e.lifted);
        let carried = self
            .editing
            .as_ref()
            .and_then(|e| e.drag.as_ref().map(|d| (d.slot, d.dx, d.dy)));

        let armed = self.armed.clone();
        for k in 0..6 {
            let q_raw = (shown - k as f32 * SLOT_LAG) / (1.0 - 5.0 * SLOT_LAG);
            let q = q_raw.clamp(0.0, 1.0);
            if q <= 0.0 {
                self.geom[k] = Rect::new_empty();
                continue;
            }
            // Overshoot is only `shown - 1`. Do not lift `q_raw` past 1: the lag
            // divide puts early slots above 1 near rest, and they would park past
            // their seats. Reduce motion: sit in place, `q` drives alpha only.
            let over = (shown - 1.0).max(0.0);
            let travel = if reduce { 1.0 } else { (q + over).min(1.15) };
            let turn = if self.clockwise { -40.0 } else { 40.0 };
            let deg = pf_client_core::ring::slot_angle_deg(k) + (1.0 - travel.min(1.0)) * turn;
            let (s, c) = deg.to_radians().sin_cos();
            let (mut x, mut y) = (cx + radius * travel * c, cy + radius * travel * s);
            let hot = self.hot[k];
            let mut r = slot_d / 2.0 * (0.6 + 0.4 * travel) * (1.0 + 0.08 * hot);
            if let Some((_, dx, dy)) = carried.filter(|(slot, _, _)| *slot == k) {
                x += dx;
                y += dy;
                r *= 1.08;
            } else if lifted == Some(k) {
                r *= 1.12;
            }
            let spec = self.cfg.ring[k].as_ref().map(|slot| self.spec(slot));
            let is_armed = spec
                .as_ref()
                .is_some_and(|s| armed.as_deref() == Some(&s.id));
            canvas.draw_circle(
                Point::new(x, y + 3.0 * scale),
                r,
                &soft_shadow(0.5 * q, 7.0 * scale),
            );
            draw_disc(
                canvas,
                fonts,
                x,
                y,
                r,
                q,
                scale,
                spec.as_ref(),
                is_armed,
                white,
            );
            if spec.is_none() && editing {
                // Editor: empty slot shows a plus. In-stream: plain glass.
                crate::icons::draw_icon(canvas, crate::icons::PLUS, x, y, r * 0.7, white(0.35 * q));
            }
            canvas.draw_circle(
                Point::new(x, y),
                r - 0.5 * scale,
                &rim_light(y - r, y, 0.32 * q, scale),
            );
            highlight_ring(canvas, x, y, r, hot * q, scale, white);
            self.geom[k] = Rect::from_xywh(x - r, y - r, 2.0 * r, 2.0 * r);
        }
        let cq_raw = (shown - 6.0 * SLOT_LAG) / (1.0 - 6.0 * SLOT_LAG);
        let cq = cq_raw.clamp(0.0, 1.0);
        if cq > 0.0 {
            // Overshoot is only `shown - 1`, same as the slots.
            let over = (shown - 1.0).max(0.0);
            let pop = if reduce { 1.0 } else { (cq + over).min(1.15) };
            let hot = self.hot[6];
            let r = CENTRE_D * scale / 2.0 * (0.6 + 0.4 * pop) * (1.0 + 0.06 * hot);
            // Editor: centre is not a slot, so dimmed and inert.
            let more = Spec {
                id: "more".into(),
                label: "More".into(),
                short: "More".into(),
                enabled: !editing,
                reason: String::new(),
                armed: false,
                toggle: false,
                state: String::new(),
            };
            canvas.draw_circle(
                Point::new(cx, cy + 3.0 * scale),
                r,
                &soft_shadow(0.5 * cq, 7.0 * scale),
            );
            draw_disc(
                canvas,
                fonts,
                cx,
                cy,
                r,
                cq,
                scale,
                Some(&more),
                false,
                white,
            );
            canvas.draw_circle(
                Point::new(cx, cy),
                r - 0.5 * scale,
                &rim_light(cy - r, cy, 0.32 * cq, scale),
            );
            highlight_ring(canvas, cx, cy, r, hot * cq, scale, white);
            self.geom[6] = Rect::from_xywh(cx - r, cy - r, 2.0 * r, 2.0 * r);
        } else {
            self.geom[6] = Rect::new_empty();
        }
        let label = self.hint.clone().or_else(|| match self.highlight {
            _ if lifted.is_some() => Some("Move to a slot and press A to swap".into()),
            Some(6) => Some("More".into()),
            Some(k) => Some(
                self.cfg.ring[k]
                    .as_ref()
                    .map(|s| self.spec(s).label)
                    .unwrap_or_else(|| {
                        if editing {
                            "Empty — press A to choose".into()
                        } else {
                            "Empty".into()
                        }
                    }),
            ),
            None => None,
        });
        if let Some(hint) = &label {
            let size = f64::from(13.0 * scale);
            let tw = fonts.measure(hint, W::Medium, size);
            let (px, py) = (14.0 * scale, 8.0 * scale);
            let (w, h) = (tw + 2.0 * px, LABEL_H * scale);
            // Fade with the ring; 8 px rise as `vis` goes to 1.
            let (x, y) = (
                cx - w / 2.0,
                cy + radius + slot_d + (1.0 - vis) * 8.0 * scale,
            );
            canvas.draw_rrect(
                RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
                &fill(Color4f::new(0.0, 0.0, 0.0, 0.62 * vis)),
            );
            fonts.draw(
                canvas,
                hint,
                f64::from(x + px),
                f64::from(y + py + size as f32 * 0.8),
                W::Medium,
                size,
                white(0.92 * vis),
            );
        }
        if self.sheet {
            self.render_sheet(canvas, width, height, scale, fonts, dt);
        } else {
            self.sheet_rect = Rect::new_empty();
        }
    }

    fn render_sheet(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        scale: f32,
        fonts: &Fonts,
        dt: f64,
    ) {
        let rows = self.sheet_rows();
        let specs: Vec<RowSpec> = rows.iter().map(|r| self.sheet_row_spec(r)).collect();
        let k = f64::from(scale);
        let w = (520.0 * scale).min(width as f32 * 0.92);
        let h = ((rows.len() as f32 * 50.0 + 24.0) * scale).min(height as f32 * 0.6);
        let rise = self.sheet_rise as f32;
        let x = (width as f32 - w) / 2.0;
        // 28 px rise into seat; rows fan in on the list's own entrance.
        let y = height as f32 - h - 16.0 * scale + (1.0 - rise) * 28.0 * scale;
        let rect = Rect::from_xywh(x, y, w, h);
        let corner = 16.0 * scale;
        canvas.draw_rrect(
            RRect::new_rect_xy(rect.with_offset((0.0, 4.0 * scale)), corner, corner),
            &soft_shadow(0.4 * rise, 12.0 * scale),
        );
        canvas.draw_rrect(
            RRect::new_rect_xy(rect, corner, corner),
            &fill(Color4f::new(0.0, 0.0, 0.0, 0.78 * rise)),
        );
        canvas.draw_rrect(
            RRect::new_rect_xy(rect, corner, corner),
            &stroke(Color4f::new(1.0, 1.0, 1.0, 0.18 * rise), scale),
        );
        let inner = Rect::from_ltrb(
            rect.left + 8.0 * scale,
            rect.top + 12.0 * scale,
            rect.right - 8.0 * scale,
            rect.bottom - 12.0 * scale,
        );
        canvas.save();
        canvas.clip_rect(inner, None, None);
        self.list.render(canvas, inner, &specs, fonts, k, dt, true);
        canvas.restore();
        self.sheet_rect = rect;
    }
}

/// Shared slot icon (`overlay_actions::slot_icon` via [`crate::icons`]) so GTK/Windows
/// editors cannot disagree. Mic swaps to its struck form while muted. `None` for a
/// shortcut (the chord is the face) and any host action beyond the three powers.
fn slot_icon(id: &str, state: &str) -> Option<crate::icons::Icon> {
    crate::icons::by_name(pf_client_core::overlay_actions::slot_icon(id, state)?)
}

/// Glow past the edge and a crisp ring, both at `amount`. Caller eases so the mark
/// travels between discs.
fn highlight_ring(
    canvas: &Canvas,
    x: f32,
    y: f32,
    r: f32,
    amount: f32,
    scale: f32,
    white: impl Fn(f32) -> Color4f,
) {
    if amount <= 0.01 {
        return;
    }
    let rr = r + 3.0 * scale;
    canvas.draw_circle(
        Point::new(x, y),
        rr,
        &glow_ring(0.5 * amount, 3.0 * scale, 6.0 * scale),
    );
    canvas.draw_circle(
        Point::new(x, y),
        rr,
        &stroke(white(0.85 * amount), 2.0 * scale),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_disc(
    canvas: &Canvas,
    fonts: &Fonts,
    x: f32,
    y: f32,
    r: f32,
    alpha: f32,
    scale: f32,
    spec: Option<&Spec>,
    armed: bool,
    white: impl Fn(f32) -> Color4f,
) {
    let base = if armed { 0.75 } else { 0.55 };
    canvas.draw_circle(
        Point::new(x, y),
        r,
        &fill(Color4f::new(0.0, 0.0, 0.0, base * alpha)),
    );
    canvas.draw_circle(
        Point::new(x, y),
        r,
        &stroke(white((if armed { 0.6 } else { 0.18 }) * alpha), scale),
    );
    let Some(spec) = spec else { return };
    let color = if armed {
        Color4f::new(1.0, 0.35, 0.35, alpha)
    } else if spec.enabled {
        white(0.95 * alpha)
    } else {
        white(0.35 * alpha)
    };
    let size = f64::from(12.0 * scale * (r / (SLOT_D * scale / 2.0)).clamp(0.6, 1.2));
    // Shortcut: stacked keycap. One line ran to the disc's edge and past it.
    if spec.id.starts_with("shortcut:") && spec.short.contains('+') {
        keycap_text(canvas, fonts, x, y, r, size, &spec.short, color);
        return;
    }
    if let Some(icon) = slot_icon(&spec.id, &spec.state) {
        crate::icons::draw_icon(canvas, icon, x, y, r * 1.05, color);
        return;
    }
    let tw = fonts.measure(&spec.short, W::SemiBold, size);
    fonts.draw(
        canvas,
        &spec.short,
        f64::from(x - tw / 2.0),
        f64::from(y + size as f32 * 0.35),
        W::SemiBold,
        size,
        color,
    );
}

/// Chord on a disc: modifiers small on top, key large under them. `size` is the
/// disc's base text size.
#[allow(clippy::too_many_arguments)]
fn keycap_text(
    canvas: &Canvas,
    fonts: &Fonts,
    x: f32,
    y: f32,
    r: f32,
    size: f64,
    chip: &str,
    color: Color4f,
) {
    let Some((mods, key)) = chip.rsplit_once('+') else {
        let tw = fonts.measure(chip, W::SemiBold, size * 1.25);
        fonts.draw(
            canvas,
            chip,
            f64::from(x - tw / 2.0),
            f64::from(y + size as f32 * 0.45),
            W::SemiBold,
            size * 1.25,
            color,
        );
        return;
    };
    let mods = mods.replace('+', " ");
    let small = size * 0.62;
    let big = size * 1.25;
    let mw = fonts.measure(&mods, W::Medium, small);
    fonts.draw(
        canvas,
        &mods,
        f64::from(x - mw / 2.0),
        f64::from(y - r * 0.22),
        W::Medium,
        small,
        color,
    );
    let kw = fonts.measure(key, W::SemiBold, big);
    fonts.draw(
        canvas,
        key,
        f64::from(x - kw / 2.0),
        f64::from(y + r * 0.42),
        W::SemiBold,
        big,
        color,
    );
}

/// Editor preview of a shortcut disc. Empty `chip` draws an empty disc.
pub(crate) fn draw_keycap_disc(
    canvas: &Canvas,
    fonts: &Fonts,
    x: f32,
    y: f32,
    r: f32,
    scale: f32,
    chip: &str,
) {
    canvas.draw_circle(
        Point::new(x, y),
        r,
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.55)),
    );
    canvas.draw_circle(
        Point::new(x, y),
        r,
        &stroke(Color4f::new(1.0, 1.0, 1.0, 0.18), scale),
    );
    if chip.is_empty() {
        return;
    }
    let size = f64::from(12.0 * scale * (r / (SLOT_D * scale / 2.0)).clamp(0.6, 1.6));
    keycap_text(
        canvas,
        fonts,
        x,
        y,
        r,
        size,
        chip,
        Color4f::new(1.0, 1.0, 1.0, 0.95),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> RingFacts {
        RingFacts {
            mode: (1920, 1080, 60),
            native_mode: (1920, 1080, 60),
            mic_available: true,
            ..RingFacts::default()
        }
    }

    #[test]
    fn scroll_inversion_sheet_tracks_live_value_and_grants() {
        let mut r = Ring::new();
        let mut f = RingFacts {
            pointer_granted: true,
            ..facts()
        };
        r.set_facts(&f);
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        let rows = r.sheet_rows();
        r.list.cursor = rows
            .iter()
            .position(|row| *row == SheetRow::ScrollInvert)
            .unwrap();
        assert_eq!(
            r.sheet_row_spec(&SheetRow::ScrollInvert).value.as_deref(),
            Some("Off")
        );
        r.sheet_msg(ListMsg::Activate, &rows);
        assert_eq!(r.take_command(), Some(RingCommand::ToggleScrollInvert));
        let damage = r.damage();
        f.invert_scroll = true;
        r.set_facts(&f);
        assert_eq!(
            r.sheet_row_spec(&SheetRow::ScrollInvert).value.as_deref(),
            Some("On")
        );
        assert_ne!(r.damage(), damage);
        f.pointer_granted = false;
        r.set_facts(&f);
        assert!(!r.sheet_row_spec(&SheetRow::ScrollInvert).enabled);
        r.sheet_msg(ListMsg::Activate, &rows);
        r.sheet_msg(ListMsg::Adjust(1), &rows);
        assert_eq!(r.take_command(), None);
    }

    #[test]
    fn a_twist_opens_and_a_lift_short_of_commit_closes() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Turn {
            progress: 0.4,
            clockwise: true,
            x: 300.0,
            y: 300.0,
        });
        assert!(r.open());
        r.input(RingInput::Cancel);
        assert!(!r.open());
        r.input(RingInput::Turn {
            progress: 1.0,
            clockwise: true,
            x: 300.0,
            y: 300.0,
        });
        r.input(RingInput::Commit);
        r.input(RingInput::Cancel);
        assert!(!r.open());
    }

    /// A keyboard has no stick to release, so every stop has to be reachable from the keys.
    /// The centre used to be reachable only through Y, which nothing on screen names, and the
    /// dial opened highlighting nothing while Enter already meant the centre.
    #[test]
    fn a_keyboard_reaches_every_stop_on_the_dial() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        assert_eq!(r.highlight(), Some(6), "the dial opens on the centre");

        // Tab tours the six discs and comes back through the centre.
        for k in 0..6 {
            r.key(Key::Tab);
            assert_eq!(r.highlight(), Some(k));
        }
        r.key(Key::Tab);
        assert_eq!(r.highlight(), Some(6), "after the last disc, the centre");

        // Up is 12 o'clock, and from 12 o'clock it is the centre. Down the same at 6.
        r.key(Key::Up);
        assert_eq!(r.highlight(), Some(0));
        r.key(Key::Up);
        assert_eq!(r.highlight(), Some(6));
        r.key(Key::Down);
        assert_eq!(r.highlight(), Some(3));
        r.key(Key::Down);
        assert_eq!(r.highlight(), Some(6));

        // The arrows still step the ring itself, and never onto the centre.
        r.key(Key::Right);
        assert_eq!(r.highlight(), Some(0));
        r.key(Key::Left);
        assert_eq!(r.highlight(), Some(5));
    }

    /// The sheet is the whole catalogue, so an action the ring can actually fire must be on
    /// it. `stream_mute` reached the editor's palette and not this list, which left a JOIN
    /// client spending one of its six discs to silence itself.
    #[test]
    fn the_sheet_offers_every_action_this_platform_can_fire() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        let on_sheet: Vec<String> = r
            .sheet_rows()
            .iter()
            .filter_map(|row| match row {
                SheetRow::Slot(s) => Some(s.id()),
                _ => None,
            })
            .collect();
        for group in pf_client_core::overlay_actions::catalogue(&r.cfg, RingPlatform::Desktop) {
            // Host actions and shortcuts are appended from what this host offers, and the
            // empty slot is the editor's eraser — none of the three is a fixed row.
            if matches!(group.title, "Host" | "Shortcuts" | "Empty") {
                continue;
            }
            for entry in group.entries {
                let Some(slot) = SlotId::parse(&entry.id) else {
                    continue;
                };
                // A platform that cannot serve it says so in the editor; the sheet may skip it.
                if !r.spec(&slot).enabled {
                    continue;
                }
                assert!(
                    on_sheet.contains(&entry.id),
                    "{} is fireable here and the sheet never lists it",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn end_stream_needs_two_presses_and_a_toggle_keeps_the_ring_open() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.fire(&SlotId::EndStream);
        assert_eq!(r.take_command(), None, "the first press only arms");
        assert_eq!(r.armed.as_deref(), Some("end_stream"));
        r.fire(&SlotId::EndStream);
        assert_eq!(r.take_command(), Some(RingCommand::EndStream));
        assert!(!r.open());

        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        r.fire(&SlotId::Stats);
        assert_eq!(r.take_command(), Some(RingCommand::CycleStats));
        assert!(r.open(), "a toggle leaves the ring open (D6)");
        r.fire(&SlotId::Keyboard);
        assert_eq!(r.take_command(), Some(RingCommand::Keyboard));
        assert!(!r.open(), "a command closes it");
    }

    #[test]
    fn the_system_buttons_tap_the_host_pad_and_close_the_ring() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        r.fire(&SlotId::Guide);
        assert_eq!(
            r.take_command(),
            Some(RingCommand::TapButton(wire::BTN_GUIDE))
        );
        assert!(!r.open(), "the host's overlay is taking the screen");
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        r.fire(&SlotId::Qam);
        assert_eq!(
            r.take_command(),
            Some(RingCommand::TapButton(wire::BTN_MISC1))
        );
    }

    /// The player's own toggle and the operator's mute are separate facts: firing the slot
    /// always asks for the local flip, and the slot never says "On" while the host is muting.
    #[test]
    fn the_mute_slot_names_whose_mute_it_is() {
        use punktfunk_core::audio::{AUDIO_MUTE_HOST, AUDIO_MUTE_LOCAL};

        let mut r = Ring::new();
        r.set_facts(&RingFacts { ..facts() });
        assert_eq!(r.spec(&SlotId::StreamMute).state, "Off");

        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        r.fire(&SlotId::StreamMute);
        assert_eq!(r.take_command(), Some(RingCommand::ToggleStreamMute));
        assert!(r.open(), "a toggle leaves the ring open");

        r.set_facts(&RingFacts {
            audio_mute: AUDIO_MUTE_LOCAL,
            ..facts()
        });
        assert_eq!(r.spec(&SlotId::StreamMute).state, "Muted on this device");
        assert_eq!(r.spec(&SlotId::StreamMute).short, "Mute ✕");

        r.set_facts(&RingFacts {
            audio_mute: AUDIO_MUTE_HOST,
            ..facts()
        });
        assert_eq!(r.spec(&SlotId::StreamMute).state, "Muted by the host");
        assert_eq!(
            r.spec(&SlotId::StreamMute).short,
            "Mute",
            "the face follows the player's own toggle; the badge carries the host's"
        );

        r.set_facts(&RingFacts {
            audio_mute: AUDIO_MUTE_HOST | AUDIO_MUTE_LOCAL,
            ..facts()
        });
        assert_eq!(
            r.spec(&SlotId::StreamMute).state,
            "Muted by the host and on this device"
        );
    }

    #[test]
    fn controller_mouse_toggles_in_place_and_needs_a_pad_and_the_pointer_grant() {
        let mut r = Ring::new();
        r.set_facts(&RingFacts {
            pad_mouse_target: 0b1,
            pointer_granted: true,
            ..facts()
        });
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        r.fire(&SlotId::PadMouse);
        assert_eq!(r.take_command(), Some(RingCommand::TogglePadMouse));
        assert!(r.open(), "a toggle leaves the ring open");

        let mut r = Ring::new();
        r.set_facts(&RingFacts {
            pad_mouse_target: 0b1,
            ..facts()
        });
        r.fire(&SlotId::PadMouse);
        assert_eq!(r.take_command(), None);
        assert!(r
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("controller input")));

        let mut r = Ring::new();
        r.set_facts(&RingFacts {
            pointer_granted: true,
            ..facts()
        });
        r.fire(&SlotId::PadMouse);
        assert_eq!(r.take_command(), None);
        assert!(r
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("No controller")));
    }

    #[test]
    fn a_dimmed_slot_says_why_and_sends_nothing() {
        let mut r = Ring::new();
        r.set_facts(&RingFacts {
            mic_available: false,
            ..facts()
        });
        r.fire(&SlotId::Mic);
        assert_eq!(r.take_command(), None);
        assert!(r.hint.as_deref().is_some_and(|h| h.contains("microphone")));
    }

    #[test]
    fn the_resolution_row_cycles_presets_from_native() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.adjust(&SheetRow::Resolution, 1);
        assert_eq!(
            r.take_command(),
            Some(RingCommand::RequestMode {
                width: 2560,
                height: 1440,
                refresh_hz: 60
            })
        );
        r.adjust(&SheetRow::Resolution, -1);
        assert_eq!(
            r.take_command(),
            Some(RingCommand::RequestMode {
                width: 1280,
                height: 720,
                refresh_hz: 60
            }),
            "wraps the other way"
        );
    }

    #[test]
    fn the_pad_steps_the_highlight_and_confirms_it() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        assert_eq!(r.highlight, Some(6), "the centre, lit, until moved");
        r.menu(MenuEvent::Move(MenuDir::Right));
        assert_eq!(
            r.highlight,
            Some(0),
            "from the centre, Right lands on 12 o'clock"
        );
        r.menu(MenuEvent::Move(MenuDir::Left));
        assert_eq!(r.highlight, Some(5), "anticlockwise wraps");
        r.menu(MenuEvent::Move(MenuDir::Down));
        assert_eq!(r.highlight, Some(3));
        r.menu(MenuEvent::Secondary);
        assert_eq!(r.highlight, Some(6));
        r.menu(MenuEvent::Confirm);
        assert!(r.sheet, "A on the centre opens the sheet");
        r.menu(MenuEvent::Back);
        assert!(!r.sheet && r.open(), "B leaves the sheet, keeps the ring");
        r.menu(MenuEvent::Move(MenuDir::Up));
        r.menu(MenuEvent::Confirm); // desktop default slot 0 is End stream: arms
        assert_eq!(r.armed.as_deref(), Some("end_stream"));
        r.menu(MenuEvent::Back);
        assert!(!r.open(), "B closes the ring");
    }

    #[test]
    fn the_damage_key_keeps_changing_until_the_ring_has_settled() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 300.0, y: 300.0 });
        assert!(r.animating(), "the arrival is on the move");
        r.tick();
        let a = r.damage();
        r.tick();
        assert_ne!(a, r.damage(), "a tick is a new key while animating");
        // No canvas: land the spring the way `render` would.
        r.spring = Spring::rest(1.0);
        r.shown = 1.0;
        assert!(!r.animating(), "landed");
        r.tick();
        let b = r.damage();
        r.tick();
        assert_eq!(b, r.damage(), "settled: a tick changes nothing");
        r.menu(MenuEvent::Move(MenuDir::Right));
        assert!(r.animating(), "the highlight's glow is on the move");
        r.input(RingInput::Toggle { x: 0.0, y: 0.0 });
        assert!(!r.open(), "closed for input at once");
        assert!(
            r.visible() && r.animating() && r.damage() != 0,
            "still drawn while it winds in"
        );
    }

    #[test]
    fn the_stick_points_at_a_slot_and_the_dpad_steps_only_when_it_lets_go() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        assert!(matches!(
            r.menu(MenuEvent::Sector(Some(4))),
            Some(MenuPulse::Move)
        ));
        assert_eq!(r.highlight, Some(4));
        assert!(
            r.menu(MenuEvent::Move(MenuDir::Right)).is_none(),
            "the stick's own move"
        );
        assert_eq!(r.highlight, Some(4));
        assert!(
            r.menu(MenuEvent::Sector(Some(4))).is_none(),
            "same sector: nothing to say"
        );
        r.menu(MenuEvent::Sector(None));
        assert_eq!(r.highlight, Some(6), "neutral is the centre");
        r.menu(MenuEvent::Move(MenuDir::Right));
        assert_eq!(r.highlight, Some(0), "the D-pad steps again");
        r.menu(MenuEvent::Back);
        assert!(!r.stick);

        let mut e = Ring::new();
        e.set_facts(&facts());
        e.edit_at(300.0, 300.0);
        e.menu(MenuEvent::Sector(Some(2)));
        assert_eq!(e.highlight, Some(2));
        e.menu(MenuEvent::Move(MenuDir::Left));
        assert_eq!(e.highlight, Some(2), "held: the move is the stick's");
        e.menu(MenuEvent::Sector(None));
        assert_eq!(
            e.highlight,
            Some(2),
            "the centre is inert here, so the slot stays"
        );
        e.menu(MenuEvent::Move(MenuDir::Left));
        assert_eq!(e.highlight, Some(1));
    }

    #[test]
    fn every_built_in_slot_has_an_icon() {
        for id in [
            "end_stream",
            "disconnect_linger",
            "touch_mode",
            "keyboard",
            "stats",
            "mic",
            "pad",
            "send_text",
            "guide",
            "qam",
            "more",
            "host:power.sleep",
            "host:power.reboot",
            "host:power.shutdown",
        ] {
            assert!(slot_icon(id, "").is_some(), "{id} has no icon");
        }
        assert!(slot_icon("mic", "Muted").is_some());
        assert!(slot_icon("host:custom.eject", "").is_none());
        assert!(slot_icon("shortcut:s1", "").is_none());
    }

    #[test]
    fn the_editor_picks_lifts_and_swaps_without_ever_firing() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.edit_at(300.0, 300.0);
        assert_eq!(r.highlight(), Some(0), "a pad has somewhere to start");
        r.menu(MenuEvent::Confirm);
        assert_eq!(r.take_edit(), Some(EditEvent::Pick(0)));
        assert_eq!(r.take_command(), None, "End stream did not arm or fire");
        assert_eq!(r.armed, None);
        r.menu(MenuEvent::Secondary);
        assert!(r.carrying());
        r.menu(MenuEvent::Move(MenuDir::Right));
        r.menu(MenuEvent::Move(MenuDir::Right));
        r.menu(MenuEvent::Confirm);
        assert_eq!(r.take_edit(), Some(EditEvent::Swap(0, 2)));
        assert!(!r.carrying());
        r.menu(MenuEvent::Secondary);
        assert!(
            matches!(r.menu(MenuEvent::Back), Some(MenuPulse::Confirm)),
            "B puts it down"
        );
        assert!(!r.carrying());
        assert!(
            r.menu(MenuEvent::Back).is_none(),
            "with nothing lifted B is the screen's"
        );
        assert!(r.open(), "and the ring stayed open");
    }

    #[test]
    fn the_editor_takes_a_click_as_a_pick_and_a_carry_as_a_swap() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.edit_at(300.0, 300.0);
        // Stand-in slot rects at 12 and 2 o'clock; the ring is fully shown.
        r.geom[0] = Rect::from_xywh(272.0, 152.0, 56.0, 56.0);
        r.geom[1] = Rect::from_xywh(376.0, 212.0, 56.0, 56.0);
        let at = |x: f64, y: f64, kind: PointerKind| Pointer { x, y, kind };
        assert!(
            !r.pointer(at(300.0, 300.0, PointerKind::Press)),
            "between the discs"
        );
        assert!(r.pointer(at(300.0, 180.0, PointerKind::Press)));
        assert!(r.pointer(at(302.0, 181.0, PointerKind::Release)));
        assert_eq!(r.take_edit(), Some(EditEvent::Pick(0)));
        assert!(r.pointer(at(300.0, 180.0, PointerKind::Press)));
        assert!(r.pointer(at(360.0, 220.0, PointerKind::Move)));
        assert!(r.carrying());
        assert!(r.pointer(at(404.0, 240.0, PointerKind::Release)));
        assert_eq!(r.take_edit(), Some(EditEvent::Swap(0, 1)));
        assert!(!r.carrying());
    }

    /// The sheet open and drawn, so its rows have real rects to press; row 0 is End stream.
    fn rendered_sheet() -> (Ring, Rect) {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 640.0, y: 300.0 });
        r.menu(MenuEvent::Confirm); // the lit centre opens the sheet
        for _ in 0..120 {
            r.render(surface.canvas(), 1280, 800, 1.0, &fonts, 1.0 / 60.0);
        }
        let row = r.list.row_rect(0).expect("the sheet drew its rows");
        (r, row)
    }

    /// A finger swipe on the sheet steps its rows and fires none. The same contact
    /// lifted in place is the tap, fired on the lift at the anchor.
    #[test]
    fn a_touch_swipe_scrolls_the_sheet_without_firing_a_row() {
        use crate::pointer::Touch;
        use pf_client_core::console::{PointerButton, PointerInput};
        let (mut r, row) = rendered_sheet();
        let (cx, cy) = (row.center_x(), row.center_y());
        let mut touch = Touch::default();
        let mut feed = |r: &mut Ring, input| touch.feed(input, 1.0, 0.0, |p| r.pointer(p));
        let down = PointerInput::Down {
            x: cx,
            y: cy,
            button: PointerButton::Primary,
            touch: true,
        };

        feed(&mut r, down);
        for i in 1..=6 {
            feed(
                &mut r,
                PointerInput::Move {
                    x: cx,
                    y: cy - (i as f32) * 40.0,
                },
            );
        }
        feed(
            &mut r,
            PointerInput::Up {
                x: cx,
                y: cy - 240.0,
                button: PointerButton::Primary,
            },
        );
        assert!(r.list.cursor > 0, "the swipe stepped the rows");
        assert!(r.sheet, "the sheet stayed up");
        assert_eq!(r.armed, None, "a swipe across a row fires nothing");
        assert_eq!(r.take_command(), None);

        feed(&mut r, down);
        assert_eq!(r.armed, None, "a finger must not act on contact");
        feed(
            &mut r,
            PointerInput::Up {
                x: cx,
                y: cy,
                button: PointerButton::Primary,
            },
        );
        assert_eq!(
            r.armed.as_deref(),
            Some("end_stream"),
            "the tap lands on the lift, at the anchor"
        );
    }

    #[test]
    fn closed_means_no_damage_and_no_pointer_consumption() {
        let mut r = Ring::new();
        assert_eq!(r.damage(), 0);
        assert!(!r.pointer(Pointer {
            x: 1.0,
            y: 1.0,
            kind: PointerKind::Press
        }));
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        assert_ne!(r.damage(), 0);
        assert!(r.pointer(Pointer {
            x: 1.0,
            y: 1.0,
            kind: PointerKind::Move
        }));
    }
}
