//! Controller menu vocabulary and synthesizer: [`MenuEvent`], [`MenuNav`], [`PadInfo`].
//!
//! [`MenuNav`] maps a [`MenuSample`] to rising-edge buttons, snapshot-on-entry,
//! and stick/D-pad moves with initial-delay auto-repeat. Cadence, dead zone, and
//! hysteresis live here so desktop SDL and Android JNI share one feel.
//!
//! Numbers match Apple `GamepadMenuInput`. Tests in this file pin the cadence.

use punktfunk_core::config::GamepadPref;
use std::time::{Duration, Instant};

/// 0.5 of full scale — Apple `GamepadMenuInput`.
pub const MENU_DEADZONE: u16 = 16384;
/// Apple `GamepadMenuInput`: 380 ms before auto-repeat.
pub const MENU_REPEAT_DELAY: Duration = Duration::from_millis(380);
/// Apple `GamepadMenuInput`: 160 ms between repeats.
pub const MENU_REPEAT_INTERVAL: Duration = Duration::from_millis(160);
/// 0.3 of full scale. An engaged direction holds until its own axis drops here,
/// so a diagonal flick does not fire two moves and a wobble does not jitter.
pub const MENU_RELEASE: u16 = 9830;
// Release must sit below engage; crossing them would drop hysteresis silently.
const _: () = assert!(MENU_RELEASE > 0 && MENU_RELEASE < MENU_DEADZONE);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuDir {
    Up,
    Down,
    Left,
    Right,
}

/// One pad action for the launcher UI. Buttons are rising-edge; `Move` auto-repeats
/// after [`MENU_REPEAT_DELAY`] / [`MENU_REPEAT_INTERVAL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    Move(MenuDir),
    /// Six 60° ring slots. `Some(k)` on change, `None` at rest. Lists still `Move`.
    Sector(Option<u8>),
    Confirm,
    Back,
    Secondary,
    Tertiary,
    JumpBack,
    JumpForward,
}

/// Menu haptic pulse. Never during a stream.
#[derive(Clone, Copy, Debug)]
pub enum MenuPulse {
    Move,
    Confirm,
    Boundary,
}

#[derive(Clone, Copy, Default)]
pub struct MenuSample {
    /// a, b, x, y, l1, r1 — the order [`MenuNav::poll`] maps to events.
    pub buttons: [bool; 6],
    /// Left stick, SDL convention (+y = down).
    pub lx: i16,
    pub ly: i16,
    /// up, down, left, right.
    pub dpad: [bool; 4],
}

/// Snapshot-on-reset: the poll after [`reset`](Self::reset) adopts held buttons
/// and direction without firing; buttons fire on the rising edge only.
pub struct MenuNav {
    /// Next poll adopts held state without firing (mode entry, stream detach, pad change).
    snapshot_pending: bool,
    was: [bool; 6],
    dir: Option<MenuDir>,
    /// Instant `dir` engaged; start of [`MENU_REPEAT_DELAY`].
    dir_since: Instant,
    last_repeat: Instant,
    sector: Option<u8>,
    /// True once a sector event went out. A snapshot-adopted sector never did,
    /// so its release stays silent.
    sector_announced: bool,
}

/// Hold this many degrees past a 30° sector edge so a stick on the boundary does not flicker.
pub const SECTOR_OVERLAP_DEG: f32 = 5.0;

/// Ring slot for the left stick, given the sector already engaged.
///
/// Past the deadzone by magnitude (a diagonal counts), angle falls into six 60°
/// sectors: slot `k` at `-90° + 60°·k`, 12 o'clock first, clockwise. An engaged
/// sector holds until magnitude < [`MENU_RELEASE`] or the angle leaves by
/// [`SECTOR_OVERLAP_DEG`]. SDL sticks are +y = down.
pub fn ring_sector(lx: i16, ly: i16, current: Option<u8>) -> Option<u8> {
    let (x, y) = (f32::from(lx), f32::from(ly));
    let mag = x.hypot(y);
    let floor = if current.is_some() {
        f32::from(MENU_RELEASE)
    } else {
        f32::from(MENU_DEADZONE)
    };
    if mag <= floor {
        return None;
    }
    // Degrees clockwise from 12 o'clock; slot k's centre is 60·k.
    let deg = (y.atan2(x).to_degrees() + 90.0).rem_euclid(360.0);
    if let Some(k) = current {
        let off = (deg - 60.0 * f32::from(k) + 180.0).rem_euclid(360.0) - 180.0;
        if off.abs() <= 30.0 + SECTOR_OVERLAP_DEG {
            return Some(k);
        }
    }
    Some((((deg + 30.0) / 60.0) as u8) % 6)
}

