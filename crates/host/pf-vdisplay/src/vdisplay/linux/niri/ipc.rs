//! niri's JSON IPC on `$NIRI_SOCKET`: one request line, one reply line, `{"Ok": …}` or
//! `{"Err": "…"}`.
//!
//! Requests are built here by hand rather than through upstream's `niri-ipc` crate: that crate
//! lacks the virtual-output requests, and every shape must parse on stock niri and on biri alike.
//! Reply types ignore fields they do not name, so either build's extras read cleanly.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// One request's budget. A healthy reply takes well under a millisecond; a wedged compositor
/// must not wedge the session thread that asked.
const BUDGET: Duration = Duration::from_secs(5);

/// Name the probe asks niri to remove. Nothing ever creates it.
const PROBE_NAME: &str = "punktfunk-probe";

/// niri answered `Err`. The text is niri's own, for logs and for [`classify_probe`].
#[derive(Debug)]
pub(crate) struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "niri refused the request: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// The live socket, resolved per call: a niri restart opens a new one.
fn socket() -> Result<String> {
    crate::session::niri_socket()
        .context("find the niri IPC socket (no NIRI_SOCKET, no niri.*.sock)")
}

/// The niri this host talks to, as its socket path: what a journal entry records as its
/// `instance`. Empty when none is found, which an undo reads as "not this one".
pub(crate) fn instance() -> String {
    socket().unwrap_or_default()
}

/// Send `request`; the `Ok` payload, or [`Refused`] carrying niri's error text.
pub(crate) fn request(request: &Value) -> Result<Value> {
    let path = socket()?;
    let mut stream = UnixStream::connect(&path).with_context(|| format!("connect {path}"))?;
    stream.set_read_timeout(Some(BUDGET))?;
    stream.set_write_timeout(Some(BUDGET))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line).context("write niri request")?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .context("read niri reply")?;
    parse_reply(&reply)
}

pub(crate) fn parse_reply(line: &str) -> Result<Value> {
    let mut v: Value = serde_json::from_str(line.trim())
        .with_context(|| format!("parse niri reply {:?}", line.trim()))?;
    if let Some(ok) = v.get_mut("Ok") {
        return Ok(ok.take());
    }
    if let Some(err) = v.get("Err").and_then(Value::as_str) {
        return Err(Refused(err.to_string()).into());
    }
    bail!("niri reply is neither Ok nor Err: {}", line.trim())
}

/// `Ok` payload `{ "<variant>": inner }` → `inner`.
fn variant(mut payload: Value, name: &str) -> Result<Value> {
    payload
        .get_mut(name)
        .map(Value::take)
        .with_context(|| format!("niri reply is not {name}: {payload}"))
}

/// Run one `Action`; niri answers `"Handled"`. Unknown output or window names are not errors to
/// niri, so a caller that cares reads the result back.
pub(crate) fn action(action: Value) -> Result<()> {
    request(&action_request(action)).map(drop)
}

pub(crate) fn action_request(action: Value) -> Value {
    json!({ "Action": action })
}

