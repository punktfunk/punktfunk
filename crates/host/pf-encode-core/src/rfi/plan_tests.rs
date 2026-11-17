//! Each wave backend's recovery order, and the proof [`plan_frame`] describes it. Per
//! backend there is a `Today`, its arbitration written out by hand without the planner,
//! and a `Planned`, the same events through [`plan_frame`] with the policy
//! and the candidates that backend passes. A table pins named sequences on both; a sweep runs
//! every short sequence through both. AMF and QSV are checked against the real mirror in
//! `ltr.rs`.
//!
//! Events, one byte each: `f` a frame, `k` a keyframe request, `a` a loss of the newest frame
//! with an anchor before it, `l` a loss from the first frame on with none, `+` / `-` a
//! confirmed reference turns resident / gone. Whether a loss has an anchor is the backend's
//! table; these models take it from the event.
//! Frames: `I` IDR, `A` anchor, `K` confirmed reference, then the wave: `S` start, `w` inside,
//! `C` marked close, `x` unmarked close. `.` is a plain chain frame.

use super::{
    plan_frame, FramePlan, FrameRef, MidWave, Pending, Recovery, RecoveryPolicy, Wave, WaveMark,
};

/// What one frame came out as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Au {
    pub(crate) refs: FrameRef,
    pub(crate) wave: Option<Wave>,
    pub(crate) mark: WaveMark,
    pub(crate) dirty: bool,
}

impl From<FramePlan> for Au {
    fn from(p: FramePlan) -> Au {
        Au {
            refs: p.refs,
            wave: p.wave,
            mark: p.mark,
            dirty: p.dirty,
        }
    }
}

impl Au {
    fn token(self) -> String {
        let refs = match self.refs {
            FrameRef::Idr => "I",
            FrameRef::Anchor => "A",
            FrameRef::Acked => "K",
            FrameRef::Chain => "",
        };
        let wave = match self.wave.map(|w| (self.mark, w.closes())) {
            None => "",
            Some((WaveMark::Start, _)) => "S",
            Some((WaveMark::Close, _)) => "C",
            Some((WaveMark::None, true)) => "x",
            Some((WaveMark::None, false)) => "w",
        };
        // Every wave picture but a marked close is part dirty.
        assert_eq!(self.dirty, matches!(wave, "S" | "w" | "x"), "{self:?}");
        match (refs, wave) {
            ("", "") => ".".into(),
            _ => format!("{refs}{wave}"),
        }
    }
}

pub(crate) trait Model {
    /// One event; `Some` for a frame.
    fn event(&mut self, e: u8) -> Option<Au>;
}

fn run(model: &mut dyn Model, events: &str) -> String {
    let frames: Vec<String> = events
        .bytes()
        .filter_map(|e| model.event(e))
        .map(Au::token)
        .collect();
    frames.join(" ")
}

/// `vulkan_video.rs` `invalidate_ref_frames`; `vk_record.rs` `record_submit`,
/// `post_submit_bookkeeping`.
struct VulkanToday {
    intra_refresh: Option<u32>,
    first_frame: bool,
    force_kf: bool,
    pending_loss: bool,
    /// No trusted slot is older than the armed loss.
    empty: bool,
    floor: bool,
    wave: Option<Wave>,
}

impl Model for VulkanToday {
    fn event(&mut self, e: u8) -> Option<Au> {
        match e {
            b'k' => self.force_kf = true,
            b'+' | b'-' => self.floor = e == b'+',
            b'a' | b'l' => {
                self.empty |= e == b'l';
                // A decline leaves an earlier arm standing.
                if !self.empty || self.intra_refresh.is_some() {
                    self.pending_loss = true;
                }
            }
            _ => return Some(self.frame()),
        }
        None
    }
}

