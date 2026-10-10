//! Pause NVIDIA Instant Replay while it would share the encoder with a stream.
//!
//! Instant Replay is a second NVENC session with no API to stop it. The NVIDIA App keeps
//! its live state and the user's own toggle hotkey in `ShareSettings.json`, so the host
//! presses that hotkey on the user's desktop and reads the flip back from the same file.
//! Never more than one press per call: the file, not the keystroke, is the truth.
//!
//! `PUNKTFUNK_INSTANT_REPLAY_PAUSE`: `auto` pauses once a stream falls behind cadence while
//! another app encodes ([`on_encoder_shared`]); `on` pauses at the first stream; `off` never
//! presses. Whatever this module paused it resumes when the last stream ends, or at the next
//! host start after a crash (the owed marker in the config dir).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};

const SETTINGS_REL: &str = r"NVIDIA Corporation\NVIDIA Overlay\ShareSettings.json";
/// The NVIDIA App rewrites the file within a frame or two of the hotkey.
const FLIP_TIMEOUT: Duration = Duration::from_secs(2);
const FLIP_POLL: Duration = Duration::from_millis(100);
const MARKER: &str = "instant-replay.paused";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    On,
    Off,
}

fn mode() -> Mode {
    match pf_host_config::row_tri("PUNKTFUNK_INSTANT_REPLAY_PAUSE") {
        Some(true) => Mode::On,
        Some(false) => Mode::Off,
        None => Mode::Auto,
    }
}

/// What this process owes the user's Instant Replay.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owed {
    Nothing,
    /// We turned it off; turn it back on at the last stream's end.
    Resume,
    /// The hotkey did not take once; do not keep pressing keys into the game.
    GaveUp,
}

static OWED: Mutex<Owed> = Mutex::new(Owed::Nothing);

fn owed() -> Owed {
    *OWED.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_owed(o: Owed) {
    *OWED.lock().unwrap_or_else(|e| e.into_inner()) = o;
}

struct State {
    enabled: bool,
    /// Virtual-key codes of the user's toggle chord, press order.
    chord: Vec<u16>,
}

/// The session user's `ShareSettings.json`. The host is SYSTEM, so `%LOCALAPPDATA%` is the
/// service profile; the user's comes from their volatile environment under `HKEY_USERS`.
fn settings_path() -> Option<PathBuf> {
    let sid = super::theme::session_sid()?;
    let env = format!("{sid}\\Volatile Environment");
    let local = super::theme::read_string(&env, "LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            super::theme::read_string(&env, "USERPROFILE")
                .map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })?;
    Some(local.join(SETTINGS_REL))
}

fn parse_state(raw: &str) -> Option<State> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let settings = &v["settings"];
    let enabled = settings["video"]["irEnabled"].as_bool()?;
    let chord: Vec<u16> = settings["shortcuts"]["DVRToggle"]
        .as_array()?
        .iter()
        .filter_map(|k| u16::try_from(k.as_u64()?).ok())
        .collect();
    (!chord.is_empty()).then_some(State { enabled, chord })
}

fn read_state(path: &std::path::Path) -> Option<State> {
    parse_state(&std::fs::read_to_string(path).ok()?)
}

fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Press the chord on the input desktop: downs in order, ups reversed. Binds this thread
/// for the send and rebinds its previous desktop after; unbound, it sends to the current one.
fn press(chord: &[u16]) -> bool {
    let mut inputs: Vec<INPUT> = chord.iter().map(|&vk| key(vk, false)).collect();
    inputs.extend(chord.iter().rev().map(|&vk| key(vk, true)));
    let _desk = super::game_term::InputDesktop::attach();
    // SAFETY: `SendInput` reads the live `inputs` slice with the exact element stride and
    // returns the count injected.
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    n as usize == inputs.len()
}

enum Outcome {
    /// Already as wanted; nothing pressed.
    Unchanged,
    Flipped,
    /// Pressed once; the file did not follow.
    NotFlipped,
}