pub(crate) fn output_request(output: &str, action: Value) -> Value {
    json!({ "Output": { "output": output, "action": action } })
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct OutputMode {
    pub width: u32,
    pub height: u32,
    /// Millihertz.
    pub refresh_rate: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Logical {
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    pub scale: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Output {
    pub name: String,
    #[serde(default)]
    pub make: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub modes: Vec<OutputMode>,
    pub current_mode: Option<usize>,
    /// `None` while the output is off.
    pub logical: Option<Logical>,
}

impl Output {
    pub fn mode(&self) -> Option<&OutputMode> {
        self.modes.get(self.current_mode?)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Workspace {
    pub id: u64,
    pub idx: u8,
    pub name: Option<String>,
    pub output: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_focused: bool,
    pub active_window_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Window {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub pid: Option<i32>,
    pub workspace_id: Option<u64>,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub layout: WindowLayout,
}

/// The one layout field the host reads: the tile's size in logical pixels.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct WindowLayout {
    #[serde(default)]
    pub tile_size: (f64, f64),
}

pub(crate) fn version() -> Result<String> {
    let v = variant(request(&json!("Version"))?, "Version")?;
    Ok(v.as_str().unwrap_or_default().to_string())
}

/// Every output niri knows, off ones included, sorted by name.
pub(crate) fn outputs() -> Result<Vec<Output>> {
    let map = variant(request(&json!("Outputs"))?, "Outputs")?;
    let map: std::collections::BTreeMap<String, Output> =
        serde_json::from_value(map).context("decode niri Outputs")?;
    Ok(map.into_values().collect())
}

pub(crate) fn workspaces() -> Result<Vec<Workspace>> {
    let v = variant(request(&json!("Workspaces"))?, "Workspaces")?;
    serde_json::from_value(v).context("decode niri Workspaces")
}

pub(crate) fn windows() -> Result<Vec<Window>> {
    let v = variant(request(&json!("Windows"))?, "Windows")?;
    serde_json::from_value(v).context("decode niri Windows")
}

/// Configure `output` (`Off`, `On`, `{"Scale":…}`, `{"CustomMode":…}`). An output niri does not
/// know is an error here, not niri's silent `OutputWasMissing`.
pub(crate) fn output_action(output: &str, action: Value) -> Result<()> {
    let reply = request(&output_request(output, action))?;
    match variant(reply, "OutputConfigChanged")?.as_str() {
        Some("OutputWasMissing") => bail!("niri has no output {output}"),
        _ => Ok(()),
    }
}

/// Create a virtual output named `name`. niri takes the refresh as whole hertz.
pub(crate) fn create_virtual_output(
    name: &str,
    width: u32,
    height: u32,
    hz: u32,
) -> Result<String> {
    let reply = request(&json!({
        "CreateVirtualOutput": {
            "width": width,
            "height": height,
            "refresh_rate": hz,
            "name": name,
        }
    }))?;
    let made = variant(reply, "VirtualOutputCreated")?;
    Ok(made.as_str().unwrap_or(name).to_string())
}

pub(crate) fn remove_virtual_output(name: &str) -> Result<()> {
    request(&json!({ "RemoveVirtualOutput": { "name": name } })).map(drop)
}

/// What this niri can do for a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Capability {
    /// Serves `CreateVirtualOutput` (biri, or a niri that merged it).
    VirtualOutputs,
    /// Stock niri: the request does not exist.
    NoVirtualOutputs,
    /// The request exists but this instance cannot honour it (nested in another compositor).
    Unsupported(String),
}

/// Ask niri to remove an output that never exists. Stock niri cannot parse the request; a niri
/// with virtual outputs answers "not found". Nothing is created or removed either way.
pub(crate) fn probe() -> Result<Capability> {
    match request(&json!({ "RemoveVirtualOutput": { "name": PROBE_NAME } })) {
        Ok(_) => Ok(Capability::VirtualOutputs),
        Err(e) => match e.downcast::<Refused>() {
            Ok(Refused(text)) => Ok(classify_probe(&text)),
            Err(e) => Err(e),
        },
    }
}

/// Stock niri flattens its parse error to exactly this text; any other refusal means the request
/// parsed, so the verb exists.
pub(crate) fn classify_probe(err: &str) -> Capability {
    if err.trim() == "error parsing request" {
        Capability::NoVirtualOutputs
    } else if err.contains("not supported") {
        Capability::Unsupported(err.to_string())
    } else {
        Capability::VirtualOutputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_unwrap_ok_and_carry_err_text() {
        assert_eq!(
            parse_reply(r#"{"Ok":"Handled"}"#).unwrap(),
            json!("Handled")
        );
        let e = parse_reply(r#"{"Err":"error parsing request"}"#).unwrap_err();
        assert_eq!(e.downcast::<Refused>().unwrap().0, "error parsing request");
        assert!(parse_reply("{}").is_err());
    }

    /// Strings recorded on niri 26.04 (stock) and biri; a change here is a change in niri.
    #[test]
    fn the_probe_tells_stock_niri_from_biri() {
        assert_eq!(
            classify_probe("error parsing request"),
            Capability::NoVirtualOutputs
        );
        assert_eq!(
            classify_probe("virtual output 'punktfunk-probe' not found"),
            Capability::VirtualOutputs
        );
        assert!(matches!(
            classify_probe("virtual outputs are not supported on the Winit backend"),
            Capability::Unsupported(_)
        ));
    }

    /// Stock 26.04 lacks `is_hidden` and `max_bpc`, which biri sends; both must decode.
    #[test]
    fn replies_from_either_build_decode() {
        let ws: Vec<Workspace> = serde_json::from_value(json!([
            {"id":1,"idx":1,"name":null,"output":"HDMI-A-1","is_urgent":false,"is_active":true,
             "is_focused":true,"active_window_id":null},
            {"id":2,"idx":2,"name":null,"output":"PF-1-1","is_urgent":false,"is_active":true,
             "is_focused":false,"active_window_id":7,"is_hidden":false}
        ]))
        .unwrap();
        assert_eq!(ws[1].active_window_id, Some(7));
        let out: Output = serde_json::from_value(json!({
            "name":"PF-1-1","make":"niri","model":"virtual","serial":null,"physical_size":null,
            "modes":[{"width":2560,"height":1600,"refresh_rate":120000,"is_preferred":true}],
            "current_mode":0,"is_custom_mode":false,"vrr_supported":false,"vrr_enabled":false,
            "logical":{"x":1920,"y":0,"width":2560,"height":1600,"scale":1.0,"transform":"Normal"},
            "max_bpc":null
        }))
        .unwrap();
        assert_eq!(out.mode().unwrap().refresh_rate, 120_000);
        assert_eq!(out.logical.unwrap().x, 1920);
    }
}
