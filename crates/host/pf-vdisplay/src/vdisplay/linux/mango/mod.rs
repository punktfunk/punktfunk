//! mango backend: one named headless output per [`create`](VirtualDisplay::create), made over
//! mango's IPC socket, set to the client's mode over `zwlr_output_manager_v1`, and captured
//! directly over `ext-image-copy-capture-v1`. There is no PipeWire node (`node_id` 0), so a
//! session needs a GPU encoder.
//!
//! mango destroys headless outputs only all at once (`destroy_all_virtual_output`), which would
//! take a user's own with ours. Teardown disables ours, the next `create` re-enables a disabled
//! `PF-*` output before it makes another, and the last teardown destroys them all when no other
//! headless output exists. Names are Hyprland's ([`head_name`]).
//! The desk around the output (tags, `exclusive`, the darken, windows) is [`desktop`].

pub(crate) mod desktop;

use super::{Mode, VirtualDisplay, VirtualOutput};
use crate::hyprland::{head_name, is_managed_output};
use crate::output_mgmt::{self, HeadState, Set};
use crate::topology_journal::{self as journal, Undo};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// One request: mango answers from its event loop, so a busy compositor is slow, not stuck.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a created output may take to reach the head list.
const LIST_WAIT: Duration = Duration::from_millis(1500);

/// The outputs this host streams or keeps, by name. Held from pick to create, so two sessions
/// never take one parked output, and through a teardown's tidy.
static LIVE: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

fn live() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

fn socket() -> Result<String> {
    crate::session::mango_socket().context("no running mango IPC socket (MANGO_INSTANCE_SIGNATURE)")
}

/// The socket path names the compositor's pid: an output of another mango is not ours.
fn instance() -> String {
    socket().unwrap_or_default()
}

/// One request line. mango answers one JSON line and closes; a line break would start another
/// request.
fn request(line: &str) -> Result<serde_json::Value> {
    if line.contains(['\n', '\r']) {
        bail!("a mango request is one line: {line:?}");
    }
    let path = socket()?;
    let mut s = UnixStream::connect(&path).with_context(|| format!("connect {path}"))?;
    s.set_read_timeout(Some(IPC_TIMEOUT))?;
    s.set_write_timeout(Some(IPC_TIMEOUT))?;
    s.write_all(format!("{line}\n").as_bytes())
        .context("send")?;
    let mut reply = String::new();
    s.read_to_string(&mut reply).context("read the reply")?;
    serde_json::from_str(reply.trim()).with_context(|| format!("parse {:?}", reply.trim()))
}

/// One argument of a request. mango splits on commas and blanks, so a name holding either would
/// become another argument.
fn word(s: &str) -> Result<&str> {
    if s.is_empty() || s.contains(|c: char| c == ',' || c.is_whitespace() || c.is_control()) {
        bail!("{s:?} is not one mango argument");
    }
    Ok(s)
}

/// A monitor argument. mango matches one as an unanchored regex, so a bare `PF-1` would also
/// take `PF-12`.
fn monitor_spec(name: &str) -> String {
    let mut spec = String::from("^");
    for c in name.chars() {
        if r"\^$.|?*+()[]{}".contains(c) {
            spec.push('\\');
        }
        spec.push(c);
    }
    spec.push('$');
    spec
}

/// `dispatch <func>[,<arg>] [client,<id>]`.
fn dispatch_line(func: &str, arg: Option<&str>, client: Option<u32>) -> Result<String> {
    let mut line = format!("dispatch {}", word(func)?);
    if let Some(arg) = arg {
        line.push(',');
        line.push_str(word(arg)?);
    }
    if let Some(id) = client {
        line.push_str(&format!(" client,{id}"));
    }
    Ok(line)
}

/// Run a dispatcher, on window `client` when given. mango answers `success` for any function it
/// knows, whatever the function then did, so a step that matters is read back.
fn dispatch(func: &str, arg: Option<&str>, client: Option<u32>) -> Result<()> {
    let reply = request(&dispatch_line(func, arg, client)?)?;
    if reply.get("success").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    bail!("mango refused {func}: {reply}")
}

/// A mango session is running for this user.
pub(crate) fn is_available() -> bool {
    crate::session::mango_socket().is_some()
}

/// mango 0.17.1 is the first to name the output it creates. An older one makes a `HEADLESS-N`
/// the host never finds, and that output stays lit.
const MIN_VERSION: (u32, u32, u32) = (0, 17, 1);