impl VulkanToday {
    fn frame(&mut self) -> Au {
        let mut is_idr = self.first_frame || self.force_kf;
        let (mut recovery, mut acked) = (false, false);
        if std::mem::take(&mut self.pending_loss) {
            if !is_idr {
                if !self.empty {
                    recovery = true;
                } else {
                    match self.intra_refresh {
                        Some(cycle) => self.wave = Some(Wave::start(cycle)),
                        None => is_idr = true,
                    }
                }
            }
        } else if self.floor && !is_idr {
            acked = true;
        }
        if is_idr || recovery || acked {
            self.wave = None;
        }
        let wave = self.wave;
        let mark = match wave {
            Some(w) if w.closes() => WaveMark::Close,
            Some(w) if w.marks() => WaveMark::Start,
            _ => WaveMark::None,
        };
        let dirty = wave.is_some_and(|w| !w.closes());
        if let Some(w) = wave {
            self.wave = w.next();
        }
        (self.first_frame, self.force_kf, self.empty) = (false, false, false);
        Au {
            refs: refs(is_idr, recovery, acked),
            wave,
            mark,
            dirty,
        }
    }
}

/// Whether loss event `e` spoils `w` with `next` pictures encoded: VAAPI's `spoiled_by`, its
/// start `index` pictures back.
fn spoils(w: Wave, next: u64, e: u8) -> bool {
    let next = next as i64;
    let lost = if e == b'l' { 0 } else { next - 1 };
    w.spoiled_by(Some(next - i64::from(w.index)), next, lost)
}

fn refs(idr: bool, anchor: bool, acked: bool) -> FrameRef {
    if idr {
        FrameRef::Idr
    } else if anchor {
        FrameRef::Anchor
    } else if acked {
        FrameRef::Acked
    } else {
        FrameRef::Chain
    }
}

/// Vulkan Video through the planner: the loss stays armed and resolves at frame-build, where
/// a confirmed reference is looked up only when no loss is armed.
struct VulkanPlanned {
    policy: RecoveryPolicy,
    state: Recovery,
    first_frame: bool,
    force_kf: bool,
    armed: bool,
    empty: bool,
    floor: bool,
}

impl Model for VulkanPlanned {
    fn event(&mut self, e: u8) -> Option<Au> {
        match e {
            b'k' => self.force_kf = true,
            b'+' | b'-' => self.floor = e == b'+',
            b'a' | b'l' => {
                self.empty |= e == b'l';
                self.armed |= !self.empty || self.policy.cycle.is_some();
            }
            _ => {
                let armed = std::mem::take(&mut self.armed);
                let pending = Pending {
                    idr: self.first_frame || self.force_kf,
                    anchor: armed && !self.empty,
                    acked: !armed && self.floor,
                    loss: armed,
                };
                let plan = plan_frame(self.state, pending, &self.policy);
                self.state = plan.next;
                (self.first_frame, self.force_kf, self.empty) = (false, false, false);
                return Some(plan.into());
            }
        }
        None
    }
}

/// `vaapi_native.rs`: `invalidate_ref_frames` and `submit`. `anchor` holds what the slot
/// would be tagged as. As in [`NvencToday`], `l` loses the first picture and `a` the newest.
struct VaapiToday {
    wave_enabled: Option<u32>,
    force_kf: bool,
    anchor: Option<FrameRef>,
    floor: bool,
    wave: Option<Wave>,
    wave_spoiled: bool,
    wave_queued: bool,
    next_wire: u64,
    empty: bool,
}

impl Model for VaapiToday {
    fn event(&mut self, e: u8) -> Option<Au> {
        match e {
            b'k' => self.force_kf = true,
            b'+' | b'-' => self.floor = e == b'+',
            b'a' | b'l' => {
                self.empty |= e == b'l';
                self.anchor = (!self.empty).then_some(FrameRef::Anchor);
                if self.anchor.is_some() {
                    return None;
                }
                if let Some(cycle) = self.wave_enabled.filter(|_| self.next_wire > 0) {
                    if let Some(w) = self.wave {
                        self.wave_spoiled |= spoils(w, self.next_wire, e);
                        self.wave_queued = true;
                        return None;
                    }
                    self.wave = Some(Wave::start(cycle));
                }
            }
            _ => return Some(self.submit()),
        }
        None
    }
}

