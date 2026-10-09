//! Virtual tablet ("Punktfunk Pen"): a uinput stylus with pressure, tilt, barrel
//! roll, hover distance, eraser, and barrel buttons.
//!
//! Compositors have no virtual-tablet protocol; they consume evdev tablets via
//! libinput and forward `zwp_tablet_v2`. udev's `input_id` builtin classifies
//! `BTN_TOOL_PEN` + `ABS_X/Y` as `ID_INPUT_TABLET`. A stable vendor:product lets
//! compositor mapping rules pin this device.
//!
//! [`PenTracker`](punktfunk_core::quic::PenTracker) feeds [`PenTransition`]s.
//! This file maps them to evdev and groups SYN frames so proximity-enter
//! carries its position in the same frame — libinput otherwise reports a stale
//! point. The uinput ABI and device live in [`crate::uinput_abi`].
//!
//! Evidence: `design/pen-tablet-input.md`.

use crate::uinput_abi::{
    AbsInfo, InputId, UinputDevice, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT, UI_SET_EVBIT,
    UI_SET_KEYBIT, UI_SET_PROPBIT,
};
use anyhow::Result;
use punktfunk_core::quic::{PenSample, PenTool, PenTransition, PEN_BARREL1, PEN_BARREL2};

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// Barrel roll on ABS_Z (Wacom Art-Pen). libinput maps min..max onto 0..360°.
const ABS_Z: u16 = 0x02;
const ABS_PRESSURE: u16 = 0x18;
const ABS_DISTANCE: u16 = 0x19;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;
const BTN_STYLUS2: u16 = 0x14c;
/// Screen tablet: libinput maps the full ABS range onto the output rect.
const INPUT_PROP_DIRECT: u16 = 0x01;

/// Full-scale wire pressure (u16) → the declared 0..4095 axis.
const PRESSURE_SHIFT: u32 = 4;
/// Wire hover distance (u16, 0xFFFF = unknown) → the declared 0..1023 axis.
const DISTANCE_SHIFT: u32 = 6;
const ABS_RANGE: f32 = 65535.0;

/// Evdev key for the in-proximity tool. The tracker re-enters on a tool switch,
/// so this only names the key to release on `ProximityOut`.
fn tool_key(tool: PenTool) -> u16 {
    match tool {
        PenTool::Eraser => BTN_TOOL_RUBBER,
        // Unknown = a newer client's future tool — nearest ink-capable behavior is the pen.
        PenTool::Pen | PenTool::Unknown => BTN_TOOL_PEN,
    }
}

/// Per-session uinput tablet.
pub struct VirtualPen {
    dev: UinputDevice,
    /// In-proximity `BTN_TOOL_*`; the `ProximityOut` release target.
    tool: u16,
    /// Current SYN frame already has a Motion; a second Motion starts a new frame.
    frame_has_motion: bool,
    frame_dirty: bool,
}

impl VirtualPen {
    pub fn create() -> Result<VirtualPen> {
        let dev = UinputDevice::open()?;
        dev.set_bits(UI_SET_EVBIT, "UI_SET_EVBIT", &[EV_KEY, EV_ABS])?;
        dev.set_bits(
            UI_SET_KEYBIT,
            "UI_SET_KEYBIT",
            &[
                BTN_TOOL_PEN,
                BTN_TOOL_RUBBER,
                BTN_TOUCH,
                BTN_STYLUS,
                BTN_STYLUS2,
            ],
        )?;
        dev.set_bits(UI_SET_PROPBIT, "UI_SET_PROPBIT", &[INPUT_PROP_DIRECT])?;

        // 0..65535, resolution 100 units/mm (~655 mm). Zero resolution trips libinput's
        // missing-resolution fixup; the mm figure is unused for pen mapping.
        let pos = AbsInfo {
            minimum: 0,
            maximum: 65535,
            resolution: 100,
            ..Default::default()
        };
        // Degrees from vertical. resolution 57 units/radian ⇒ 1 unit = 1° (Wacom).
        let tilt = AbsInfo {
            minimum: -90,
            maximum: 90,
            resolution: 57,
            ..Default::default()
        };
        for (code, info) in [
            (ABS_X, pos),
            (ABS_Y, pos),
            (
                ABS_PRESSURE,
                AbsInfo {
                    minimum: 0,
                    maximum: 4095,
                    ..Default::default()
                },
            ),
            (
                ABS_DISTANCE,
                AbsInfo {
                    minimum: 0,
                    maximum: 1023,
                    ..Default::default()
                },
            ),
            (ABS_TILT_X, tilt),
            (ABS_TILT_Y, tilt),
            (
                // 0..359: libinput maps the declared range linearly onto 0..360°.
                ABS_Z,
                AbsInfo {
                    minimum: 0,
                    maximum: 359,
                    ..Default::default()
                },
            ),
        ] {
            dev.abs(code, info)?;
        }

        // pid.codes VID + "PF" PID so compositor tablet-mapping can target this device.
        let id = InputId {
            bustype: 0x0006, // BUS_VIRTUAL
            vendor: 0x1209,
            product: 0x5046, // "PF"
            version: 1,
        };
        dev.create(id, b"Punktfunk Pen", 0)?;
        tracing::info!("virtual tablet created (Punktfunk Pen, uinput)");

        Ok(VirtualPen {
            dev,
            tool: BTN_TOOL_PEN,
            frame_has_motion: false,
            frame_dirty: false,
        })
    }

