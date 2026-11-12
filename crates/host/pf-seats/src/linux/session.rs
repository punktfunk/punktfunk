//! A seat's systemd unit: its environment file, its bind-mount drop-in and its state.
//!
//! `punktfunk-seat@<user>.service` runs the seat's user in a logind session. The supervisor
//! writes its files under `/run` before each start: `punktfunk/seats/<user>.env`, the seat
//! contract the host reads plus its management token; a drop-in that binds the seat's private
//! `compatdata`, `shadercache` and `downloading` over the shared games folder's, shows it the
//! owner's libraries, hides the box's mounted drives and binds the supervisor's uinput proxy
//! over `/dev/uinput`; and one that
//! binds [`NO_UPDATE`] over Steam's OS and firmware update helpers. The seat user's own
//! `user@<uid>.service` gets the same binds ([`render_manager`]). All are rewritten from the
//! ledger on a start, so a reboot that empties `/run` loses nothing.

use super::accounts::Passwd;
use super::{err, libraries, run, shared, Own};
use crate::backend::BackendError;
use crate::model::{RuntimeState, RuntimeStatus, Seat, SeatId};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Where each seat's environment file lives.
pub(super) const ENV_DIR: &str = "/run/punktfunk/seats";
/// Where the supervisor's transient unit drop-ins live.
pub(super) const DROPIN_ROOT: &str = "/run/systemd/system";

/// The seat directories bound over the shared `steamapps` folder of the same name.
pub(super) const PRIVATE_DIRS: [&str; 3] = ["compatdata", "shadercache", "downloading"];

/// What a seat's Steam runs in place of [`UPDATE_HELPERS`]. They escalate through pkexec, which
/// a seat's session is refused, and Steam's first-run setup stops on that failure. A seat never
/// updates the box, so each answers "nothing to do": 7 from `steamos-update`, else 0.
pub(super) const NO_UPDATE: &str = "/run/punktfunk/seat-no-update";
const NO_UPDATE_SCRIPT: &str = "#!/bin/sh\n\
# A punktfunk seat never updates the box's OS or firmware.\n\
if [ \"${0##*/}\" = steamos-update ]; then\n\
    case \" $* \" in *\" --supports-duplicate-detection \"*) exit 0 ;; esac\n\
    exit 7\n\
fi\n\
exit 0\n";

/// Steam's update helpers, by the absolute paths it calls them on SteamOS-like distros.
const UPDATE_HELPERS: [&str; 3] = [
    "/usr/bin/steamos-polkit-helpers/steamos-update",
    "/usr/bin/steamos-polkit-helpers/jupiter-biosupdate",
    "/usr/bin/jupiter-initial-firmware-update",
];

pub(super) fn unit(account: &str) -> String {
    format!("punktfunk-seat@{account}.service")
}

pub(super) fn env_path(account: &str) -> PathBuf {
    Path::new(ENV_DIR).join(format!("{account}.env"))
}

/// The unit's `RuntimeDirectory=`: the runner records its session id here, and the owner's runner
/// marks `ready` once the host in the headless session serves.
pub(super) fn runtime_dir(account: &str) -> PathBuf {
    PathBuf::from(format!("/run/punktfunk-seat-{account}"))
}

pub(super) fn ready_path(account: &str) -> PathBuf {
    runtime_dir(account).join("ready")
}

pub(super) fn dropin_dir(account: &str) -> PathBuf {
    Path::new(DROPIN_ROOT).join(format!("{}.d", unit(account)))
}

/// `KEY="value"`, the form `EnvironmentFile=` parses without word splitting. Inside double quotes
/// only `\` and `"` need an escape; `$` and `%` are not expanded in an environment file.
fn quoted(key: &str, value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{key}=\"{escaped}\"\n")
}

const TOKEN_KEY: &str = "PUNKTFUNK_MGMT_TOKEN=";

/// The management token as the one `KEY=value` line the box's client and `EnvironmentFile=` both
/// read. Hex, so it needs no quoting.
pub(super) fn token_line(token: &str) -> String {
    format!("{TOKEN_KEY}{token}\n")
}

/// The token `account`'s environment file holds. Only a token [`mint_token`] could have made
/// counts: a file an owner once owned may hold anything.
pub(super) fn env_token(account: &str) -> Option<String> {
    token_in(&std::fs::read_to_string(env_path(account)).ok()?).map(str::to_owned)
}

fn token_in(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix(TOKEN_KEY))
        .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A fresh 32-byte token, hex-encoded.