impl VaapiToday {
    fn submit(&mut self) -> Au {
        if self.floor && self.anchor.is_none() {
            self.anchor = Some(FrameRef::Acked);
        }
        if self.force_kf || self.anchor.is_some() {
            self.wave = None;
            self.wave_spoiled = false;
            self.wave_queued = false;
        }
        let wave = self.wave;
        let mark = wave.map_or(WaveMark::None, |w| w.mark(self.wave_spoiled));
        let anchor = self.anchor.take().filter(|_| !self.force_kf);
        let au = match (anchor, wave) {
            (Some(refs), _) => Au {
                refs,
                wave: None,
                mark: WaveMark::None,
                dirty: false,
            },
            (None, Some(w)) => Au {
                refs: FrameRef::Chain,
                wave,
                mark,
                dirty: !w.closes() || self.wave_spoiled,
            },
            (None, None) => Au {
                refs: refs(self.force_kf, false, false),
                wave: None,
                mark: WaveMark::None,
                dirty: false,
            },
        };
        self.force_kf = false;
        if let Some(w) = wave {
            self.wave = w.next();
            if self.wave.is_none() {
                self.wave_spoiled = false;
                if std::mem::take(&mut self.wave_queued) {
                    self.wave = Some(Wave::start(w.cycle));
                }
            }
        }
        self.next_wire += 1;
        self.empty = false;
        au
    }
}

/// VAAPI through the planner: a loss resolves at the report, a loss inside the sweep spoils,
/// and the session cannot wave before its first picture.
struct VaapiPlanned {
    cycle: Option<u32>,
    state: Recovery,
    force_kf: bool,
    anchor: bool,
    floor: bool,
    next_wire: u64,
    empty: bool,
}

impl VaapiPlanned {
    fn policy(&self) -> RecoveryPolicy {
        RecoveryPolicy {
            cycle: self.cycle.filter(|_| self.next_wire > 0),
            mid_wave: MidWave::Queue,
            abandons: true,
        }
    }
}

impl Model for VaapiPlanned {
    fn event(&mut self, e: u8) -> Option<Au> {
        match e {
            b'k' => self.force_kf = true,
            b'+' | b'-' => self.floor = e == b'+',
            b'a' | b'l' => {
                self.empty |= e == b'l';
                self.anchor = !self.empty;
                if !self.anchor {
                    let spoils = self
                        .state
                        .wave
                        .is_some_and(|w| spoils(w, self.next_wire, e));
                    self.state = self
                        .state
                        .lose(&self.policy(), spoils)
                        .unwrap_or(self.state);
                }
            }
            _ => {
                let pending = Pending {
                    idr: std::mem::take(&mut self.force_kf),
                    anchor: std::mem::take(&mut self.anchor),
                    acked: self.floor,
                    loss: false,
                };
                let plan = plan_frame(self.state, pending, &self.policy());
                self.state = plan.next;
                self.next_wire += 1;
                self.empty = false;
                return Some(plan.into());
            }
        }
        None
    }
}

/// `nvenc_session.rs`: `invalidate_ref_frames`, `start_wave`, `encode`, `ltr_peek`. The range
/// plan's answer is the event: `a` anchors, `l` declines.
#[derive(Default)]
pub(crate) struct NvencToday {
    cycle: u32,
    frame_idx: i64,
    force_kf: bool,
    pending_anchor: bool,
    wave: Option<Wave>,
    wave_span: Option<(i64, i64)>,
    wave_spoiled: bool,
    wave_queued: bool,
    floor: bool,
}

