//! Resolves every [`registry`] row and persists the console's values.
//!
//! Order per row: a CLI flag pin, then env (name, then aliases), then
//! `<config>/host-settings.json`, then the row default. A write re-resolves
//! and swaps the process snapshot, so a caller that reads [`crate::config`]
//! per session sees the change on its next session.

use crate::registry::{self, Apply, Setting, SETTINGS};
use crate::HostConfig;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Store,
    Env,
    Flag,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Store => "store",
            Source::Env => "env",
            Source::Flag => "flag",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub setting: &'static Setting,
    /// The value in force.
    pub value: Value,
    /// The console's value, in force or not.
    pub stored: Option<Value>,
    pub source: Source,
    /// The env name or CLI flag behind `value` when `source` is `Env` or `Flag`.
    pub origin: Option<&'static str>,
}

/// One resolution of every row plus the [`HostConfig`] built from it. Rows stay in
/// registry order, unavailable ones included, so fields never depend on the OS filter.
#[derive(Debug)]
pub struct Snapshot {
    pub config: HostConfig,
    pub settings: Vec<Resolved>,
}

impl Snapshot {
    pub fn get(&self, id: &str) -> Option<&Resolved> {
        self.settings.iter().find(|r| r.setting.id == id)
    }
}

#[derive(Debug)]
pub enum SaveError {
    Unknown(String),
    Invalid { id: String, reason: String },
    Io(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Unknown(id) => write!(f, "unknown setting {id:?}"),
            SaveError::Invalid { id, reason } => write!(f, "{id}: {reason}"),
            SaveError::Io(e) => write!(f, "write host-settings.json: {e}"),
        }
    }
}

impl std::error::Error for SaveError {}

type Pin = (&'static str, &'static str, Value);

// Each write leaks the previous Snapshot (a few KB) so `config()` can hand out `&'static`.
// Writes are a person clicking; swap in an `Arc` if they ever become programmatic.
static CURRENT: RwLock<Option<&'static Snapshot>> = RwLock::new(None);
static STARTED: OnceLock<&'static Snapshot> = OnceLock::new();
static PINS: Mutex<Vec<Pin>> = Mutex::new(Vec::new());
static WRITE: Mutex<()> = Mutex::new(());

pub fn store_path() -> PathBuf {
    pf_paths::config_dir().join("host-settings.json")
}

/// The current resolution, built on first access.
pub fn snapshot() -> &'static Snapshot {
    if let Some(s) = *CURRENT.read().unwrap_or_else(PoisonError::into_inner) {
        return s;
    }
    let mut cur = CURRENT.write().unwrap_or_else(PoisonError::into_inner);
    cur.get_or_insert_with(|| Box::leak(Box::new(build_current())))
}

/// Re-resolve from env, the store file and the pins, then swap.
pub fn reload() {
    let next: &'static Snapshot = Box::leak(Box::new(build_current()));
    *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = Some(next);
}

/// A CLI flag that sets a row outranks env and the store, and the console shows it locked.
pub fn pin(id: &'static str, flag: &'static str, value: Value) {
    debug_assert!(registry::find(id).is_some(), "pin for unknown row {id}");
    PINS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((id, flag, value));
    reload();
}

/// Freeze the resolution the serving planes were built from.
pub fn mark_started() {
    let _ = STARTED.set(snapshot());
}

/// A Bool row's value in force, in the registry's one grammar: the value the console shows.
pub fn row_bool(env: &str) -> bool {
    row_bool_in(&snapshot().settings, env)
}

/// A tri-state (`auto`/`on`/`off`) row's value in force; `None` is auto.
pub fn row_tri(env: &str) -> Option<bool> {
    row_tri_in(&snapshot().settings, env)
}

pub(crate) fn row_bool_in(rows: &[Resolved], env: &str) -> bool {
    rows.iter()
        .find(|r| r.setting.env == env)
        .and_then(|r| r.value.as_bool())
        .unwrap_or_default()
}