impl Default for MenuNav {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuNav {
    pub fn new() -> MenuNav {
        MenuNav {
            snapshot_pending: true,
            was: [false; 6],
            dir: None,
            dir_since: Instant::now(),
            last_repeat: Instant::now(),
            sector: None,
            sector_announced: false,
        }
    }

    /// Next poll adopts held state without firing.
    pub fn reset(&mut self) {
        self.snapshot_pending = true;
        self.dir = None;
        self.sector = None;
        self.sector_announced = false;
    }

    /// Dominant axis past the deadzone, else dpad. SDL +y = down.
    pub fn resolve_dir(s: &MenuSample) -> Option<MenuDir> {
        let (ax, ay) = (s.lx.unsigned_abs(), s.ly.unsigned_abs());
        if ax > MENU_DEADZONE || ay > MENU_DEADZONE {
            return Some(if ax >= ay {
                if s.lx > 0 {
                    MenuDir::Right
                } else {
                    MenuDir::Left
                }
            } else if s.ly > 0 {
                MenuDir::Down
            } else {
                MenuDir::Up
            });
        }
        let [up, down, left, right] = s.dpad;
        if left {
            Some(MenuDir::Left)
        } else if right {
            Some(MenuDir::Right)
        } else if up {
            Some(MenuDir::Up)
        } else if down {
            Some(MenuDir::Down)
        } else {
            None
        }
    }

    /// True while this stick axis is still past [`MENU_RELEASE`]. Holds the
    /// engaged direction so dominant-axis cannot flip mid-gesture.
    fn stick_holds(s: &MenuSample, d: MenuDir) -> bool {
        match d {
            MenuDir::Left => s.lx < -(MENU_RELEASE as i16),
            MenuDir::Right => s.lx > MENU_RELEASE as i16,
            MenuDir::Up => s.ly < -(MENU_RELEASE as i16),
            MenuDir::Down => s.ly > MENU_RELEASE as i16,
        }
    }

    fn resolve_engaged(&self, s: &MenuSample) -> Option<MenuDir> {
        match self.dir {
            Some(d) if Self::stick_holds(s, d) => Some(d),
            _ => Self::resolve_dir(s),
        }
    }