pub(super) fn mint_token() -> String {
    use rand::Rng as _;
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The seat contract the host reads (`multi-seat-contract.md`) for a row of `kind`, one
/// `KEY="value"` per line, then the management token the box's client presents. Only the door
/// advertises, so no seat does.
///
/// The owner's row keeps the owner's own config directory, library, host name and GameStream
/// setting: it is the owner's host, behind the door. `PUNKTFUNK_SEAT_OWNER` tells the runner and
/// the host so. Every other seat plays the box's library, from its trust copy. A person's row
/// is such a seat on an account that is theirs: `PUNKTFUNK_SEAT_OWN_ACCOUNT` tells the runner
/// alone, and its host keeps its identity in the row's directory, not in their home.
pub(super) fn render_env(
    seat: &Seat,
    kind: Own,
    home: &Path,
    state: &Path,
    token: &str,
    browser: &Browser,
) -> String {
    let trust = shared::trust_dir(state, &seat.id)
        .to_string_lossy()
        .into_owned();
    let mut rows = vec![
        ("PUNKTFUNK_SEAT_SESSION", "1".to_owned()),
        ("PUNKTFUNK_SEAT_ID", seat.id.to_string()),
    ];
    let owner = kind == Own::Owner;
    let config = match kind {
        Own::Owner => {
            rows.push(("PUNKTFUNK_SEAT_OWNER", "1".to_owned()));
            None
        }
        Own::Person => {
            rows.push(("PUNKTFUNK_SEAT_OWN_ACCOUNT", "1".to_owned()));
            Some(shared::person_config_dir(state, &seat.id))
        }
        Own::Seat => Some(home.join(".config/punktfunk")),
    };
    if let Some(config) = config {
        rows.push((
            "PUNKTFUNK_CONFIG_DIR",
            config.to_string_lossy().into_owned(),
        ));
    }
    rows.extend([
        ("PUNKTFUNK_TRUST_DIR", trust.clone()),
        ("PUNKTFUNK_PAIRING", "refused".to_owned()),
    ]);
    if !owner {
        rows.push(("PUNKTFUNK_LIBRARY_DIR", trust));
    }
    rows.extend([
        ("PUNKTFUNK_NATIVE_PORT", seat.native_port.to_string()),
        (
            "PUNKTFUNK_MGMT_BIND",
            format!("127.0.0.1:{}", seat.mgmt_port),
        ),
    ]);
    if !owner {
        rows.push(("PUNKTFUNK_HOST_NAME", seat.name.clone()));
        rows.push(("PUNKTFUNK_GAMESTREAM", "0".to_owned()));
    }
    rows.push(("PUNKTFUNK_MDNS", "0".to_owned()));
    // Off unless the box streams to browsers: an owner whose own setting has it on would
    // otherwise bind the door's port.
    let port = seat.browser_port().filter(|_| browser.on);
    rows.push((
        "PUNKTFUNK_WEBTRANSPORT",
        if port.is_some() { "1" } else { "0" }.to_owned(),
    ));
    if let Some(port) = port {
        rows.push(("PUNKTFUNK_WEBTRANSPORT_PORT", port.to_string()));
        if !browser.origins.is_empty() {
            rows.push(("PUNKTFUNK_WEBTRANSPORT_ORIGINS", browser.origins.join(",")));
        }
    }
    let mut out: String = rows.iter().map(|(key, value)| quoted(key, value)).collect();
    out.push_str(&token_line(token));
    out
}

/// The drop-in that gives the unit an empty `/run/media` holding the shared games folder at
/// [`libraries::games_view`], its private prefix directories over both of that folder's paths,
/// the owner's libraries and granted roots ([`libraries`]), and the supervisor's uinput proxy as
/// `/dev/uinput`.
/// A SteamOS-mode Steam adopts every mounted drive holding a `steamapps` as a library, and the
/// drive as mounted is the owner's alone. Without the proxy the unit starts all the same.
pub(super) fn render_binds(
    state: &Path,
    id: &SeatId,
    account: &str,
    shares: Option<&libraries::Shares>,
) -> String {
    let seat = shared::seat_dir(state, id);
    let (games, view) = (shared::games_dir(state), libraries::games_view());
    let mut out = format!(
        "[Service]\nTemporaryFileSystem=/run/media:ro\nBindPaths={}:{}\n",
        games.display(),
        view.display()
    );
    for steamapps in [games.join("steamapps"), view.join("steamapps")] {
        for name in PRIVATE_DIRS {
            out.push_str(&format!(
                "BindPaths={}:{}\n",
                seat.join(name).display(),
                steamapps.join(name).display()
            ));
        }
    }
    if let Some(shares) = shares {
        out.push_str(&libraries::unit_lines(state, id, shares));
    }
    let proxy = pf_paths::seat::uinput_devname(account);
    out.push_str(&format!("BindPaths=-/dev/{proxy}:/dev/uinput\n"));
    out
}

/// The drop-in that binds [`NO_UPDATE`] over each update helper `present` on this box. `None`
/// when none is: a bind onto a missing path fails the unit.
pub(super) fn render_updates(present: impl Fn(&Path) -> bool) -> Option<String> {
    let binds: String = UPDATE_HELPERS
        .iter()
        .filter(|helper| present(Path::new(helper)))
        .map(|helper| format!("BindReadOnlyPaths={NO_UPDATE}:{helper}\n"))
        .collect();
    (!binds.is_empty()).then(|| format!("[Service]\n{binds}"))
}

/// A made seat's user's manager's drop-in: `user@<uid>.service`, which runs what Plasma
/// launches, autostarts and D-Bus activates, outside the seat unit's namespace. A person's own
/// manager never gets one.
pub(super) fn manager_dropin(uid: u32) -> PathBuf {
    Path::new(DROPIN_ROOT)
        .join(format!("user@{uid}.service.d"))
        .join("punktfunk-seat.conf")
}

/// The seat unit's `dropins` as its user's manager takes them: every bind, so an app it starts
/// sees what the seat sees, and no mount command, which only the seat unit runs.
pub(super) fn render_manager(dropins: &[&str]) -> String {
    let mut out = String::from("[Service]\n");
    for line in dropins.iter().flat_map(|dropin| dropin.lines()) {
        if line == "[Service]"
            || line.starts_with("ExecStartPre=")
            || line.starts_with("ExecStopPost=")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The drop-in of the owner's row: a `background` session, so the owner's login at the monitor
/// stays the one logind and polkit treat as the owner's display. Another person's row keeps the
/// unit's `user` class: a shutdown at the machine counts it as another user's session.
pub(super) fn render_class() -> &'static str {
    "[Service]\nEnvironment=XDG_SESSION_CLASS=background\n"
}

/// The drop-in of a start through the login helper (`super::login`): the unit's main process is
/// `helper login <account>`, run as root by `!`, which skips only the switch to `User=`. So
/// systemd opens no PAM session of its own, the runtime directory stays the person's and a stop
/// runs as today's. In SELinux `context` when there is one; a helper that ends ends the start,
/// and the journal keeps the seat's own tag. `None` for a path a unit file can't hold verbatim;
/// `context` is the caller's to check.
pub(super) fn render_login(helper: &Path, context: Option<&str>) -> Option<String> {
    let path = helper.to_str()?;
    let plain = path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b));
    plain.then(|| {
        format!(
            "[Service]\nExecStart=\nExecStart=!{path} {} %i\nRestart=no\n\
             SyslogIdentifier=seat-session\n{}",
            super::login::VERB,
            context.map_or(String::new(), |context| format!(
                "SELinuxContext={context}\n"
            )),
        )
    })
}