impl Model for NvencToday {
    fn event(&mut self, e: u8) -> Option<Au> {
        match e {
            b'k' => self.force_kf = true,
            b'+' | b'-' => self.floor = e == b'+',
            // No session before the first frame: the caller keyframes.
            b'a' | b'l' if self.frame_idx == 0 => {}
            b'a' | b'l' => {
                let lost = if e == b'l' { 0 } else { self.frame_idx - 1 };
                if self.wave.is_some() || e == b'l' {
                    self.start_wave(lost, lost);
                } else {
                    self.pending_anchor = true;
                }
            }
            _ => return Some(self.encode()),
        }
        None
    }
}

impl NvencToday {
    fn start_wave(&mut self, first: i64, last: i64) -> bool {
        if self.cycle == 0 || first < 0 || first > last || first >= self.frame_idx {
            return false;
        }
        if let Some(w) = self.wave {
            let span_start = self.wave_span.map(|(start, _)| start);
            self.wave_spoiled |= w.spoiled_by(span_start, self.frame_idx, last);
            self.wave_queued = true;
            return true;
        }
        self.wave = Some(Wave::start(self.cycle));
        true
    }

    fn encode(&mut self) -> Au {
        let opening = self.frame_idx == 0;
        let pts = self.frame_idx;
        self.frame_idx += 1;
        let forced = std::mem::take(&mut self.force_kf);
        let anchor = std::mem::take(&mut self.pending_anchor) && !forced;
        if forced {
            self.wave = None;
            self.wave_spoiled = false;
            self.wave_queued = false;
            self.wave_span = self.wave_span.map(|(s, close)| (s, close.min(pts)));
        }
        let wave = self.wave;
        let spoiled = self.wave_spoiled;
        let mark = wave.map_or(WaveMark::None, |w| w.mark(spoiled));
        if let Some(w) = wave {
            if w.index == 0 {
                let start = match self.wave_span {
                    Some((s, _)) if self.wave_spoiled => s,
                    _ => pts,
                };
                self.wave_span = Some((start, pts + i64::from(w.cycle) - 1));
                self.wave_spoiled = false;
            }
            self.wave = w.next();
            if self.wave.is_none() && std::mem::take(&mut self.wave_queued) {
                self.wave = Some(Wave::start(w.cycle));
            }
        }
        let idr = forced || opening;
        let ltr_force = self.floor && !idr && wave.is_none();
        Au {
            refs: refs(idr, anchor && wave.is_none(), ltr_force),
            wave,
            mark,
            // There is no per-picture bit: `wave_span` covers a spoiled close from the queued
            // wave's start, which is the next frame.
            dirty: wave.is_some_and(|w| !w.closes() || spoiled),
        }
    }
}

/// NVENC through the planner: a sweep in flight takes every loss, and one spoils it when the
/// lost frame sits inside a sweep that has started.
pub(crate) struct NvencPlanned {
    policy: RecoveryPolicy,
    state: Recovery,
    frames: i64,
    force_kf: bool,
    pending_anchor: bool,
    floor: bool,
}

impl Model for NvencPlanned {
    fn event(&mut self, e: u8) -> Option<Au> {
        match e {
            b'k' => self.force_kf = true,
            b'+' | b'-' => self.floor = e == b'+',
            b'a' | b'l' if self.frames == 0 => {}
            b'a' | b'l' => {
                if self.state.wave.is_some() || e == b'l' {
                    let spoils = e == b'a' && self.state.wave.is_some_and(|w| w.index > 0);
                    self.state = self.state.lose(&self.policy, spoils).unwrap_or(self.state);
                } else {
                    self.pending_anchor = true;
                }
            }
            _ => {
                let pending = Pending {
                    idr: std::mem::take(&mut self.force_kf) || self.frames == 0,
                    anchor: std::mem::take(&mut self.pending_anchor),
                    acked: self.floor,
                    loss: false,
                };
                let plan = plan_frame(self.state, pending, &self.policy);
                self.state = plan.next;
                self.frames += 1;
                return Some(plan.into());
            }
        }
        None
    }
}

