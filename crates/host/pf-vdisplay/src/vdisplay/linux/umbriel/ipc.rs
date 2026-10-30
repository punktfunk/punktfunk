//! Umbriel's JSON IPC on `$UMBRIEL_SOCKET`: one request line, one reply line `{"ok":…}` or
//! `{"err":"…"}`, then the server closes the connection.
//!
//! Umbriel reads two request shapes. Builds from 2026-10-29 take `args`, an array of words, and
//! typed `action` requests. Older builds take `arg`, one string, and `msg`. Distributions ship
//! both, so every request here serves both.
//!
//! Reply types ignore fields they do not name, and every optional field defaults: the project
//! is young and its shapes grow.

use super::UmbrielIpcMismatch;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// One request's budget. Umbriel drops a request after 1 s itself; a wedged compositor must
/// not wedge the session thread that asked.
const BUDGET: Duration = Duration::from_secs(2);

/// Name the probe asks Umbriel to destroy. Nothing ever creates it.
const PROBE_NAME: &str = "punktfunk-probe";

/// Umbriel's answer to a request it could not parse.
const MALFORMED: &str = "malformed request";

/// Umbriel answered `err`. The text is Umbriel's own, for logs and for [`unknown_command`].
#[derive(Debug)]
pub(crate) struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "umbriel refused the request: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// The live socket, resolved per call: an Umbriel restart opens a new one.
fn socket() -> Result<String> {
    crate::session::umbriel_socket()
        .context("find the Umbriel IPC socket (no UMBRIEL_SOCKET, no umbriel-*.sock)")
}

/// The Umbriel this host talks to, as its socket path: what a journal entry records as its
/// `instance`. Empty when none is found, which an undo reads as "not this one".
pub(crate) fn instance() -> String {
    socket().unwrap_or_default()
}

/// The request as it goes on the wire, `args` under both keys: each build reads its own and
/// ignores the other. Built by hand: the key order is part of the pin.
pub(crate) fn request_line(cmd: &str, args: &[&str]) -> String {
    let quote = |s: &str| Value::String(s.to_string()).to_string();
    if args.is_empty() {
        return format!("{{\"cmd\":{}}}\n", quote(cmd));
    }
    format!(
        "{{\"cmd\":{},\"arg\":{},\"args\":{}}}\n",
        quote(cmd),
        quote(&args.join(" ")),
        Value::from(args)
    )
}

/// Send one request line; the `ok` payload, or [`Refused`] carrying Umbriel's error text.
fn exchange(line: &str) -> Result<Value> {
    let path = socket()?;
    let mut stream = UnixStream::connect(&path).with_context(|| format!("connect {path}"))?;
    stream.set_read_timeout(Some(BUDGET))?;
    stream.set_write_timeout(Some(BUDGET))?;
    let what = line.trim();
    stream
        .write_all(line.as_bytes())
        .with_context(|| format!("write umbriel request {what}"))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .with_context(|| format!("read umbriel reply to {what}"))?;
    parse_reply(&reply)
}

pub(crate) fn request(cmd: &str, args: &[&str]) -> Result<Value> {
    exchange(&request_line(cmd, args))
}

pub(crate) fn parse_reply(line: &str) -> Result<Value> {
    let mut v: Value = serde_json::from_str(line.trim())
        .with_context(|| format!("parse umbriel reply {:?}", line.trim()))?;
    if let Some(ok) = v.get_mut("ok") {
        return Ok(ok.take());
    }
    if let Some(err) = v.get("err").and_then(Value::as_str) {
        return Err(Refused(err.to_string()).into());
    }
    bail!("umbriel reply is neither ok nor err: {}", line.trim())
}

/// One action in both shapes.
pub(crate) struct Action {
    /// The whole `action` request.
    pub typed: Value,
    /// The `msg` argument.
    pub word: String,
}

/// Run one action. Umbriel answers `ok` even when the action matched nothing, so a caller
/// that cares reads the result back.
pub(crate) fn msg(action: &Action) -> Result<()> {
    send_action(action, exchange)
}