/// The IPC socket answers, from a mango that names its outputs. A version it cannot read passes.
pub(crate) fn probe() -> Result<()> {
    let reply = request("get version").context("ask mango over its IPC socket")?;
    let v = reply.get("version").and_then(|v| v.as_str()).unwrap_or("");
    match parse_version(v) {
        Some(have) if have < MIN_VERSION => {
            bail!("mango {v} can't name a virtual output; update it to 0.17.1 or newer")
        }
        _ => Ok(()),
    }
}

/// The first three numbers of `0.18.6`, `v0.18.6` or `0.18.6-3-gabc`.
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut n = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(str::parse::<u32>);
    Some((n.next()?.ok()?, n.next()?.ok()?, n.next()?.ok()?))
}

/// Every head mango lists, for the monitor picker, the mirror pin and the journal. A head mango
/// put to sleep is listed lit ([`desktop`]).
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let mut out = desktop::monitors(&desktop::desk()?);
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Stream a head mango already has (the monitor-mirror pin): the host captures it directly by
/// name, so there is nothing to start here.
pub(crate) fn stream_existing_output(connector: &str) -> crate::mirror::MirrorStream {
    tracing::info!(output = %connector, "mango: mirroring by direct capture");
    crate::mirror::MirrorStream {
        node_id: 0,
        remote_fd: None,
        cursor_mode: None,
        keepalive: Box::new(()),
    }
}

/// Focus `name`: new windows open on the focused output.
pub(crate) fn focus_output(name: &str) {
    if let Err(e) = dispatch("focusmon", Some(&monitor_spec(name)), None) {
        tracing::warn!(output = %name, error = %format!("{e:#}"), "mango: output not focused");
    }
}

/// `PF-<pid>-<n>`, the name of an output made for a device without a slot.
fn is_counter_name(name: &str) -> bool {
    is_managed_output(name) && name.matches('-').count() == 2
}

/// The output `create` streams, and whether mango lists it already. A disabled `PF-*` output is
/// ours and free. `PF-<slot>` for a device with a slot; else a disabled counter-named output;
/// else a new name.
fn pick_output(slot: Option<u32>, heads: &[HeadState]) -> (String, bool) {
    if let Some(standing) = slot.map(|s| format!("PF-{s}")) {
        match heads.iter().find(|h| h.name == standing) {
            None => return (standing, false),
            Some(h) if !h.enabled => return (standing, true),
            Some(_) => {}
        }
    }
    if let Some(free) = heads
        .iter()
        .find(|h| !h.enabled && is_counter_name(&h.name))
    {
        return (free.name.clone(), true);
    }
    (head_name(None, |_| false), false)
}

fn wait_listed(name: &str) -> Result<()> {
    let deadline = Instant::now() + LIST_WAIT;
    loop {
        if output_mgmt::heads()?.iter().any(|h| h.name == name) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("mango listed no {name} within {LIST_WAIT:?}: is its headless backend running?")
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Turn `name` off. An output mango no longer lists, or lists off, needs nothing.
fn disable(name: &str) -> Result<()> {
    match output_mgmt::heads()?.iter().find(|h| h.name == name) {
        Some(h) if h.enabled => output_mgmt::configure(name, Set::Off, None),
        _ => Ok(()),
    }
}

/// wlroots describes a headless output as `Headless output N`, renamed or not.
fn is_headless(h: &HeadState) -> bool {
    h.description.starts_with("Headless output")
}

/// Parked outputs of ours that are lit: a config reload lights every output no rule names.
fn strays<'a>(live: &BTreeSet<String>, heads: &'a [HeadState]) -> Vec<&'a str> {
    heads
        .iter()
        .filter(|h| h.enabled && is_headless(h) && is_managed_output(&h.name))
        .filter(|h| !live.contains(&h.name))
        .map(|h| h.name.as_str())
        .collect()
}

/// Nothing live, and every headless output is a parked one of ours: destroying them all, the
/// only removal mango has, takes nothing else.
fn all_parked(live: &BTreeSet<String>, heads: &[HeadState]) -> bool {
    let mut headless = heads.iter().filter(|h| is_headless(h)).peekable();
    live.is_empty() && headless.peek().is_some() && headless.all(|h| is_managed_output(&h.name))
}

