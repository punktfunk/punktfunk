//! The operator-facing settings: one row per setting, in page order.
//!
//! A row is the single source for a setting's env name, store key, kind,
//! default, and when a change takes effect. [`crate::snapshot`] resolves every
//! row as pin > env > store > default; the web console renders the rows the
//! host serves. Env-only knobs (debug, packaging) never get a row.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    /// Inclusive range. An env value outside it is clamped; a store value is refused.
    Int {
        min: i64,
        max: i64,
        unit: &'static str,
    },
    /// [`Kind::Int`] for a value that takes a fraction.
    Decimal {
        min: f64,
        max: f64,
        unit: &'static str,
    },
    /// Canonical spellings, lowercase snake_case.
    Enum(&'static [&'static str]),
    Text {
        max_len: usize,
    },
    /// Comma-separated in env, a string array in the store.
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Streaming,
    Video,
    Audio,
    Input,
    Network,
    GameMode,
    Session,
    System,
}

impl Group {
    pub fn as_str(self) -> &'static str {
        match self {
            Group::Streaming => "streaming",
            Group::Video => "video",
            Group::Audio => "audio",
            Group::Input => "input",
            Group::Network => "network",
            Group::GameMode => "game_mode",
            Group::Session => "session",
            Group::System => "system",
        }
    }
}

/// When a changed value reaches the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    Now,
    NextSession,
    Restart,
}

impl Apply {
    pub fn as_str(self) -> &'static str {
        match self {
            Apply::Now => "now",
            Apply::NextSession => "next_session",
            Apply::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Int(i64),
    Decimal(f64),
    Str(&'static str),
    List(&'static [&'static str]),
    /// Linux, then everything else.
    PerOs(&'static DefaultValue, &'static DefaultValue),
}

impl DefaultValue {
    pub fn to_value(self) -> Value {
        match self {
            DefaultValue::Bool(b) => Value::Bool(b),
            DefaultValue::Int(n) => Value::from(n),
            DefaultValue::Decimal(x) => Value::from(x),
            DefaultValue::Str(s) => Value::from(s),
            DefaultValue::List(l) => Value::from(l.to_vec()),
            DefaultValue::PerOs(linux, other) => {
                if cfg!(target_os = "linux") {
                    linux.to_value()
                } else {
                    other.to_value()
                }
            }
        }
    }
}

/// An older env name. `value: None` reads it with the row's own grammar; `Some(v)` means
/// "set at all" selects `v`, whatever it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alias {
    pub name: &'static str,
    pub value: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// Store key and API id.
    pub id: &'static str,
    pub env: &'static str,
    pub kind: Kind,
    pub default: DefaultValue,
    pub group: Group,
    pub advanced: bool,
    pub apply: Apply,
    /// `std::env::consts::OS` values this row applies on; empty = every host.
    pub os: &'static [&'static str],
    /// English label, ≤ 3 words. The console localises by `id` and falls back to this.
    pub title: &'static str,
    /// docs-site page slug under `/docs/`.
    pub docs: &'static str,
    /// Checked in order after `env`.
    pub aliases: &'static [Alias],
    /// Extra env spellings for an enum: `(spelling, canonical)`.
    pub spellings: &'static [(&'static str, &'static str)],
}

impl Setting {
    pub fn available(&self) -> bool {
        self.os.is_empty() || self.os.contains(&std::env::consts::OS)
    }