/// Writes `dropin`, [`render_login`]'s, for `account`'s unit, or removes it for `None`, and
/// reloads systemd when that changed it.
pub(super) fn set_login(account: &str, dropin: Option<&str>) -> Result<(), BackendError> {
    let path = dropin_dir(account).join("login.conf");
    let changed = match dropin {
        Some(contents) => write_if_changed(&path, contents, 0o644),
        None => remove_if_present(&path),
    };
    let changed =
        changed.map_err(|e| err("session_files", format!("update the login drop-in: {e}")))?;
    if changed {
        daemon_reload()?;
    }
    Ok(())
}

/// Writes `contents` at `path` with `mode` unless it is already this process's file holding
/// exactly that. One another uid owns is replaced: root reads these files. `true` when it
/// changed.
pub(super) fn write_if_changed(path: &Path, contents: &str, mode: u32) -> std::io::Result<bool> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    let unchanged = std::fs::File::open(path).and_then(|mut file| {
        let mut current = String::new();
        file.read_to_string(&mut current)?;
        let ours = file.metadata()?.uid() == rustix::process::geteuid().as_raw();
        Ok(ours && current == contents)
    });
    if unchanged.unwrap_or(false) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(true)
}

/// The box's browser streaming as its Seat defaults hand it on (`pf_host_config::seat_defaults`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Browser {
    pub on: bool,
    pub origins: Vec<String>,
}

/// [`Browser`] from the box's Seat defaults in `box_dir`. Off when the box wrote none, or one
/// that can't be read: a browser plane is a port nobody asked to open.
pub(super) fn browser(box_dir: &Path) -> Browser {
    let read = || -> std::io::Result<Option<Browser>> {
        let path = box_dir.join("seat-defaults.json");
        let Some((bytes, _)) = super::safe_open::read(&path, super::shared::MAX_TRUST_BYTES)?
        else {
            return Ok(None);
        };
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        Ok(Some(Browser {
            on: v["browser"].as_bool().unwrap_or(false),
            origins: v["browser_origins"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().map(str::to_string))
                .collect(),
        }))
    };
    read().ok().flatten().unwrap_or_default()
}

/// Writes the environment file for `seat`, a row of `kind` on `passwd`'s account, root's `0600`:
/// it holds a secret, and the unit's root commands take their environment from it. The owner's
/// own user unit reads theirs too, through [`reader_acl`] for their uid. `true` when the file
/// changed.
pub(super) fn write_env(
    seat: &Seat,
    kind: Own,
    passwd: &Passwd,
    state: &Path,
    token: &str,
    browser: &Browser,
) -> Result<bool, BackendError> {
    let io = |what: &str, error: std::io::Error| err("session_files", format!("{what}: {error}"));
    let path = env_path(&seat.account);
    let text = render_env(seat, kind, &passwd.home, state, token, browser);
    let written = write_if_changed(&path, &text, 0o600)
        .map_err(|e| io("write the seat environment file", e))?;
    if kind == Own::Owner {
        let (acl, flags) = (reader_acl(passwd.uid), rustix::fs::XattrFlags::empty());
        rustix::fs::lsetxattr(&path, shared::ACL_ACCESS, &acl, flags)
            .map_err(|e| io("let the owner read the environment file", e.into()))?;
    }
    Ok(written)
}

/// The access ACL of a file only root writes and `uid` may read.
fn reader_acl(uid: u32) -> Vec<u8> {
    use shared::{ACL_GROUP_OBJ, ACL_MASK, ACL_NO_ID, ACL_OTHER, ACL_USER, ACL_USER_OBJ};
    shared::acl_bytes(&[
        (ACL_USER_OBJ, 6, ACL_NO_ID),
        (ACL_USER, 4, uid),
        (ACL_GROUP_OBJ, 0, ACL_NO_ID),
        (ACL_MASK, 4, ACL_NO_ID),
        (ACL_OTHER, 0, ACL_NO_ID),
    ])
}