    pub fn poll(&mut self, s: &MenuSample, now: Instant, out: &mut Vec<MenuEvent>) {
        let dir = self.resolve_engaged(s);
        let sector = ring_sector(s.lx, s.ly, self.sector);
        if self.snapshot_pending {
            self.snapshot_pending = false;
            self.was = s.buttons;
            self.dir = dir;
            self.dir_since = now;
            self.last_repeat = now;
            self.sector = sector;
            self.sector_announced = false;
            return;
        }
        // Sector before buttons and Move of the same sample, so the ring is
        // engaged before a step, and A lands on the slot the stick points at.
        if sector != self.sector {
            self.sector = sector;
            if sector.is_some() || self.sector_announced {
                out.push(MenuEvent::Sector(sector));
            }
            self.sector_announced = sector.is_some();
        }
        const EVENTS: [MenuEvent; 6] = [
            MenuEvent::Confirm,
            MenuEvent::Back,
            MenuEvent::Tertiary,
            MenuEvent::Secondary,
            MenuEvent::JumpBack,
            MenuEvent::JumpForward,
        ];
        for (i, ev) in EVENTS.iter().enumerate() {
            if s.buttons[i] && !self.was[i] {
                out.push(*ev);
            }
            self.was[i] = s.buttons[i];
        }
        if dir != self.dir {
            self.dir = dir;
            self.dir_since = now;
            self.last_repeat = now;
            if let Some(d) = dir {
                out.push(MenuEvent::Move(d));
            }
        } else if let Some(d) = dir {
            if now.duration_since(self.dir_since) >= MENU_REPEAT_DELAY
                && now.duration_since(self.last_repeat) >= MENU_REPEAT_INTERVAL
            {
                self.last_repeat = now;
                out.push(MenuEvent::Move(d));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct PadInfo {
    pub name: String,
    /// `vid:pid:name`. SDL instance ids are per-run; [`Settings::forward_pad`](crate::trust::Settings) persists this.
    pub key: String,
    /// What "Automatic" resolves this physical pad to. Fallback is Xbox 360.
    pub pref: GamepadPref,
    /// Steam Input's emulated pad (Valve 28de:11ff). Shadows the physical
    /// controller and has no sensors, so auto-selection skips it while a real pad is connected.
    pub steam_virtual: bool,
    /// Local SDL power state. `None` is wired / unreported — show no battery, not 0 %.
    pub battery: Option<PadBattery>,
    /// Line under the name (`VID:PID · gamepad · dpad`). Empty is not an error.
    pub detail: String,
    /// Forwarded: real, non-virtual, OS-classified GAMEPAD. Joystick siblings are listed, not forwarded.
    pub forwarded: bool,
    pub rumble: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadBattery {
    /// 0–100. SDL −1 ("on power, level unknown") is mapped to `None` by callers.
    pub percent: u8,
    pub charging: bool,
}

impl PadInfo {
    /// Settings-list kind; `""` for a plain Xbox / standard pad.
    pub fn kind_label(&self) -> &'static str {
        match self.pref {
            GamepadPref::DualSense => "DualSense",
            GamepadPref::DualSenseEdge => "DualSense Edge",
            GamepadPref::DualShock4 => "DualShock 4",
            GamepadPref::XboxOne => "Xbox One",
            // SDL has no Elite GamepadType, but a pinned setting can carry it.
            GamepadPref::XboxElite => "Xbox Elite Series 2",
            GamepadPref::SteamDeck => "Steam Deck",
            GamepadPref::SteamController => "Steam Controller",
            GamepadPref::SteamController2 => "Steam Controller 2",
            GamepadPref::SteamController2Puck => "Steam Controller 2 Puck",
            GamepadPref::SwitchPro => "Switch Pro",
            GamepadPref::EightBitDoUltimate2 => "8BitDo Ultimate 2",
            GamepadPref::EightBitDoPro2 => "8BitDo Pro 2",
            GamepadPref::EightBitDoPro3 => "8BitDo Pro 3",
            GamepadPref::HoripadSteam => "HORIPAD for Steam",
            GamepadPref::JoyConPair => "Joy-Con pair",
            GamepadPref::Switch2Pro => "Switch 2 Pro",
            GamepadPref::Switch2GameCube => "GameCube (Switch 2)",
            _ => "",
        }
    }
}

#[cfg(test)]
mod menu_nav_tests {
    use super::*;

    fn sample() -> MenuSample {
        MenuSample::default()
    }

    fn events(nav: &mut MenuNav, s: &MenuSample, at: Instant) -> Vec<MenuEvent> {
        let mut out = Vec::new();
        nav.poll(s, at, &mut out);
        out
    }

    #[test]
    fn snapshot_adopts_held_state_without_firing() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        let mut held = sample();
        held.buttons[0] = true; // A
        held.lx = 30000; // right
        assert!(events(&mut nav, &held, t).is_empty(), "snapshot poll fired");
        assert!(events(&mut nav, &held, t + Duration::from_millis(10)).is_empty());
        assert!(events(&mut nav, &sample(), t + Duration::from_millis(20)).is_empty());
        assert_eq!(
            events(&mut nav, &held, t + Duration::from_millis(30)),
            vec![
                MenuEvent::Sector(Some(2)),
                MenuEvent::Confirm,
                MenuEvent::Move(MenuDir::Right)
            ]
        );
    }

    #[test]
    fn buttons_fire_on_rising_edge_only() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t); // consume the snapshot
        let mut s = sample();
        s.buttons[1] = true; // B
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![MenuEvent::Back]
        );
        for i in 2..20 {
            assert!(
                events(&mut nav, &s, t + Duration::from_millis(10 * i)).is_empty(),
                "held button re-fired"
            );
        }
    }

    #[test]
    fn reset_rearms_the_snapshot() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        nav.reset();
        let mut s = sample();
        s.buttons[1] = true;
        assert!(
            events(&mut nav, &s, t + Duration::from_millis(10)).is_empty(),
            "post-reset poll fired a held button"
        );
    }

