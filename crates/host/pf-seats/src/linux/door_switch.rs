//! Switching the door on and off: the root verb `punktfunk-seats door on|off|ports <user>`,
//! which `door-helper` runs.
//!
//! With the door off, the box's host runs as the owner. With the door on, it runs as the
//! `punktfunk` system user and the owner plays on a seat. A switch moves the box's identity,
//! pairing store and console password between the owner's folder and the door's directory, and
//! starts or stops the door's units. [`admit`] decides who may switch now, [`port_lines`] which
//! of the owner's port settings the door takes, and [`main`] runs the switch. It needs no running
//! supervisor and does not use the supervisor's socket: an off stops the supervisor.
//!
//! Root never opens a path in the owner's home or in the door's directory. Such a path is a
//! [`hands::Theirs`], and all that can be done with one is to pass it to a command that runs as
//! that side (`runuser -u <side> --`, [`hands::Cmd::by`]). Their bytes pass through root in two
//! places, the port lines and the password lines, and root filters both. Root writes only its own
//! files in the state directory: the owner record, the door-on mark, the port file and the hold,
//! which keeps the door's units off until an on has copied the files or an off has ended.
//!
//! Every program runs through [`hands::pipe`], which the tests replace. So each switch is a table
//! of commands and each step can be made to fail: see `the_door_s_on_runs_exactly_these`,
//! `a_failed_step_puts_the_box_back` and `no_door_step_touches_their_path_as_root`.

use super::accounts::{Passwd, Switchable};
use super::err;
use super::shared::MAX_TRUST_BYTES;
use crate::backend::BackendError;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The keys the door takes from the owner's files, in the order it writes them.
const PORT_KEYS: [&str; 3] = [
    "PUNKTFUNK_MGMT_BIND",
    "PUNKTFUNK_NATIVE_PORT",
    "PUNKTFUNK_UI_BIND",
];

/// The most [`ports`] reads: the owner's two env files, theirs to make any size.
const MAX_PORT_INPUT: u64 = 1 << 20;

/// The most [`kept`] reads, such as an avatar listing that the door's account writes. 1 MiB is
/// tens of thousands of names.
const MAX_LISTING: u64 = 1 << 20;

/// Whom a switch is for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Admit {
    /// On for this account: the first time, or again.
    On(Passwd),
    /// On over `gone`, a recorded owner whose account no longer exists.
    Takeover { gone: String, to: Passwd },
    /// Off for the recorded owner.
    Off(Passwd),
    /// Off for the recorded owner, whose account no longer exists.
    Gone,
}

/// Whether `user` may switch the door `on` or off while root's record names `recorded`. `look`
/// is [`super::accounts::switchable`]: `user` passes it first, then the record decides. On is
/// refused while the record names an account that exists or may exist; off runs only for the
/// account it names.
pub(super) fn admit(
    on: bool,
    user: &str,
    recorded: Option<&str>,
    look: impl Fn(&str, bool) -> Result<Switchable, BackendError>,
) -> Result<Admit, BackendError> {
    let found = look(user, on)?;
    match (on, found, recorded) {
        (true, Switchable::Account(to), Some(owner)) if owner != user => match look(owner, false) {
            Ok(Switchable::Gone) => Ok(Admit::Takeover {
                gone: owner.to_owned(),
                to,
            }),
            Ok(Switchable::Account(_)) => Err(err(
                "door_on_for_other",
                format!("the door is on for {owner}: turn it off from that account first"),
            )
            .param("owner", owner)),
            Err(doubt) => Err(err(
                "door_on_for_other",
                format!("the door is on for {owner}: {}", doubt.message),
            )
            .param("owner", owner)),
        },
        (true, Switchable::Account(to), _) => Ok(Admit::On(to)),
        (false, found, Some(owner)) if owner == user => Ok(match found {
            Switchable::Account(p) => Admit::Off(p),
            Switchable::Gone => Admit::Gone,
        }),
        (false, _, _) => Err(err(
            "door_not_on_for",
            format!("the door is not on for {user}"),
        )),
        // `switchable` answers gone only to an off.
        (true, Switchable::Gone, _) => Err(err(
            "account_missing",
            format!("account '{user}' doesn't exist"),
        )),
    }
}

/// The C locale's `[[:space:]]`, which counts VT where `u8::is_ascii_whitespace` doesn't.
fn space(b: &u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !space(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !space(b)).map_or(start, |i| i + 1);
    &s[start..end]
}

/// The door's port lines from the owner's `host.env` then `web.env`: per [`PORT_KEYS`] key the
/// last line that sets it, one pair of matching quotes off, kept only when its value is
/// address characters. A line is `KEY=value` with blanks around both; any NUL drops all.
pub(super) fn port_lines(text: &[u8]) -> String {
    if text.contains(&0) {
        return String::new();
    }
    let mut last: [Option<&[u8]>; 3] = [None; 3];
    for line in text.split(|&b| b == b'\n') {
        let Some(eq) = line.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = trim(&line[..eq]);
        let Some(i) = PORT_KEYS.iter().position(|k| k.as_bytes() == key) else {
            continue;
        };
        let mut value = trim(&line[eq + 1..]);
        if let [quote @ (b'"' | b'\''), inner @ .., end] = value
            && end == quote
        {
            value = inner;
        }
        last[i] = Some(value);
    }
    let address = |b: &u8| b.is_ascii_alphanumeric() || b".:%_-[]".contains(b);
    PORT_KEYS
        .iter()
        .zip(last)
        .filter_map(|(key, value)| {
            let value = value.filter(|v| !v.is_empty() && v.iter().all(address))?;
            Some(format!("{key}={}\n", String::from_utf8_lossy(value)))
        })
        .collect()
}

/// The [`port_lines`] of `input` into `to`, root's and 0644, replaced whole. Input past
/// [`MAX_PORT_INPUT`] is refused and the last copy stays.
fn ports(input: impl Read, to: &Path) -> Result<(), BackendError> {
    use std::os::unix::fs::PermissionsExt;
    let mut text = Vec::new();
    input
        .take(MAX_PORT_INPUT + 1)
        .read_to_end(&mut text)
        .map_err(|e| err("door_ports", format!("read the port lines: {e}")))?;
    if text.len() as u64 > MAX_PORT_INPUT {
        return Err(err("door_ports", "the port lines run past 1 MiB"));
    }
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    pf_paths::write_secret_file(tmp, port_lines(&text).as_bytes())
        .and_then(|()| std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o644)))
        .and_then(|()| std::fs::rename(tmp, to))
        .map_err(|e| err("door_ports", format!("write {}: {e}", to.display())))
}

/// The door's directory. `install -d` makes it; only the door opens a path inside it.
const BOX_DIR: &str = "/var/lib/punktfunk";
/// Held for a whole on or off. `ports` runs inside the door's start, which an on waits for while
/// it holds this lock, so `ports` never takes it.
const LOCK: &str = "/run/punktfunk-door.lock";
const DOOR: &str = "punktfunk";
/// The door's primary group. Never `punktfunk`, which owns the usbip attach nodes and passes
/// pf-dm-helper: the door opens no device.
const DOOR_GROUP: &str = "punktfunk-door";
/// The shared games folder's group; the owner's Steam moves games into it at their next login.
const GAMES_GROUP: &str = "punktfunk-games";
const SEATS_UNIT: &str = "punktfunk-seats.service";
const DOOR_UNIT: &str = "punktfunk-door.service";
const WEB_DOOR_UNIT: &str = "punktfunk-web-door.service";
/// What the box's host keeps: identity, id, pairings, profiles, the console's password hash and
/// what seals its sessions, so a signed-in console stays signed in across a switch. `profiles/`
/// holds the avatars. The display policy and the hooks stay the owner's host's.
const FILES: [&str; 8] = [
    "native-cert.pem",
    "native-key.pem",
    "uniqueid",
    "punktfunk1-paired.json",
    "profiles.json",
    "web-password",
    "web-session-key",
    "web-session-epoch",
];
/// SteamOS keeps the console's password in `web.env`, beside its session secret and bind.
const PASSWORD_LINE: &str = "^[[:space:]]*PUNKTFUNK_UI_PASSWORD(_HASH)?=";
const SECRET_LINE: &str = "^[[:space:]]*PUNKTFUNK_UI_SECRET=";
/// The value root hands back from the door's `web-password`: bare shell-safe characters, or
/// single quotes with no quote inside. The owner's console loads the file as an environment
/// file and SteamOS sources it in a shell, so no other variable and no shell syntax comes back.
const SAFE_VALUE: &str = "=([A-Za-z0-9_.,:=+/@%^-]{0,255}|'[^'[:cntrl:]]{0,255}')[[:blank:]]*$";

/// `grep -E`'s pattern for the lines of `keys`, an alternation, that carry a [`SAFE_VALUE`].
fn safe_pattern(keys: &str) -> String {
    format!("^[[:blank:]]*PUNKTFUNK_UI_({keys}){SAFE_VALUE}")
}

/// Root's `grep` over the door's bytes for [`safe_pattern`]'s lines of `keys`.
fn safe_lines(keys: &str, flags: &str) -> Cmd {
    Cmd::root(&["grep", flags, "-E", safe_pattern(keys).as_str()])
}
/// A write as one side: stdin beside `$1`, which [`hand_over`] moves over it or drops.
const TO_DOOR: &str = r#"umask 077; cat >"$1.tmp""#;
/// The owner's write fails past `$2` bytes, so a file the door's account grew never moves home.
const TO_OWNER: &str = r#"umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2""#;
const MOVE: &str = r#"mv -f "$1.tmp" "$1""#;
const DROP: &str = r#"rm -f "$1.tmp""#;
/// `$1`, the owner's `web.env`, less its `$3` lines, then `$2`: the door's last password line.
const SPLICE: &str = r#"umask 077; { grep -vE "$3" "$1" || true; } >"$1.tmp" && printf "%s\n" "$2" >>"$1.tmp" && mv -f "$1.tmp" "$1""#;
/// What `find` hands the door's files with another group.
const REGROUP: [&str; 12] = [
    "-xdev", "-user", DOOR, "!", "-group", DOOR_GROUP, "-exec", "chgrp", "-h", DOOR_GROUP, "{}",
    "+",
];