/// Writes the environment file and the drop-ins for `seat`, a row of `kind` on `passwd`'s
/// account, then reloads systemd when a drop-in changed. Every seat gets the update helpers'
/// binds. The owner's row gets the `background` class, a made seat its private directories and
/// `shares`, and another person's row neither. A drop-in the row doesn't get is removed: its
/// account may have been another kind of row before. A made seat's user gets the seat's binds in
/// its own systemd manager too; a person's manager, the owner's included, is theirs.
pub(super) fn write_unit_files(
    seat: &Seat,
    kind: Own,
    passwd: &Passwd,
    state: &Path,
    token: &str,
    shares: Option<&libraries::Shares>,
    browser: &Browser,
) -> Result<(), BackendError> {
    let io = |what: &str, error: std::io::Error| err("session_files", format!("{what}: {error}"));
    write_env(seat, kind, passwd, state, token, browser)?;
    let mut changed = false;
    let dropins = kind_dropins(kind, seat, state, shares);
    for (name, contents) in &dropins {
        let dropin = dropin_dir(&seat.account).join(name);
        changed |= match contents {
            Some(contents) => write_if_changed(&dropin, contents, 0o644)
                .map_err(|e| io("write the seat unit drop-in", e))?,
            None => remove_if_present(&dropin).map_err(|e| io("remove a seat unit drop-in", e))?,
        };
    }
    let updates = dropin_dir(&seat.account).join("updates.conf");
    let update_binds = render_updates(|helper| helper.is_file());
    match &update_binds {
        Some(binds) => {
            write_if_changed(Path::new(NO_UPDATE), NO_UPDATE_SCRIPT, 0o755)
                .map_err(|e| io("write the seat's update helper", e))?;
            changed |= write_if_changed(&updates, binds, 0o644)
                .map_err(|e| io("write the seat unit drop-in", e))?;
        }
        None => changed |= std::fs::remove_file(&updates).is_ok(),
    }
    if let Some(parts) = manager_parts(&dropins, update_binds.as_deref()) {
        let manager = manager_dropin(passwd.uid);
        changed |= write_if_changed(&manager, &render_manager(&parts), 0o644)
            .map_err(|e| io("write the seat user's manager drop-in", e))?;
    }
    if changed {
        daemon_reload()?;
    }
    Ok(())
}

/// The class and binds drop-ins by file name, each with what a row of `kind` gets in it or `None`
/// for one it must not have.
fn kind_dropins(
    kind: Own,
    seat: &Seat,
    state: &Path,
    shares: Option<&libraries::Shares>,
) -> [(&'static str, Option<String>); 2] {
    let class = (kind == Own::Owner).then(|| render_class().to_owned());
    let binds = (kind.ours()).then(|| render_binds(state, &seat.id, &seat.account, shares));
    [("class.conf", class), ("binds.conf", binds)]
}

/// What a seat user's own manager takes of the seat's drop-ins: the binds and the update binds
/// of a made seat. `None` for a row with no binds, the owner's or a person's: their manager is
/// theirs.
fn manager_parts<'a>(
    dropins: &'a [(&'static str, Option<String>); 2],
    update_binds: Option<&'a str>,
) -> Option<Vec<&'a str>> {
    let binds = dropins
        .iter()
        .find(|(name, _)| *name == "binds.conf")
        .and_then(|(_, contents)| contents.as_deref())?;
    let mut parts = vec![binds];
    parts.extend(update_binds);
    Some(parts)
}

/// Removes `path`. `true` when there was one.
fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn daemon_reload() -> Result<(), BackendError> {
    run("systemctl_failed", "systemctl", &["daemon-reload"]).map(drop)
}

/// A unit's state as `systemctl show` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct UnitState {
    pub active: String,
    pub sub: String,
    /// `success`, or why the last run ended (`exit-code`, `signal`, `start-limit-hit`, ...).
    pub result: String,
    /// How the main process of the last run ended, once it has.
    pub ended: Option<Ended>,
}

/// A process's end: its exit code, or the signal that killed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ended {
    Exited(i32),
    Killed(i32),
}

pub(super) fn parse_show(text: &str) -> UnitState {
    let mut state = UnitState::default();
    let (mut code, mut status) = (None, None);
    for line in text.lines() {
        match line.split_once('=') {
            Some(("ActiveState", value)) => state.active = value.to_owned(),
            Some(("SubState", value)) => state.sub = value.to_owned(),
            Some(("Result", value)) => state.result = value.to_owned(),
            Some(("ExecMainCode", value)) => code = value.parse::<i32>().ok(),
            Some(("ExecMainStatus", value)) => status = value.parse::<i32>().ok(),
            _ => {}
        }
    }
    // `si_code`: 1 exited, 2 killed, 3 dumped core.
    state.ended = match (code, status) {
        (Some(1), Some(status)) => Some(Ended::Exited(status)),
        (Some(2 | 3), Some(signal)) => Some(Ended::Killed(signal)),
        _ => None,
    };
    state
}

impl UnitState {
    pub(super) fn runtime(&self) -> RuntimeStatus {
        match self.active.as_str() {
            "active" => RuntimeStatus::running(),
            "activating" if self.sub == "auto-restart" => RuntimeStatus {
                state: RuntimeState::Starting,
                detail: Some(format!("restarting after {}", self.result)),
                ..RuntimeStatus::default()
            },
            "activating" | "reloading" => RuntimeStatus {
                state: RuntimeState::Starting,
                detail: Some("starting".into()),
                ..RuntimeStatus::default()
            },
            "deactivating" => RuntimeStatus {
                state: RuntimeState::Stopping,
                detail: None,
                ..RuntimeStatus::default()
            },
            "failed" => RuntimeStatus::failed(format!("seat unit failed: {}", self.result)),
            "inactive" => RuntimeStatus::stopped(),
            _ => RuntimeStatus::default(),
        }
    }

    /// The unit is up or on its way, so its seat still needs the trust refresh.
    pub(super) fn live(&self) -> bool {
        matches!(self.active.as_str(), "active" | "activating" | "reloading")
    }
}

pub(super) fn state(account: &str) -> Result<UnitState, BackendError> {
    let shown = run(
        "systemctl_failed",
        "systemctl",
        &[
            "show",
            &unit(account),
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            "-p",
            "Result",
            "-p",
            "ExecMainCode",
            "-p",
            "ExecMainStatus",
        ],
    )?;
    Ok(parse_show(&shown))
}

/// The users that own a TCP listener that answers `127.0.0.1:port`, from the kernel's tables:
/// one entry per socket. A table that can't be read counts as empty, and is logged unless it is
/// the IPv6 one on a kernel without IPv6.
fn listeners(port: u16) -> Vec<u32> {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .into_iter()
        .filter_map(|path| match std::fs::read_to_string(path) {
            Ok(table) => Some(table),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(%error, path, "listening sockets not readable");
                None
            }
        })
        .flat_map(|table| listener_uids(&table, port))
        .collect()
}