    #[test]
    fn direction_repeats_after_delay_at_interval() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut s = sample();
        s.dpad[3] = true; // right
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        assert!(events(&mut nav, &s, t + Duration::from_millis(300)).is_empty());
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(400)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        assert!(events(&mut nav, &s, t + Duration::from_millis(500)).is_empty());
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(570)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        assert!(events(&mut nav, &sample(), t + Duration::from_millis(580)).is_empty());
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(590)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
    }

    #[test]
    fn direction_change_fires_immediately() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut right = sample();
        right.lx = 30000;
        let mut left = sample();
        left.lx = -30000;
        // 3 o'clock is the 1/2 edge; 9 o'clock is 4/5.
        assert_eq!(
            events(&mut nav, &right, t + Duration::from_millis(10)),
            vec![MenuEvent::Sector(Some(2)), MenuEvent::Move(MenuDir::Right)]
        );
        assert_eq!(
            events(&mut nav, &left, t + Duration::from_millis(20)),
            vec![MenuEvent::Sector(Some(5)), MenuEvent::Move(MenuDir::Left)]
        );
    }

    #[test]
    fn direction_resolution() {
        let mut s = sample();
        s.lx = MENU_DEADZONE as i16;
        assert_eq!(MenuNav::resolve_dir(&s), None);
        // Dominant axis wins; SDL +y = down.
        s.lx = 20000;
        s.ly = 25000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Down));
        s.ly = -25000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Up));
        s.lx = 26000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Right));
        s.lx = -26000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Left));
        let mut d = sample();
        d.dpad[1] = true;
        assert_eq!(MenuNav::resolve_dir(&d), Some(MenuDir::Down));
        d.lx = 30000;
        assert_eq!(MenuNav::resolve_dir(&d), Some(MenuDir::Right));
    }

    #[test]
    fn the_stick_angle_picks_a_ring_sector_with_hysteresis() {
        assert_eq!(ring_sector(0, 0, None), None);
        assert_eq!(
            ring_sector(12000, 12000, None),
            Some(2),
            "16 971 > 16 384, 45° = slot 2"
        );
        assert_eq!(ring_sector(11000, 11000, None), None, "15 556 < 16 384");
        // Six centres, 12 o'clock first, clockwise (SDL +y = down).
        assert_eq!(ring_sector(0, -30000, None), Some(0));
        assert_eq!(ring_sector(26000, -15000, None), Some(1));
        assert_eq!(ring_sector(26000, 15000, None), Some(2));
        assert_eq!(ring_sector(0, 30000, None), Some(3));
        assert_eq!(ring_sector(-26000, 15000, None), Some(4));
        assert_eq!(ring_sector(-26000, -15000, None), Some(5));
        let (x, y) = (14084, -26489); // 30 000 at 28°
        assert_eq!(ring_sector(x, y, None), Some(0));
        let (x, y) = (15897, -25441); // 30 000 at 32°
        assert_eq!(
            ring_sector(x, y, Some(0)),
            Some(0),
            "5° of overlap holds it"
        );
        assert_eq!(ring_sector(x, y, None), Some(1));
        assert_eq!(
            ring_sector(x, y, Some(5)),
            Some(1),
            "36° from slot 5's edge lets go"
        );
        // Engaged sector releases under MENU_RELEASE, not the deadzone.
        assert_eq!(ring_sector(0, -12000, Some(0)), Some(0));
        assert_eq!(ring_sector(0, -9000, Some(0)), None);
    }

    #[test]
    fn the_sector_goes_out_before_the_move_and_only_on_change() {
        let mut nav = MenuNav::new();
        let t0 = Instant::now();
        assert!(events(&mut nav, &sample(), t0).is_empty(), "snapshot");
        let mut s = sample();
        s.lx = 26000;
        s.ly = 15000;
        assert_eq!(
            events(&mut nav, &s, t0),
            vec![MenuEvent::Sector(Some(2)), MenuEvent::Move(MenuDir::Right)]
        );
        assert!(
            events(&mut nav, &s, t0).is_empty(),
            "held: no repeat yet, no new sector"
        );
        s.ly = -15000;
        assert_eq!(events(&mut nav, &s, t0), vec![MenuEvent::Sector(Some(1))]);
        assert_eq!(
            events(&mut nav, &sample(), t0),
            vec![MenuEvent::Sector(None)],
            "neutral releases the sector"
        );
    }

    #[test]
    fn engaged_direction_holds_until_its_own_axis_releases() {
        // Engaged DOWN holds while ly is above MENU_RELEASE even if lx dominates.
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut s = sample();
        s.ly = 25_000;
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![MenuEvent::Sector(Some(3)), MenuEvent::Move(MenuDir::Down)]
        );
        s.lx = 30_000;
        s.ly = 12_000; // above MENU_RELEASE (9830), below MENU_DEADZONE
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(20)),
            vec![MenuEvent::Sector(Some(2))],
            "the dominant-axis flip fired RIGHT while DOWN was still engaged; the ring's \
             sector, which has no axis to hold, followed the angle to 4 o'clock"
        );
        s.ly = 5_000;
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(30)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        // No hold across sign.
        s.lx = -30_000;
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(40)),
            vec![MenuEvent::Sector(Some(4)), MenuEvent::Move(MenuDir::Left)]
        );
    }

    #[test]
    fn shoulder_and_face_button_mapping() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut s = sample();
        s.buttons = [false, false, true, true, true, true]; // x, y, l1, r1
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![
                MenuEvent::Tertiary,
                MenuEvent::Secondary,
                MenuEvent::JumpBack,
                MenuEvent::JumpForward,
            ]
        );
    }
}