fn vulkan(cycle: Option<u32>) -> (VulkanToday, VulkanPlanned) {
    let today = VulkanToday {
        intra_refresh: cycle,
        first_frame: true,
        force_kf: false,
        pending_loss: false,
        empty: true,
        floor: false,
        wave: None,
    };
    let planned = VulkanPlanned {
        policy: RecoveryPolicy {
            cycle,
            mid_wave: MidWave::Restart,
            abandons: true,
        },
        state: Recovery::default(),
        first_frame: true,
        force_kf: false,
        armed: false,
        empty: true,
        floor: false,
    };
    (today, planned)
}

fn vaapi(cycle: Option<u32>) -> (VaapiToday, VaapiPlanned) {
    let today = VaapiToday {
        wave_enabled: cycle,
        force_kf: true,
        anchor: None,
        floor: false,
        wave: None,
        wave_spoiled: false,
        wave_queued: false,
        next_wire: 0,
        empty: true,
    };
    let planned = VaapiPlanned {
        cycle,
        state: Recovery::default(),
        force_kf: true,
        anchor: false,
        floor: false,
        next_wire: 0,
        empty: true,
    };
    (today, planned)
}

pub(crate) fn nvenc(cycle: Option<u32>) -> (NvencToday, NvencPlanned) {
    let today = NvencToday {
        cycle: cycle.unwrap_or(0),
        ..NvencToday::default()
    };
    let planned = NvencPlanned {
        policy: RecoveryPolicy {
            cycle,
            mid_wave: MidWave::Queue,
            abandons: false,
        },
        state: Recovery::default(),
        frames: 0,
        force_kf: false,
        pending_anchor: false,
        floor: false,
    };
    (today, planned)
}