/// The `action` request, then `msg` on a build that has no such command.
fn send_action(action: &Action, mut send: impl FnMut(&str) -> Result<Value>) -> Result<()> {
    match send(&format!("{}\n", action.typed)) {
        Err(e)
            if e.downcast_ref::<Refused>()
                .is_some_and(|r| unknown_command(&r.0)) =>
        {
            send(&request_line("msg", &[&action.word])).map(drop)
        }
        reply => reply.map(drop),
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct OutputMode {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub refresh_mhz: u32,
    #[serde(default)]
    pub current: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Output {
    pub name: String,
    #[serde(default)]
    pub make: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub powered: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub position: Option<Position>,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub modes: Vec<OutputMode>,
}

impl Output {
    pub fn mode(&self) -> Option<&OutputMode> {
        self.modes.iter().find(|m| m.current)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Workspace {
    /// 1-based position on its output: what `workspace-switch` takes, never the `id`.
    pub index: u32,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub occupied: bool,
    #[serde(default)]
    pub window_count: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Window {
    /// The ext-foreign-toplevel identifier.
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub output: Option<String>,
    /// A string or a number, depending on the build: [`super::desktop`] spells either.
    #[serde(default)]
    pub workspace: Value,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub focused: bool,
}

/// Every output Umbriel knows, disabled ones included.
pub(crate) fn outputs() -> Result<Vec<Output>> {
    serde_json::from_value(request("outputs", &[])?).context("decode umbriel outputs")
}

pub(crate) fn workspaces() -> Result<Vec<Workspace>> {
    serde_json::from_value(request("workspaces", &[])?).context("decode umbriel workspaces")
}

pub(crate) fn windows() -> Result<Vec<Window>> {
    serde_json::from_value(request("windows", &[])?).context("decode umbriel windows")
}

/// `<W>x<H>@<Hz>`: the refresh is always given, or Umbriel paces at its default.
pub(crate) fn create_mode(width: u32, height: u32, hz: u32) -> String {
    format!("{width}x{height}@{hz}")
}

/// Create virtual output `name`. It exists and is enabled when this returns.
pub(crate) fn create_output(name: &str, width: u32, height: u32, hz: u32) -> Result<()> {
    request("output-create", &[name, &create_mode(width, height, hz)]).map(drop)
}

/// Destroy virtual output `name`. Umbriel moves its windows to another output itself.
pub(crate) fn destroy_output(name: &str) -> Result<()> {
    request("output-destroy", &[name]).map(drop)
}

/// Whether this Umbriel can create outputs: asks it to destroy one that never exists.
pub(crate) fn has_virtual_outputs() -> Result<bool> {
    reads_as_virtual_outputs(destroy_output(PROBE_NAME))
}

/// The probe's reply. A build without the verb refuses the command itself, and one that parses
/// neither shape fails as [`UmbrielIpcMismatch`]. Any other refusal means the verb exists.
fn reads_as_virtual_outputs(reply: Result<()>) -> Result<bool> {
    let Err(e) = reply else { return Ok(true) };
    match e.downcast::<Refused>() {
        Ok(Refused(text)) if text == MALFORMED => Err(UmbrielIpcMismatch.into()),
        Ok(Refused(text)) => Ok(!unknown_command(&text)),
        Err(e) => Err(e),
    }
}

fn unknown_command(err: &str) -> bool {
    err.to_ascii_lowercase().contains("unknown command")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Byte for byte: the server parses one line, keys in this order.
    #[test]
    fn requests_are_pinned_to_the_wire_line() {
        assert_eq!(
            request_line("output-create", &["PF-1-0", &create_mode(2560, 1600, 120)]),
            "{\"cmd\":\"output-create\",\"arg\":\"PF-1-0 2560x1600@120\",\
             \"args\":[\"PF-1-0\",\"2560x1600@120\"]}\n"
        );
        assert_eq!(request_line("outputs", &[]), "{\"cmd\":\"outputs\"}\n");
        assert_eq!(
            request_line("msg", &["window-focus:a\"b"]),
            "{\"cmd\":\"msg\",\"arg\":\"window-focus:a\\\"b\",\
             \"args\":[\"window-focus:a\\\"b\"]}\n"
        );
    }

    /// The lines one action sends, each of `replies` answering in turn, and how it ended.
    fn run_action(replies: Vec<Result<Value>>) -> (Vec<String>, Result<()>) {
        let action = Action {
            typed: json!({"cmd":"action","action":"window-focus","window":"t1"}),
            word: "window-focus:t1".into(),
        };
        let (mut sent, mut replies) = (Vec::new(), replies.into_iter());
        let end = send_action(&action, |line| {
            sent.push(line.to_string());
            replies.next().expect("one reply per request")
        });
        (sent, end)
    }

    fn refused(text: &str) -> Result<Value> {
        Err(Refused(text.to_string()).into())
    }

    #[test]
    fn a_build_with_actions_gets_only_the_typed_request() {
        let (sent, end) = run_action(vec![Ok(Value::Null)]);
        assert!(end.is_ok());
        assert_eq!(sent.len(), 1);
        let typed: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(typed["action"], "window-focus");
        assert!(sent[0].ends_with('\n'));
    }

    #[test]
    fn a_build_without_actions_gets_msg() {
        let (sent, end) = run_action(vec![refused("unknown command: action"), Ok(Value::Null)]);
        assert!(end.is_ok());
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1], request_line("msg", &["window-focus:t1"]));
    }

    /// Only a missing command falls back: the action's own refusal is the one to report.
    #[test]
    fn a_refused_action_stays_refused() {
        let (sent, end) = run_action(vec![refused("unknown action: window-focus")]);
        assert_eq!(sent.len(), 1);
        let e = end.unwrap_err().downcast::<Refused>().unwrap();
        assert_eq!(e.0, "unknown action: window-focus");
    }

    #[test]
    fn the_probe_reads_every_answer() {
        let said = |text: &str| Err(Refused(text.to_string()).into());
        assert!(reads_as_virtual_outputs(Ok(())).unwrap());
        assert!(reads_as_virtual_outputs(said("unknown output: x")).unwrap());
        assert!(!reads_as_virtual_outputs(said("unknown command: output-destroy")).unwrap());
        let shape = reads_as_virtual_outputs(said("malformed request")).unwrap_err();
        assert!(shape.is::<UmbrielIpcMismatch>());
        let dead = reads_as_virtual_outputs(Err(anyhow::anyhow!("connect"))).unwrap_err();
        assert!(!dead.is::<UmbrielIpcMismatch>());
    }

    #[test]
    fn replies_unwrap_ok_and_carry_err_text() {
        assert_eq!(parse_reply(r#"{"ok":"PF-1-0"}"#).unwrap(), json!("PF-1-0"));
        assert_eq!(parse_reply("{\"ok\":null}\n").unwrap(), Value::Null);
        let e = parse_reply(r#"{"err":"no focused window"}"#).unwrap_err();
        assert_eq!(e.downcast::<Refused>().unwrap().0, "no focused window");
        assert!(parse_reply("{}").is_err());
    }

    #[test]
    fn only_an_unknown_command_means_an_umbriel_too_old() {
        assert!(unknown_command("unknown command: output-destroy"));
        assert!(!unknown_command("not a virtual output: punktfunk-probe"));
        assert!(!unknown_command("unknown output: punktfunk-probe"));
    }

    #[test]
    fn an_outputs_reply_decodes() {
        let outs: Vec<Output> = serde_json::from_value(json!([
            {"name":"DP-1","description":"ACME U2720Q","make":"ACME","model":"U2720Q",
             "serial":null,"config_name":"DP-1","physical_size":{"width_mm":600,"height_mm":340},
             "enabled":true,"powered":true,"focused":false,"active_workspace":1,
             "position":{"x":0,"y":0},"logical_size":{"width":1920,"height":1080},
             "usable_area":{"x":0,"y":0,"width":1920,"height":1050},"transform":"normal",
             "scale":1.0,"adaptive_sync":false,"hdr_active":false,"direct_scanout":{"active":false},
             "modes":[{"width":1920,"height":1080,"refresh_mhz":60000,"preferred":true,"current":true}]},
            {"name":"PF-1-0","make":"headless","model":"headless","enabled":true,"powered":true,
             "focused":true,"position":{"x":1920.0,"y":0.0},"scale":1.0,
             "modes":[{"width":2560,"height":1600,"refresh_mhz":120000,"preferred":false,"current":true}]},
            {"name":"HDMI-A-1","enabled":false,"powered":false,"modes":[]}
        ]))
        .unwrap();
        assert_eq!(outs[0].mode().unwrap().refresh_mhz, 60_000);
        assert_eq!(outs[1].position.as_ref().unwrap().x, 1920.0);
        assert!(outs[1].focused);
        assert!(!outs[2].enabled && outs[2].mode().is_none());
    }

    #[test]
    fn windows_and_workspaces_replies_decode() {
        let ws: Vec<Workspace> = serde_json::from_value(json!([
            {"id":"PF-1-0:1","name":"1","index":1,"output":"PF-1-0","active":true,"focused":true,
             "occupied":true,"window_count":2,"focused_window":"t1"}
        ]))
        .unwrap();
        assert_eq!((ws[0].index, ws[0].window_count), (1, 2));
        let wins: Vec<Window> = serde_json::from_value(json!([
            {"id":"t1","workspace":"PF-1-0:1","app_id":"steam_app_570","title":"Dota 2",
             "output":"PF-1-0","fullscreen":true,"focused":true,"active":true,"pid":4242,
             "x":0,"y":0,"w":2560,"h":1600}
        ]))
        .unwrap();
        assert_eq!(wins[0].pid, Some(4242));
        assert!(wins[0].fullscreen);
    }
}