/// Turn the [`strays`] off. Whether there were any.
fn park_strays(live: &BTreeSet<String>, heads: &[HeadState]) -> bool {
    let strays = strays(live, heads);
    for name in &strays {
        match output_mgmt::configure(name, Set::Off, None) {
            Ok(()) => tracing::info!(output = %name, "mango: parked output lit again, turned off"),
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "mango: parked output lit again, not turned off"
            ),
        }
    }
    !strays.is_empty()
}

/// Turn `name` off, then tidy: destroy every parked output once nothing is live and nothing
/// else is headless, else turn off the strays. Windows leave a shared output first, so mango
/// remembers the desk for them: re-enabled for another device, it pulls none of them back.
fn retire(name: &str) -> Result<()> {
    let mut live = live();
    live.remove(name);
    if is_counter_name(name) {
        desktop::evict(name);
    }
    let off = disable(name);
    let heads = output_mgmt::heads().unwrap_or_default();
    if all_parked(&live, &heads) {
        match dispatch("destroy_all_virtual_output", None, None) {
            Ok(()) => tracing::info!("mango: destroyed the parked virtual outputs"),
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "mango: parked virtual outputs not destroyed"
            ),
        }
    } else {
        park_strays(&live, &heads);
    }
    off
}

/// One virtual output, journaled before it exists. Drop retires it and settles the entry.
struct OutputGuard {
    name: String,
    seq: u64,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match retire(&self.name) {
            Ok(()) => {
                journal::done(self.seq);
                tracing::info!(output = %self.name, "mango: virtual output off")
            }
            Err(e) => tracing::warn!(
                output = %self.name, error = %format!("{e:#}"),
                "mango: virtual output not turned off"
            ),
        }
    }
}

/// Create `name` unless mango lists it, then set it to `mode` at `mhz`.
fn bring_up(name: &str, listed: bool, mode: Mode, mhz: u32) -> Result<()> {
    if !listed {
        dispatch("create_virtual_output", Some(name), None)
            .context("create a mango virtual output")?;
        wait_listed(name)?;
    }
    output_mgmt::configure(name, Set::Custom(mode.width, mode.height, mhz), None)
        .with_context(|| format!("set {name} to {}x{}@{mhz}mHz", mode.width, mode.height))
}

/// A head of mango's Wayland or X11 backend: mango runs nested, and makes a window for a virtual
/// output instead of a screen.
fn nested_head(heads: &[HeadState]) -> Option<&HeadState> {
    heads.iter().find(|h| {
        h.description.starts_with("Wayland output") || h.description.starts_with("X11 output")
    })
}

#[derive(Default)]
pub struct MangoDisplay {
    /// Whose display this is, for the per-device topology resolved inside `create`.
    client_fp: Option<[u8; 32]>,
    /// The session's topology ([`VirtualDisplay::set_topology`]).
    topology: Option<crate::policy::Topology>,
    /// The identity slot `create` resolves for `client_fp`, peeked by `set_client_identity`
    /// before it: the owner key of this output once kept.
    last_slot: Option<u32>,
    /// Journal seqs of the heads `exclusive` turned off, across every `create` of this instance.
    /// The registry takes them; [`Drop`] runs what a create that never reached the pool left.
    pending_undo: Vec<u64>,
}

impl Drop for MangoDisplay {
    fn drop(&mut self) {
        let seqs = std::mem::take(&mut self.pending_undo);
        journal::apply(&seqs, self);
    }
}