    /// An env string in this row's grammar. `Ok(None)` for blank: blank is unset.
    pub fn parse_env(&self, raw: &str) -> Result<Option<Value>, String> {
        let s = raw.trim();
        if s.is_empty() {
            return Ok(None);
        }
        let lower = s.to_ascii_lowercase();
        let v = match self.kind {
            Kind::Bool => {
                Value::Bool(parse_bool(&lower).ok_or_else(|| format!("{s:?} is not on or off"))?)
            }
            Kind::Int { min, max, .. } => {
                let n: i64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
                Value::from(n.clamp(min, max))
            }
            Kind::Decimal { min, max, .. } => {
                let x = s
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite())
                    .ok_or_else(|| format!("{s:?} is not a number"))?;
                Value::from(x.clamp(min, max))
            }
            Kind::Enum(options) => {
                let norm = lower.replace('-', "_");
                let hit = [lower.as_str(), norm.as_str()].into_iter().find_map(|c| {
                    options.iter().find(|o| **o == c).copied().or_else(|| {
                        self.spellings
                            .iter()
                            .find(|(sp, _)| *sp == c)
                            .map(|(_, canon)| *canon)
                    })
                });
                Value::from(
                    hit.ok_or_else(|| format!("{s:?} is not one of {}", options.join("/")))?,
                )
            }
            Kind::Text { max_len } => {
                if s.chars().count() > max_len {
                    return Err(format!("longer than {max_len} characters"));
                }
                Value::from(s)
            }
            Kind::List => Value::from(
                s.split(',')
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>(),
            ),
        };
        Ok(Some(v))
    }

    /// A store or API value. Stricter than env: out of range is refused, not clamped.
    pub fn validate(&self, v: &Value) -> Result<Value, String> {
        match (self.kind, v) {
            (Kind::Bool, Value::Bool(_)) => Ok(v.clone()),
            (Kind::Int { min, max, .. }, Value::Number(n)) => match n.as_i64() {
                Some(i) if (min..=max).contains(&i) => Ok(v.clone()),
                _ => Err(format!("must be a whole number from {min} to {max}")),
            },
            (Kind::Decimal { min, max, .. }, Value::Number(n)) => match n.as_f64() {
                Some(x) if (min..=max).contains(&x) => Ok(Value::from(x)),
                _ => Err(format!("must be a number from {min} to {max}")),
            },
            (Kind::Enum(options), Value::String(s)) if options.contains(&s.as_str()) => {
                Ok(v.clone())
            }
            (Kind::Enum(options), _) => Err(format!("must be one of {}", options.join(", "))),
            (Kind::Text { max_len }, Value::String(s)) => {
                if s.trim().chars().count() > max_len {
                    Err(format!("must be at most {max_len} characters"))
                } else {
                    Ok(Value::from(s.trim()))
                }
            }
            (Kind::List, Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it.as_str().map(str::trim) {
                        Some("") => {}
                        Some(s) if !s.contains(',') => out.push(Value::from(s)),
                        _ => return Err("must be a list of names without commas".into()),
                    }
                }
                Ok(Value::Array(out))
            }
            (Kind::Bool, _) => Err("must be true or false".into()),
            (Kind::Int { .. } | Kind::Decimal { .. }, _) => Err("must be a number".into()),
            (Kind::Text { .. }, _) => Err("must be text".into()),
            (Kind::List, _) => Err("must be a list".into()),
        }
    }
}