pub(crate) fn row_tri_in(rows: &[Resolved], env: &str) -> Option<bool> {
    match rows.iter().find(|r| r.setting.env == env)?.value.as_str() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    }
}

/// A raw `PUNKTFUNK_*` read that also sees the console, for numeric, text and enum rows: the
/// env value when env set the row, else the console's value spelled as the env var would be,
/// else `None`, so the reader keeps its own default. Any other name reads the environment.
/// Bool and tri-state rows read through [`row_bool`] and [`row_tri`].
pub fn knob(name: &str) -> Option<String> {
    knob_in(&snapshot().settings, name)
}

/// [`knob`] against given rows. The snapshot build uses this; [`knob`] would deadlock there.
pub(crate) fn knob_in(rows: &[Resolved], name: &str) -> Option<String> {
    let Some(r) = rows.iter().find(|r| r.setting.env == name) else {
        return std::env::var(name).ok();
    };
    // An env value keeps its own spelling; an alias that selects a value stands for it. An enum
    // row reads canonical: its spellings are the registry's, and every reader takes the options.
    if r.source == Source::Env {
        let origin = r.origin?;
        match r.setting.aliases.iter().find(|a| a.name == origin) {
            Some(registry::Alias { value: Some(v), .. }) => return Some(v.to_string()),
            _ if !matches!(r.setting.kind, registry::Kind::Enum(_)) => {
                return std::env::var(origin).ok()
            }
            _ => {}
        }
    } else if r.source == Source::Default || r.value == r.setting.default.to_value() {
        return None;
    }
    Some(match &r.value {
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        // Tri-state rows name their states `auto`/`on`/`off`; readers take `1`/`0`.
        Value::String(s) if s == "on" => "1".to_string(),
        Value::String(s) if s == "off" => "0".to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    })
}

/// Restart-class rows whose value differs from what the planes started with.
pub fn restart_pending() -> Vec<&'static str> {
    let Some(start) = STARTED.get() else {
        return Vec::new();
    };
    pending_between(start, snapshot())
}

fn pending_between(start: &Snapshot, now: &Snapshot) -> Vec<&'static str> {
    now.settings
        .iter()
        .zip(&start.settings)
        .filter(|(n, s)| n.setting.apply == Apply::Restart && n.value != s.value)
        .map(|(n, _)| n.setting.id)
        .collect()
}

/// Merge `patch` into the store (`null` clears a key), then reload. Validates every key
/// before writing anything.
pub fn save(patch: &Map<String, Value>) -> Result<(), SaveError> {
    let _w = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    merge_into(&store_path(), patch)?;
    reload();
    Ok(())
}

/// [`save`] into the store at `path`, without reloading this process. The installer writes a
/// host's store this way before the host runs.
pub fn save_at(path: &Path, patch: &Map<String, Value>) -> Result<(), SaveError> {
    let _w = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    merge_into(path, patch)
}

fn merge_into(path: &Path, patch: &Map<String, Value>) -> Result<(), SaveError> {
    let mut file = read_file(path).map_err(SaveError::Io)?;
    apply_patch(&mut file, patch)?;
    write_file(path, &file).map_err(SaveError::Io)
}

fn apply_patch(file: &mut Map<String, Value>, patch: &Map<String, Value>) -> Result<(), SaveError> {
    let mut checked = Vec::with_capacity(patch.len());
    for (id, v) in patch {
        let s = registry::find(id).ok_or_else(|| SaveError::Unknown(id.clone()))?;
        let v = if v.is_null() {
            None
        } else {
            Some(s.validate(v).map_err(|reason| SaveError::Invalid {
                id: id.clone(),
                reason,
            })?)
        };
        checked.push((s.id, v));
    }
    for (id, v) in checked {
        match v {
            Some(v) => file.insert(id.to_string(), v),
            None => file.remove(id),
        };
    }
    file.insert("version".into(), Value::from(1));
    Ok(())
}

fn write_file(path: &Path, file: &Map<String, Value>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    pf_paths::replace_secret_file(path, &bytes)
}