/// `(events, frames, the rule)`, on a four-frame wave unless the rule says the session has
/// none.
pub(crate) type Row = (&'static str, &'static str, &'static str);

const VULKAN: &[Row] = &[
    ("ff", "I .", "the first frame is an IDR"),
    ("fkff", "I I .", "a keyframe request"),
    ("faff", "I A .", "a loss with an anchor"),
    ("flfffff", "I S w w C .", "a loss with none: a wave"),
    (
        "flfflfffff",
        "I S w S w w C .",
        "a loss mid-wave restarts it",
    ),
    ("fllfffff", "I S w w C .", "two losses, one wave"),
    ("falfffff", "I S w w C .", "the re-pick finds no anchor"),
    ("flffaff", "I S w A .", "an anchor abandons the wave"),
    ("flffkff", "I S w I .", "an IDR abandons the wave"),
    ("flkff", "I I .", "an IDR drops the armed loss"),
    ("fakff", "I I .", "an IDR outranks the anchor"),
    ("f+ff-f", "I K K .", "a confirmed reference"),
    ("f+aff", "I A K", "an anchor outranks it"),
    (
        "flff+f-f",
        "I S w K .",
        "a confirmed reference abandons the wave",
    ),
    ("f+lff", "I S K", "an armed loss skips the confirmed lookup"),
];

/// A session with no intra refresh.
const VULKAN_NO_WAVE: &[Row] = &[
    ("flff", "I . .", "a fresh loss is declined"),
    ("falff", "I I .", "a stale arm takes the IDR"),
];

const VAAPI: &[Row] = &[
    ("ff", "I .", "the first frame is an IDR"),
    ("fkff", "I I .", "a keyframe request"),
    ("faff", "I A .", "a loss with an anchor"),
    ("flfffff", "I S w w C .", "a loss with none: a wave"),
    (
        "flfflfffffff",
        "I S w w C S w w C .",
        "a loss before the sweep queues a wave",
    ),
    (
        "fllfffffffff",
        "I S w w C S w w C .",
        "a wave not yet started is never spoiled",
    ),
    ("falfffff", "I S w w C .", "a later loss drops the anchor"),
    ("flffaff", "I S w A .", "an anchor abandons the wave"),
    ("flfflkff", "I S w I .", "an IDR drops wave and queue"),
    ("fakff", "I I .", "an IDR outranks the anchor"),
    ("f+ff-f", "I K K .", "a confirmed reference"),
    ("f+aff", "I A K", "an anchor outranks it"),
    (
        "flff+f-f",
        "I S w K .",
        "a confirmed reference abandons the wave",
    ),
    ("f+lff", "I K K", "also one a loss just started"),
    ("lff", "I .", "no wave before the first picture"),
];

const VAAPI_NO_WAVE: &[Row] = &[("flff", "I . .", "a loss with no anchor is declined")];

pub(crate) const NVENC: &[Row] = &[
    ("ff", "I .", "the opening frame is an IDR"),
    ("fkff", "I I .", "a keyframe request"),
    ("faff", "I A .", "a loss the range plan anchors"),
    ("flfffff", "I S w w C .", "a loss it declines: a wave"),
    (
        "flfflfffffff",
        "I S w w C S w w C .",
        "a loss before the sweep queues a wave",
    ),
    (
        "flffafffffff",
        "I S w w x S w w C .",
        "a loss inside the sweep also spoils it",
    ),
    (
        "fllfffffffff",
        "I S w w C S w w C .",
        "a wave not yet started is never spoiled",
    ),
    (
        "falfffff",
        "I S w w C .",
        "a declined loss leaves the wave start untagged",
    ),
    ("flffakff", "I S w I .", "an IDR drops wave and queue"),
    ("fakff", "I I .", "an IDR outranks the anchor"),
    ("f+ff-f", "I K K .", "a confirmed reference"),
    ("f+aff", "I A K", "an anchor outranks it"),
    (
        "flff+ffff",
        "I S w w C K K",
        "a confirmed reference waits for the close",
    ),
    ("aff", "I .", "no session before the first frame"),
];

/// AV1, or the wave turned off.
pub(crate) const NVENC_NO_WAVE: &[Row] = &[("flff", "I . .", "a declined loss stays the caller's")];

pub(crate) fn check<T: Model, P: Model>(rows: &[Row], make: impl Fn() -> (T, P)) {
    for &(events, frames, rule) in rows {
        let (mut today, mut planned) = make();
        assert_eq!(run(&mut today, events), frames, "today: {rule} ({events})");
        assert_eq!(
            run(&mut planned, events),
            frames,
            "planned: {rule} ({events})"
        );
    }
}

/// Every sequence of up to seven events, frame by frame, on both models.
pub(crate) fn sweep<T: Model, P: Model>(make: impl Fn() -> (T, P)) {
    const EVENTS: &[u8] = b"fkal+-";
    for len in 1..=7u32 {
        for n in 0..EVENTS.len().pow(len) {
            let (mut today, mut planned) = make();
            let mut digits = n;
            let mut seen = [0u8; 7];
            for slot in seen.iter_mut().take(len as usize) {
                *slot = EVENTS[digits % EVENTS.len()];
                digits /= EVENTS.len();
                let (was, plan) = (today.event(*slot), planned.event(*slot));
                assert_eq!(was, plan, "after {:?}", String::from_utf8_lossy(&seen));
            }
        }
    }
}

#[test]
fn vulkan_recovery_order() {
    check(VULKAN, || vulkan(Some(4)));
    check(VULKAN_NO_WAVE, || vulkan(None));
    for cycle in [None, Some(2), Some(3)] {
        sweep(|| vulkan(cycle));
    }
}

#[test]
fn vaapi_recovery_order() {
    check(VAAPI, || vaapi(Some(4)));
    check(VAAPI_NO_WAVE, || vaapi(None));
    for cycle in [None, Some(2), Some(3)] {
        sweep(|| vaapi(cycle));
    }
}

#[test]
fn nvenc_recovery_order() {
    check(NVENC, || nvenc(Some(4)));
    check(NVENC_NO_WAVE, || nvenc(None));
    for cycle in [None, Some(2), Some(3)] {
        sweep(|| nvenc(cycle));
    }
}
