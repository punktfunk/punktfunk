//! Recording [`VirtualDisplay`] for tests.
//!
//! A [`FakeDisplay`] plays a [`Script`] and logs a [`Call`] for every `&mut self` trait call,
//! in order. `&self` getters answer from the script and are not logged. A dropped keepalive,
//! a run topology restore and a closed session cast log too, so teardown order reads off the
//! same list.
//!
//! Setters log and change nothing: only [`FakeDisplay::script`] changes an answer. Clones
//! share one log and one script, so box one for the code under test and keep the other.
//! `take_undo_seqs` answers the seqs its creates journaled and is not logged: it would only pad
//! every acquire.
//!
//! [`in_child`] runs a test body in a child of the test binary, for code on process globals.
//!
//! Pin the contract with the tests below.

use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};

/// One logged event of a [`FakeDisplay`]. The first group is the trait call of that name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Create(Mode),
    SetSessionCastHandoff(bool),
    SessionCastFor(String),
    JoinCast(String, u32),
    SetLaunchCommand(Option<String>),
    SetGamescopeRoute(Option<crate::GamescopeRoute>),
    SetSessionIsolation(Option<crate::SessionIsolation>),
    SetClientIdentity(Option<[u8; 32]>),
    SetJoinLive(bool),
    SetQuitFlag,
    SetClientHdr(Option<pf_frame::HdrMeta>),
    SetHdr(bool),
    SetHwCursor(bool),
    ApplyPosition(i32, i32),
    TakeTopologyRestore,
    Undo(crate::topology_journal::Undo),
    SetFirstInGroup(bool),
    KeptDisplayAlive(u32),
    ResizeKept(Option<String>, Mode),
    /// The keepalive of the create that returned this node dropped.
    KeepaliveDropped(u32),
    /// The topology restore the create of this node prepared ran.
    RestoreRan(u32),
    /// The session cast with this node closed.
    CastClosed(u32),
    /// A test's own marker ([`FakeDisplay::log`]).
    Note(String),
}

/// What a [`FakeDisplay`] answers. [`FakeDisplay::script`] edits it between calls.
pub(crate) struct Script {
    /// `create` fails with this message. `None` creates node 1, 2, … in order.
    pub(crate) fail_create: Option<&'static str>,
    /// Ownership `ownership` answers and a created output reports.
    pub(crate) ownership: DisplayOwnership,
    /// A successful create prepares a topology restore for `take_topology_restore`.
    pub(crate) restore: bool,
    /// A successful create journals this for `take_undo_seqs` (Linux, where the journal is).
    pub(crate) undo: Option<crate::topology_journal::Undo>,
    /// `undo` of exactly this fails; every other undo succeeds.
    pub(crate) fail_undo: Option<crate::topology_journal::Undo>,
    /// The next `undo` logs its call, then blocks until this gate's sender sends or drops.
    pub(crate) undo_gate: Option<std::sync::mpsc::Receiver<()>>,
    /// `heads`.
    pub(crate) heads: Vec<crate::monitors::PhysicalMonitor>,
    /// `isolation_key`.
    pub(crate) isolation: Option<String>,
    /// `sole_instance`.
    pub(crate) sole_instance: bool,
    /// `kept_display_alive`.
    pub(crate) alive: bool,
    /// `can_resize_kept`.
    pub(crate) can_resize: bool,
    /// `resize_kept`.
    pub(crate) resizes: bool,
    /// A created output's `output_name`. With one, `session_cast_for` returns a cast on node
    /// 1001, 1002, … in order.
    pub(crate) output_name: Option<&'static str>,
    /// `session_cast_for` fails after logging its call.
    pub(crate) cast_fails: bool,
    /// `last_identity_slot`.
    pub(crate) identity_slot: Option<u32>,
    /// The device this display serves, as `set_client_identity` names it to a pooled backend.
    /// `accepts_kept` answers by it, as those backends do.
    pub(crate) client_fp: Option<[u8; 32]>,
    /// `join_live`; `join_cast` then casts the node it names.
    pub(crate) join_live: bool,
}

struct Shared {
    script: Script,
    calls: Vec<Call>,
    nodes: u32,
    casts: u32,
    restore: Option<Box<dyn FnOnce() + Send>>,
    undo_seqs: Vec<u64>,
}

/// The fakes [`crate::backend_by_id`] answers by name ([`FakeDisplay::serve_by_name`]).
static SERVED: Mutex<Vec<FakeDisplay>> = Mutex::new(Vec::new());