impl VirtualDisplay for MangoDisplay {
    fn name(&self) -> &'static str {
        "mango"
    }

    fn heads(&self) -> Result<Vec<crate::monitors::PhysicalMonitor>> {
        list_monitors()
    }

    /// Direct capture paints mango's pointer into the frames and has no cursor metadata.
    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        Some(crate::PortalCursorMode::Embedded)
    }

    /// Called before every `create`; peeks the slot `create` will resolve, as on Umbriel.
    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
        let default = crate::policy::Identity::Shared;
        self.last_slot = crate::identity::peek_slot(fingerprint, (0, 0), default);
    }

    fn set_topology(&mut self, topology: Option<crate::policy::Topology>) {
        self.topology = topology;
    }

    fn topology(&self) -> Option<crate::policy::Topology> {
        Some(crate::create_topology(self.topology, self.client_fp))
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn take_undo_seqs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_undo)
    }

    fn undo(&mut self, undo: &Undo) -> Result<()> {
        desktop::undo(undo)
    }

    /// A keep-alive reconnect reuses the kept output by name, for the device that owns it.
    fn accepts_kept(&self, identity_slot: Option<u32>, output_name: Option<&str>) -> bool {
        output_name.is_some() && crate::identity::may_reuse(identity_slot, self.client_fp)
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        probe()?;
        self.last_slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        let mhz = mode.millihz().max(1000);
        let (output, reused, made) = {
            let mut live = live();
            let mut heads = output_mgmt::heads().context("list mango's outputs")?;
            if let Some(h) = nested_head(&heads) {
                bail!(
                    "mango runs nested ({}): its virtual outputs are windows, not screens",
                    h.name
                );
            }
            if park_strays(&live, &heads) {
                heads = output_mgmt::heads().context("list mango's outputs")?;
            }
            let (name, listed) = pick_output(self.last_slot, &heads);
            let seq = journal::push(
                "mango",
                Undo::RemoveOutput {
                    name: name.clone(),
                    instance: instance(),
                },
            );
            live.insert(name.clone());
            // Owned before the request: an error drops the guard, which turns off what the
            // request made.
            let output = OutputGuard { name, seq };
            let made = bring_up(&output.name, listed, mode, mhz);
            (output, listed, made)
        };
        // An error drops the guard here, outside the lock its teardown takes.
        made?;
        let name = output.name.clone();
        focus_output(&name);
        // Last: nothing fails after this, so no path turns heads off and then unwinds.
        if crate::backend::exclusive_wanted("mango", self.topology, self.client_fp) {
            desktop::disable_other_heads(&name, &mut self.pending_undo);
        }
        tracing::info!(
            output = %name, w = mode.width, h = mode.height, mhz, reused,
            "mango virtual output ready"
        );
        let mut out = VirtualOutput::owned(
            0,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(output),
        );
        out.refresh_mhz = Some(mhz);
        // Direct capture and absolute input find the output by its `wl_output` name.
        out.output_name = Some(name);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(name: &str, enabled: bool) -> HeadState {
        HeadState {
            name: name.into(),
            enabled,
            ..HeadState::default()
        }
    }

    /// A device's own name first, a disabled counter output next, a new one last. A lit output
    /// and a user's own disabled head are never taken.
    #[test]
    fn a_disabled_output_of_ours_is_taken_before_a_new_one() {
        let pick = |slot, heads: &[HeadState]| pick_output(slot, heads);
        assert_eq!(pick(Some(3), &[]), ("PF-3".into(), false));
        assert_eq!(pick(Some(3), &[head("PF-3", false)]), ("PF-3".into(), true));
        let heads = [
            head("DP-1", false),
            head("PF-office", false),
            head("PF-3", true),
            head("PF-77-2", true),
            head("PF-77-1", false),
        ];
        assert_eq!(pick(Some(3), &heads), ("PF-77-1".into(), true));
        assert_eq!(pick(None, &heads), ("PF-77-1".into(), true));
        let (fresh, listed) = pick(None, &heads[..4]);
        assert!(is_counter_name(&fresh) && !listed, "{fresh}");
        assert!(!heads.iter().any(|h| h.name == fresh));
    }

    /// 0.17.1 is the floor; a version string that is not three numbers passes the probe.
    #[test]
    fn a_version_reads_its_first_three_numbers() {
        assert_eq!(parse_version("0.18.6"), Some((0, 18, 6)));
        assert_eq!(parse_version("v0.17.1-3-gabc"), Some((0, 17, 1)));
        assert_eq!(parse_version("0.17"), None);
        assert_eq!(parse_version(""), None);
        assert!(parse_version("0.17.0").is_some_and(|v| v < MIN_VERSION));
        assert!(parse_version("0.17.1").is_some_and(|v| v >= MIN_VERSION));
    }

    /// mango splits a dispatch on commas and blanks: a name holding either is refused, never sent.
    #[test]
    fn a_dispatch_line_carries_one_word_per_argument() {
        let line = |f, a, c| dispatch_line(f, a, c).unwrap();
        assert_eq!(
            line("focusmon", Some("PF-3"), None),
            "dispatch focusmon,PF-3"
        );
        assert_eq!(line("focusid", None, Some(4)), "dispatch focusid client,4");
        assert_eq!(
            line("tagmon", Some("DP-1"), Some(4)),
            "dispatch tagmon,DP-1 client,4"
        );
        for bad in ["", "PF-3,spawn", "PF 3", "PF-3\nget", "DP\t1"] {
            assert!(
                dispatch_line("focusmon", Some(bad), None).is_err(),
                "{bad:?}"
            );
        }
    }

    /// mango matches a monitor argument as an unanchored regex.
    #[test]
    fn a_monitor_argument_names_one_monitor_exactly() {
        assert_eq!(monitor_spec("PF-1"), "^PF-1$");
        assert_eq!(monitor_spec("DP.1(a)"), r"^DP\.1\(a\)$");
    }

    fn headless(name: &str, enabled: bool) -> HeadState {
        HeadState {
            description: "Headless output 4".into(),
            ..head(name, enabled)
        }
    }

    /// A lit parked output nobody streams goes off; all are destroyed only with nothing live and
    /// no headless output of the user's.
    #[test]
    fn parked_outputs_are_destroyed_only_when_nothing_else_is_headless() {
        let live: BTreeSet<String> = ["PF-3".to_string()].into();
        let none = BTreeSet::new();
        let heads = [
            head("DP-1", true),
            headless("PF-3", true),
            headless("PF-77-1", true),
            headless("PF-77-2", false),
        ];
        assert_eq!(strays(&live, &heads), ["PF-77-1"]);
        assert!(!all_parked(&live, &heads));
        assert!(all_parked(&none, &heads));
        assert!(
            !all_parked(&none, &[head("DP-1", true)]),
            "no headless output"
        );
        let users = [headless("HEADLESS-1", true), headless("PF-77-1", false)];
        assert!(!all_parked(&none, &users));
        assert!(strays(&none, &users).is_empty());
    }

    #[test]
    fn a_nested_mango_is_named_by_its_window_output() {
        let window = HeadState {
            description: "Wayland output 1".into(),
            ..head("WL-1", true)
        };
        assert!(nested_head(&[head("DP-1", true), headless("PF-3", true)]).is_none());
        assert_eq!(
            nested_head(std::slice::from_ref(&window)).map(|h| h.name.as_str()),
            Some("WL-1")
        );
    }

    fn wait(what: &str, ok: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Against a running mango with two lit monitors of the user's. `PF_MANGO_CLIENT` is a
    /// program mango spawns for the window checks; without it they are skipped.
    #[test]
    #[ignore = "needs a running mango: MANGO_INSTANCE_SIGNATURE and WAYLAND_DISPLAY"]
    fn against_a_running_mango() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        use crate::policy::Topology;
        use crate::toplevels::WindowVerb;
        let head = |n: &str| {
            output_mgmt::heads()
                .unwrap()
                .into_iter()
                .find(|h| h.name == n)
        };
        let lit = |n: &str| head(n).is_some_and(|h| h.enabled);
        let at = |n: &str| {
            let m = list_monitors()
                .unwrap()
                .into_iter()
                .find(|m| m.connector == n)
                .unwrap();
            (m.enabled, m.x, m.y)
        };
        probe().unwrap();
        let before = list_monitors().unwrap();
        eprintln!("before: {before:#?}");
        let desk: Vec<(String, i32, i32)> = before
            .iter()
            .filter(|m| m.enabled && !m.managed)
            .map(|m| (m.connector.clone(), m.x, m.y))
            .collect();
        assert!(desk.len() >= 2, "two monitors of the user's: {desk:?}");

        // A new output at the asked mode, focused; `exclusive` turns the desk off.
        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            refresh_mhz: 0,
        };
        let mut display = MangoDisplay::default();
        display.topology = Some(Topology::Exclusive);
        let out = display.create(mode).unwrap();
        let name = out.output_name.clone().unwrap();
        let h = head(&name).unwrap();
        eprintln!("ours: {h:?}");
        assert!(h.enabled && is_headless(&h) && is_counter_name(&name));
        assert_eq!(h.mode.map(|m| (m.0, m.1)), Some((1280, 720)));
        for (n, ..) in &desk {
            assert!(!lit(n), "{n} still lit under exclusive");
        }
        assert!(!display.pending_undo.is_empty());

        // A window on the shown tag sends the launch to the next empty one, read back.
        let client = std::env::var("PF_MANGO_CLIENT").ok();
        let window = client.map(|program| {
            request(&format!("dispatch spawn,{program}")).unwrap();
            wait("the window maps on ours", || {
                !desktop::toplevels(Some(&name)).is_empty()
            });
            let w = desktop::toplevels(Some(&name)).remove(0);
            eprintln!("window: {w:?}");
            assert_eq!(w.workspace, "1");
            let toplevels = crate::toplevels::list_toplevels(crate::Compositor::Mango, &name);
            assert!(toplevels.iter().any(|t| t.id == w.id));
            crate::toplevels::window_action(
                crate::Compositor::Mango,
                &name,
                WindowVerb::Fullscreen,
                &w.id,
            )
            .unwrap();
            wait("full screen", || {
                desktop::toplevels(Some(&name))
                    .iter()
                    .any(|t| t.id == w.id && t.fullscreen)
            });
            w.id
        });
        let shown = || {
            let reply = request(&format!("get tags {name}")).unwrap();
            let tags = reply["tags"].as_array().unwrap().clone();
            let active = tags.iter().find(|t| t["is_active"] == true);
            active.and_then(|t| t["index"].as_i64())
        };
        let (tag, restore) = desktop::claim_workspace(&name, None).expect("a tag");
        eprintln!("tag {tag}, restore {restore}");
        assert_eq!(shown(), Some(tag));
        if window.is_some() {
            assert_eq!((tag, restore), (2, 1));
        }

        // The desk comes back where it was.
        drop(display);
        for (n, x, y) in &desk {
            assert_eq!(at(n), (true, *x, *y), "{n} after the restore");
        }

        if let Some(id) = &window {
            desktop::move_to_output(id, &desk[0].0).unwrap();
            desktop::move_to_output(id, &name).unwrap();
        }
        desktop::focus_workspace(&name, restore).unwrap();
        assert_eq!(shown(), Some(restore), "released");

        // Asleep, a head keeps its place and reads powered off; a mirror wake lights it.
        let (n2, x2, y2) = &desk[1];
        assert!(desktop::set_dpms(n2, false).unwrap());
        assert_eq!(desktop::head_powered(n2), Some(false));
        assert_eq!(at(n2), (true, *x2, *y2), "asleep");
        assert!(crate::panel_dpms::wake_head(crate::Compositor::Mango, n2).unwrap());
        assert_eq!(at(n2), (true, *x2, *y2), "awake");
        assert!(lit(n2));

        // Teardown moves the window off the shared output and turns it off; the next create
        // takes it again, without the window.
        drop(out);
        assert!(!lit(&name));
        if let Some(id) = &window {
            let w = desktop::toplevels(None)
                .into_iter()
                .find(|t| &t.id == id)
                .unwrap();
            assert_ne!(w.output, name);
        }
        let mut again = MangoDisplay::default();
        again.topology = Some(Topology::Extend);
        let out = again.create(mode).unwrap();
        assert_eq!(out.output_name.as_deref(), Some(name.as_str()));
        if let Some(id) = &window {
            std::thread::sleep(Duration::from_millis(300));
            assert!(desktop::toplevels(Some(&name)).is_empty(), "came back");
            dispatch("killclient", None, Some(id.parse().unwrap())).unwrap();
        }
        drop(out);
        assert!(!lit(&name));
        eprintln!("after: {:#?}", list_monitors().unwrap());
    }

    /// Against a running mango with no headless output of the user's: an output stays parked
    /// while another streams, and the last teardown destroys both.
    #[test]
    #[ignore = "needs a running mango with no headless output of the user's"]
    fn a_last_teardown_destroys_the_parked_outputs() {
        use crate::policy::Topology;
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let listed = |n: &str| output_mgmt::heads().unwrap().iter().any(|h| h.name == n);
        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            refresh_mhz: 0,
        };
        let (mut a, mut b) = (MangoDisplay::default(), MangoDisplay::default());
        a.topology = Some(Topology::Extend);
        b.topology = Some(Topology::Extend);
        let one = a.create(mode).unwrap();
        let two = b.create(mode).unwrap();
        let n1 = one.output_name.clone().unwrap();
        let n2 = two.output_name.clone().unwrap();
        assert_ne!(n1, n2);
        drop(one);
        assert!(listed(&n1), "parked while another streams");
        drop(two);
        wait("destroyed", || !listed(&n1) && !listed(&n2));
    }

    #[test]
    fn only_a_counter_name_is_free_for_any_device() {
        assert!(is_counter_name("PF-4242-1"));
        for name in ["PF-3", "PF-office", "DP-1", "PF-1-2-3"] {
            assert!(!is_counter_name(name), "{name}");
        }
    }
}