/// The programs a switch runs, and the one way a path of theirs reaches one.
mod hands {
    use std::ffi::{OsStr, OsString};
    use std::io::Read as _;
    use std::os::unix::process::ExitStatusExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};

    /// A path in the owner's home or in the box directory. It has no `AsRef<Path>` and no
    /// accessor: [`Cmd::by`] is the only way it becomes an argument, after `runuser -u <side> --`.
    pub(super) struct Theirs(PathBuf);

    impl Theirs {
        /// The box directory.
        pub(super) fn boxed() -> Self {
            Theirs(PathBuf::from(super::BOX_DIR))
        }

        /// The owner's config folder under `home`.
        pub(super) fn config(home: &Path) -> Self {
            Theirs(home.join(".config/punktfunk"))
        }

        pub(super) fn join(&self, name: &str) -> Self {
            Theirs(self.0.join(name))
        }
    }

    /// Whose rights a command on [`Theirs`] runs with.
    pub(super) enum Side<'a> {
        Owner(&'a str),
        Door,
    }

    /// What the last stage's stdout does: the switch's own, `/dev/null`, or read back.
    #[derive(Clone, Copy)]
    enum Out {
        Shown,
        Hidden,
        Kept(u64),
    }

    pub(super) struct Cmd {
        argv: Vec<OsString>,
        out: Out,
        err: bool,
        /// The highest exit code [`pipe_held`] counts as this stage holding.
        fine: i32,
    }

    impl Cmd {
        /// Root's own program. Words only, so no [`Theirs`] can be one.
        pub(super) fn root(words: &[&str]) -> Self {
            Cmd {
                argv: words.iter().map(OsString::from).collect(),
                out: Out::Shown,
                err: true,
                fine: 0,
            }
        }

        /// `head`, `path`, `tail` as `side`.
        pub(super) fn by(
            side: &Side,
            head: &[&str],
            path: Option<&Theirs>,
            tail: &[&OsStr],
        ) -> Self {
            let user = match side {
                Side::Owner(user) => user,
                Side::Door => super::DOOR,
            };
            let mut cmd = Cmd::root(&["runuser", "-u", user, "--"]);
            cmd.argv.extend(head.iter().map(OsString::from));
            cmd.argv.extend(path.map(|p| p.0.clone().into_os_string()));
            cmd.argv.extend(tail.iter().map(OsString::from));
            cmd
        }

        /// `>/dev/null`.
        pub(super) fn hidden(mut self) -> Self {
            self.out = Out::Hidden;
            self
        }

        /// `2>/dev/null`.
        pub(super) fn quiet(mut self) -> Self {
            self.err = false;
            self
        }

        /// Its stdout read back, up to `limit` bytes; past that the pipe closes.
        pub(super) fn kept(mut self, limit: u64) -> Self {
            self.out = Out::Kept(limit);
            self
        }

        /// `grep`'s 1, no line matched, counts as holding.
        pub(super) fn may_match_nothing(mut self) -> Self {
            self.fine = 1;
            self
        }
    }

    /// Runs `stages` as a shell pipeline: each one's stdout into the next one's stdin, the first
    /// on `/dev/null`. The exit code is the last stage's, as the shell's without `pipefail`; a
    /// program that doesn't start is 127. The kept stdout is the last stage's.
    pub(super) fn pipe(stages: &[Cmd]) -> (i32, Vec<u8>) {
        let (codes, kept) = run(stages);
        (codes.last().copied().unwrap_or(127), kept)
    }

    /// Runs `stages` as [`pipe`] does: `Err` with the code of the first stage that didn't hold.
    pub(super) fn pipe_held(stages: &[Cmd]) -> Result<(), i32> {
        let (codes, _) = run(stages);
        match stages
            .iter()
            .zip(codes)
            .find(|(cmd, code)| *code > cmd.fine)
        {
            Some((_, code)) => Err(code),
            None => Ok(()),
        }
    }

    /// Every stage's exit code, and the last one's kept stdout.
    fn run(stages: &[Cmd]) -> (Vec<i32>, Vec<u8>) {
        #[cfg(test)]
        if let Some(answer) = fake(stages) {
            return answer;
        }
        let mut feed: Option<ChildStdout> = None;
        let mut running: Vec<Option<Child>> = Vec::new();
        for (i, cmd) in stages.iter().enumerate() {
            let last = i + 1 == stages.len();
            let stdout = match (last, cmd.out) {
                (false, _) | (true, Out::Kept(_)) => Stdio::piped(),
                (true, Out::Shown) => Stdio::inherit(),
                (true, Out::Hidden) => Stdio::null(),
            };
            let spawned = Command::new(&cmd.argv[0])
                .args(&cmd.argv[1..])
                .stdin(feed.take().map_or_else(Stdio::null, Stdio::from))
                .stdout(stdout)
                .stderr(if cmd.err {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .spawn();
            match spawned {
                Ok(mut child) => {
                    if !last {
                        feed = child.stdout.take();
                    }
                    running.push(Some(child));
                }
                Err(error) => {
                    if cmd.err {
                        eprintln!("door-helper: {}: {error}", cmd.argv[0].to_string_lossy());
                    }
                    running.push(None);
                }
            }
        }
        let mut kept = Vec::new();
        if let (Some(Out::Kept(limit)), Some(Some(child))) =
            (stages.last().map(|c| c.out), running.last_mut())
            && let Some(stdout) = child.stdout.take()
        {
            // A read error ends the bytes there, as a closed pipe would.
            let _ = stdout.take(limit).read_to_end(&mut kept);
        }
        let codes = running
            .into_iter()
            .map(|child| child.map_or(127, |mut c| c.wait().map_or(1, code_of)))
            .collect();
        (codes, kept)
    }

    /// The shell's `$?`: the exit code, or 128 and the signal that ended it.
    fn code_of(status: ExitStatus) -> i32 {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    }

    /// Each stage's answer from the installed fake, in order; `None` when none is installed.
    #[cfg(test)]
    fn fake(stages: &[Cmd]) -> Option<(Vec<i32>, Vec<u8>)> {
        let mut codes = Vec::new();
        let mut last = None;
        for cmd in stages {
            let argv: Vec<String> = cmd
                .argv
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
            let answer = super::super::fake::answer(&argv[0], &args)?;
            codes.push(code_of(answer.status));
            last = Some(answer);
        }
        let answer = last?;
        let mut stdout = answer.stdout;
        match stages.last()?.out {
            Out::Kept(limit) => stdout.truncate(usize::try_from(limit).unwrap_or(usize::MAX)),
            Out::Shown | Out::Hidden => stdout.clear(),
        }
        Some((codes, stdout))
    }
}

use hands::{Cmd, Side, Theirs};

/// A switch that ends early, with the helper's exit code. Its reason is on stderr already.
#[derive(Debug, PartialEq, Eq)]
struct Stop(u8);

fn die(message: &str) -> Stop {
    eprintln!("door-helper: {message}");
    Stop(1)
}

fn status(cmd: Cmd) -> i32 {
    hands::pipe(&[cmd]).0
}

fn ok(cmd: Cmd) -> bool {
    status(cmd) == 0
}

/// `stages`, whose failure ends the switch with the last stage's code, as `set -e` does.
fn must(stages: &[Cmd]) -> Result<(), Stop> {
    match hands::pipe(stages).0 {
        0 => Ok(()),
        code => Err(Stop(u8::try_from(code).unwrap_or(1))),
    }
}

/// `$(cmd)`: its stdout less the trailing newlines, or `None` past [`MAX_LISTING`] bytes.
fn kept(cmd: Cmd) -> Option<Vec<u8>> {
    let mut out = hands::pipe(&[cmd.kept(MAX_LISTING + 1)]).1;
    if out.len() as u64 > MAX_LISTING {
        return None;
    }
    while out.last() == Some(&b'\n') {
        out.pop();
    }
    Some(out)
}

/// Root's `rm -f`: every path is tried, and any failure but a missing file fails the whole.
fn remove(paths: &[&Path]) -> Result<(), Stop> {
    let mut failed = false;
    for path in paths {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("door-helper: remove {}: {error}", path.display());
            failed = true;
        }
    }
    if failed {
        Err(Stop(1))
    } else {
        Ok(())
    }
}

fn touch(path: &Path) -> Result<(), Stop> {
    std::fs::File::create(path)
        .map(drop)
        .map_err(|e| die(&format!("write {}: {e}", path.display())))
}

fn active(unit: &str) -> bool {
    ok(Cmd::root(&["systemctl", "is-active", "--quiet", unit]))
}

fn ensure_group(group: &str) -> Result<(), Stop> {
    if ok(Cmd::root(&["getent", "group", group]).hidden()) {
        return Ok(());
    }
    must(&[Cmd::root(&["groupadd", "--system", group])])
}

fn door_exists() -> bool {
    ok(Cmd::root(&["getent", "passwd", DOOR]).hidden())
}

/// Units that start at boot. NixOS wires them in its configuration and its unit directory can't
/// be written, so a refused enable is fine for a unit already enabled.
fn wire(units: &[&str]) -> Result<(), Stop> {
    for unit in units {
        if !ok(Cmd::root(&["systemctl", "enable", unit]))
            && !ok(Cmd::root(&["systemctl", "is-enabled", "--quiet", unit]))
        {
            return Err(die(&format!("can't enable {unit}")));
        }
    }
    Ok(())
}

/// Units that stop and no longer start at boot, or, where they can't be disabled, only stop.
fn unwire(units: &[&str]) -> bool {
    let with = |verb: &[&str]| Cmd::root(&[&["systemctl"][..], verb, units].concat());
    ok(with(&["disable", "--now"])) || ok(with(&["stop"]))
}

/// `command -v nologin`, else where most systems keep it.
fn nologin() -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("nologin"))
        .find(|f| {
            f.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .and_then(|f| f.into_os_string().into_string().ok())
        .unwrap_or_else(|| "/usr/sbin/nologin".into())
}

/// The account a switch is for, and its config folder.
struct Owner<'a> {
    user: &'a str,
    uid: u32,
    own: Theirs,
}

impl<'a> Owner<'a> {
    fn of(user: &'a str, passwd: &Passwd) -> Self {
        Owner {
            user,
            uid: passwd.uid,
            own: Theirs::config(&passwd.home),
        }
    }

    fn side(&self) -> Side<'a> {
        Side::Owner(self.user)
    }

    fn file(&self, name: &str) -> Theirs {
        self.own.join(name)
    }

    /// The owner's systemd manager. A user with no login and no linger has none, and then there
    /// is nothing running to change.
    fn ctl(&self, verb: &[&str]) -> Cmd {
        let runtime = format!("XDG_RUNTIME_DIR=/run/user/{}", self.uid);
        let head = [&["env", runtime.as_str(), "systemctl", "--user"][..], verb].concat();
        Cmd::by(&self.side(), &head, None, &[]).quiet()
    }

    /// `head`, then `name` in the owner's folder, as the owner.
    fn cmd(&self, head: &[&str], name: &str) -> Cmd {
        Cmd::by(&self.side(), head, Some(&self.file(name)), &[])
    }

    fn test(&self, flag: &str, name: &str) -> bool {
        ok(self.cmd(&["test", flag], name))
    }

    fn cat(&self, name: &str) -> Cmd {
        self.cmd(&["cat", "--"], name)
    }
}

fn in_box(name: &str) -> Theirs {
    Theirs::boxed().join(name)
}

/// `head`, then `name` in the box directory, as the door.
fn as_door(head: &[&str], name: &str) -> Cmd {
    Cmd::by(&Side::Door, head, Some(&in_box(name)), &[])
}

fn door_test(name: &str) -> bool {
    ok(as_door(&["test", "-f"], name))
}

fn door_rm(name: &str) -> i32 {
    status(as_door(&["rm", "-f"], name))
}

/// `name` in the box directory, written from stdin as the door.
fn to_door(name: &str) -> Cmd {
    as_door(&["sh", "-c", TO_DOOR, "sh"], name)
}

/// `stages` into `to`'s temp file, which the last one writes as `side`. It moves over `to` only
/// when every stage held: a read that ended early would leave a short copy. Otherwise it is
/// dropped and the switch ends with the code of the stage that failed.
fn hand_over(stages: &[Cmd], side: &Side, to: &Theirs) -> Result<(), Stop> {
    let script = |script| Cmd::by(side, &["sh", "-c", script, "sh"], Some(to), &[]);
    match hands::pipe_held(stages) {
        Ok(()) => must(&[script(MOVE)]),
        Err(code) => {
            status(script(DROP));
            Err(Stop(u8::try_from(code).unwrap_or(1)))
        }
    }
}

/// `name` from the owner's folder into the box directory. A `fresh` take of a file the owner
/// lacks removes the box's copy, so nothing of an earlier owner stays.
fn take(owner: &Owner, name: &str, fresh: bool) -> Result<(), Stop> {
    if !owner.test("-f", name) {
        return if fresh {
            must(&[as_door(&["rm", "-f"], name)])
        } else {
            Ok(())
        };
    }
    hand_over(
        &[owner.cat(name), to_door(name)],
        &Side::Door,
        &in_box(name),
    )
}

/// `name` in the owner's folder, written from stdin as the owner. Past [`MAX_TRUST_BYTES`], the
/// size the trust copy refuses for the same files, the write fails.
fn to_owner(owner: &Owner, name: &str) -> Cmd {
    let cap = MAX_TRUST_BYTES.to_string();
    let (head, tail) = (["sh", "-c", TO_OWNER, "sh"], [OsStr::new(&cap)]);
    Cmd::by(&owner.side(), &head, Some(&owner.file(name)), &tail)
}

/// `name` from the box directory back to the owner: read as the door, written as the owner, so
/// neither side's links lead anywhere that side couldn't go. A file too large fails the step.
fn give(owner: &Owner, name: &str) -> Result<(), Stop> {
    if !door_test(name) {
        return Ok(());
    }
    let stages = [as_door(&["cat", "--"], name), to_owner(owner, name)];
    hand_over(&stages, &owner.side(), &owner.file(name))
}

/// The SteamOS layout: the password lives in `web.env`, and there is no `web-password`.
fn steamos_password(owner: &Owner) -> bool {
    owner.test("-f", "web.env") && !owner.test("-f", "web-password")
}

/// The password and secret lines of a SteamOS `web.env` become the door's `web-password`.
fn take_password(owner: &Owner) -> Result<(), Stop> {
    if !steamos_password(owner) {
        return Ok(());
    }
    let lines = format!("{PASSWORD_LINE}|{SECRET_LINE}");
    let grep = Cmd::root(&["grep", "-E", lines.as_str()]).may_match_nothing();
    let stages = [owner.cat("web.env"), grep, to_door("web-password")];
    hand_over(&stages, &Side::Door, &in_box("web-password"))
}

/// The door's `web-password` back to the owner, less every line [`safe_lines`] refuses. A file
/// with no line left changes nothing: the owner keeps the password they had before the door.
/// On SteamOS the door's last password line is spliced into `web.env`: the one line this
/// process holds of the door's files.
fn give_password(owner: &Owner) -> Result<(), Stop> {
    let door = || as_door(&["cat", "--"], "web-password");
    if !steamos_password(owner) {
        if !door_test("web-password") {
            return Ok(());
        }
        let keys = "PASSWORD(_HASH)?|SECRET";
        if hands::pipe(&[door(), safe_lines(keys, "-aq")]).0 != 0 {
            eprintln!("door-helper: {BOX_DIR}/web-password holds no line safe to hand back");
            return Ok(());
        }
        let stages = [
            door(),
            safe_lines(keys, "-a"),
            to_owner(owner, "web-password"),
        ];
        return hand_over(&stages, &owner.side(), &owner.file("web-password"));
    }
    let stages = [
        door().quiet(),
        safe_lines("PASSWORD(_HASH)?", "-a"),
        Cmd::root(&["tail", "-n1"]).kept(4096),
    ];
    let mut line = hands::pipe(&stages).1;
    while line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.is_empty() {
        return Ok(());
    }
    use std::os::unix::ffi::OsStrExt;
    let tail = [OsStr::from_bytes(&line), OsStr::new(PASSWORD_LINE)];
    let splice = Cmd::by(
        &owner.side(),
        &["sh", "-c", SPLICE, "sh"],
        Some(&owner.file("web.env")),
        &tail,
    );
    must(&[splice])
}