/// A clone of the fake served as `id`, as [`crate::backend_by_id`] returns a backend.
pub(crate) fn served(id: &str) -> Option<Box<dyn VirtualDisplay>> {
    let served = SERVED.lock().unwrap();
    served.iter().find(|f| f.name == id).map(FakeDisplay::boxed)
}

/// Logs its call when dropped: a keepalive or a cast, whose end a test orders.
struct OnDrop(FakeDisplay, Call);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.log(self.1.clone());
    }
}

#[derive(Clone)]
pub(crate) struct FakeDisplay {
    name: &'static str,
    shared: Arc<Mutex<Shared>>,
}

impl FakeDisplay {
    /// A backend called `name` that creates owned, poolable, live displays and prepares no
    /// topology restore.
    pub(crate) fn new(name: &'static str) -> FakeDisplay {
        FakeDisplay {
            name,
            shared: Arc::new(Mutex::new(Shared {
                script: Script {
                    fail_create: None,
                    ownership: DisplayOwnership::Owned,
                    restore: false,
                    undo: None,
                    fail_undo: None,
                    undo_gate: None,
                    heads: Vec::new(),
                    isolation: None,
                    sole_instance: false,
                    alive: true,
                    can_resize: false,
                    resizes: false,
                    output_name: None,
                    cast_fails: false,
                    identity_slot: None,
                    client_fp: None,
                    join_live: false,
                },
                calls: Vec::new(),
                nodes: 0,
                casts: 0,
                restore: None,
                undo_seqs: Vec::new(),
            })),
        }
    }

    /// Have [`crate::backend_by_id`] answer this fake for its name in this process, so the
    /// registry's teardown undo reaches it.
    pub(crate) fn serve_by_name(&self) {
        SERVED.lock().unwrap().push(self.clone());
    }

    /// A clone as the boxed trait object the registry takes.
    pub(crate) fn boxed(&self) -> Box<dyn VirtualDisplay> {
        Box::new(self.clone())
    }

    pub(crate) fn script(&self, edit: impl FnOnce(&mut Script)) {
        edit(&mut self.shared().script);
    }

    /// The log so far.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.shared().calls.clone()
    }

    /// The log so far, emptied: each step of a test asserts only its own calls.
    pub(crate) fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut self.shared().calls)
    }

    /// Append to the log. A test's own marker is a [`Call::Note`], in order with the calls.
    pub(crate) fn log(&self, call: Call) {
        self.shared().calls.push(call);
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn answer<T>(&self, read: impl FnOnce(&Script) -> T) -> T {
        read(&self.shared().script)
    }
}