fn set_enabled(want: bool) -> Result<Outcome> {
    let path = settings_path().context("no console user with an NVIDIA App profile")?;
    let state = read_state(&path).context("read NVIDIA App ShareSettings.json")?;
    if state.enabled == want {
        return Ok(Outcome::Unchanged);
    }
    if !press(&state.chord) {
        anyhow::bail!("SendInput did not deliver the Instant Replay hotkey");
    }
    let deadline = Instant::now() + FLIP_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(FLIP_POLL);
        if read_state(&path).is_some_and(|s| s.enabled == want) {
            return Ok(Outcome::Flipped);
        }
    }
    Ok(Outcome::NotFlipped)
}

fn marker() -> PathBuf {
    pf_paths::config_dir().join(MARKER)
}

fn pause(why: &'static str) {
    match set_enabled(false) {
        Ok(Outcome::Unchanged) => {}
        Ok(Outcome::Flipped) => {
            set_owed(Owed::Resume);
            let _ = std::fs::write(marker(), b"");
            tracing::info!(why, "paused NVIDIA Instant Replay for the stream");
        }
        Ok(Outcome::NotFlipped) => {
            set_owed(Owed::GaveUp);
            tracing::warn!("Instant Replay hotkey did not take — leaving it on");
        }
        Err(e) => tracing::debug!(error = %e, "instant replay pause skipped"),
    }
}

fn resume() {
    match set_enabled(true) {
        Ok(Outcome::NotFlipped) => {
            tracing::warn!("couldn't turn NVIDIA Instant Replay back on — use its hotkey");
        }
        Ok(Outcome::Flipped) => {
            let _ = std::fs::remove_file(marker());
            tracing::info!("NVIDIA Instant Replay back on");
        }
        Ok(Outcome::Unchanged) => {
            let _ = std::fs::remove_file(marker());
        }
        Err(e) => tracing::debug!(error = %e, "instant replay resume skipped"),
    }
    set_owed(Owed::Nothing);
}

fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

/// First live stream. `on` pauses now; `auto` waits for [`on_encoder_shared`].
pub fn on_stream_start() {
    if mode() == Mode::On {
        spawn("punktfunk-ir-pause", || pause("stream start"));
    }
}

/// Last live stream ended: give back what was paused.
pub fn on_stream_end() {
    if owed() == Owed::Resume {
        spawn("punktfunk-ir-resume", resume);
    } else {
        set_owed(Owed::Nothing);
    }
}

/// A stream is behind cadence with another encoder on the engine. Already off the stream
/// thread ([`crate::encoder_sessions::on_behind_cadence`]).
pub fn on_encoder_shared() {
    if mode() == Mode::Auto && owed() == Owed::Nothing {
        pause("encoder shared while behind cadence");
    }
}

/// A prior host paused it and never resumed. Off the startup path: the flip waits on a file.
pub fn startup_recover() {
    if marker().exists() {
        spawn("punktfunk-ir-recover", resume);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARE: &str = r#"{"settings":{"shortcuts":{"DVRToggle":[18,16,121],"PTT":[192]},
        "video":{"irEnabled":true,"irBufferLength":300}}}"#;

    #[test]
    fn parses_the_flag_and_the_chord() {
        let s = parse_state(SHARE).unwrap();
        assert!(s.enabled);
        assert_eq!(s.chord, [18, 16, 121]);
    }

    #[test]
    fn an_unbound_chord_is_no_state() {
        let raw = SHARE.replace("[18,16,121]", "[]");
        assert!(parse_state(&raw).is_none());
    }

    #[test]
    fn a_chord_presses_down_in_order_and_up_reversed() {
        let chord = [18u16, 16, 121];
        let mut inputs: Vec<INPUT> = chord.iter().map(|&vk| key(vk, false)).collect();
        inputs.extend(chord.iter().rev().map(|&vk| key(vk, true)));
        // SAFETY: every element was built by `key` as a keyboard INPUT, so `ki` is the live arm.
        let vks: Vec<(u16, bool)> = inputs
            .iter()
            .map(|i| unsafe {
                (
                    i.Anonymous.ki.wVk.0,
                    i.Anonymous.ki.dwFlags == KEYEVENTF_KEYUP,
                )
            })
            .collect();
        assert_eq!(
            vks,
            [
                (18, false),
                (16, false),
                (121, false),
                (121, true),
                (16, true),
                (18, true)
            ]
        );
    }
}