/// The names `ls -1` prints, split as the shell splits words, that [`safe_name`] keeps. A listing
/// past [`MAX_LISTING`] ends the switch, as a file that can't be read does.
fn avatars(ls: Cmd) -> Result<Vec<String>, Stop> {
    let listing = kept(ls.quiet()).ok_or_else(|| die("the avatar listing runs past 1 MiB"))?;
    Ok(listing
        .split(|b| matches!(b, b' ' | b'\t' | b'\n'))
        .filter(|name| safe_name(name))
        .map(|name| format!("profiles/{}", String::from_utf8_lossy(name)))
        .collect())
}

fn safe_name(name: &[u8]) -> bool {
    !matches!(name, b"" | b"." | b"..")
        && name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
}

/// The avatars by name: one [`safe_name`] refuses stays put. A `fresh` take empties the box's
/// folder first, so it holds the owner's avatars alone.
fn take_avatars(owner: &Owner, fresh: bool) -> Result<(), Stop> {
    if fresh {
        must(&[as_door(&["rm", "-rf"], "profiles")])?;
    }
    must(&[as_door(&["mkdir", "-p", "-m", "0700"], "profiles")])?;
    for name in avatars(owner.cmd(&["ls", "-1", "--"], "profiles"))? {
        take(owner, &name, false)?;
    }
    Ok(())
}

fn give_avatars(owner: &Owner) -> Result<(), Stop> {
    for name in avatars(as_door(&["ls", "-1", "--"], "profiles"))? {
        give(owner, &name)?;
    }
    Ok(())
}

/// `from`'s bytes as `side`, up to one past what [`ports`] reads.
fn read(side: &Side, from: &Theirs) -> Vec<u8> {
    let cat = Cmd::by(side, &["cat", "--"], Some(from), &[]);
    hands::pipe(&[cat.quiet().kept(MAX_PORT_INPUT + 1)]).1
}

/// The hold's name in the state directory. The door's units don't start under it. An on holds it
/// from before the record until the files are in the box, with [`ON_HOLD`] inside; an off holds
/// it empty from its first step to its last.
const HOLD: &str = "door-hold";
/// What an on's hold holds. An off's is empty.
const ON_HOLD: &str = "on\n";

/// Whether the hold in `state` is an off's: an off that began and didn't finish. An on's hold
/// doesn't count. It checks the size, which needs no read right on root's file.
pub(super) fn off_held(state: &Path) -> bool {
    std::fs::metadata(state.join(HOLD)).is_ok_and(|m| m.len() == 0)
}

/// What a switch puts back when it ends early.
enum Back<'a> {
    /// A first on: no record, mark, hold or owner row, and the supervisor off unless it was on.
    On { user: &'a str, seats_were_up: bool },
    /// An off whose files didn't arrive home: it ends as [`Door::stand_down`] does and keeps the
    /// record, the mark and the hold. The door stays off; an on, then an off, hands them back.
    Door {
        user: &'a str,
        owner: Option<&'a Owner<'a>>,
    },
}

/// Puts the box back as [`Back`] says unless disarmed, on every way out of a switch.
struct Undo<'a> {
    door: &'a Door,
    back: Option<Back<'a>>,
}

impl Undo<'_> {
    fn disarm(mut self) {
        self.back = None;
    }
}

impl Drop for Undo<'_> {
    fn drop(&mut self) {
        match self.back.take() {
            Some(Back::On {
                user,
                seats_were_up,
            }) => self.door.undo_on(user, seats_were_up),
            Some(Back::Door { user, owner }) => {
                eprintln!(
                    "door-helper: couldn't hand the door's files back, switch the door off again"
                );
                self.door.stand_down(user, owner);
            }
            None => {}
        }
    }
}

type Look<'a> = &'a dyn Fn(&str, bool) -> Result<Switchable, BackendError>;

/// Where a switch keeps root's own files, and the binary whose verbs ask the supervisor.
struct Door {
    state: String,
    lock: String,
    contracts: String,
    seats: String,
}

/// `punktfunk-seats door on|off <user>` and `door ports`: the switch, as root, with the refusals,
/// lines and exit codes `door-helper` has. Returns the exit code.
pub fn main(args: &[String]) -> u8 {
    let verb = args.first().map_or("", String::as_str);
    let user = args.get(1).map_or("", String::as_str);
    if !matches!(verb, "on" | "off" | "ports") {
        return die("usage: door-helper on|off <user>, or door-helper ports").0;
    }
    if !rustix::process::geteuid().is_root() {
        return die("run as root").0;
    }
    let seats = match std::env::current_exe().map(|p| p.into_os_string().into_string()) {
        Ok(Ok(seats)) => seats,
        Ok(Err(path)) => {
            return die(&format!("find punktfunk-seats: {}", path.to_string_lossy())).0;
        }
        Err(e) => return die(&format!("find punktfunk-seats: {e}")).0,
    };
    let door = Door {
        state: super::STATE_DIR.into(),
        lock: LOCK.into(),
        contracts: super::session::ENV_DIR.into(),
        seats,
    };
    match door.switch(verb, user, &super::accounts::switchable) {
        Ok(()) => 0,
        Err(Stop(code)) => code,
    }
}

impl Door {
    fn record(&self) -> PathBuf {
        Path::new(&self.state).join("owner")
    }

    fn mark(&self) -> PathBuf {
        Path::new(&self.state).join("door-on")
    }

    fn port_file(&self) -> PathBuf {
        Path::new(&self.state).join("host.env")
    }

    /// The door's units don't start while it exists ([`HOLD`]), so a switch that stops halfway
    /// stays off at boot, where the units can't be disabled.
    fn hold(&self) -> PathBuf {
        Path::new(&self.state).join(HOLD)
    }

    /// The owner's contract, which a stopped supervisor leaves behind.
    fn contract(&self, user: &str) -> PathBuf {
        Path::new(&self.contracts).join(format!("{user}.env"))
    }

    /// The record as `$(cat …)` reads it, `None` when it can't be read.
    fn read_record(&self) -> Option<String> {
        let bytes = std::fs::read(self.record()).ok()?;
        Some(
            String::from_utf8_lossy(&bytes)
                .trim_end_matches('\n')
                .to_owned(),
        )
    }

    fn write_record(&self, user: &str) -> Result<(), Stop> {
        use std::os::unix::fs::PermissionsExt;
        let record = self.record();
        let tmp = record.with_extension("tmp");
        std::fs::write(&tmp, format!("{user}\n"))
            .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)))
            .and_then(|()| std::fs::rename(&tmp, &record))
            .map_err(|e| die(&format!("write {}: {e}", record.display())))
    }

    #[clippy::msrv = "1.89"]
    fn lock(&self) -> Result<std::fs::File, Stop> {
        let file = std::fs::File::create(&self.lock)
            .map_err(|e| die(&format!("open {}: {e}", self.lock)))?;
        file.lock()
            .map_err(|e| die(&format!("lock {}: {e}", self.lock)))?;
        Ok(file)
    }

    /// [`admit`] by root's record, its refusal's code and reason on stderr.
    fn check(&self, on: bool, user: &str, look: Look) -> Result<Admit, Stop> {
        let recorded = super::accounts::recorded_owner(Path::new(&self.state));
        admit(on, user, recorded.as_deref(), look).map_err(|e| {
            eprintln!("{e}");
            die(&format!("'{user}' can't be switched"))
        })
    }

    /// One switch under the lock, or the ports outside it.
    fn switch(&self, verb: &str, user: &str, look: Look) -> Result<(), Stop> {
        if verb == "ports" {
            return self.ports(look);
        }
        named(user)?;
        let _held = self.lock()?;
        let recorded = self.read_record().unwrap_or_default();
        match self.check(verb == "on", user, look)? {
            Admit::On(to) => self.on(&Owner::of(user, &to), None, &recorded),
            Admit::Takeover { gone, to } => {
                eprintln!("door-helper: {gone} is gone, the door passes to {user}");
                self.on(&Owner::of(user, &to), Some(&gone), &recorded)
            }
            Admit::Off(from) => self.off(user, Some(&Owner::of(user, &from)), &recorded),
            Admit::Gone => self.off(user, None, &recorded),
        }
    }

    /// Puts the door's account in a group of its own. This runs at every door start and at every
    /// on. An account with any other primary group is moved, together with the files that group
    /// owned: the door's directory, which the door changes itself, and the library copy, which is
    /// root's.
    ///
    /// `strict` is for an on: a failed step ends the switch. A door start is not strict. It tries
    /// every step and fails only when the library copy's step fails.
    fn door_account_group(&self, strict: bool) -> Result<(), Stop> {
        if !door_exists() {
            return Ok(());
        }
        let step = |done: Result<(), Stop>| if strict { done } else { Ok(()) };
        step(ensure_group(DOOR_GROUP))?;
        if kept(Cmd::root(&["id", "-gn", DOOR])).as_deref() != Some(DOOR_GROUP.as_bytes()) {
            step(must(&[Cmd::root(&["usermod", "-g", DOOR_GROUP, DOOR])]))?;
        }
        // The door's own files only: a file another account left there stays as it is.
        let regroup = REGROUP.map(OsStr::new);
        if !ok(Cmd::by(
            &Side::Door,
            &["find"],
            Some(&Theirs::boxed()),
            &regroup,
        )) {
            eprintln!("door-helper: some files in {BOX_DIR} keep their group");
        }
        let library = format!("{}/library", self.state);
        if Path::new(&library).is_dir() {
            must(&[Cmd::root(
                &[&["find", library.as_str(), "-xdev"][..], &REGROUP[3..]].concat(),
            )])?;
        }
        Ok(())
    }

    /// A mark in the box directory, where the door can write, moves to root's for a recorded
    /// owner and is dropped otherwise, before anything reads the mark.
    fn legacy_mark(&self, recorded: &str) -> Result<(), Stop> {
        if !door_exists() {
            return Ok(());
        }
        if !recorded.is_empty() && door_test("door-on") {
            touch(&self.mark())?;
        }
        door_rm("door-on");
        Ok(())
    }

    /// Switches the door on for `owner`. The hold goes up before the record and comes down once
    /// the files are in the box and the supervisor has the owner's row. So an on that stops
    /// halfway, killed or failed, leaves the door's units off, and the next on or off finishes the
    /// switch. A first on that fails puts the box back ([`Door::undo_on`]).
    fn on(&self, owner: &Owner, gone: Option<&str>, recorded: &str) -> Result<(), Stop> {
        let user = owner.user;
        let seats_were_up = active(SEATS_UNIT);
        ensure_group(DOOR_GROUP)?;
        if !door_exists() {
            let shell = nologin();
            must(&[Cmd::root(&[
                "useradd",
                "--system",
                "--no-user-group",
                "--gid",
                DOOR_GROUP,
                "--home-dir",
                BOX_DIR,
                "--shell",
                shell.as_str(),
                DOOR,
            ])])?;
        }
        let install = |owner: &str, group: &str, dir: &str| {
            must(&[Cmd::root(&[
                "install", "-d", "-m", "0711", "-o", owner, "-g", group, dir,
            ])])
        };
        install(DOOR, DOOR_GROUP, BOX_DIR)?;
        self.door_account_group(true)?;
        ensure_group(GAMES_GROUP)?;
        must(&[Cmd::root(&["usermod", "-aG", GAMES_GROUP, user])])?;
        install("root", "root", self.state.as_str())?;
        self.legacy_mark(recorded)?;
        // A gone owner's files stay in the box; this member's replace them file by file.
        if gone.is_some() {
            remove(&[&self.mark()])?;
        }
        let undo = Undo {
            door: self,
            back: recorded.is_empty().then_some(Back::On {
                user,
                seats_were_up,
            }),
        };
        // A hold already there stays as it is: a failed off's stays empty for `off_held`.
        if !self.hold().exists() {
            std::fs::write(self.hold(), ON_HOLD)
                .map_err(|e| die(&format!("write {}: {e}", self.hold().display())))?;
        }
        self.write_record(user)?;
        if !self.mark().exists() {
            // The box holds this member's files alone; only a gone owner's stay where this
            // member has none. `host-settings.json` is never handed back: the door sends its
            // rows to the owner's host before it switches off.
            let fresh = gone.is_none();
            for name in FILES.into_iter().chain(["host-settings.json"]) {
                take(owner, name, fresh)?;
            }
            take_password(owner)?;
            take_avatars(owner, fresh)?;
            touch(&self.mark())?;
        }
        wire(&[SEATS_UNIT])?;
        // Ready once its socket is bound (Type=notify).
        must(&[Cmd::root(&["systemctl", "start", SEATS_UNIT])])?;
        must(&[Cmd::root(&[self.seats.as_str(), "adopt-owner", user]).hidden()])?;
        // The files are in the box. The hold, this on's or a failed off's, would keep the door's
        // units from starting below.
        remove(&[&self.hold()])?;
        wire(&[DOOR_UNIT, WEB_DOOR_UNIT])?;
        undo.disarm();
        // The box's ports are the door's from here, so the user's console and host leave them
        // first. The host restarts on the owner row's contract: its own ports, the box's identity.
        status(owner.ctl(&["stop", "punktfunk-web.service"]));
        status(owner.ctl(&["try-restart", "punktfunk-host.service"]));
        must(&[Cmd::root(&[
            "systemctl",
            "restart",
            DOOR_UNIT,
            WEB_DOOR_UNIT,
        ])])?;
        println!("door on for {user}");
        Ok(())
    }

    /// A first on that fails puts the box back as [`Back::On`] says. The user's own files were
    /// only copied.
    fn undo_on(&self, user: &str, seats_were_up: bool) {
        eprintln!("door-helper: couldn't switch the door on, putting the box back");
        if active(SEATS_UNIT) {
            status(Cmd::root(&[self.seats.as_str(), "release-owner"]).hidden());
        }
        // The hold goes only after the record: a record without a hold starts the door's units.
        let gone = remove(&[&self.mark(), &self.record(), &self.contract(user)])
            .and_then(|()| remove(&[&self.hold()]));
        if gone.is_ok() && !seats_were_up {
            unwire(&[SEATS_UNIT]);
        }
    }

    fn off(&self, user: &str, owner: Option<&Owner>, recorded: &str) -> Result<(), Stop> {
        // The door's units stay down until this off ends whole.
        touch(&self.hold())?;
        let undo = Undo {
            door: self,
            back: Some(Back::Door { user, owner }),
        };
        status(Cmd::root(&["systemctl", "stop", WEB_DOOR_UNIT, DOOR_UNIT]));
        self.legacy_mark(recorded)?;
        match owner {
            None => eprintln!("door-helper: {user} is gone, the door's files stay in {BOX_DIR}"),
            Some(owner) if self.mark().exists() => {
                for name in FILES.into_iter().filter(|&n| n != "web-password") {
                    give(owner, name)?;
                }
                give_password(owner)?;
                give_avatars(owner)?;
            }
            Some(_) => {}
        }
        undo.disarm();
        // The files are home. A repeat must not copy the door's stale ones over the user's.
        remove(&[&self.mark()])?;
        // From here only forward: the record and the hold stay until every step held.
        if !self.stand_down(user, owner) {
            return Err(die(&format!(
                "cleanup after switching off {user} failed, switch it off again"
            )));
        }
        remove(&[&self.record()])?;
        remove(&[&self.hold()])?;
        println!("door off for {user}");
        Ok(())
    }

    /// The steps of an off that move no file. Each is tried, and the result is `false` when one
    /// didn't hold:
    ///
    /// - the seats that the door placed are stopped
    /// - the owner's row and its contract are removed
    /// - the units no longer start at boot
    /// - the owner's host and console come back as the owner's own
    fn stand_down(&self, user: &str, owner: Option<&Owner>) -> bool {
        let mut held = true;
        if active(SEATS_UNIT) {
            // Seats the door placed stop with it; their accounts and profiles stay.
            held &= ok(Cmd::root(&[self.seats.as_str(), "stop-all"]).hidden());
            held &= ok(Cmd::root(&[self.seats.as_str(), "release-owner"]).hidden());
        }
        let unbound = remove(&[&self.contract(user)]).is_ok();
        held &= unwire(&[WEB_DOOR_UNIT, DOOR_UNIT, SEATS_UNIT]);
        // A contract that stays would bring their host back as the box's.
        if unbound && let Some(owner) = owner {
            status(owner.ctl(&["try-restart", "punktfunk-host.service"]));
            // Their console comes back only if it was theirs to begin with.
            if ok(owner.ctl(&["is-enabled", "--quiet", "punktfunk-web.service"])) {
                status(owner.ctl(&["start", "punktfunk-web.service"]));
            }
        }
        held && unbound
    }

    /// Every door start: the account moves whoever the owner is, then the recorded owner gives
    /// the ports. With no record the door has none to take.
    fn ports(&self, look: Look) -> Result<(), Stop> {
        use std::os::unix::fs::PermissionsExt;
        if self.door_account_group(false).is_err() {
            eprintln!("door-helper: couldn't move the door's account to {DOOR_GROUP}");
        }
        let record = self.record();
        if record.is_file() {
            std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644))
                .map_err(|e| die(&format!("chmod {}: {e}", record.display())))?;
        }
        let Some(user) = self.read_record() else {
            return Ok(());
        };
        named(&user)?;
        let owner = match self.check(false, &user, look)? {
            Admit::Off(from) => Some(Owner::of(&user, &from)),
            _ => None,
        };
        self.legacy_mark(&user)?;
        // The copy the door kept before the port file: filtered the same way, once, then removed.
        if !self.port_file().exists() {
            self.write_ports(&read(&Side::Door, &in_box("host.env")))?;
            door_rm("host.env");
        }
        match owner {
            Some(owner) => self.take_ports(&owner),
            None => Ok(()),
        }
    }

    /// The owner's `host.env` then `web.env`, at each door start: the files stay theirs, and the
    /// door never writes a port.
    fn take_ports(&self, owner: &Owner) -> Result<(), Stop> {
        // Their files out of reach, as a home that isn't mounted yet: the last copy stays.
        if !ok(Cmd::by(
            &owner.side(),
            &["test", "-d"],
            Some(&owner.own),
            &[],
        )) {
            return Ok(());
        }
        let mut text = read(&owner.side(), &owner.file("host.env"));
        // The newline keeps a host.env that lacks its last one off web.env's first line.
        text.push(b'\n');
        text.extend(read(&owner.side(), &owner.file("web.env")));
        self.write_ports(&text)
    }

    fn write_ports(&self, text: &[u8]) -> Result<(), Stop> {
        ports(text, &self.port_file()).map_err(|e| {
            eprintln!("{e}");
            Stop(1)
        })
    }
}