/// Every key, unknown ones included, so a save from an older host keeps a newer host's keys.
/// A file that exists but is not a JSON object is an error, so a save never overwrites it.
fn read_file(path: &Path) -> std::io::Result<Map<String, Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a JSON object",
        )),
    }
}

/// [`read_file`], with env and defaults standing in for a store that does not read.
fn load_file(path: &Path) -> Map<String, Value> {
    read_file(path).unwrap_or_else(|e| {
        warn_once(format!(
            "punktfunk: {}: {e} — using env and defaults",
            path.display()
        ));
        Map::new()
    })
}

fn build_current() -> Snapshot {
    let pins = PINS.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let env = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
    build(&env, &load_file(&store_path()), &pins)
}

fn build(
    env: &dyn Fn(&str) -> Option<String>,
    file: &Map<String, Value>,
    pins: &[Pin],
) -> Snapshot {
    let settings = resolve(env, file, pins);
    let mut config = HostConfig::from_rows(&settings);
    config.apply_settings(&settings);
    Snapshot { config, settings }
}

pub(crate) fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    file: &Map<String, Value>,
    pins: &[Pin],
) -> Vec<Resolved> {
    SETTINGS
        .iter()
        .map(|s| {
            let stored = file.get(s.id).and_then(|v| match s.validate(v) {
                Ok(v) => Some(v),
                Err(reason) => {
                    warn_once(format!(
                        "punktfunk: host-settings.json {}: {reason} — ignoring it",
                        s.id
                    ));
                    None
                }
            });
            let (value, source, origin) =
                if let Some((_, flag, v)) = pins.iter().find(|(id, _, _)| *id == s.id) {
                    (v.clone(), Source::Flag, Some(*flag))
                } else if let Some((name, v)) = from_env(s, env) {
                    (v, Source::Env, Some(name))
                } else if let Some(v) = &stored {
                    (v.clone(), Source::Store, None)
                } else {
                    (s.default.to_value(), Source::Default, None)
                };
            Resolved {
                setting: s,
                value,
                stored,
                source,
                origin,
            }
        })
        .collect()
}

fn from_env(
    s: &'static Setting,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<(&'static str, Value)> {
    if let Some(raw) = env(s.env) {
        match s.parse_env(&raw) {
            Ok(Some(v)) => return Some((s.env, v)),
            Ok(None) => {}
            Err(reason) => warn_once(format!(
                "punktfunk: {}={raw:?}: {reason} — ignoring it",
                s.env
            )),
        }
    }
    for alias in s.aliases {
        let Some(raw) = env(alias.name) else { continue };
        let v = match alias.value {
            Some(v) => Value::from(v),
            None => match s.parse_env(&raw) {
                Ok(Some(v)) => v,
                _ => continue,
            },
        };
        warn_once(format!(
            "punktfunk: {} is an old name — set {} instead",
            alias.name, s.env
        ));
        return Some((alias.name, v));
    }
    None
}

