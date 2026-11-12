//! The Seat defaults a seat applies itself (`design/web-console-structure-2026-10.md` §2.1).
//!
//! The box resolves them from its own settings and writes [`FILE`] into its config dir, at start
//! and after every settings write. A seat that follows the contract reads it from its trust dir,
//! re-reading when the file changes, so a default reaches a running seat's next session. The
//! Linux supervisor reads `library_writable` from the box's copy at each seat start. The box's
//! upkeep reads the other two (kept warm, idle stop) from its own config. `dualsense_usbip` is
//! the box's own switch handed on, not a Seats row. A seat's profile may hold session settings of
//! its own in the box's library, which come before `end_on_game_exit`.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub const FILE: &str = "seat-defaults.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatDefaults {
    /// A seat's stream ends when its game exits.
    pub end_on_game_exit: bool,
    /// The highest mode a seat grants a device without a cap of its own, `WIDTHxHEIGHT@HZ`.
    pub max_mode: Option<String>,
    /// A seat updates and installs into the owner's Steam libraries; off mounts them read-only.
    pub library_writable: bool,
    /// A seat's DualSense is a USB/IP device with a real sound card, as the box's is.
    pub dualsense_usbip: bool,
    /// The box streams to browsers, so each seat runs a browser plane of its own.
    pub browser: bool,
    /// The browser origins the box admits, which each seat's plane admits too.
    pub browser_origins: Vec<String>,
    /// Every app a Windows seat starts reads GameInput in the background.
    pub background_input: bool,
}

/// The rows [`SeatDefaults::of`] reads. On a door they are the door's own: its file is the one
/// the seats' copies come from.
pub const ROWS: &[&str] = &[
    "seat_end_on_game_exit",
    "seat_max_mode",
    "seat_library_writable",
    "dualsense_usbip",
    "webtransport",
    "webtransport_origins",
    "seat_background_input",
];

impl SeatDefaults {
    pub fn of(config: &crate::HostConfig) -> Self {
        SeatDefaults {
            end_on_game_exit: config.seat_end_on_game_exit,
            max_mode: config.seat_max_mode.clone(),
            library_writable: config.seat_library_writable,
            dualsense_usbip: config.dualsense_usbip,
            browser: config.webtransport,
            browser_origins: config.webtransport_origins.clone(),
            background_input: config.seat_background_input,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "end_on_game_exit": self.end_on_game_exit,
            "max_mode": self.max_mode,
            "library_writable": self.library_writable,
            "dualsense_usbip": self.dualsense_usbip,
            "browser": self.browser,
            "browser_origins": self.browser_origins,
            "background_input": self.background_input,
        })
    }

    /// A field the file lacks takes its row default, so an older box's file still reads.
    fn from_json(v: &Value) -> Self {
        SeatDefaults {
            end_on_game_exit: v["end_on_game_exit"].as_bool().unwrap_or(true),
            max_mode: v["max_mode"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            library_writable: v["library_writable"].as_bool().unwrap_or(true),
            dualsense_usbip: v["dualsense_usbip"].as_bool().unwrap_or(false),
            browser: v["browser"].as_bool().unwrap_or(false),
            browser_origins: v["browser_origins"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().map(str::to_string))
                .collect(),
            background_input: v["background_input"].as_bool().unwrap_or(true),
        }
    }
}

/// Writes the box's resolved defaults into `dir`. A seat host has none of its own to hand on.
pub fn write(dir: &Path) -> std::io::Result<()> {
    if pf_paths::seat::role().is_seat_host() {
        return Ok(());
    }
    let body = serde_json::to_vec_pretty(&SeatDefaults::of(crate::config()).to_json())
        .map_err(std::io::Error::other)?;
    pf_paths::replace_secret_file(&dir.join(FILE), &body)
}

/// The file read last, its mtime then, and what it held.
type Cached = (PathBuf, Option<SystemTime>, Option<SeatDefaults>);

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// This seat's defaults, as the box last wrote them. `None` on any host that does not follow the
/// contract, and while the box has written none: the seat then keeps its built-in defaults.
pub fn current() -> Option<SeatDefaults> {
    if !pf_paths::seat::role().follows_contract() {
        return None;
    }
    of_box()
}

/// The defaults the box last wrote, as any seat host reads them, the owner's included. Only
/// [`current`] decides which of them a seat applies as its own settings. `None` off a seat host.
pub fn of_box() -> Option<SeatDefaults> {
    if !pf_paths::seat::role().is_seat_host() {
        return None;
    }
    read_cached(&pf_paths::seat::trust_dir()?.join(FILE))
}

fn read_cached(path: &Path) -> Option<SeatDefaults> {
    let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, _, v)) = cache.as_ref().filter(|(p, s, _)| p == path && *s == stamp) {
        return v.clone();
    }
    let value = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .map(|v| SeatDefaults::from_json(&v));
    *cache = Some((path.to_path_buf(), stamp, value.clone()));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_reads_back_and_a_missing_field_takes_its_default() {
        let d = SeatDefaults {
            end_on_game_exit: false,
            max_mode: Some("2560x1440@120".into()),
            library_writable: false,
            dualsense_usbip: true,
            browser: true,
            browser_origins: vec!["https://box:47992".into()],
            background_input: false,
        };
        assert_eq!(SeatDefaults::from_json(&d.to_json()), d);
        assert_eq!(
            SeatDefaults::from_json(&json!({ "max_mode": "" })),
            SeatDefaults {
                end_on_game_exit: true,
                max_mode: None,
                library_writable: true,
                dualsense_usbip: false,
                browser: false,
                browser_origins: Vec::new(),
                background_input: true,
            }
        );
    }

    #[test]
    fn the_reader_follows_the_file() {
        let dir = std::env::temp_dir().join(format!("pf-seat-defaults-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        assert_eq!(read_cached(&path), None);
        std::fs::write(
            &path,
            br#"{"end_on_game_exit":false,"max_mode":"1920x1080@60"}"#,
        )
        .unwrap();
        assert_eq!(
            read_cached(&path).map(|d| d.max_mode),
            Some(Some("1920x1080@60".into()))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