    fn flush(&mut self) {
        if self.frame_dirty {
            self.dev.emit(EV_SYN, SYN_REPORT, 0);
            self.frame_dirty = false;
            self.frame_has_motion = false;
        }
    }

    fn motion(&mut self, s: &PenSample) {
        self.dev.emit(EV_ABS, ABS_X, (s.x * ABS_RANGE) as i32);
        self.dev.emit(EV_ABS, ABS_Y, (s.y * ABS_RANGE) as i32);
        self.dev
            .emit(EV_ABS, ABS_PRESSURE, (s.pressure >> PRESSURE_SHIFT) as i32);
        if s.distance != punktfunk_core::quic::PEN_DISTANCE_UNKNOWN {
            self.dev
                .emit(EV_ABS, ABS_DISTANCE, (s.distance >> DISTANCE_SHIFT) as i32);
        }
        // Polar → tiltX/tiltY. Azimuth clockwise from north: east (90°) is +X, south (180°) is +Y.
        if s.tilt_deg != punktfunk_core::quic::PEN_TILT_UNKNOWN
            && s.azimuth_deg != punktfunk_core::quic::PEN_ANGLE_UNKNOWN
        {
            let az = (s.azimuth_deg as f32).to_radians();
            let tilt = s.tilt_deg as f32;
            self.dev
                .emit(EV_ABS, ABS_TILT_X, (tilt * az.sin()).round() as i32);
            self.dev
                .emit(EV_ABS, ABS_TILT_Y, (-tilt * az.cos()).round() as i32);
        }
        if s.roll_deg != punktfunk_core::quic::PEN_ANGLE_UNKNOWN {
            self.dev.emit(EV_ABS, ABS_Z, (s.roll_deg % 360) as i32);
        }
        self.frame_dirty = true;
        self.frame_has_motion = true;
    }

    /// Apply one batch of tracker transitions as SYN frames. Close a frame before
    /// `ProximityIn` (entry must carry its own position) and before a second
    /// `Motion` (consecutive samples are consecutive instants), then close at the
    /// end. `[ProxIn, Motion, TipDown]` is one frame; `[Motion, Motion]` is two.
    pub fn apply_batch(&mut self, transitions: &[PenTransition]) {
        for t in transitions {
            match t {
                PenTransition::ProximityIn { tool } => {
                    self.flush();
                    self.tool = tool_key(*tool);
                    self.dev.emit(EV_KEY, self.tool, 1);
                    self.frame_dirty = true;
                }
                PenTransition::Motion { sample } => {
                    if self.frame_has_motion {
                        self.flush();
                    }
                    self.motion(sample);
                }
                PenTransition::TipDown => {
                    self.dev.emit(EV_KEY, BTN_TOUCH, 1);
                    self.frame_dirty = true;
                }
                PenTransition::ButtonsChanged { pressed, released } => {
                    for (bit, key) in [(PEN_BARREL1, BTN_STYLUS), (PEN_BARREL2, BTN_STYLUS2)] {
                        if pressed & bit != 0 {
                            self.dev.emit(EV_KEY, key, 1);
                            self.frame_dirty = true;
                        }
                        if released & bit != 0 {
                            self.dev.emit(EV_KEY, key, 0);
                            self.frame_dirty = true;
                        }
                    }
                }
                PenTransition::TipUp => {
                    self.dev.emit(EV_KEY, BTN_TOUCH, 0);
                    self.dev.emit(EV_ABS, ABS_PRESSURE, 0);
                    self.frame_dirty = true;
                }
                PenTransition::ProximityOut => {
                    self.dev.emit(EV_KEY, self.tool, 0);
                    self.frame_dirty = true;
                }
            }
        }
        self.flush();
    }
}