fn warn_once(msg: String) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if seen.get_or_insert_with(HashSet::new).insert(msg.clone()) {
        eprintln!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn row<'a>(r: &'a [Resolved], id: &str) -> &'a Resolved {
        r.iter().find(|x| x.setting.id == id).unwrap()
    }

    #[test]
    fn flag_beats_env_beats_store_beats_default() {
        let file = obj(json!({"gamestream": true, "max_fps": 60, "ten_bit": false}));
        let env = env_of(&[("PUNKTFUNK_MAX_FPS", "30"), ("PUNKTFUNK_GAMESTREAM", "0")]);
        let pins = [("gamestream", "--gamestream", Value::Bool(true))];
        let r = resolve(&env, &file, &pins);

        let gs = row(&r, "gamestream");
        assert_eq!((gs.source, gs.origin), (Source::Flag, Some("--gamestream")));
        assert_eq!(gs.value, json!(true));

        let fps = row(&r, "max_fps");
        assert_eq!((fps.source, &fps.value), (Source::Env, &json!(30)));
        // The console's value survives underneath the lock.
        assert_eq!(fps.stored, Some(json!(60)));

        let tb = row(&r, "ten_bit");
        assert_eq!((tb.source, &tb.value), (Source::Store, &json!(false)));

        let chroma = row(&r, "chroma_444");
        assert_eq!(
            (chroma.source, &chroma.value),
            (Source::Default, &json!(true))
        );
    }

    #[test]
    fn aliases_select_values_and_report_their_name() {
        let r = resolve(&env_of(&[("PUNKTFUNK_HOST_AUDIO", "")]), &Map::new(), &[]);
        let m = row(&r, "audio_output_mode");
        assert_eq!(m.value, json!("host_and_client"));
        assert_eq!(m.origin, Some("PUNKTFUNK_HOST_AUDIO"));
        // KEEP_DEFAULT is checked first; the real name beats both.
        let both = env_of(&[
            ("PUNKTFUNK_HOST_AUDIO", "1"),
            ("PUNKTFUNK_KEEP_DEFAULT", "1"),
        ]);
        assert_eq!(
            row(&resolve(&both, &Map::new(), &[]), "audio_output_mode").value,
            json!("follow_default")
        );
        let named = env_of(&[
            ("PUNKTFUNK_HOST_AUDIO", "1"),
            ("PUNKTFUNK_AUDIO_OUTPUT_MODE", "client"),
        ]);
        assert_eq!(
            row(&resolve(&named, &Map::new(), &[]), "audio_output_mode").value,
            json!("client_only")
        );
    }

    #[test]
    fn junk_env_and_store_values_fall_through() {
        let file = obj(json!({"max_fps": "sixty", "clipboard": "text"}));
        let env = env_of(&[
            ("PUNKTFUNK_CLIPBOARD", "sometimes"),
            ("PUNKTFUNK_GAMESTREAM", " "),
        ]);
        let r = resolve(&env, &file, &[]);
        assert_eq!(row(&r, "max_fps").source, Source::Default);
        assert_eq!(row(&r, "max_fps").stored, None);
        assert_eq!(row(&r, "clipboard").value, json!("text"));
        assert_eq!(row(&r, "clipboard").source, Source::Store);
        // Blank is unset, not "on".
        assert_eq!(row(&r, "gamestream").source, Source::Default);
    }

    #[test]
    fn patch_validates_everything_before_writing() {
        let mut file = obj(json!({"version": 1, "future_key": 7, "max_fps": 60}));
        let bad = obj(json!({"clipboard": "text", "max_fps": 999}));
        assert!(matches!(
            apply_patch(&mut file, &bad),
            Err(SaveError::Invalid { ref id, .. }) if id == "max_fps"
        ));
        assert_eq!(
            file.get("clipboard"),
            None,
            "a refused patch writes nothing"
        );
        assert!(matches!(
            apply_patch(&mut file, &obj(json!({"nope": 1}))),
            Err(SaveError::Unknown(_))
        ));
        apply_patch(
            &mut file,
            &obj(json!({"clipboard": "text", "max_fps": null})),
        )
        .unwrap();
        assert_eq!(file.get("clipboard"), Some(&json!("text")));
        assert_eq!(file.get("max_fps"), None);
        assert_eq!(file.get("future_key"), Some(&json!(7)));
    }

    #[test]
    fn store_file_round_trips_and_tolerates_junk() {
        let dir = std::env::temp_dir().join(format!("pf-host-settings-{}", std::process::id()));
        let path = dir.join("host-settings.json");
        let mut file = Map::new();
        apply_patch(&mut file, &obj(json!({"host_name": " Living Room "}))).unwrap();
        write_file(&path, &file).unwrap();
        assert_eq!(
            load_file(&path).get("host_name"),
            Some(&json!("Living Room"))
        );
        std::fs::write(&path, b"[1,2]").unwrap();
        assert!(load_file(&path).is_empty());
        assert!(load_file(&dir.join("absent.json")).is_empty());
        // A save refuses to replace a store it cannot read, rather than wipe its keys.
        std::fs::write(&path, b"{\"host_name\": \"x\",}").unwrap();
        assert!(merge_into(&path, &obj(json!({"max_fps": 60}))).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"host_name\": \"x\",}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn knobs_spell_the_console_value_and_leave_defaults_to_the_reader() {
        let file = obj(json!({
            "max_fps": 60,
            "ten_bit": false,
            "gamestream": false,
            "audio_voice_apps": ["discord", "mumble"],
        }));
        let rows = resolve(&env_of(&[]), &file, &[]);
        assert_eq!(knob_in(&rows, "PUNKTFUNK_MAX_FPS").as_deref(), Some("60"));
        assert_eq!(knob_in(&rows, "PUNKTFUNK_10BIT").as_deref(), Some("0"));
        assert_eq!(
            knob_in(&rows, "PUNKTFUNK_GAMESTREAM"),
            None,
            "a stored default is still the reader's default"
        );
        assert_eq!(
            knob_in(&rows, "PUNKTFUNK_AUDIO_VOICE_APPS").as_deref(),
            Some("discord,mumble")
        );
        assert_eq!(
            knob_in(&rows, "PUNKTFUNK_444"),
            None,
            "at its default the reader decides"
        );
        assert_eq!(knob_in(&rows, "PUNKTFUNK_NOT_A_KNOB"), None);
    }

    #[test]
    fn an_env_enum_reads_in_the_spelling_the_console_shows() {
        let knob = |raw: &str| {
            let pairs = [("PUNKTFUNK_GAMESTREAM_ENCRYPT", raw)];
            knob_in(
                &resolve(&env_of(&pairs), &Map::new(), &[]),
                "PUNKTFUNK_GAMESTREAM_ENCRYPT",
            )
        };
        assert_eq!(knob("video_only").as_deref(), Some("video"));
        assert_eq!(knob("Off").as_deref(), Some("0"));
        assert_eq!(knob("supported").as_deref(), Some("supported"));
    }

    #[test]
    fn typed_rows_read_the_value_the_console_shows() {
        let file = obj(json!({"instant_replay_pause": "on", "pen": false}));
        let env = env_of(&[
            ("PUNKTFUNK_STEAM_GADGET", "auto"),
            ("PUNKTFUNK_SESSION_WATCH", "TRUE"),
            ("PUNKTFUNK_PAD_AUDIO", "no"),
            ("PUNKTFUNK_UPDATE_CHECK", " off "),
            ("PUNKTFUNK_DUALSENSE_USBIP", "Yes"),
        ]);
        let rows = resolve(&env, &file, &[]);
        let row_tri = |k: &str| row_tri_in(&rows, k);
        let row_bool = |k: &str| row_bool_in(&rows, k);
        assert_eq!(row_tri("PUNKTFUNK_INSTANT_REPLAY_PAUSE"), Some(true));
        assert_eq!(
            row_tri("PUNKTFUNK_STEAM_GADGET"),
            None,
            "auto defers to the host"
        );
        assert_eq!(row_tri("PUNKTFUNK_SESSION_WATCH"), Some(true));
        assert_eq!(row_tri("PUNKTFUNK_GAMESCOPE_BIND"), None);
        assert!(!row_bool("PUNKTFUNK_PEN") && !row_bool("PUNKTFUNK_PAD_AUDIO"));
        assert!(!row_bool("PUNKTFUNK_UPDATE_CHECK"));
        assert!(row_bool("PUNKTFUNK_DUALSENSE_USBIP"));
        assert!(row_bool("PUNKTFUNK_MDNS"), "a default is the row's own");
        let cfg = build(
            &env_of(&[
                ("PUNKTFUNK_GAMESCOPE_VRR", "false"),
                ("PUNKTFUNK_AUDIO_REDUNDANCY", "auto"),
            ]),
            &Map::new(),
            &[],
        )
        .config;
        assert!(!cfg.gamescope_vrr);
        assert_eq!(cfg.audio_redundancy, None);
    }

    /// A Bool or tri-state row reads through `row_bool`/`row_tri`, never a raw-string reader
    /// with a grammar of its own that the console does not share. A crate without this one
    /// (a client reading the same name) has no rows and is skipped.
    #[test]
    fn typed_rows_are_read_through_the_typed_readers() {
        let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")];
        let mut typed_calls = 0;
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                let manifest = std::fs::read_to_string(p.join("Cargo.toml"));
                if manifest.is_ok_and(|m| !m.contains("pf-host-config")) {
                    continue;
                }
                stack.extend(std::fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for (i, _) in text.match_indices("\"PUNKTFUNK_") {
                let name = text[i + 1..].split('"').next().unwrap();
                let call = text[..i].trim_end();
                let row = SETTINGS.iter().find(|s| s.env == name);
                let is_bool = row.is_some_and(|s| s.kind == registry::Kind::Bool);
                let is_tri = row.is_some_and(|s| s.kind == registry::TRI);
                let at = p.display();
                if call.ends_with("row_bool(") {
                    assert!(is_bool, "{at}: {name} is not a Bool row");
                    typed_calls += 1;
                } else if call.ends_with("row_tri(") {
                    assert!(is_tri, "{at}: {name} is not a tri-state row");
                    typed_calls += 1;
                } else if ["knob(", "on(", "val(", "flag("]
                    .iter()
                    .any(|c| call.ends_with(c))
                {
                    assert!(
                        !is_bool && !is_tri,
                        "{at}: read {name} with row_bool/row_tri"
                    );
                }
            }
        }
        assert!(typed_calls > 10, "the scan found the workspace");
    }

    #[test]
    fn only_restart_rows_are_pending() {
        let base = build(&env_of(&[]), &Map::new(), &[]);
        let changed = build(
            &env_of(&[]),
            &obj(json!({"gamestream": true, "max_fps": 60})),
            &[],
        );
        assert_eq!(pending_between(&base, &changed), vec!["gamestream"]);
        assert!(pending_between(&changed, &changed).is_empty());
    }

    #[test]
    fn config_fields_follow_the_rows() {
        let file = obj(json!({
            "clipboard": "text",
            "max_fps": 0,
            "host_name": "",
            "audio_voice_apps": ["Discord", "firefox"],
            "audio_voice_chat": "host",
        }));
        let s = build(&env_of(&[("PUNKTFUNK_MAX_FPS", "500")]), &file, &[]);
        let c = &s.config;
        assert_eq!(c.clipboard, crate::ClipboardPolicy::Text);
        assert_eq!(c.max_fps, Some(240));
        assert_eq!(c.host_name, None);
        // Listed apps join the built-in ones; a repeat is not added twice.
        assert_eq!(
            c.audio_voice_apps.len(),
            crate::DEFAULT_VOICE_APPS.len() + 1
        );
        assert_eq!(
            c.audio_voice_apps.last().map(String::as_str),
            Some("firefox")
        );
        assert_eq!(c.audio_voice_chat, crate::VoiceChatRoute::Host);
        assert!(c.ten_bit && c.four_four_four && !c.gamestream);
        let zero = build(&env_of(&[]), &file, &[]);
        assert_eq!(zero.config.max_fps, None, "0 is no limit");
        let empty = build(&env_of(&[]), &obj(json!({"audio_voice_apps": []})), &[]);
        assert_eq!(empty.config.audio_voice_apps, crate::DEFAULT_VOICE_APPS);
        let off = build(&env_of(&[]), &obj(json!({"vdisplay_hz_mult": 1})), &[]);
        assert_eq!(off.config.vdisplay_hz_mult, 1, "the console turns it off");
        for (raw, want) in [("0", 0), ("9", 4)] {
            let s = build(
                &env_of(&[("PUNKTFUNK_VDISPLAY_HZ_MULT", raw)]),
                &Map::new(),
                &[],
            );
            assert_eq!(s.config.vdisplay_hz_mult, want, "{raw}");
        }
    }
}