/// The instance name is data: one that starts with a dash would read as an option.
fn named(user: &str) -> Result<(), Stop> {
    if user.is_empty() || user.starts_with('-') {
        return Err(die(&format!(
            "'{user}' is not a user name the seats accept"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// An answer as one line, for `door_admission_table`.
    impl std::fmt::Display for Admit {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Admit::On(p) => write!(f, "on {} {}", p.uid, p.home.display()),
                Admit::Takeover { gone, to } => {
                    write!(f, "takeover {gone} {} {}", to.uid, to.home.display())
                }
                Admit::Off(p) => write!(f, "off {} {}", p.uid, p.home.display()),
                Admit::Gone => f.write_str("gone"),
            }
        }
    }

    fn passwd(name: &str, uid: u32) -> Passwd {
        Passwd {
            name: name.into(),
            uid,
            gid: uid,
            gecos: String::new(),
            home: PathBuf::from(format!("/home/{name}")),
        }
    }

    /// `switchable` over a fixed box: alice and bob in `punktfunk-update`, carol who left it,
    /// dave gone from a box whose accounts are all local, erin behind a directory service.
    fn look(name: &str, on: bool) -> Result<Switchable, BackendError> {
        super::super::accounts::check_name(name)?;
        let member = |uid| Ok(Switchable::Account(passwd(name, uid)));
        match (name, on) {
            ("alice", _) => member(1000),
            ("bob", _) => member(1001),
            ("carol", false) => member(1002),
            ("carol", true) => Err(err("not_in_update_group", "not in the group")),
            ("dave", false) => Ok(Switchable::Gone),
            ("erin", false) => Err(err("account_unknown", "can't tell if 'erin' is gone")),
            _ => Err(err(
                "account_missing",
                format!("account '{name}' doesn't exist"),
            )),
        }
    }

    /// Every answer [`admit`] gives, by the fix that decided it.
    #[test]
    fn door_admission_table() {
        type Row = (
            &'static str,
            bool,
            &'static str,
            Option<&'static str>,
            Result<&'static str, &'static str>,
        );
        let rows: [Row; 13] = [
            (
                "fresh on (1cbdc6d68)",
                true,
                "alice",
                None,
                Ok("on 1000 /home/alice"),
            ),
            (
                "repeat on",
                true,
                "alice",
                Some("alice"),
                Ok("on 1000 /home/alice"),
            ),
            (
                "on while a present owner holds it (1cbdc6d68)",
                true,
                "bob",
                Some("alice"),
                Err("door_on_for_other"),
            ),
            (
                "on over a gone owner (cfe8ee003)",
                true,
                "bob",
                Some("dave"),
                Ok("takeover dave 1001 /home/bob"),
            ),
            (
                "on while the owner lookup is uncertain (1d57cabeb)",
                true,
                "bob",
                Some("erin"),
                Err("door_on_for_other"),
            ),
            (
                "on outside the group, before the record",
                true,
                "carol",
                Some("alice"),
                Err("not_in_update_group"),
            ),
            (
                "on for a name the seats refuse (3e1b3f6c7)",
                true,
                "Bob.S",
                Some("alice"),
                Err("account_invalid"),
            ),
            (
                "off for the recorded owner (eccea23f4)",
                false,
                "alice",
                Some("alice"),
                Ok("off 1000 /home/alice"),
            ),
            (
                "off after leaving the group (1cbdc6d68)",
                false,
                "carol",
                Some("carol"),
                Ok("off 1002 /home/carol"),
            ),
            (
                "off for a gone owner (cfe8ee003)",
                false,
                "dave",
                Some("dave"),
                Ok("gone"),
            ),
            (
                "off while the owner lookup is uncertain (1d57cabeb)",
                false,
                "erin",
                Some("erin"),
                Err("account_unknown"),
            ),
            (
                "off for someone else (1cbdc6d68)",
                false,
                "bob",
                Some("alice"),
                Err("door_not_on_for"),
            ),
            (
                "off with no record",
                false,
                "bob",
                None,
                Err("door_not_on_for"),
            ),
        ];
        for (row, on, user, recorded, want) in rows {
            let got = admit(on, user, recorded, look)
                .map(|a| a.to_string())
                .map_err(|e| e.code);
            assert_eq!(got.as_deref().map_err(String::as_str), want, "{row}");
        }
    }

    /// What [`port_lines`] keeps of each input: the owner's `host.env` then `web.env`, each read
    /// whole by `cat`.
    #[test]
    fn door_port_lines_table() {
        let rows: [(&str, &[u8], &str); 24] = [
            (
                "the three keys, in the door's order (9c97fefed)",
                b"PUNKTFUNK_UI_BIND=127.0.0.1:47990\nPUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_MGMT_BIND=0.0.0.0:48001\n",
                "PUNKTFUNK_MGMT_BIND=0.0.0.0:48001\nPUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
            ),
            (
                "no other key",
                b"PUNKTFUNK_LOG=debug\nRUST_LOG=trace\nPUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_UI_PASSWORD=x\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "the last line wins",
                b"PUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_NATIVE_PORT=49000\n",
                "PUNKTFUNK_NATIVE_PORT=49000\n",
            ),
            (
                "a dropped last line drops the key",
                b"PUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_NATIVE_PORT=48 000\n",
                "",
            ),
            (
                "an empty last line drops the key",
                b"PUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_NATIVE_PORT=\n",
                "",
            ),
            ("double quotes off", b"PUNKTFUNK_NATIVE_PORT=\"48000\"\n", "PUNKTFUNK_NATIVE_PORT=48000\n"),
            ("single quotes off", b"PUNKTFUNK_MGMT_BIND='[::1]:48001'\n", "PUNKTFUNK_MGMT_BIND=[::1]:48001\n"),
            ("one pair only", b"PUNKTFUNK_NATIVE_PORT=\"\"48000\"\"\n", ""),
            ("quotes that don't match stay", b"PUNKTFUNK_NATIVE_PORT=\"48000'\n", ""),
            ("a lone quote", b"PUNKTFUNK_NATIVE_PORT=\"\n", ""),
            ("empty quotes are an empty value", b"PUNKTFUNK_NATIVE_PORT=\"\"\n", ""),
            (
                "blanks around the key and the value, and a CR",
                b" \tPUNKTFUNK_NATIVE_PORT \t= 48000 \r\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "blanks outside the quotes go, inside they stay",
                b"PUNKTFUNK_NATIVE_PORT= \"48000\" \nPUNKTFUNK_UI_BIND=\" 127.0.0.1:47990\"\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "VT and FF are blanks",
                b"\x0bPUNKTFUNK_NATIVE_PORT\x0c=\x0b48000\x0c\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "address characters",
                b"PUNKTFUNK_MGMT_BIND=fe80::1%eth0\nPUNKTFUNK_UI_BIND=box-1.lan_x:47990\n",
                "PUNKTFUNK_MGMT_BIND=fe80::1%eth0\nPUNKTFUNK_UI_BIND=box-1.lan_x:47990\n",
            ),
            (
                "anything else drops the line",
                b"PUNKTFUNK_NATIVE_PORT=$(reboot)\nPUNKTFUNK_MGMT_BIND=0.0.0.0:48001 # lan\nPUNKTFUNK_UI_BIND=a=b\n",
                "",
            ),
            ("a byte past ASCII drops the line", "PUNKTFUNK_UI_BIND=b\u{fc}:1\n".as_bytes(), ""),
            (
                "only a line that is the key",
                b"export PUNKTFUNK_NATIVE_PORT=1\n#PUNKTFUNK_NATIVE_PORT=2\nPUNKTFUNK_NATIVE_PORTS=3\npunktfunk_native_port=4\nPUNKTFUNK NATIVE_PORT=5\n",
                "",
            ),
            (
                "web.env is read after host.env (9c97fefed)",
                b"PUNKTFUNK_UI_BIND=0.0.0.0:47990\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
                "PUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
            ),
            (
                "web.env without host.env",
                b"PUNKTFUNK_UI_PASSWORD_HASH=x\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
                "PUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
            ),
            (
                "a host.env without its last newline joins web.env's first line",
                b"PUNKTFUNK_NATIVE_PORT=48000PUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
                "",
            ),
            ("a last line without a newline", b"PUNKTFUNK_NATIVE_PORT=48000", "PUNKTFUNK_NATIVE_PORT=48000\n"),
            ("nothing, an empty file", b"", ""),
            ("a NUL anywhere: grep passes no line", b"PUNKTFUNK_NATIVE_PORT=48000\n\0\n", ""),
        ];
        for (row, input, want) in rows {
            assert_eq!(port_lines(input), want, "{row}");
        }
    }

    /// The copy is root's to replace whole and every account's to read; a flood leaves it be.
    #[test]
    fn the_door_s_port_file_is_replaced_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("host.env");
        let kept = "PUNKTFUNK_NATIVE_PORT=48000\n";
        ports(kept.as_bytes(), &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), kept);
        let mode = std::fs::metadata(&to).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        let flood = std::io::repeat(b'#').take(MAX_PORT_INPUT + 1);
        assert_eq!(ports(flood, &to).unwrap_err().code, "door_ports");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), kept);
    }

    /// A doubt about the recorded owner refuses with its cause in the line. Either refusal names
    /// the owner for the console's sentence.
    #[test]
    fn a_refused_on_names_the_owner_and_any_doubt() {
        let refused = admit(true, "bob", Some("erin"), look).unwrap_err();
        assert_eq!(
            refused.message,
            "the door is on for erin: can't tell if 'erin' is gone"
        );
        assert_eq!(refused.params["owner"], "erin");
        let refused = admit(true, "bob", Some("alice"), look).unwrap_err();
        assert_eq!(refused.params["owner"], "alice");
    }

    /// A box as the switch's fake sees it: a side's `test`, `cat` and `ls` read its folder here,
    /// `getent` the accounts and groups, `systemctl` the active units, and the `fail_at`th call
    /// exits 7.
    struct World {
        home: &'static str,
        owner: Vec<(&'static str, &'static str)>,
        door: Vec<(&'static str, &'static str)>,
        has: RefCell<BTreeSet<String>>,
        active: RefCell<BTreeSet<String>>,
        door_gid: &'static str,
        web_enabled: bool,
        fail_at: Option<usize>,
        count: Cell<usize>,
    }

    /// A switch to run: its world, and root's files in the state directory before it.
    struct Scenario {
        verb: &'static str,
        user: &'static str,
        world: World,
        record: Option<&'static str>,
        mark: bool,
        hold: bool,
        library: bool,
        port_file: bool,
    }

    const ALL_HAS: [&str; 3] = [
        "passwd punktfunk",
        "group punktfunk-door",
        "group punktfunk-games",
    ];
    const ALICE: [(&str, &str); 9] = [
        ("native-cert.pem", "cert-alice"),
        ("native-key.pem", "key-alice"),
        ("uniqueid", "id-alice"),
        ("web-password", "PUNKTFUNK_UI_PASSWORD_HASH=alice-1"),
        ("host-settings.json", "{\"alice\":1}"),
        (
            "host.env",
            "PUNKTFUNK_NATIVE_PORT=48100\nPUNKTFUNK_LOG=debug",
        ),
        ("profiles/alice.png", "png-alice"),
        ("profiles/a b.png", "png-ab"),
        ("profiles/c%d.png", "png-cd"),
    ];
    const DOOR_FILES: [(&str, &str); 8] = [
        ("native-cert.pem", "cert-door"),
        ("native-key.pem", "key-door"),
        ("uniqueid", "id-door"),
        ("punktfunk1-paired.json", "[]"),
        (
            "web-password",
            "PUNKTFUNK_UI_PASSWORD_HASH=door-1\nPUNKTFUNK_UI_SECRET=s-1\nPUNKTFUNK_UI_PASSWORD_HASH=door-2",
        ),
        ("web-session-epoch", "7"),
        ("profiles/alice.png", "png-door"),
        ("profiles/c%d.png", "png-cd"),
    ];
    const STEAMOS_WEB_ENV: &str = "PUNKTFUNK_UI_BIND=127.0.0.1:47990\nPUNKTFUNK_UI_PASSWORD_HASH=alice-1\n  PUNKTFUNK_UI_SECRET=s-1\nPUNKTFUNK_LOG=debug";

    fn world(has: &[&str], active: bool) -> World {
        World {
            home: "/home/alice",
            owner: Vec::new(),
            door: Vec::new(),
            has: RefCell::new(has.iter().map(|s| s.to_string()).collect()),
            active: RefCell::new(active.then(|| SEATS_UNIT.to_owned()).into_iter().collect()),
            door_gid: DOOR_GROUP,
            web_enabled: false,
            fail_at: None,
            count: Cell::new(0),
        }
    }

    fn scenario(verb: &'static str, user: &'static str, world: World) -> Scenario {
        Scenario {
            verb,
            user,
            world,
            record: None,
            mark: false,
            hold: false,
            library: false,
            port_file: false,
        }
    }

    /// A first on: no door account or groups yet, the supervisor down.
    fn on_fresh() -> Scenario {
        let mut world = world(&[], false);
        world.owner = ALICE.to_vec();
        scenario("on", "alice", world)
    }

    /// Again, after an off whose hand-back failed: the mark and the hold are there.
    fn on_repeat() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.owner = ALICE.to_vec();
        Scenario {
            record: Some("alice"),
            mark: true,
            hold: true,
            ..scenario("on", "alice", world)
        }
    }

    /// Bob over dave, whose account is gone, with the door account in the wrong group, the
    /// library copy and a mark left in the box directory.
    fn on_takeover() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.home = "/home/bob";
        world.owner = vec![("uniqueid", "id-bob"), ("profiles.json", "{\"bob\":1}")];
        world.door = vec![("door-on", "")];
        world.door_gid = "punktfunk";
        Scenario {
            record: Some("dave"),
            mark: true,
            library: true,
            ..scenario("on", "bob", world)
        }
    }

    /// A first on of the SteamOS layout, the supervisor already up.
    fn on_steamos() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.owner = vec![("uniqueid", "id-alice"), ("web.env", STEAMOS_WEB_ENV)];
        scenario("on", "alice", world)
    }

    fn off_owner() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.owner = ALICE.to_vec();
        world.door = DOOR_FILES.to_vec();
        world.web_enabled = true;
        Scenario {
            record: Some("alice"),
            mark: true,
            ..scenario("off", "alice", world)
        }
    }

    fn off_steamos() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.owner = vec![(
            "web.env",
            "PUNKTFUNK_UI_BIND=127.0.0.1:47990\nPUNKTFUNK_UI_PASSWORD_HASH=alice-1\nPUNKTFUNK_UI_SECRET=s-1",
        )];
        world.door = DOOR_FILES.to_vec();
        Scenario {
            record: Some("alice"),
            mark: true,
            ..scenario("off", "alice", world)
        }
    }

    fn off_gone() -> Scenario {
        let mut world = world(&ALL_HAS, false);
        world.door = DOOR_FILES.to_vec();
        Scenario {
            record: Some("dave"),
            mark: true,
            ..scenario("off", "dave", world)
        }
    }

    /// A door start with the port file there and both of the owner's files.
    fn ports_owner() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.owner = vec![
            ("host.env", "PUNKTFUNK_NATIVE_PORT=48100"),
            ("web.env", "PUNKTFUNK_UI_BIND=127.0.0.1:47990"),
        ];
        world.door = vec![("host.env", "PUNKTFUNK_MGMT_BIND=0.0.0.0:48001")];
        Scenario {
            record: Some("alice"),
            port_file: true,
            ..scenario("ports", "", world)
        }
    }

    /// The first door start since the port file: the box directory still holds the last copy.
    fn ports_legacy() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.owner = vec![("host.env", "PUNKTFUNK_NATIVE_PORT=48100")];
        world.door = vec![("host.env", "PUNKTFUNK_MGMT_BIND=0.0.0.0:48001")];
        Scenario {
            record: Some("alice"),
            ..scenario("ports", "", world)
        }
    }

    fn ports_gone() -> Scenario {
        let mut world = world(&ALL_HAS, true);
        world.door = vec![("host.env", "PUNKTFUNK_MGMT_BIND=0.0.0.0:48001")];
        Scenario {
            record: Some("dave"),
            ..scenario("ports", "", world)
        }
    }

    fn ports_none() -> Scenario {
        Scenario {
            library: true,
            ..scenario("ports", "", world(&ALL_HAS, true))
        }
    }

    impl World {
        fn answer(&self, program: &str, args: &[&str]) -> (i32, String) {
            self.count.set(self.count.get() + 1);
            if self.fail_at == Some(self.count.get()) {
                return (7, String::new());
            }
            let done = |yes: bool, code: i32| (if yes { 0 } else { code }, String::new());
            match (program, args) {
                ("getent", [db, key]) if self.has.borrow().contains(&format!("{db} {key}")) => {
                    (0, format!("{key}:x:990:990::{BOX_DIR}:/usr/sbin/nologin\n"))
                }
                ("getent", _) => (2, String::new()),
                ("groupadd", [.., group]) => {
                    self.has.borrow_mut().insert(format!("group {group}"));
                    done(true, 0)
                }
                ("useradd", [.., name]) => {
                    self.has.borrow_mut().insert(format!("passwd {name}"));
                    done(true, 0)
                }
                ("id", ["-gn", _]) => (0, format!("{}\n", self.door_gid)),
                ("systemctl", ["is-active", _, unit]) => {
                    done(self.active.borrow().contains(*unit), 3)
                }
                ("systemctl", ["start" | "restart", units @ ..]) => {
                    let started = units.iter().filter(|u| **u != "--no-block");
                    self.active
                        .borrow_mut()
                        .extend(started.map(|u| u.to_string()));
                    done(true, 0)
                }
                ("systemctl", ["stop" | "disable", units @ ..]) => {
                    self.active
                        .borrow_mut()
                        .retain(|u| !units.contains(&u.as_str()));
                    done(true, 0)
                }
                ("runuser", ["-u", _, "--", command @ ..]) => self.by(command),
                ("tail", _) => {
                    let lines = self.tree("/var/lib/punktfunk/web-password").unwrap_or("");
                    let last = lines.lines().rev().find(|l| {
                        let l = l.trim_start();
                        l.starts_with("PUNKTFUNK_UI_PASSWORD=")
                            || l.starts_with("PUNKTFUNK_UI_PASSWORD_HASH=")
                    });
                    (0, last.map(|l| format!("{l}\n")).unwrap_or_default())
                }
                _ => done(true, 0),
            }
        }

        /// A command as one side: reads answer from that side's folder.
        fn by(&self, command: &[&str]) -> (i32, String) {
            let no = (1, String::new());
            match command {
                ["test", "-f", path] => (i32::from(self.tree(path).is_none()), String::new()),
                ["test", "-d", path] => (i32::from(!self.dir(path)), String::new()),
                ["cat", "--", path] => self.tree(path).map_or(no, |c| (0, format!("{c}\n"))),
                ["ls", "-1", "--", dir] => {
                    let names: BTreeSet<&str> = self
                        .files(dir)
                        .filter_map(|(rel, _)| rel.strip_prefix(&format!("{}/", self.rel(dir).1)))
                        .filter(|name| !name.contains('/'))
                        .collect();
                    if names.is_empty() {
                        return (2, String::new());
                    }
                    (0, names.into_iter().map(|n| format!("{n}\n")).collect())
                }
                ["env", _, "systemctl", "--user", "is-enabled", ..] if !self.web_enabled => no,
                _ => (0, String::new()),
            }
        }

        /// Which folder `path` is in, and its name there.
        fn rel<'p>(&self, path: &'p str) -> (bool, &'p str) {
            let own = format!("{}/.config/punktfunk", self.home);
            match path.strip_prefix(&own) {
                Some(rel) => (true, rel.trim_start_matches('/')),
                None => (
                    false,
                    path.trim_start_matches(BOX_DIR).trim_start_matches('/'),
                ),
            }
        }

        fn files(&self, path: &str) -> impl Iterator<Item = &(&'static str, &'static str)> {
            if self.rel(path).0 {
                self.owner.iter()
            } else {
                self.door.iter()
            }
        }

        fn tree(&self, path: &str) -> Option<&'static str> {
            let rel = self.rel(path).1;
            self.files(path)
                .find(|(name, _)| *name == rel)
                .map(|(_, c)| *c)
        }

        fn dir(&self, path: &str) -> bool {
            let (owner, rel) = self.rel(path);
            (owner && rel.is_empty())
                || self
                    .files(path)
                    .any(|(n, _)| n.starts_with(&format!("{rel}/")))
        }
    }

    type Make = fn() -> Scenario;

    /// What a switch left: its exit code, every call, root's files, the owner's contract, and
    /// the supervisor.
    struct Ran {
        exit: u8,
        calls: Vec<String>,
        record: Option<String>,
        mark: bool,
        hold: bool,
        off_held: bool,
        contract: bool,
        seats: bool,
        ports: Option<String>,
    }

    fn switch(scenario: Scenario) -> Ran {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        if let Some(user) = scenario.record {
            std::fs::write(state.join("owner"), format!("{user}\n")).unwrap();
        }
        if scenario.mark {
            std::fs::write(state.join("door-on"), "").unwrap();
        }
        if scenario.hold {
            std::fs::write(state.join("door-hold"), "").unwrap();
        }
        let contract = dir
            .path()
            .join("run")
            .join(format!("{}.env", scenario.user));
        std::fs::create_dir(dir.path().join("run")).unwrap();
        std::fs::write(&contract, "").unwrap();
        if scenario.library {
            std::fs::create_dir(state.join("library")).unwrap();
        }
        if scenario.port_file {
            std::fs::write(state.join("host.env"), "PUNKTFUNK_NATIVE_PORT=1\n").unwrap();
        }
        let path = |p: PathBuf| p.into_os_string().into_string().unwrap();
        let door = Door {
            state: path(state.clone()),
            lock: path(dir.path().join("lock")),
            contracts: path(dir.path().join("run")),
            seats: "punktfunk-seats".into(),
        };
        let world = Rc::new(scenario.world);
        let fake = Rc::clone(&world);
        let _fake = super::super::fake::install(move |p, a| fake.answer(p, a));
        let exit = match door.switch(scenario.verb, scenario.user, &look) {
            Ok(()) => 0,
            Err(Stop(code)) => code,
        };
        let calls = super::super::fake::calls()
            .into_iter()
            .map(|c| {
                c.replace(&door.state, STATE)
                    .replace(&door.contracts, "/run/punktfunk/seats")
            })
            .collect();
        let read = |name| std::fs::read_to_string(state.join(name)).ok();
        Ran {
            exit,
            calls,
            record: read("owner").map(|r| r.trim_end().to_owned()),
            mark: state.join("door-on").exists(),
            hold: state.join("door-hold").exists(),
            off_held: off_held(&state),
            contract: contract.exists(),
            seats: world.active.borrow().contains(SEATS_UNIT),
            ports: read("host.env"),
        }
    }

    const STATE: &str = "/var/lib/punktfunk-seats";

    /// A recorded table, with this machine's `nologin` where the recording's machine had its own.
    fn recorded(table: &[&str]) -> Vec<String> {
        table
            .iter()
            .map(|c| {
                c.replace(
                    "--shell /usr/sbin/nologin",
                    &format!("--shell {}", nologin()),
                )
            })
            .collect()
    }

    /// The calls of each on, in order, against a [`World`].
    #[test]
    fn the_door_s_on_runs_exactly_these() {
        let rows: [(&str, Make, &[&str], &str); 4] = [
            ("fresh", on_fresh, ON_FRESH, "alice"),
            ("repeat", on_repeat, ON_REPEAT, "alice"),
            ("takeover", on_takeover, ON_TAKEOVER, "bob"),
            ("SteamOS password", on_steamos, ON_STEAMOS, "alice"),
        ];
        for (row, scenario, want, owner) in rows {
            let ran = switch(scenario());
            assert_eq!(ran.calls, recorded(want), "{row}");
            assert_eq!(ran.exit, 0, "{row}");
            assert_eq!(ran.record.as_deref(), Some(owner), "{row}");
            assert!(ran.mark && ran.seats && !ran.hold, "{row}");
        }
    }

    /// What root's `grep` lets back from the door's password file, run for real. The lines the
    /// console, `web-init.sh` and the SteamOS installer write pass. A line that sets another
    /// variable, or that a shell sourcing the file would run, does not.
    #[test]
    fn only_a_plain_password_line_comes_back_from_the_door() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let rows = [
            ("PUNKTFUNK_UI_PASSWORD=Ab3dEf9hIjKlMnOpQrSt", true),
            (
                "PUNKTFUNK_UI_PASSWORD_HASH='$argon2id$v=19$m=65536,t=2,p=1$c2FsdA$aGFzaA+/='",
                true,
            ),
            ("  PUNKTFUNK_UI_SECRET=s3cret \t", true),
            ("LD_PRELOAD=/var/lib/punktfunk/x.so", false),
            ("PUNKTFUNK_UI_BIND=0.0.0.0", false),
            ("PUNKTFUNK_UI_PASSWORD_EXTRA=1", false),
            ("PUNKTFUNK_UI_PASSWORD_HASH=x;touch /tmp/planted", false),
            ("PUNKTFUNK_UI_PASSWORD=$(id)", false),
            ("PUNKTFUNK_UI_PASSWORD=`id`", false),
            ("PUNKTFUNK_UI_PASSWORD=a b", false),
            ("PUNKTFUNK_UI_PASSWORD=\"$(id)\"", false),
            ("PUNKTFUNK_UI_PASSWORD='a' LD_PRELOAD=x", false),
            ("PUNKTFUNK_UI_PASSWORD='a'\\''b'", false),
            ("PUNKTFUNK_UI_PASSWORD='a\u{1}b'", false),
        ];
        let input: String = rows.iter().map(|(line, _)| format!("{line}\n")).collect();
        let mut grep = Command::new("grep")
            .args(["-a", "-E", &safe_pattern("PASSWORD(_HASH)?|SECRET")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = grep.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
        drop(stdin);
        let out = grep.wait_with_output().unwrap();
        let kept: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
        let want: Vec<&str> = rows.iter().filter(|r| r.1).map(|r| r.0).collect();
        assert_eq!(kept, want);
    }

    #[test]
    fn the_door_s_off_runs_exactly_these() {
        let rows: [(&str, Make, &[&str]); 3] = [
            ("the owner", off_owner, OFF_OWNER),
            ("the owner, SteamOS password", off_steamos, OFF_STEAMOS),
            ("a gone owner", off_gone, OFF_GONE),
        ];
        for (row, scenario, want) in rows {
            let ran = switch(scenario());
            assert_eq!(ran.calls, recorded(want), "{row}");
            assert_eq!(ran.exit, 0, "{row}");
            assert_eq!(
                (ran.record, ran.mark, ran.hold, ran.contract, ran.seats),
                (None, false, false, false, false),
                "{row}"
            );
        }
    }

    /// Each door start's ports; the port file holds what [`port_lines`] made of the recorded input.
    #[test]
    fn the_door_s_ports_run_exactly_these() {
        let rows: [(&str, Make, &[&str], Option<&str>); 4] = [
            (
                "the owner's, over the last copy",
                ports_owner,
                PORTS,
                Some("PUNKTFUNK_NATIVE_PORT=48100\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n"),
            ),
            (
                "the box's legacy copy, then the owner's",
                ports_legacy,
                PORTS_LEGACY,
                Some("PUNKTFUNK_NATIVE_PORT=48100\n"),
            ),
            (
                "a gone owner: the legacy copy only",
                ports_gone,
                PORTS_GONE,
                Some("PUNKTFUNK_MGMT_BIND=0.0.0.0:48001\n"),
            ),
            ("no record: the account only", ports_none, PORTS_NONE, None),
        ];
        for (row, scenario, want, ports) in rows {
            let ran = switch(scenario());
            assert_eq!(ran.calls, recorded(want), "{row}");
            assert_eq!(ran.exit, 0, "{row}");
            assert_eq!(ran.ports.as_deref(), ports, "{row}");
        }
    }

    /// The failed call's number, then the helper's exit code, record, mark and supervisor after it.
    type Fail = (usize, u8, Option<&'static str>, bool, bool);

    /// Every call of a first on and of an off fails in turn. A hand-over's read that fails ends
    /// its step, and its move is a call of its own. A first on that fails before the door's
    /// ports move leaves no record, mark or hold, asks the supervisor to release the owner's row
    /// and leaves it as it was. An off whose hand-back fails ends as an off does, the door off and
    /// the owner's host back, and keeps the record, the mark and the hold.
    #[test]
    fn a_failed_step_puts_the_box_back() {
        let rows: [(&str, Make, &[Fail]); 3] = [
            ("fresh on, supervisor down", on_fresh, ON_FRESH_FAILS),
            ("SteamOS on, supervisor up", on_steamos, ON_STEAMOS_FAILS),
            ("off", off_owner, OFF_OWNER_FAILS),
        ];
        for (row, make, fails) in rows {
            assert_eq!(
                fails.len(),
                switch(make()).calls.len(),
                "{row}: every call fails once"
            );
            let up = make().world.active.borrow().contains(SEATS_UNIT);
            for &(n, exit, record, mark, seats) in fails {
                let mut scenario = make();
                scenario.world.fail_at = Some(n);
                let ran = switch(scenario);
                let failed = &ran.calls[n - 1];
                let got = (ran.exit, ran.record.as_deref(), ran.mark, ran.seats);
                assert_eq!(got, (exit, record, mark, seats), "{row}: {failed}");
                if exit != 0 && record.is_none() {
                    assert_eq!(seats, up, "{row}: {failed}");
                    let asked = |verb: &str| ran.calls.iter().any(|c| c.ends_with(verb));
                    let released = !asked(" adopt-owner alice") || asked(" release-owner");
                    assert!(released, "{row}: {failed}");
                }
                if make().verb == "off" {
                    assert_eq!(ran.hold, record.is_some(), "{row}: {failed}");
                } else {
                    assert!(!ran.hold, "{row}: {failed}");
                }
                if exit != 0 && mark && make().verb == "off" {
                    assert!(ran.calls.ends_with(&stand_down()), "{row}: {failed}");
                }
            }
        }
    }

    /// A call that names a path in a home or in the box directory runs as that side. Root names
    /// the box directory itself only to make it and to give the door account its home.
    #[test]
    fn no_door_step_touches_their_path_as_root() {
        let all: [Make; 11] = [
            on_fresh,
            on_repeat,
            on_takeover,
            on_steamos,
            off_owner,
            off_steamos,
            off_gone,
            ports_owner,
            ports_legacy,
            ports_gone,
            ports_none,
        ];
        let mut runs = Vec::new();
        for make in all {
            let total = switch(make()).calls.len();
            for n in 0..=total {
                let mut scenario = make();
                scenario.world.fail_at = (n > 0).then_some(n);
                runs.push(switch(scenario));
            }
        }
        for call in runs.iter().flat_map(|r| &r.calls) {
            let theirs = call
                .split(' ')
                .any(|word| word.starts_with("/home/") || word.starts_with(&format!("{BOX_DIR}/")));
            if theirs {
                assert!(call.starts_with("runuser -u "), "{call}");
            } else if call.split(' ').any(|word| word == BOX_DIR)
                && !call.starts_with("runuser -u ")
            {
                let making = call.starts_with("install -d ") || call.starts_with("useradd ");
                assert!(making, "{call}");
            }
        }
    }

    /// The first call that is `call`, counted from one.
    fn nth(calls: &[String], call: &str) -> usize {
        calls.iter().position(|c| c == call).unwrap() + 1
    }

    /// A take whose read fails in a first on leaves the box's copy as it was: the temp file goes,
    /// nothing moves, and the on is put back.
    #[test]
    fn a_take_whose_read_fails_replaces_nothing() {
        let own = "/home/alice/.config/punktfunk/native-key.pem";
        let theirs = "/var/lib/punktfunk/native-key.pem";
        let mut scenario = on_fresh();
        scenario.world.fail_at = Some(nth(
            &switch(on_fresh()).calls,
            &format!("runuser -u alice -- cat -- {own}"),
        ));
        let ran = switch(scenario);
        let drop = format!(r#"runuser -u punktfunk -- sh -c rm -f "$1.tmp" sh {theirs}"#);
        let moved = format!(r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh {theirs}"#);
        assert!(ran.calls.contains(&drop), "{:#?}", ran.calls);
        assert!(!ran.calls.contains(&moved), "{:#?}", ran.calls);
        assert_eq!(
            (ran.exit, ran.record, ran.mark, ran.seats),
            (7, None, false, false)
        );
    }

    /// A takeover puts nothing back, so a step that fails halfway through the copy leaves what a
    /// kill leaves. The hold keeps the door's units off a box with half of the member's files,
    /// and the owner's host doesn't read it as an off that didn't finish.
    #[test]
    fn an_on_cut_short_leaves_the_door_held() {
        let mut scenario = on_takeover();
        let read = "runuser -u bob -- cat -- /home/bob/.config/punktfunk/profiles.json";
        scenario.world.fail_at = Some(nth(&switch(on_takeover()).calls, read));
        let ran = switch(scenario);
        assert_eq!(
            (ran.exit, ran.record.as_deref(), ran.mark, ran.hold),
            (7, Some("bob"), false, true)
        );
        assert!(!ran.off_held);
    }

    /// What a killed on leaves, the record and the hold with no mark, is finished by the next
    /// on: it copies the owner's files as a first on does and lifts the hold.
    #[test]
    fn an_on_after_a_killed_one_copies_and_lifts_the_hold() {
        let mut world = world(&ALL_HAS, true);
        world.owner = ALICE.to_vec();
        let ran = switch(Scenario {
            record: Some("alice"),
            hold: true,
            ..scenario("on", "alice", world)
        });
        let fresh = recorded(ON_FRESH);
        let from = nth(
            &fresh,
            "runuser -u alice -- test -f /home/alice/.config/punktfunk/native-cert.pem",
        );
        let to = nth(&fresh, "systemctl enable punktfunk-seats.service");
        let copy = &fresh[from - 1..to - 1];
        let copied = ran.calls.windows(copy.len()).any(|w| w == copy);
        assert!(copied, "{:#?}", ran.calls);
        assert_eq!(
            (ran.exit, ran.record.as_deref(), ran.mark, ran.hold),
            (0, Some("alice"), true, false)
        );
        assert!(ran.seats);
    }

    /// A listing of the door's avatars past the cap ends the off as an unreadable file does: the
    /// door stays off, and the record, the mark and the hold stay for the next off.
    #[test]
    fn an_avatar_listing_past_the_cap_ends_the_off() {
        let flood = format!("profiles/{}", "a".repeat(MAX_LISTING as usize));
        let mut scenario = off_owner();
        let world = &mut scenario.world;
        world.door.push((Box::leak(flood.into_boxed_str()), "png"));
        let ran = switch(scenario);
        assert_eq!(
            (ran.exit, ran.record.as_deref(), ran.mark, ran.hold),
            (1, Some("alice"), true, true)
        );
        assert!(ran.calls.ends_with(&stand_down()));
    }

    /// The owner's side of a hand-back writes at most the cap. A file past it fails the
    /// hand-over, so nothing moves; a file at the cap arrives whole. Run for real.
    #[test]
    fn a_hand_back_past_the_cap_fails() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("profiles/alice.png");
        let to = to.to_str().unwrap();
        let write = || Cmd::root(&["sh", "-c", TO_OWNER, "sh", to, "4"]);
        let feed = |bytes| Cmd::root(&["printf", bytes]);
        assert!(hands::pipe_held(&[feed("12345"), write()]).is_err());
        assert!(hands::pipe_held(&[Cmd::root(&["yes"]), write()]).is_err());
        assert_eq!(hands::pipe_held(&[feed("1234"), write()]), Ok(()));
        assert_eq!(std::fs::read(format!("{to}.tmp")).unwrap(), b"1234");
    }

    /// What an off runs once the files are home, a failed hand-back's too: the seats released,
    /// the units off, the owner's host and console back.
    fn stand_down() -> Vec<String> {
        let all = recorded(OFF_OWNER);
        let from = nth(&all, "systemctl is-active --quiet punktfunk-seats.service");
        all[from - 1..].to_vec()
    }

    /// A give whose read fails in an off leaves the owner's file as it was, keeps the record, the
    /// mark and the hold, and leaves the door off and the owner's host back.
    #[test]
    fn a_give_whose_read_fails_replaces_nothing() {
        let own = "/home/alice/.config/punktfunk/uniqueid";
        let mut scenario = off_owner();
        let read = "runuser -u punktfunk -- cat -- /var/lib/punktfunk/uniqueid";
        scenario.world.fail_at = Some(nth(&switch(off_owner()).calls, read));
        let ran = switch(scenario);
        let drop = format!(r#"runuser -u alice -- sh -c rm -f "$1.tmp" sh {own}"#);
        let moved = format!(r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh {own}"#);
        assert!(ran.calls.contains(&drop), "{:#?}", ran.calls);
        assert!(!ran.calls.contains(&moved), "{:#?}", ran.calls);
        assert!(ran.calls.ends_with(&stand_down()), "{:#?}", ran.calls);
        let starts = |c: &String| c.starts_with("systemctl start") && c.contains(DOOR_UNIT);
        assert!(!ran.calls.iter().any(starts), "{:#?}", ran.calls);
        assert_eq!(
            (ran.exit, ran.record.as_deref(), ran.mark, ran.hold),
            (7, Some("alice"), true, true)
        );
        assert!(!ran.contract && !ran.seats && ran.off_held);
    }

    /// A first on gives the box this member's files alone: each one they lack, and every avatar,
    /// goes from the box before theirs arrive. Over a gone owner, what they lack stays.
    #[test]
    fn a_first_on_drops_the_box_files_the_owner_lacks() {
        let rm = |name: &str| format!("runuser -u punktfunk -- rm -f {BOX_DIR}/{name}");
        let avatars = format!("runuser -u punktfunk -- rm -rf {BOX_DIR}/profiles");
        let mut stale = on_fresh();
        stale.world.door = vec![
            ("punktfunk1-paired.json", "[\"carol's phone\"]"),
            ("web-session-key", "carol"),
            ("profiles/carol.png", "png-carol"),
        ];
        let ran = switch(stale);
        assert_eq!(ran.exit, 0);
        let lacks = [
            "punktfunk1-paired.json",
            "profiles.json",
            "web-session-key",
            "web-session-epoch",
        ];
        for name in FILES.into_iter().chain(["host-settings.json"]) {
            let has = ALICE.iter().any(|(n, _)| *n == name);
            assert_eq!(ran.calls.contains(&rm(name)), !has, "{name}");
            assert_eq!(lacks.contains(&name), !has, "{name}");
        }
        let emptied = nth(&ran.calls, &avatars);
        let made = format!("runuser -u punktfunk -- mkdir -p -m 0700 {BOX_DIR}/profiles");
        assert!(emptied < nth(&ran.calls, &made), "{:#?}", ran.calls);

        let over_gone = switch(on_takeover());
        let removes = |c: &String| c.contains(" -- rm -f ") || c == &avatars;
        let kept: Vec<&String> = over_gone.calls.iter().filter(|c| removes(c)).collect();
        assert_eq!(kept, [&rm("door-on")], "a gone owner's files stay");
    }

    /// With no fake installed the hands run real programs, as the shell runs a pipeline without
    /// `pipefail`: stdin on `/dev/null`, the last stage's code and bytes, a closed pipe past the
    /// limit.
    #[test]
    fn the_hands_run_a_pipeline_as_the_shell_does() {
        let sh = |script: &str| Cmd::root(&["sh", "-c", script]);
        let last = Cmd::root(&["tail", "-n1"]).kept(64);
        assert_eq!(
            hands::pipe(&[sh("printf 'a\\nb\\n'"), last]),
            (0, b"b\n".to_vec())
        );
        assert_eq!(hands::pipe(&[sh("exit 3"), sh("cat")]).0, 0);
        assert_eq!(hands::pipe(&[sh("true"), sh("exit 7")]).0, 7);
        assert_eq!(hands::pipe_held(&[sh("exit 3"), sh("cat")]), Err(3));
        let nothing = Cmd::root(&["grep", "x"]).may_match_nothing();
        assert_eq!(hands::pipe_held(&[sh("true"), nothing]), Ok(()));
        assert_eq!(hands::pipe(&[sh("kill -TERM $$")]).0, 128 + 15);
        let missing = Cmd::root(&["/nonexistent/door-program"]).quiet();
        assert_eq!(hands::pipe(&[missing]).0, 127);
        assert_eq!(hands::pipe(&[Cmd::root(&["cat"]).kept(8)]), (0, Vec::new()));
        let flood = hands::pipe(&[Cmd::root(&["yes"]).kept(10)]).1;
        assert_eq!(flood, b"y\ny\ny\ny\ny\n");
    }

    // The programs each switch runs, in order. Its own file work is not among them, and each
    // hand-over's write and its move are separate calls.
    // A first on also removes the box files the owner lacks.
    const ON_FRESH: &[&str] = &[
        "systemctl is-active --quiet punktfunk-seats.service",
        "getent group punktfunk-door",
        "groupadd --system punktfunk-door",
        "getent passwd punktfunk",
        "useradd --system --no-user-group --gid punktfunk-door --home-dir /var/lib/punktfunk --shell /usr/sbin/nologin punktfunk",
        "install -d -m 0711 -o punktfunk -g punktfunk-door /var/lib/punktfunk",
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent group punktfunk-games",
        "groupadd --system punktfunk-games",
        "usermod -aG punktfunk-games alice",
        "install -d -m 0711 -o root -g root /var/lib/punktfunk-seats",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/native-cert.pem",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/native-cert.pem",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/native-cert.pem"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/native-cert.pem"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/native-key.pem",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/native-key.pem",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/native-key.pem"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/native-key.pem"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/uniqueid",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/uniqueid",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/uniqueid"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/uniqueid"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/punktfunk1-paired.json",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/punktfunk1-paired.json",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/profiles.json",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/profiles.json",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-password",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/web-password",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/web-password"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/web-password"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-session-key",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/web-session-key",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-session-epoch",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/web-session-epoch",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/host-settings.json",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/host-settings.json",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/host-settings.json"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/host-settings.json"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web.env",
        "runuser -u punktfunk -- rm -rf /var/lib/punktfunk/profiles",
        "runuser -u punktfunk -- mkdir -p -m 0700 /var/lib/punktfunk/profiles",
        "runuser -u alice -- ls -1 -- /home/alice/.config/punktfunk/profiles",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/profiles/a",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/profiles/b.png",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/profiles/alice.png",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/profiles/alice.png",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/profiles/alice.png"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/profiles/alice.png"#,
        "systemctl enable punktfunk-seats.service",
        "systemctl start punktfunk-seats.service",
        "punktfunk-seats adopt-owner alice",
        "systemctl enable punktfunk-door.service",
        "systemctl enable punktfunk-web-door.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user stop punktfunk-web.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user try-restart punktfunk-host.service",
        "systemctl restart punktfunk-door.service punktfunk-web-door.service",
    ];

    const ON_REPEAT: &[&str] = &[
        "systemctl is-active --quiet punktfunk-seats.service",
        "getent group punktfunk-door",
        "getent passwd punktfunk",
        "install -d -m 0711 -o punktfunk -g punktfunk-door /var/lib/punktfunk",
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent group punktfunk-games",
        "usermod -aG punktfunk-games alice",
        "install -d -m 0711 -o root -g root /var/lib/punktfunk-seats",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "systemctl enable punktfunk-seats.service",
        "systemctl start punktfunk-seats.service",
        "punktfunk-seats adopt-owner alice",
        "systemctl enable punktfunk-door.service",
        "systemctl enable punktfunk-web-door.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user stop punktfunk-web.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user try-restart punktfunk-host.service",
        "systemctl restart punktfunk-door.service punktfunk-web-door.service",
    ];

    const ON_TAKEOVER: &[&str] = &[
        "systemctl is-active --quiet punktfunk-seats.service",
        "getent group punktfunk-door",
        "getent passwd punktfunk",
        "install -d -m 0711 -o punktfunk -g punktfunk-door /var/lib/punktfunk",
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "usermod -g punktfunk-door punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "find /var/lib/punktfunk-seats/library -xdev ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent group punktfunk-games",
        "usermod -aG punktfunk-games bob",
        "install -d -m 0711 -o root -g root /var/lib/punktfunk-seats",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/native-cert.pem",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/native-key.pem",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/uniqueid",
        "runuser -u bob -- cat -- /home/bob/.config/punktfunk/uniqueid",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/uniqueid"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/uniqueid"#,
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/punktfunk1-paired.json",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/profiles.json",
        "runuser -u bob -- cat -- /home/bob/.config/punktfunk/profiles.json",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/profiles.json"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/profiles.json"#,
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/web-password",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/web-session-key",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/web-session-epoch",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/host-settings.json",
        "runuser -u bob -- test -f /home/bob/.config/punktfunk/web.env",
        "runuser -u punktfunk -- mkdir -p -m 0700 /var/lib/punktfunk/profiles",
        "runuser -u bob -- ls -1 -- /home/bob/.config/punktfunk/profiles",
        "systemctl enable punktfunk-seats.service",
        "systemctl start punktfunk-seats.service",
        "punktfunk-seats adopt-owner bob",
        "systemctl enable punktfunk-door.service",
        "systemctl enable punktfunk-web-door.service",
        "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 systemctl --user stop punktfunk-web.service",
        "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 systemctl --user try-restart punktfunk-host.service",
        "systemctl restart punktfunk-door.service punktfunk-web-door.service",
    ];

    const ON_STEAMOS: &[&str] = &[
        "systemctl is-active --quiet punktfunk-seats.service",
        "getent group punktfunk-door",
        "getent passwd punktfunk",
        "install -d -m 0711 -o punktfunk -g punktfunk-door /var/lib/punktfunk",
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent group punktfunk-games",
        "usermod -aG punktfunk-games alice",
        "install -d -m 0711 -o root -g root /var/lib/punktfunk-seats",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/native-cert.pem",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/native-cert.pem",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/native-key.pem",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/native-key.pem",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/uniqueid",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/uniqueid",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/uniqueid"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/uniqueid"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/punktfunk1-paired.json",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/punktfunk1-paired.json",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/profiles.json",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/profiles.json",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-password",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/web-password",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-session-key",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/web-session-key",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-session-epoch",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/web-session-epoch",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/host-settings.json",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/host-settings.json",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web.env",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-password",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/web.env",
        "grep -E ^[[:space:]]*PUNKTFUNK_UI_PASSWORD(_HASH)?=|^[[:space:]]*PUNKTFUNK_UI_SECRET=",
        r#"runuser -u punktfunk -- sh -c umask 077; cat >"$1.tmp" sh /var/lib/punktfunk/web-password"#,
        r#"runuser -u punktfunk -- sh -c mv -f "$1.tmp" "$1" sh /var/lib/punktfunk/web-password"#,
        "runuser -u punktfunk -- rm -rf /var/lib/punktfunk/profiles",
        "runuser -u punktfunk -- mkdir -p -m 0700 /var/lib/punktfunk/profiles",
        "runuser -u alice -- ls -1 -- /home/alice/.config/punktfunk/profiles",
        "systemctl enable punktfunk-seats.service",
        "systemctl start punktfunk-seats.service",
        "punktfunk-seats adopt-owner alice",
        "systemctl enable punktfunk-door.service",
        "systemctl enable punktfunk-web-door.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user stop punktfunk-web.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user try-restart punktfunk-host.service",
        "systemctl restart punktfunk-door.service punktfunk-web-door.service",
    ];

    const OFF_OWNER: &[&str] = &[
        "systemctl stop punktfunk-web-door.service punktfunk-door.service",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/native-cert.pem",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/native-cert.pem",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/native-cert.pem 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/native-cert.pem"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/native-key.pem",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/native-key.pem",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/native-key.pem 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/native-key.pem"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/uniqueid",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/uniqueid",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/uniqueid 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/uniqueid"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/punktfunk1-paired.json",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/punktfunk1-paired.json",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/punktfunk1-paired.json 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/punktfunk1-paired.json"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/profiles.json",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/web-session-key",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/web-session-epoch",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/web-session-epoch",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/web-session-epoch 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/web-session-epoch"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web.env",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/web-password",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/web-password",
        "grep -aq -E ^[[:blank:]]*PUNKTFUNK_UI_(PASSWORD(_HASH)?|SECRET)=([A-Za-z0-9_.,:=+/@%^-]{0,255}|'[^'[:cntrl:]]{0,255}')[[:blank:]]*$",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/web-password",
        "grep -a -E ^[[:blank:]]*PUNKTFUNK_UI_(PASSWORD(_HASH)?|SECRET)=([A-Za-z0-9_.,:=+/@%^-]{0,255}|'[^'[:cntrl:]]{0,255}')[[:blank:]]*$",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/web-password 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/web-password"#,
        "runuser -u punktfunk -- ls -1 -- /var/lib/punktfunk/profiles",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/profiles/alice.png",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/profiles/alice.png",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/profiles/alice.png 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/profiles/alice.png"#,
        "systemctl is-active --quiet punktfunk-seats.service",
        "punktfunk-seats stop-all",
        "punktfunk-seats release-owner",
        "systemctl disable --now punktfunk-web-door.service punktfunk-door.service punktfunk-seats.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user try-restart punktfunk-host.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user is-enabled --quiet punktfunk-web.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user start punktfunk-web.service",
    ];

    const OFF_STEAMOS: &[&str] = &[
        "systemctl stop punktfunk-web-door.service punktfunk-door.service",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/native-cert.pem",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/native-cert.pem",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/native-cert.pem 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/native-cert.pem"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/native-key.pem",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/native-key.pem",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/native-key.pem 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/native-key.pem"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/uniqueid",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/uniqueid",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/uniqueid 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/uniqueid"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/punktfunk1-paired.json",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/punktfunk1-paired.json",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/punktfunk1-paired.json 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/punktfunk1-paired.json"#,
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/profiles.json",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/web-session-key",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/web-session-epoch",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/web-session-epoch",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/web-session-epoch 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/web-session-epoch"#,
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web.env",
        "runuser -u alice -- test -f /home/alice/.config/punktfunk/web-password",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/web-password",
        "grep -a -E ^[[:blank:]]*PUNKTFUNK_UI_(PASSWORD(_HASH)?)=([A-Za-z0-9_.,:=+/@%^-]{0,255}|'[^'[:cntrl:]]{0,255}')[[:blank:]]*$",
        "tail -n1",
        r#"runuser -u alice -- sh -c umask 077; { grep -vE "$3" "$1" || true; } >"$1.tmp" && printf "%s\n" "$2" >>"$1.tmp" && mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/web.env PUNKTFUNK_UI_PASSWORD_HASH=door-2 ^[[:space:]]*PUNKTFUNK_UI_PASSWORD(_HASH)?="#,
        "runuser -u punktfunk -- ls -1 -- /var/lib/punktfunk/profiles",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/profiles/alice.png",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/profiles/alice.png",
        r#"runuser -u alice -- sh -c umask 077; mkdir -p "$(dirname "$1")"; head -c "$(($2 + 1))" >"$1.tmp" && test "$(wc -c <"$1.tmp")" -le "$2" sh /home/alice/.config/punktfunk/profiles/alice.png 8388608"#,
        r#"runuser -u alice -- sh -c mv -f "$1.tmp" "$1" sh /home/alice/.config/punktfunk/profiles/alice.png"#,
        "systemctl is-active --quiet punktfunk-seats.service",
        "punktfunk-seats stop-all",
        "punktfunk-seats release-owner",
        "systemctl disable --now punktfunk-web-door.service punktfunk-door.service punktfunk-seats.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user try-restart punktfunk-host.service",
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user is-enabled --quiet punktfunk-web.service",
    ];

    const OFF_GONE: &[&str] = &[
        "systemctl stop punktfunk-web-door.service punktfunk-door.service",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "systemctl is-active --quiet punktfunk-seats.service",
        "systemctl disable --now punktfunk-web-door.service punktfunk-door.service punktfunk-seats.service",
    ];

    const PORTS: &[&str] = &[
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u alice -- test -d /home/alice/.config/punktfunk",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/host.env",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/web.env",
    ];

    const PORTS_LEGACY: &[&str] = &[
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/host.env",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/host.env",
        "runuser -u alice -- test -d /home/alice/.config/punktfunk",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/host.env",
        "runuser -u alice -- cat -- /home/alice/.config/punktfunk/web.env",
    ];

    const PORTS_GONE: &[&str] = &[
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "getent passwd punktfunk",
        "runuser -u punktfunk -- test -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/door-on",
        "runuser -u punktfunk -- cat -- /var/lib/punktfunk/host.env",
        "runuser -u punktfunk -- rm -f /var/lib/punktfunk/host.env",
    ];

    const PORTS_NONE: &[&str] = &[
        "getent passwd punktfunk",
        "getent group punktfunk-door",
        "id -gn punktfunk",
        "runuser -u punktfunk -- find /var/lib/punktfunk -xdev -user punktfunk ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
        "find /var/lib/punktfunk-seats/library -xdev ! -group punktfunk-door -exec chgrp -h punktfunk-door {} +",
    ];

    // The outcome with each call failing in turn.
    const ON_FRESH_FAILS: &[Fail] = &[
        (1, 0, Some("alice"), true, true),
        (2, 0, Some("alice"), true, true),
        (3, 7, None, false, false),
        (4, 0, Some("alice"), true, true),
        (5, 7, None, false, false),
        (6, 7, None, false, false),
        (7, 0, Some("alice"), true, true),
        (8, 0, Some("alice"), true, true),
        (9, 0, Some("alice"), true, true),
        (10, 0, Some("alice"), true, true),
        (11, 0, Some("alice"), true, true),
        (12, 7, None, false, false),
        (13, 7, None, false, false),
        (14, 7, None, false, false),
        (15, 0, Some("alice"), true, true),
        (16, 0, Some("alice"), true, true),
        (17, 0, Some("alice"), true, true),
        (18, 7, None, false, false),
        (19, 7, None, false, false),
        (20, 7, None, false, false),
        (21, 0, Some("alice"), true, true),
        (22, 7, None, false, false),
        (23, 7, None, false, false),
        (24, 7, None, false, false),
        (25, 0, Some("alice"), true, true),
        (26, 7, None, false, false),
        (27, 7, None, false, false),
        (28, 7, None, false, false),
        (29, 0, Some("alice"), true, true),
        (30, 7, None, false, false),
        (31, 0, Some("alice"), true, true),
        (32, 7, None, false, false),
        (33, 0, Some("alice"), true, true),
        (34, 7, None, false, false),
        (35, 7, None, false, false),
        (36, 7, None, false, false),
        (37, 0, Some("alice"), true, true),
        (38, 7, None, false, false),
        (39, 0, Some("alice"), true, true),
        (40, 7, None, false, false),
        (41, 0, Some("alice"), true, true),
        (42, 7, None, false, false),
        (43, 7, None, false, false),
        (44, 7, None, false, false),
        (45, 0, Some("alice"), true, true),
        (46, 7, None, false, false),
        (47, 7, None, false, false),
        (48, 0, Some("alice"), true, true),
        (49, 0, Some("alice"), true, true),
        (50, 0, Some("alice"), true, true),
        (51, 0, Some("alice"), true, true),
        (52, 7, None, false, false),
        (53, 7, None, false, false),
        (54, 7, None, false, false),
        (55, 0, Some("alice"), true, true),
        (56, 7, None, false, false),
        (57, 7, None, false, false),
        (58, 0, Some("alice"), true, true),
        (59, 0, Some("alice"), true, true),
        (60, 0, Some("alice"), true, true),
        (61, 0, Some("alice"), true, true),
        (62, 7, Some("alice"), true, true),
    ];

    const ON_STEAMOS_FAILS: &[Fail] = &[
        (1, 0, Some("alice"), true, true),
        (2, 0, Some("alice"), true, true),
        (3, 0, Some("alice"), true, true),
        (4, 7, None, false, true),
        (5, 0, Some("alice"), true, true),
        (6, 0, Some("alice"), true, true),
        (7, 0, Some("alice"), true, true),
        (8, 0, Some("alice"), true, true),
        (9, 0, Some("alice"), true, true),
        (10, 7, None, false, true),
        (11, 7, None, false, true),
        (12, 0, Some("alice"), true, true),
        (13, 0, Some("alice"), true, true),
        (14, 0, Some("alice"), true, true),
        (15, 7, None, false, true),
        (16, 0, Some("alice"), true, true),
        (17, 7, None, false, true),
        (18, 0, Some("alice"), true, true),
        (19, 7, None, false, true),
        (20, 7, None, false, true),
        (21, 7, None, false, true),
        (22, 0, Some("alice"), true, true),
        (23, 7, None, false, true),
        (24, 0, Some("alice"), true, true),
        (25, 7, None, false, true),
        (26, 0, Some("alice"), true, true),
        (27, 7, None, false, true),
        (28, 0, Some("alice"), true, true),
        (29, 7, None, false, true),
        (30, 0, Some("alice"), true, true),
        (31, 7, None, false, true),
        (32, 0, Some("alice"), true, true),
        (33, 7, None, false, true),
        (34, 0, Some("alice"), true, true),
        (35, 0, Some("alice"), true, true),
        (36, 7, None, false, true),
        (37, 7, None, false, true),
        (38, 7, None, false, true),
        (39, 7, None, false, true),
        (40, 7, None, false, true),
        (41, 7, None, false, true),
        (42, 0, Some("alice"), true, true),
        (43, 0, Some("alice"), true, true),
        (44, 7, None, false, true),
        (45, 7, None, false, true),
        (46, 0, Some("alice"), true, true),
        (47, 0, Some("alice"), true, true),
        (48, 0, Some("alice"), true, true),
        (49, 0, Some("alice"), true, true),
        (50, 7, Some("alice"), true, true),
    ];

    const OFF_OWNER_FAILS: &[Fail] = &[
        (1, 0, None, false, false),
        (2, 0, None, false, false),
        (3, 0, None, false, false),
        (4, 0, None, false, false),
        (5, 0, None, false, false),
        (6, 7, Some("alice"), true, false),
        (7, 7, Some("alice"), true, false),
        (8, 7, Some("alice"), true, false),
        (9, 0, None, false, false),
        (10, 7, Some("alice"), true, false),
        (11, 7, Some("alice"), true, false),
        (12, 7, Some("alice"), true, false),
        (13, 0, None, false, false),
        (14, 7, Some("alice"), true, false),
        (15, 7, Some("alice"), true, false),
        (16, 7, Some("alice"), true, false),
        (17, 0, None, false, false),
        (18, 7, Some("alice"), true, false),
        (19, 7, Some("alice"), true, false),
        (20, 7, Some("alice"), true, false),
        (21, 0, None, false, false),
        (22, 0, None, false, false),
        (23, 0, None, false, false),
        (24, 7, Some("alice"), true, false),
        (25, 7, Some("alice"), true, false),
        (26, 7, Some("alice"), true, false),
        (27, 0, None, false, false),
        (28, 0, None, false, false),
        (29, 0, None, false, false),
        (30, 0, None, false, false),
        (31, 7, Some("alice"), true, false),
        (32, 7, Some("alice"), true, false),
        (33, 7, Some("alice"), true, false),
        (34, 7, Some("alice"), true, false),
        (35, 0, None, false, false),
        (36, 0, None, false, false),
        (37, 7, Some("alice"), true, false),
        (38, 7, Some("alice"), true, false),
        (39, 7, Some("alice"), true, false),
        (40, 0, None, false, false),
        (41, 1, Some("alice"), false, false),
        (42, 1, Some("alice"), false, false),
        (43, 0, None, false, false),
        (44, 0, None, false, false),
        (45, 0, None, false, false),
        (46, 0, None, false, false),
    ];
}