/// Local addresses a connect to `127.0.0.1` can reach, as `/proc/net/tcp` prints them on a
/// little-endian machine: loopback and the wildcard, and the same two behind an IPv6 socket.
const LOOPBACK_REACHED: [&str; 4] = [
    "0100007F",
    "00000000",
    "0000000000000000FFFF00000100007F",
    "00000000000000000000000000000000",
];

/// The owners of the `LISTEN` rows on `port` in one `/proc/net/tcp` table that answer
/// `127.0.0.1`: the local address and port are field 1, the state field 3 (`0A`) and the owner's
/// uid field 7. A listener on another address, `::1` included, is not in the way.
fn listener_uids(table: &str, port: u16) -> Vec<u32> {
    table
        .lines()
        .skip(1)
        .filter_map(|row| {
            let fields: Vec<&str> = row.split_whitespace().collect();
            let (address, local_port) = fields.get(1)?.rsplit_once(':')?;
            let listening = fields.get(3) == Some(&"0A");
            let wanted = LOOPBACK_REACHED.contains(&address)
                && u16::from_str_radix(local_port, 16).ok()? == port;
            if !listening || !wanted {
                return None;
            }
            fields.get(7)?.parse().ok()
        })
        .collect()
}

/// Whether anything listens on `port`, whoever it is: the seat's host could not bind it.
pub(super) fn port_taken(port: u16) -> bool {
    !listeners(port).is_empty()
}