/// The one boolean grammar for registry rows.
pub fn parse_bool(lower: &str) -> Option<bool> {
    match lower {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

pub fn find(id: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.id == id)
}

const LINUX: &[&str] = &["linux"];
const LINUX_WINDOWS: &[&str] = &["linux", "windows"];
/// Row `pyrowave_bpp`'s default: Themaister's clean point for 4:2:0, 200 Mbps at 1080p60.
pub const PYROWAVE_BPP: f64 = 1.6;
const TRI: Kind = Kind::Enum(&["auto", "on", "off"]);
const TRI_SPELLINGS: &[(&str, &str)] = &[
    ("1", "on"),
    ("true", "on"),
    ("yes", "on"),
    ("0", "off"),
    ("false", "off"),
    ("no", "off"),
];

#[cfg(target_os = "windows")]
const ENCODERS: &[&str] = &["auto", "nvenc", "amf", "qsv", "mf"];
#[cfg(target_os = "windows")]
const ENCODER_SPELLINGS: &[(&str, &str)] = &[
    ("hw", "nvenc"),
    ("nvidia", "nvenc"),
    ("cuda", "nvenc"),
    ("amd", "amf"),
    ("intel", "qsv"),
    ("mediafoundation", "mf"),
];
#[cfg(not(target_os = "windows"))]
const ENCODERS: &[&str] = &["auto", "nvenc", "vaapi", "vulkan", "pyrowave", "software"];
#[cfg(not(target_os = "windows"))]
const ENCODER_SPELLINGS: &[(&str, &str)] = &[
    ("nvidia", "nvenc"),
    ("cuda", "nvenc"),
    ("amd", "vaapi"),
    ("intel", "vaapi"),
    ("vaapi_native", "vaapi"),
    ("vulkan_video", "vulkan"),
    ("sw", "software"),
    ("openh264", "software"),
];

#[cfg(target_os = "windows")]
const GAMEPADS: &[&str] = &[
    "auto",
    "xbox360",
    "xboxone",
    "xboxelite",
    "dualsense",
    "dualsenseedge",
    "dualshock4",
    "steamdeck",
    "steamcontroller2",
];
#[cfg(not(target_os = "windows"))]
const GAMEPADS: &[&str] = &[
    "auto",
    "xbox360",
    "xboxone",
    "dualsense",
    "dualsenseedge",
    "dualshock4",
    "steamdeck",
    "steamcontroller",
    "steamcontroller2",
    "switchpro",
];
const GAMEPAD_SPELLINGS: &[(&str, &str)] = &[
    ("xbox", "xbox360"),
    ("x360", "xbox360"),
    ("series", "xboxone"),
    ("ds", "dualsense"),
    ("ps5", "dualsense"),
    ("edge", "dualsenseedge"),
    ("ds4", "dualshock4"),
    ("ps4", "dualshock4"),
    ("deck", "steamdeck"),
    ("switch", "switchpro"),
    ("sc2", "steamcontroller2"),
];

/// A row with the common defaults: every OS, not advanced, no aliases or spellings.
#[allow(clippy::too_many_arguments)]
const fn row(
    id: &'static str,
    env: &'static str,
    kind: Kind,
    default: DefaultValue,
    group: Group,
    apply: Apply,
    title: &'static str,
    docs: &'static str,
) -> Setting {
    Setting {
        id,
        env,
        kind,
        default,
        group,
        advanced: false,
        apply,
        os: &[],
        title,
        docs,
        aliases: &[],
        spellings: &[],
    }
}

impl Setting {
    const fn advanced(mut self) -> Setting {
        self.advanced = true;
        self
    }

    const fn only(mut self, os: &'static [&'static str]) -> Setting {
        self.os = os;
        self
    }

    const fn aliases(mut self, aliases: &'static [Alias]) -> Setting {
        self.aliases = aliases;
        self
    }

    const fn spellings(mut self, spellings: &'static [(&'static str, &'static str)]) -> Setting {
        self.spellings = spellings;
        self
    }
}

use Apply::{NextSession, Now, Restart};
use DefaultValue as D;
use Group::{Audio, GameMode, Input, Network, Streaming, System, Video};