impl VirtualDisplay for FakeDisplay {
    fn name(&self) -> &'static str {
        self.name
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let mut s = self.shared();
        s.calls.push(Call::Create(mode));
        if let Some(why) = s.script.fail_create {
            anyhow::bail!("{why}");
        }
        s.nodes += 1;
        let node = s.nodes;
        if s.script.restore {
            let fake = self.clone();
            s.restore = Some(Box::new(move || fake.log(Call::RestoreRan(node))));
        }
        #[cfg(target_os = "linux")]
        if let Some(undo) = s.script.undo.clone() {
            let seq = crate::topology_journal::push(self.name, undo);
            s.undo_seqs.push(seq);
        }
        let mut out = VirtualOutput::owned(
            node,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(OnDrop(self.clone(), Call::KeepaliveDropped(node))),
        );
        out.ownership = s.script.ownership;
        #[cfg(target_os = "linux")]
        {
            out.output_name = s.script.output_name.map(str::to_string);
        }
        Ok(out)
    }

    #[cfg(target_os = "linux")]
    fn set_session_cast_handoff(&mut self, enabled: bool) {
        self.log(Call::SetSessionCastHandoff(enabled));
    }

    #[cfg(target_os = "linux")]
    fn session_cast_for(&mut self, name: &str) -> Result<Option<crate::SessionCastParts>> {
        let mut s = self.shared();
        s.calls.push(Call::SessionCastFor(name.to_string()));
        if s.script.cast_fails {
            anyhow::bail!("scripted: the cast of {name} failed");
        }
        if s.script.output_name.is_none() {
            return Ok(None);
        }
        s.casts += 1;
        let node = 1000 + s.casts;
        let closed = OnDrop(self.clone(), Call::CastClosed(node));
        Ok(Some((node, None, Box::new(closed))))
    }

    #[cfg(target_os = "linux")]
    fn join_cast(&mut self, name: &str, node_id: u32) -> Result<Option<crate::SessionCastParts>> {
        self.log(Call::JoinCast(name.to_string(), node_id));
        let joins = self.answer(|s| s.join_live);
        Ok(joins.then(|| {
            let closed: Box<dyn Send> = Box::new(OnDrop(self.clone(), Call::CastClosed(node_id)));
            (node_id, None, closed)
        }))
    }

    fn set_launch_command(&mut self, cmd: Option<String>) {
        self.log(Call::SetLaunchCommand(cmd));
    }

    fn set_gamescope_route(&mut self, route: Option<crate::GamescopeRoute>) {
        self.log(Call::SetGamescopeRoute(route));
    }

    fn set_session_isolation(&mut self, iso: Option<crate::SessionIsolation>) {
        self.log(Call::SetSessionIsolation(iso));
    }

    fn isolation_key(&self) -> Option<String> {
        self.answer(|s| s.isolation.clone())
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.log(Call::SetClientIdentity(fingerprint));
    }

    fn set_join_live(&mut self, on: bool) {
        self.log(Call::SetJoinLive(on));
    }

    fn join_live(&self) -> bool {
        self.answer(|s| s.join_live)
    }

    fn set_quit_flag(&mut self, _quit: Arc<std::sync::atomic::AtomicBool>) {
        self.log(Call::SetQuitFlag);
    }

    fn set_client_hdr(&mut self, hdr: Option<pf_frame::HdrMeta>) {
        self.log(Call::SetClientHdr(hdr));
    }

    fn set_hdr(&mut self, on: bool) {
        self.log(Call::SetHdr(on));
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.log(Call::SetHwCursor(on));
    }

    fn apply_position(&mut self, x: i32, y: i32) {
        self.log(Call::ApplyPosition(x, y));
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        let mut s = self.shared();
        s.calls.push(Call::TakeTopologyRestore);
        s.restore.take()
    }

    fn take_undo_seqs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.shared().undo_seqs)
    }

    fn undo(&mut self, undo: &crate::topology_journal::Undo) -> Result<()> {
        let gate = {
            let mut s = self.shared();
            s.calls.push(Call::Undo(undo.clone()));
            s.script.undo_gate.take()
        };
        if let Some(gate) = gate {
            let _ = gate.recv();
        }
        if self.answer(|s| s.fail_undo.as_ref() == Some(undo)) {
            anyhow::bail!("scripted to fail");
        }
        Ok(())
    }

    fn heads(&self) -> Result<Vec<crate::monitors::PhysicalMonitor>> {
        Ok(self.answer(|s| s.heads.clone()))
    }

    fn set_first_in_group(&mut self, first: bool) {
        self.log(Call::SetFirstInGroup(first));
    }

    fn ownership(&self) -> DisplayOwnership {
        self.answer(|s| s.ownership)
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.answer(|s| s.identity_slot)
    }

    fn sole_instance(&self) -> bool {
        self.answer(|s| s.sole_instance)
    }

    fn accepts_kept(&self, identity_slot: Option<u32>, _output_name: Option<&str>) -> bool {
        crate::identity::may_reuse(identity_slot, self.answer(|s| s.client_fp))
    }

    fn kept_display_alive(&mut self, node_id: u32) -> bool {
        self.log(Call::KeptDisplayAlive(node_id));
        self.answer(|s| s.alive)
    }

    fn can_resize_kept(&self) -> bool {
        self.answer(|s| s.can_resize)
    }

    fn resize_kept(&mut self, seat: Option<&str>, mode: Mode) -> bool {
        self.log(Call::ResizeKept(seat.map(str::to_string), mode));
        self.answer(|s| s.resizes)
    }
}

/// Set on the child only: the scratch dir that is its config, state, runtime dir and home.
const CHILD_DIR: &str = "PF_VDISPLAY_TEST_DIR";

/// Run `body` in a child of this test binary and pass when it does. `module` is the caller's
/// `module_path!()`, `test` its name. The child is an unconfigured host on a box of its own: one
/// empty scratch dir for config, state, runtime dir and home, no inherited `PUNKTFUNK_*`, then
/// `env`. Nothing it writes or `systemctl --user` reaches the box's.
pub(crate) fn in_child(module: &str, test: &str, env: &[(&str, &str)], body: fn()) {
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        body();
        // The body's last act. A filter that matched no test would exit 0 without it.
        std::fs::write(std::path::Path::new(&dir).join("ran"), "").expect("mark the run");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pf-vdisplay-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let module = module.split_once("::").expect("crate::module").1;
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test binary"));
    let test = format!("{module}::{test}");
    child.args(["--exact", &test, "--include-ignored", "--nocapture"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PUNKTFUNK_") {
            child.env_remove(key);
        }
    }
    for var in [
        CHILD_DIR,
        "PUNKTFUNK_CONFIG_DIR",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "HOME",
    ] {
        child.env(var, &dir);
    }
    let child = child
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .envs(env.iter().copied())
        .output()
        .expect("run the child");
    let ran = dir.join("ran").exists();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(ran, "the child ran no test");
}