/// Whether `uid` listens on `port` and nobody else does: the seat's host, not a squatter's.
pub(super) fn port_held_by(port: u16, uid: u32) -> bool {
    let owners = listeners(port);
    !owners.is_empty() && owners.iter().all(|&owner| owner == uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RuntimeStatus, Seat, SeatId};

    fn seat(name: &str) -> Seat {
        Seat {
            id: SeatId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            name: name.into(),
            account: "pf-seat-1".into(),
            display_slot: 12,
            native_port: 9779,
            mgmt_port: 47995,
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            owner: false,
            fingerprint: None,
            adopted_sid: None,
            adopted_uid: None,
            rdp_granted: false,
        }
    }

    /// The owner's row for the account `enrico`, as adopted.
    fn owner() -> Seat {
        Seat {
            account: "enrico".into(),
            name: "enrico".into(),
            owner: true,
            ..seat("enrico")
        }
    }

    /// The owner's host is the owner's own: no config directory, library, host name or
    /// GameStream setting from the row, only the contract that moves its ports behind the door.
    #[test]
    fn the_owner_s_env_file_leaves_its_config_and_settings_alone() {
        let text = render_env(
            &owner(),
            Own::Owner,
            Path::new("/home/enrico"),
            Path::new("/var/lib/punktfunk-seats"),
            "ab12",
            &Browser::default(),
        );
        assert_eq!(
            text,
            "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
             PUNKTFUNK_SEAT_ID=\"0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_SEAT_OWNER=\"1\"\n\
             PUNKTFUNK_TRUST_DIR=\"/var/lib/punktfunk-seats/trust/0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_PAIRING=\"refused\"\n\
             PUNKTFUNK_NATIVE_PORT=\"9779\"\n\
             PUNKTFUNK_MGMT_BIND=\"127.0.0.1:47995\"\n\
             PUNKTFUNK_MDNS=\"0\"\n\
             PUNKTFUNK_WEBTRANSPORT=\"0\"\n\
             PUNKTFUNK_MGMT_TOKEN=ab12\n"
        );
        assert!(!text.contains("CONFIG_DIR"));
        assert!(!text.contains("LIBRARY_DIR"));
        assert!(render_class().contains("XDG_SESSION_CLASS=background"));
    }

    /// Only the owner's row is a `background` session and only a made seat gets the binds; a
    /// person's row gets neither, so it runs as the unit's `user` class and a drop-in either
    /// left from an earlier start is removed.
    #[test]
    fn each_row_kind_gets_its_own_drop_ins_and_no_other() {
        let state = Path::new("/var/lib/punktfunk-seats");
        let person = Seat {
            account: "jdoe".into(),
            adopted_uid: Some(1500),
            ..seat("Jane")
        };
        let kinds = |seat: &Seat| {
            kind_dropins(Own::of(seat), seat, state, None)
                .map(|(name, contents)| (name, contents.is_some()))
        };
        let gets = |class, binds| [("class.conf", class), ("binds.conf", binds)];
        assert_eq!(kinds(&owner()), gets(true, false));
        assert_eq!(kinds(&seat("Desk")), gets(false, true));
        assert_eq!(kinds(&person), gets(false, false));
        let [(_, class), _] = kind_dropins(Own::Owner, &owner(), state, None);
        assert_eq!(class.as_deref(), Some(render_class()));
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("class.conf");
        std::fs::write(&stale, render_class()).unwrap();
        assert!(remove_if_present(&stale).unwrap());
        assert!(
            !remove_if_present(&stale).unwrap(),
            "a second start finds none"
        );
    }

    /// Only a made seat's user gets the seat's binds in its own systemd manager. The owner's and
    /// a person's manager get nothing.
    #[test]
    fn only_a_made_seat_s_user_manager_gets_the_seat_s_binds() {
        let state = Path::new("/var/lib/punktfunk-seats");
        let person = Seat {
            account: "jdoe".into(),
            adopted_uid: Some(1500),
            ..seat("Jane")
        };
        let updates = Some("[Service]\nBindReadOnlyPaths=/a:/b\n");
        for row in [owner(), person] {
            let dropins = kind_dropins(Own::of(&row), &row, state, None);
            assert!(manager_parts(&dropins, updates).is_none());
        }
        let dropins = kind_dropins(Own::Seat, &seat("Desk"), state, None);
        assert_eq!(manager_parts(&dropins, updates).unwrap().len(), 2);
    }

    /// A person's row is a contract seat on their own account: the box's library and the row's
    /// name, a host identity under the row's directory and nothing under their home, and the
    /// runner's marker without the owner's.
    #[test]
    fn a_person_s_env_file_keeps_their_home_clear() {
        let person = Seat {
            account: "jdoe".into(),
            adopted_uid: Some(1500),
            ..seat("Jane")
        };
        let id = "0123456789abcdef0123456789abcdef";
        let text = render_env(
            &person,
            Own::Person,
            Path::new("/home/jdoe"),
            Path::new("/var/lib/punktfunk-seats"),
            "ab12",
            &Browser::default(),
        );
        assert_eq!(
            text,
            format!(
                "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
                 PUNKTFUNK_SEAT_ID=\"{id}\"\n\
                 PUNKTFUNK_SEAT_OWN_ACCOUNT=\"1\"\n\
                 PUNKTFUNK_CONFIG_DIR=\"/var/lib/punktfunk-seats/seats/{id}/config\"\n\
                 PUNKTFUNK_TRUST_DIR=\"/var/lib/punktfunk-seats/trust/{id}\"\n\
                 PUNKTFUNK_PAIRING=\"refused\"\n\
                 PUNKTFUNK_LIBRARY_DIR=\"/var/lib/punktfunk-seats/trust/{id}\"\n\
                 PUNKTFUNK_NATIVE_PORT=\"9779\"\n\
                 PUNKTFUNK_MGMT_BIND=\"127.0.0.1:47995\"\n\
                 PUNKTFUNK_HOST_NAME=\"Jane\"\n\
                 PUNKTFUNK_GAMESTREAM=\"0\"\n\
                 PUNKTFUNK_MDNS=\"0\"\n\
                 PUNKTFUNK_WEBTRANSPORT=\"0\"\n\
                 PUNKTFUNK_MGMT_TOKEN=ab12\n"
            )
        );
        assert!(!text.contains("/home/jdoe") && !text.contains("SEAT_OWNER"));
    }

    /// The whole seat contract, in the order the doc lists it, with each path under its seat.
    #[test]
    fn the_env_file_carries_the_seat_contract() {
        let text = render_env(
            &seat("Mara"),
            Own::Seat,
            Path::new("/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/home"),
            Path::new("/var/lib/punktfunk-seats"),
            "ab12",
            &Browser::default(),
        );
        assert_eq!(
            text,
            "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
             PUNKTFUNK_SEAT_ID=\"0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_CONFIG_DIR=\"/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/home/.config/punktfunk\"\n\
             PUNKTFUNK_TRUST_DIR=\"/var/lib/punktfunk-seats/trust/0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_PAIRING=\"refused\"\n\
             PUNKTFUNK_LIBRARY_DIR=\"/var/lib/punktfunk-seats/trust/0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_NATIVE_PORT=\"9779\"\n\
             PUNKTFUNK_MGMT_BIND=\"127.0.0.1:47995\"\n\
             PUNKTFUNK_HOST_NAME=\"Mara\"\n\
             PUNKTFUNK_GAMESTREAM=\"0\"\n\
             PUNKTFUNK_MDNS=\"0\"\n\
             PUNKTFUNK_WEBTRANSPORT=\"0\"\n\
             PUNKTFUNK_MGMT_TOKEN=ab12\n"
        );
    }

    /// With the box streaming to browsers each seat runs its own plane on its own port, under the
    /// box's origins.
    #[test]
    fn a_seat_streams_to_browsers_on_its_own_port_when_the_box_does() {
        let browser = Browser {
            on: true,
            origins: vec!["https://box:47992".into(), "https://web.example".into()],
        };
        let text = render_env(
            &seat("Mara"),
            Own::Seat,
            Path::new("/h"),
            Path::new("/b"),
            "ab12",
            &browser,
        );
        assert!(text.contains("PUNKTFUNK_WEBTRANSPORT=\"1\"\n"));
        assert!(text.contains("PUNKTFUNK_WEBTRANSPORT_PORT=\"9783\"\n"));
        assert!(text.contains(
            "PUNKTFUNK_WEBTRANSPORT_ORIGINS=\"https://box:47992,https://web.example\"\n"
        ));
        let off = render_env(
            &seat("Mara"),
            Own::Seat,
            Path::new("/h"),
            Path::new("/b"),
            "ab12",
            &Browser::default(),
        );
        assert!(off.contains("PUNKTFUNK_WEBTRANSPORT=\"0\"\n"));
        assert!(!off.contains("PUNKTFUNK_WEBTRANSPORT_PORT"));
    }

    /// A minted token reads back from the file it was rendered into; any other doesn't.
    #[test]
    fn only_a_minted_token_reads_back_from_an_environment_file() {
        let token = mint_token();
        let text = render_env(
            &seat("Mara"),
            Own::Seat,
            Path::new("/h"),
            Path::new("/b"),
            &token,
            &Browser::default(),
        );
        assert_eq!(token_in(&text), Some(token.as_str()));
        assert_eq!(token_in("PUNKTFUNK_MGMT_TOKEN=ab12\n"), None);
        let not_hex = format!("PUNKTFUNK_MGMT_TOKEN={}\n", "z".repeat(64));
        assert_eq!(token_in(&not_hex), None);
        assert_eq!(token_in("PUNKTFUNK_SEAT_ID=\"x\"\n"), None);
    }

    /// The box's Seat defaults name its browser plane; a box that wrote none streams to none.
    #[test]
    fn the_box_s_browser_plane_reads_from_its_seat_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(browser(dir.path()), Browser::default());
        let defaults = pf_host_config::seat_defaults::SeatDefaults {
            end_on_game_exit: true,
            max_mode: None,
            library_writable: true,
            dualsense_usbip: false,
            browser: true,
            browser_origins: vec!["https://box:47992".into()],
            background_input: false,
        };
        std::fs::write(
            dir.path().join(pf_host_config::seat_defaults::FILE),
            defaults.to_json().to_string(),
        )
        .unwrap();
        assert_eq!(
            browser(dir.path()),
            Browser {
                on: true,
                origins: vec!["https://box:47992".into()],
            }
        );
    }

    /// A seat name is free text: quotes and backslashes are escaped so it can't end the value or
    /// add a line, and `$` and `%` pass through because an environment file expands neither.
    #[test]
    fn a_seat_name_cannot_break_out_of_its_value() {
        let text = render_env(
            &seat(r#"A "B" \ $HOME 100%"#),
            Own::Seat,
            Path::new("/h"),
            Path::new("/b"),
            "ab12",
            &Browser::default(),
        );
        let line = text
            .lines()
            .find(|l| l.starts_with("PUNKTFUNK_HOST_NAME="))
            .unwrap();
        assert_eq!(line, r#"PUNKTFUNK_HOST_NAME="A \"B\" \\ $HOME 100%""#);
        assert_eq!(text.lines().count(), 13);
    }

    /// The token line is the exact form the box's client parses, and every mint differs.
    #[test]
    fn the_token_line_and_a_minted_token_have_the_shape_the_client_reads() {
        assert_eq!(token_line("deadbeef"), "PUNKTFUNK_MGMT_TOKEN=deadbeef\n");
        let (a, b) = (mint_token(), mint_token());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn the_binds_drop_in_maps_each_private_dir_and_the_uinput_proxy() {
        let id = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            render_binds(Path::new("/var/lib/punktfunk-seats"), &id, "pf-seat-1", None),
            "[Service]\n\
             TemporaryFileSystem=/run/media:ro\n\
             BindPaths=/var/lib/punktfunk-seats/games:/run/media/punktfunk/games\n\
             BindPaths=/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/compatdata:/var/lib/punktfunk-seats/games/steamapps/compatdata\n\
             BindPaths=/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/shadercache:/var/lib/punktfunk-seats/games/steamapps/shadercache\n\
             BindPaths=/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/downloading:/var/lib/punktfunk-seats/games/steamapps/downloading\n\
             BindPaths=/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/compatdata:/run/media/punktfunk/games/steamapps/compatdata\n\
             BindPaths=/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/shadercache:/run/media/punktfunk/games/steamapps/shadercache\n\
             BindPaths=/var/lib/punktfunk-seats/seats/0123456789abcdef0123456789abcdef/downloading:/run/media/punktfunk/games/steamapps/downloading\n\
             BindPaths=-/dev/punktfunk/uinput-pf-seat-1:/dev/uinput\n"
        );
    }

    /// The seat user's manager gets every bind of both drop-ins under one section, and none of
    /// the mount commands, which run once, in the seat unit.
    #[test]
    fn the_manager_drop_in_keeps_the_binds_without_the_mounts() {
        let binds = "[Service]\n\
            TemporaryFileSystem=/run/media:ro\n\
            ExecStartPre=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/mount --bind \"/a\" /stage\n\
            ExecStopPost=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/umount -l /stage\n\
            InaccessiblePaths=\"-/a\"\n\
            BindPaths=/stage:/seat/libraries/k/steamapps\n";
        let updates = "[Service]\nBindReadOnlyPaths=/run/punktfunk/seat-no-update:/usr/bin/x\n";
        assert_eq!(
            render_manager(&[binds, updates]),
            "[Service]\n\
             TemporaryFileSystem=/run/media:ro\n\
             InaccessiblePaths=\"-/a\"\n\
             BindPaths=/stage:/seat/libraries/k/steamapps\n\
             BindReadOnlyPaths=/run/punktfunk/seat-no-update:/usr/bin/x\n"
        );
        assert_eq!(
            manager_dropin(951),
            Path::new("/run/systemd/system/user@951.service.d/punktfunk-seat.conf")
        );
    }

    #[test]
    fn only_the_update_helpers_on_this_box_are_bound() {
        assert_eq!(render_updates(|_| false), None);
        assert_eq!(
            render_updates(|p| p.starts_with("/usr/bin/steamos-polkit-helpers")).unwrap(),
            "[Service]\n\
             BindReadOnlyPaths=/run/punktfunk/seat-no-update:/usr/bin/steamos-polkit-helpers/steamos-update\n\
             BindReadOnlyPaths=/run/punktfunk/seat-no-update:/usr/bin/steamos-polkit-helpers/jupiter-biosupdate\n"
        );
    }

    /// The codes Steam reads as "nothing to do", under each name the script is bound to. `sh`
    /// reads the file: executing it races another test thread's fork (`ETXTBSY`).
    #[test]
    fn the_update_helper_reports_nothing_to_do() {
        let dir = std::env::temp_dir().join(format!("pf-no-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let code = |name: &str, args: &[&str]| {
            let path = dir.join(name);
            std::fs::write(&path, NO_UPDATE_SCRIPT).unwrap();
            std::process::Command::new("sh")
                .arg(&path)
                .args(args)
                .status()
                .unwrap()
                .code()
        };
        assert_eq!(
            code("steamos-update", &["--enable-duplicate-detection", "check"]),
            Some(7)
        );
        assert_eq!(code("steamos-update", &[]), Some(7));
        assert_eq!(
            code("steamos-update", &["--supports-duplicate-detection"]),
            Some(0)
        );
        assert_eq!(code("jupiter-biosupdate", &["check"]), Some(0));
        assert_eq!(code("jupiter-initial-firmware-update", &["check"]), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn systemctl_show_maps_to_a_runtime_status() {
        let status = |text: &str| parse_show(text).runtime();
        assert_eq!(
            status("ActiveState=active\nSubState=running\nResult=success\n"),
            RuntimeStatus::running()
        );
        assert_eq!(
            status("ActiveState=inactive\nSubState=dead\nResult=success\n").state,
            RuntimeState::Stopped
        );
        assert_eq!(
            status("ActiveState=activating\nSubState=start\nResult=success\n").state,
            RuntimeState::Starting
        );
        let restarting =
            status("ActiveState=activating\nSubState=auto-restart\nResult=exit-code\n");
        assert_eq!(restarting.state, RuntimeState::Starting);
        assert_eq!(
            restarting.detail.as_deref(),
            Some("restarting after exit-code")
        );
        let failed = status("ActiveState=failed\nSubState=failed\nResult=signal\n");
        assert_eq!(failed.state, RuntimeState::Failed);
        assert!(failed.detail.unwrap().contains("signal"));
        assert_eq!(status("").state, RuntimeState::Unknown);
        assert!(parse_show("ActiveState=activating\n").live());
        assert!(!parse_show("ActiveState=failed\n").live());
    }

    /// Only a socket in `LISTEN` on the port that answers `127.0.0.1` counts, whatever its family
    /// or owner; a listener on another address is not in the way.
    #[test]
    fn the_listeners_on_a_port_are_the_listen_rows_with_that_port() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            0: 0100007F:B8B3 00000000:0000 0A 00000000:00000000 00:00000000 00000000   975        0 111 1 0 100 0 0 10 0\n\
            1: 0100007F:B8B4 00000000:0000 0A 00000000:00000000 00:00000000 00000000   976        0 112 1 0 100 0 0 10 0\n\
            2: 0100007F:B8B3 0100007F:9C40 01 00000000:00000000 00:00000000 00000000   975        0 113 1 0 100 0 0 10 0\n\
            3: 00000000:B8B3 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 114 1 0 100 0 0 10 0\n\
            4: 00000000000000000000000000000000:B8B3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  0 0 115 1 0 100 0 0 10 0\n\
            5: 0000000000000000FFFF00000100007F:B8B3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  977 0 116 1 0 100 0 0 10 0\n\
            6: 00000000000000000000000001000000:B8B3 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  978 0 117 1 0 100 0 0 10 0\n\
            7: 0501A8C0:B8B3 00000000:0000 0A 00000000:00000000 00:00000000 00000000   979        0 118 1 0 100 0 0 10 0\n";
        assert_eq!(listener_uids(table, 0xB8B3), [975, 1000, 0, 977]);
        assert_eq!(listener_uids(table, 0xB8B4), [976]);
        assert!(listener_uids(table, 0xB8B5).is_empty());
        assert!(listener_uids("", 1).is_empty());
        assert!(listener_uids("header\nshort row\n", 1).is_empty());
    }

    /// A listener this process owns is found by its port and held by its user, not by another.
    /// The table is the kernel's, so this runs on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_listener_is_held_by_its_owner_alone() {
        let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        let me = rustix::process::getuid().as_raw();
        assert!(port_taken(port));
        assert!(port_held_by(port, me));
        assert!(!port_held_by(port, me + 1));
    }

    #[test]
    fn a_file_is_rewritten_only_when_it_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a/b.env");
        assert!(write_if_changed(&path, "X=\"1\"\n", 0o640).unwrap());
        assert!(!write_if_changed(&path, "X=\"1\"\n", 0o640).unwrap());
        assert!(write_if_changed(&path, "X=\"2\"\n", 0o640).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "X=\"2\"\n");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    /// The owner reads the environment file and only root writes it: the owner's entry and the
    /// mask are `r--`, the group and others get nothing.
    #[test]
    fn the_owner_may_read_the_env_file_and_never_write_it() {
        let entry = |tag: u16, perm: u16, id: u32| {
            [
                &tag.to_le_bytes()[..],
                &perm.to_le_bytes(),
                &id.to_le_bytes(),
            ]
            .concat()
        };
        let none = u32::MAX;
        assert_eq!(
            reader_acl(1000),
            [
                2u32.to_le_bytes().to_vec(),
                entry(0x01, 6, none),
                entry(0x02, 4, 1000),
                entry(0x04, 0, none),
                entry(0x10, 4, none),
                entry(0x20, 0, none),
            ]
            .concat()
        );
    }

    /// A file another uid owns is replaced even when it holds the same text, and the kernel takes
    /// the owner's read entry on the replacement. Giving a file away needs root, so elsewhere this
    /// checks nothing.
    #[test]
    fn a_file_another_uid_owns_is_replaced() {
        use std::os::unix::fs::MetadataExt as _;
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owner.env");
        assert!(write_if_changed(&path, "X=\"1\"\n", 0o600).unwrap());
        std::os::unix::fs::chown(&path, Some(1000), Some(1000)).unwrap();
        assert!(write_if_changed(&path, "X=\"1\"\n", 0o600).unwrap());
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (0, 0));
        assert!(!write_if_changed(&path, "X=\"1\"\n", 0o600).unwrap());
        let acl = reader_acl(1000);
        let flags = rustix::fs::XattrFlags::empty();
        match rustix::fs::lsetxattr(&path, shared::ACL_ACCESS, &acl, flags) {
            Err(rustix::io::Errno::NOTSUP) => {}
            set => {
                set.unwrap();
                assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
            }
        }
    }
}