/// Page order. Add a row where it reads best; ids are never reused.
#[rustfmt::skip]
pub static SETTINGS: &[Setting] = &[
    // --- Streaming
    row("gamestream", "PUNKTFUNK_GAMESTREAM", Kind::Bool, D::Bool(false), Streaming, Restart, "GameStream", "moonlight"),
    row("webtransport", "PUNKTFUNK_WEBTRANSPORT", Kind::Bool, D::Bool(false), Streaming, Restart, "Browser streaming", "clients"),
    row("clipboard", "PUNKTFUNK_CLIPBOARD", Kind::Enum(&["off", "text", "files"]), D::Str("off"), Streaming, NextSession, "Shared clipboard", "clipboard")
        .only(LINUX_WINDOWS)
        .spellings(&[
            ("0", "off"),
            ("false", "off"),
            ("no", "off"),
            ("text_only", "text"),
            ("no_files", "text"),
            ("1", "files"),
            ("on", "files"),
            ("true", "files"),
            ("yes", "files"),
            ("all", "files"),
        ]),
    row("host_name", "PUNKTFUNK_HOST_NAME", Kind::Text { max_len: 63 }, D::Str(""), Streaming, Restart, "Host name", "configuration"),
    row("gamestream_encrypt", "PUNKTFUNK_GAMESTREAM_ENCRYPT", Kind::Enum(&["supported", "video", "off", "required"]), D::Str("supported"), Streaming, Restart, "GameStream encryption", "moonlight")
        .advanced()
        .aliases(&[Alias { name: "PUNKTFUNK_GS_ENCRYPT", value: None }])
        .spellings(&[
            ("1", "supported"),
            ("0", "off"),
            ("false", "off"),
            ("no", "off"),
            ("video_only", "video"),
            ("require", "required"),
        ]),
    row("gamestream_adapt", "PUNKTFUNK_GAMESTREAM_ADAPT", Kind::Bool, D::Bool(true), Streaming, Restart, "Moonlight adaptive bitrate", "moonlight")
        .advanced()
        .aliases(&[Alias { name: "PUNKTFUNK_GS_ADAPT", value: None }]),
    row("chacha20", "PUNKTFUNK_CHACHA20", Kind::Bool, D::Bool(true), Streaming, NextSession, "ChaCha20 cipher", "configuration").advanced(),
    row("webtransport_origins", "PUNKTFUNK_WEBTRANSPORT_ORIGINS", Kind::List, D::List(&[]), Streaming, Restart, "Browser origins", "clients").advanced(),
    // --- Video
    row("encoder", "PUNKTFUNK_ENCODER", Kind::Enum(ENCODERS), D::Str("auto"), Video, NextSession, "Encoder", "configuration").spellings(ENCODER_SPELLINGS),
    row("ten_bit", "PUNKTFUNK_10BIT", Kind::Bool, D::Bool(true), Video, NextSession, "10-bit and HDR", "hdr"),
    row("chroma_444", "PUNKTFUNK_444", Kind::Bool, D::Bool(true), Video, NextSession, "Full color 4:4:4", "configuration"),
    row("max_fps", "PUNKTFUNK_MAX_FPS", Kind::Int { min: 0, max: 240, unit: "fps" }, D::Int(0), Video, NextSession, "Game frame limit", "gamescope").only(LINUX),
    row("portal_cursor_mode", "PUNKTFUNK_PORTAL_CURSOR_MODE", Kind::Enum(&["auto", "embedded", "metadata", "hidden"]), D::Str("auto"), Video, NextSession, "Cursor capture", "configuration")
        .advanced()
        .only(LINUX)
        .spellings(&[("composited", "embedded"), ("meta", "metadata"), ("none", "hidden")]),
    row("vulkan_encode", "PUNKTFUNK_VULKAN_ENCODE", Kind::Bool, D::Bool(true), Video, NextSession, "Vulkan encoding", "configuration").advanced().only(LINUX),
    row("direct_capture", "PUNKTFUNK_DIRECT_CAPTURE", Kind::Bool, D::Bool(true), Video, NextSession, "Direct capture", "configuration").advanced().only(LINUX),
    row("lazy_capture", "PUNKTFUNK_LAZY_CAPTURE", Kind::Bool, D::Bool(true), Video, NextSession, "On-demand capture", "configuration").advanced().only(LINUX),
    row("kwin_paced", "PUNKTFUNK_KWIN_PACED", Kind::Bool, D::Bool(false), Video, NextSession, "KWin capture pacing", "kde").advanced().only(LINUX),
    row("pyrowave_bpp", "PUNKTFUNK_PYROWAVE_BPP", Kind::Decimal { min: 0.25, max: 4.0, unit: "bits/pixel" }, D::Decimal(PYROWAVE_BPP), Video, NextSession, "PyroWave quality", "pyrowave"),
    row("pyrowave_max_mbps", "PUNKTFUNK_PYROWAVE_MAX_MBPS", Kind::Int { min: 0, max: 10_000, unit: "Mbps" }, D::Int(0), Video, NextSession, "PyroWave bitrate cap", "pyrowave").advanced(),
    // --- Audio
    row("audio_output_mode", "PUNKTFUNK_AUDIO_OUTPUT_MODE", Kind::Enum(&["client_only", "host_and_client", "follow_default"]), D::Str("client_only"), Audio, NextSession, "Where audio plays", "configuration")
        .only(LINUX_WINDOWS)
        // KEEP_DEFAULT first: a stale host-audio flag must not override "do not touch my devices".
        .aliases(&[
            Alias { name: "PUNKTFUNK_KEEP_DEFAULT", value: Some("follow_default") },
            Alias { name: "PUNKTFUNK_HOST_AUDIO", value: Some("host_and_client") },
        ])
        .spellings(&[
            ("client", "client_only"),
            ("both", "host_and_client"),
            ("host", "host_and_client"),
            ("follow", "follow_default"),
        ]),
    row("audio_quality", "PUNKTFUNK_AUDIO_QUALITY", Kind::Enum(&["low", "standard", "high"]), D::Str("high"), Audio, NextSession, "Audio quality", "configuration")
        .spellings(&[("normal", "standard"), ("medium", "standard")]),
    row("audio_hires", "PUNKTFUNK_AUDIO_HIRES", Kind::Bool, D::Bool(true), Audio, NextSession, "Lossless audio", "configuration"),
    row("audio_voice_chat", "PUNKTFUNK_AUDIO_VOICE_CHAT", Kind::Enum(&["stream", "host"]), D::Str("stream"), Audio, NextSession, "Voice chat", "configuration")
        .only(LINUX_WINDOWS)
        .spellings(&[("client", "stream"), ("speakers", "host")]),
    // Extra apps; the host always adds `DEFAULT_VOICE_APPS`.
    row("audio_voice_apps", "PUNKTFUNK_AUDIO_VOICE_APPS", Kind::List, D::List(&[]), Audio, NextSession, "Voice chat apps", "configuration").only(LINUX_WINDOWS),
    row("pad_audio", "PUNKTFUNK_PAD_AUDIO", Kind::Bool, D::Bool(true), Audio, NextSession, "Controller speaker", "controller-audio").only(LINUX_WINDOWS),
    row("audio_redundancy", "PUNKTFUNK_AUDIO_REDUNDANCY", TRI, D::Str("auto"), Audio, NextSession, "Audio redundancy", "configuration")
        .advanced()
        .spellings(TRI_SPELLINGS),
    // --- Input
    row("gamepad", "PUNKTFUNK_GAMEPAD", Kind::Enum(GAMEPADS), D::Str("auto"), Input, NextSession, "Default gamepad", "input")
        .only(LINUX_WINDOWS)
        .spellings(GAMEPAD_SPELLINGS),
    row("pen", "PUNKTFUNK_PEN", Kind::Bool, D::Bool(true), Input, NextSession, "Pen input", "input").only(LINUX_WINDOWS),
    row("steam_gadget", "PUNKTFUNK_STEAM_GADGET", TRI, D::Str("auto"), Input, NextSession, "Steam USB gadget", "input")
        .advanced()
        .only(LINUX)
        .spellings(TRI_SPELLINGS),
    row("dualsense_usbip", "PUNKTFUNK_DUALSENSE_USBIP", Kind::Bool, D::Bool(false), Input, NextSession, "DualSense over USB/IP", "controller-audio").advanced().only(LINUX),
    // --- Game Mode
    row("gamescope_attach", "PUNKTFUNK_GAMESCOPE_ATTACH", Kind::Bool, D::Bool(false), GameMode, NextSession, "Attach mode", "gamescope").only(LINUX),
    row("gamescope_hdr", "PUNKTFUNK_GAMESCOPE_HDR", Kind::Bool, D::Bool(true), GameMode, NextSession, "Game Mode HDR", "gamescope").only(LINUX),
    row("gamescope_managed", "PUNKTFUNK_GAMESCOPE_MANAGED", Kind::Bool, D::Bool(false), GameMode, NextSession, "Force managed mode", "gamescope").advanced().only(LINUX),
    row("gamescope_vrr", "PUNKTFUNK_GAMESCOPE_VRR", Kind::Bool, D::Bool(true), GameMode, NextSession, "Adaptive sync", "gamescope").advanced().only(LINUX),
    row("gamescope_sdr_nits", "PUNKTFUNK_GAMESCOPE_SDR_NITS", Kind::Int { min: 1, max: 10_000, unit: "nits" }, D::Int(203), GameMode, NextSession, "SDR brightness", "hdr").advanced().only(LINUX),
    row("gamescope_refresh_rates", "PUNKTFUNK_GAMESCOPE_REFRESH_RATES", Kind::List, D::List(&[]), GameMode, NextSession, "Extra refresh rates", "gamescope").advanced().only(LINUX),
    row("gamescope_steam", "PUNKTFUNK_GAMESCOPE_STEAM", Kind::Bool, D::Bool(false), GameMode, NextSession, "Steam integration", "gamescope").advanced().only(LINUX),
    row("gamescope_splash", "PUNKTFUNK_GAMESCOPE_SPLASH", Kind::Bool, D::Bool(true), GameMode, NextSession, "Startup splash", "gamescope").advanced().only(LINUX),
    row("gamescope_isolate", "PUNKTFUNK_GAMESCOPE_ISOLATE", Kind::Bool, D::Bool(true), GameMode, NextSession, "Per-session isolation", "gamescope").advanced().only(LINUX),
    row("gamescope_grab_cursor", "PUNKTFUNK_GAMESCOPE_GRAB_CURSOR", Kind::Bool, D::Bool(false), GameMode, NextSession, "Grab the cursor", "gamescope").advanced().only(LINUX),
    row("steam_seat_home", "PUNKTFUNK_STEAM_SEAT_HOME", Kind::Bool, D::Bool(false), GameMode, NextSession, "Steam per seat", "gamescope").advanced().only(LINUX),
    row("steam_seat_sandbox", "PUNKTFUNK_STEAM_SEAT_SANDBOX", Kind::Bool, D::Bool(false), GameMode, NextSession, "Pads per seat", "gamescope").advanced().only(LINUX),
    row("steam_prewarm", "PUNKTFUNK_STEAM_PREWARM", Kind::Int { min: 0, max: 8, unit: "seats" }, D::Int(1), GameMode, Restart, "Seats kept warm", "gamescope").advanced().only(LINUX),
    row("gamescope_bind", "PUNKTFUNK_GAMESCOPE_BIND", TRI, D::Str("auto"), GameMode, NextSession, "Bind patched gamescope", "gamescope")
        .advanced()
        .only(LINUX)
        .spellings(TRI_SPELLINGS),
    row("session_watch", "PUNKTFUNK_SESSION_WATCH", TRI, D::Str("auto"), GameMode, NextSession, "Follow mode switches", "gamescope")
        .advanced()
        .only(LINUX)
        .spellings(TRI_SPELLINGS),
    // --- Network
    row("mdns", "PUNKTFUNK_MDNS", Kind::Bool, D::Bool(true), Network, Restart, "Local discovery", "troubleshooting-connect").advanced(),
    row("idle_timeout_ms", "PUNKTFUNK_IDLE_TIMEOUT_MS", Kind::Int { min: 1_000, max: 120_000, unit: "ms" }, D::Int(8_000), Network, Restart, "Disconnect timeout", "configuration").advanced(),
    // --- System
    row("update_check", "PUNKTFUNK_UPDATE_CHECK", Kind::Bool, D::Bool(true), System, Now, "Check for updates", "updating"),
    row("update_apply", "PUNKTFUNK_UPDATE_APPLY", Kind::Bool, D::Bool(true), System, Now, "Console updates", "updating").advanced(),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_unique_and_well_formed() {
        let mut ids = std::collections::HashSet::new();
        let mut envs = std::collections::HashSet::new();
        for s in SETTINGS {
            assert!(ids.insert(s.id), "duplicate id {}", s.id);
            assert!(
                s.id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{} is not snake_case",
                s.id
            );
            for name in std::iter::once(s.env).chain(s.aliases.iter().map(|a| a.name)) {
                assert!(name.starts_with("PUNKTFUNK_"), "{name}");
                assert!(envs.insert(name), "env name {name} is used twice");
            }
            assert!(s.title.split_whitespace().count() <= 3, "{} title", s.id);
            // The default must satisfy the row's own validation, or a reset stores junk.
            let d = s.default.to_value();
            assert_eq!(s.validate(&d).as_ref(), Ok(&d), "{} default", s.id);
            if let Kind::Enum(options) = s.kind {
                for (_, canon) in s.spellings {
                    assert!(options.contains(canon), "{} spelling -> {canon}", s.id);
                }
            }
        }
    }

    const DOCS_HEADER: &str = "| Setting | `host.env` | Values | Default | Applies |";

    fn docs_table() -> String {
        let code = |s: &str| format!("`{s}`");
        let mut out = format!("{DOCS_HEADER}\n|---|---|---|---|---|\n");
        for s in SETTINGS {
            let values = match s.kind {
                Kind::Bool => "`on` · `off`".to_string(),
                Kind::Int { min, max, unit } => format!("{min}–{max} {unit}"),
                Kind::Decimal { min, max, unit } => format!("{min}–{max} {unit}"),
                Kind::Enum(o) => o.iter().map(|x| code(x)).collect::<Vec<_>>().join(" · "),
                Kind::Text { max_len } => format!("text, up to {max_len} characters"),
                Kind::List => "comma list".to_string(),
            };
            let default = match s.default.to_value() {
                Value::Bool(b) => code(if b { "on" } else { "off" }),
                Value::String(t) if t.is_empty() => "—".to_string(),
                Value::Array(a) if a.is_empty() => "—".to_string(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(code)
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::String(t) => code(&t),
                v => code(&v.to_string()),
            };
            let applies = match s.apply {
                Apply::Now => "at once",
                Apply::NextSession => "next session",
                Apply::Restart => "after a restart",
            };
            let only = if s.os.is_empty() {
                String::new()
            } else {
                let names: Vec<_> =
                    s.os.iter()
                        .map(|o| match *o {
                            "linux" => "Linux",
                            "windows" => "Windows",
                            other => other,
                        })
                        .collect();
                format!(" ({})", names.join(", "))
            };
            out += &format!(
                "| {}{only} | `{}` | {values} | {default} | {applies} |\n",
                s.title, s.env
            );
        }
        out
    }

    /// `configuration.md` carries the table this registry renders. `UPDATE_SETTINGS_DOCS=1`
    /// rewrites it in place. Not on Windows: the encoder and gamepad options differ there.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn docs_table_is_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs-site/content/docs/(reference)/configuration.md"
        );
        let doc = std::fs::read_to_string(path).expect("read configuration.md");
        let start = doc
            .find(DOCS_HEADER)
            .expect("configuration.md has the settings table");
        let len = doc[start..]
            .split_inclusive('\n')
            .take_while(|l| l.starts_with('|'))
            .map(str::len)
            .sum::<usize>();
        let want = docs_table();
        if std::env::var_os("UPDATE_SETTINGS_DOCS").is_some() {
            let next = format!("{}{want}{}", &doc[..start], &doc[start + len..]);
            std::fs::write(path, next).expect("write configuration.md");
            return;
        }
        assert_eq!(
            &doc[start..start + len],
            want,
            "the settings table in configuration.md is stale — rerun with UPDATE_SETTINGS_DOCS=1"
        );
    }

    /// Pre-warming only ever runs under a seat home, so its own default costs a box nothing
    /// until the seat-home knob is on.
    #[test]
    fn seats_are_kept_warm_one_at_a_time() {
        let s = find("steam_prewarm").expect("the row exists");
        assert_eq!(s.default.to_value(), Value::Number(1.into()));
        assert_eq!(s.env, "PUNKTFUNK_STEAM_PREWARM");
        assert!(
            s.validate(&Value::Number(0.into())).is_ok(),
            "0 turns it off"
        );
    }

    /// A seat home costs a Steam sign-in per device, so it is never on by accident.
    #[test]
    fn a_steam_seat_home_is_off_until_an_operator_asks_for_it() {
        let s = find("steam_seat_home").expect("the row exists");
        assert_eq!(s.default.to_value(), Value::Bool(false));
        assert_eq!(s.apply, Apply::NextSession);
        assert_eq!(s.env, "PUNKTFUNK_STEAM_SEAT_HOME");
    }

    /// The pad filter rides a seat home, so it is off until an operator asks for both.
    #[test]
    fn a_seat_pad_filter_is_off_until_an_operator_asks_for_it() {
        let s = find("steam_seat_sandbox").expect("the row exists");
        assert_eq!(s.default.to_value(), Value::Bool(false));
        assert_eq!(s.apply, Apply::NextSession);
        assert_eq!(s.env, "PUNKTFUNK_STEAM_SEAT_SANDBOX");
    }

    #[test]
    fn bool_grammar_is_one_grammar() {
        let s = find("gamestream").unwrap();
        for on in ["1", "true", "ON", " yes "] {
            assert_eq!(s.parse_env(on), Ok(Some(Value::Bool(true))), "{on:?}");
        }
        for off in ["0", "false", "Off", "no"] {
            assert_eq!(s.parse_env(off), Ok(Some(Value::Bool(false))), "{off:?}");
        }
        assert_eq!(s.parse_env("  "), Ok(None));
        assert!(s.parse_env("maybe").is_err());
    }

    #[test]
    fn env_enum_spellings_and_int_clamp() {
        let clip = find("clipboard").unwrap();
        assert_eq!(clip.parse_env("text-only"), Ok(Some(Value::from("text"))));
        assert_eq!(clip.parse_env("on"), Ok(Some(Value::from("files"))));
        assert_eq!(clip.parse_env("FILES"), Ok(Some(Value::from("files"))));
        assert!(clip.parse_env("sometimes").is_err());
        let mode = find("audio_output_mode").unwrap();
        assert_eq!(
            mode.parse_env("host-and-client"),
            Ok(Some(Value::from("host_and_client")))
        );
        let fps = find("max_fps").unwrap();
        assert_eq!(fps.parse_env("500"), Ok(Some(Value::from(240))));
        assert!(fps.parse_env("sixty").is_err());
        let bpp = find("pyrowave_bpp").unwrap();
        assert_eq!(bpp.parse_env("0.8"), Ok(Some(Value::from(0.8))));
        assert_eq!(bpp.parse_env("9"), Ok(Some(Value::from(4.0))));
        assert!(bpp.parse_env("NaN").is_err());
    }

    #[test]
    fn store_values_are_validated_not_clamped() {
        let fps = find("max_fps").unwrap();
        assert!(fps.validate(&Value::from(60)).is_ok());
        assert!(fps.validate(&Value::from(500)).is_err());
        assert!(fps.validate(&Value::from("60")).is_err());
        let bpp = find("pyrowave_bpp").unwrap();
        assert_eq!(bpp.validate(&Value::from(2)), Ok(Value::from(2.0)));
        assert!(bpp.validate(&Value::from(5.0)).is_err());
        let apps = find("audio_voice_apps").unwrap();
        assert_eq!(
            apps.validate(&serde_json::json!([" discord ", ""])),
            Ok(serde_json::json!(["discord"]))
        );
        assert!(apps.validate(&serde_json::json!(["a,b"])).is_err());
        let name = find("host_name").unwrap();
        assert!(name.validate(&Value::from("x".repeat(64))).is_err());
    }
}