/// A fake `tool` for `test`'s [`in_child`]: `script` is its `sh` body. Returns its directory,
/// for the caller to remove, and the `PATH` that puts it first.
#[cfg(target_os = "linux")]
pub(crate) fn fake_tool(test: &str, tool: &str, script: &str) -> (std::path::PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("pf-{tool}-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fake bin dir");
    let bin = dir.join(tool);
    std::fs::write(&bin, format!("#!/bin/sh\n{script}")).expect("write the fake");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (dir, path)
}

/// The lines of `$HOME/<file>` in an [`in_child`] body, where a fake tool logs; empty when none.
#[cfg(target_os = "linux")]
pub(crate) fn home_lines(file: &str) -> Vec<String> {
    let home = std::env::var_os("HOME").expect("HOME");
    std::fs::read_to_string(std::path::Path::new(&home).join(file))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Append `line` to `$HOME/<file>`, beside what a fake tool logs there.
#[cfg(target_os = "linux")]
pub(crate) fn note_home(file: &str, line: &str) {
    use std::io::Write;
    let home = std::env::var_os("HOME").expect("HOME");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(&home).join(file))
        .expect("open the log");
    // One write per line: cast threads note at once, and `writeln!` may split a line in two.
    log.write_all(format!("{line}\n").as_bytes()).expect("note");
}

/// Every journal file of this process's state dir, concatenated; empty when none is left.
#[cfg(target_os = "linux")]
pub(crate) fn journal_text() -> String {
    let dir = pf_paths::state_dir().join("topology-journal");
    std::fs::read_dir(dir).map_or(String::new(), |files| {
        files
            .flatten()
            .map(|f| std::fs::read_to_string(f.path()).unwrap_or_default())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODE: Mode = Mode {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };

    #[test]
    fn a_fake_logs_each_call_in_order_and_plays_its_script() {
        let fake = FakeDisplay::new("fake");
        fake.script(|s| s.restore = true);
        let mut vd = fake.boxed();
        let out = vd.create(MODE).expect("scripted to succeed");
        assert_eq!(
            (vd.name(), out.node_id, out.ownership),
            ("fake", 1, DisplayOwnership::Owned)
        );
        vd.take_topology_restore().expect("scripted to prepare one")();
        assert!(vd.take_topology_restore().is_none(), "taken once");
        drop(out);
        fake.log(Call::Note("torn down".into()));
        fake.script(|s| s.fail_create = Some("no output for you"));
        let err = vd.create(MODE).err().expect("scripted to fail");
        assert_eq!(err.to_string(), "no output for you");
        assert_eq!(
            fake.take_calls(),
            [
                Call::Create(MODE),
                Call::TakeTopologyRestore,
                Call::RestoreRan(1),
                Call::TakeTopologyRestore,
                Call::KeepaliveDropped(1),
                Call::Note("torn down".into()),
                Call::Create(MODE),
            ]
        );
        assert!(fake.calls().is_empty(), "take_calls empties the log");
    }

    /// A setter `SessionParams::apply` forgot would leave a session on another one's value.
    #[test]
    fn session_params_reach_every_setter_once() {
        let iso = crate::SessionIsolation {
            id: "cafe0123".into(),
            ei_relay: "/run/pf-ei".into(),
            sink: None,
            mic_source: None,
        };
        let params = crate::SessionParams {
            client_fp: Some([7; 32]),
            client_hdr: Some(pf_frame::HdrMeta::default()),
            hdr: true,
            hw_cursor: true,
            join_live: true,
            quit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            launch: Some("steam".into()),
            route: Some(crate::GamescopeRoute::Spawn),
            isolation: Some(iso.clone()),
        };
        let fake = FakeDisplay::new("fake");
        params.apply(&mut *fake.boxed());
        let calls = fake.take_calls();
        let want = [
            Call::SetClientIdentity(Some([7; 32])),
            Call::SetJoinLive(true),
            Call::SetClientHdr(Some(pf_frame::HdrMeta::default())),
            Call::SetHdr(true),
            Call::SetHwCursor(true),
            Call::SetQuitFlag,
            Call::SetLaunchCommand(Some("steam".into())),
            Call::SetGamescopeRoute(Some(crate::GamescopeRoute::Spawn)),
            Call::SetSessionIsolation(Some(iso)),
        ];
        assert_eq!(calls.len(), want.len(), "{calls:?}");
        for call in want {
            assert!(calls.contains(&call), "{call:?} missing from {calls:?}");
        }
    }
}
