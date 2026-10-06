//! A seat's systemd unit: its environment file, its bind-mount drop-in and its state.
//!
//! `punktfunk-seat@<user>.service` runs the seat's user in a logind session. The supervisor
//! writes two files under `/run` before each start: `punktfunk/seats/<user>.env`, the seat
//! contract the host reads plus its management token, and a drop-in that binds the seat's private
//! `compatdata`, `shadercache` and `downloading` over the shared games folder's. Both are
//! rewritten from the ledger on a start, so a reboot that empties `/run` loses nothing.

use super::{err, run, shared};
use crate::backend::BackendError;
use crate::model::{RuntimeState, RuntimeStatus, Seat, SeatId};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where each seat's environment file lives.
pub(super) const ENV_DIR: &str = "/run/punktfunk/seats";
/// Where the supervisor's transient unit drop-ins live.
pub(super) const DROPIN_ROOT: &str = "/run/systemd/system";

/// The seat directories bound over the shared `steamapps` folder of the same name.
pub(super) const PRIVATE_DIRS: [&str; 3] = ["compatdata", "shadercache", "downloading"];

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

/// The management token as the one `KEY=value` line the box's client and `EnvironmentFile=` both
/// read. Hex, so it needs no quoting.
pub(super) fn token_line(token: &str) -> String {
    format!("PUNKTFUNK_MGMT_TOKEN={token}\n")
}

/// A fresh 32-byte token, hex-encoded.
pub(super) fn mint_token() -> String {
    use rand::RngCore as _;
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The seat contract the host reads (`multi-seat-contract.md`), one `KEY="value"` per line, then
/// the management token the box's client presents. Only the door advertises, so no seat does.
///
/// The owner's row keeps the owner's own config directory, host name and GameStream setting: it
/// is the owner's host, behind the door. `PUNKTFUNK_SEAT_OWNER` tells the runner and the host so.
pub(super) fn render_env(seat: &Seat, home: &Path, box_dir: &Path, token: &str) -> String {
    let trust = shared::trust_dir(box_dir, &seat.id);
    let mut rows = vec![
        ("PUNKTFUNK_SEAT_SESSION", "1".to_owned()),
        ("PUNKTFUNK_SEAT_ID", seat.id.to_string()),
    ];
    if seat.owner {
        rows.push(("PUNKTFUNK_SEAT_OWNER", "1".to_owned()));
    } else {
        let config = home.join(".config/punktfunk");
        rows.push((
            "PUNKTFUNK_CONFIG_DIR",
            config.to_string_lossy().into_owned(),
        ));
    }
    rows.extend([
        ("PUNKTFUNK_TRUST_DIR", trust.to_string_lossy().into_owned()),
        ("PUNKTFUNK_PAIRING", "refused".to_owned()),
        ("PUNKTFUNK_NATIVE_PORT", seat.native_port.to_string()),
        (
            "PUNKTFUNK_MGMT_BIND",
            format!("127.0.0.1:{}", seat.mgmt_port),
        ),
    ]);
    if !seat.owner {
        rows.push(("PUNKTFUNK_HOST_NAME", seat.name.clone()));
        rows.push(("PUNKTFUNK_GAMESTREAM", "0".to_owned()));
    }
    rows.push(("PUNKTFUNK_MDNS", "0".to_owned()));
    let mut out: String = rows.iter().map(|(key, value)| quoted(key, value)).collect();
    out.push_str(&token_line(token));
    out
}

/// The drop-in that gives the unit its private prefix directories.
pub(super) fn render_binds(box_dir: &Path, id: &SeatId) -> String {
    let seat = shared::seat_dir(box_dir, id);
    let shared_steamapps = shared::games_dir(box_dir).join("steamapps");
    let mut out = String::from("[Service]\n");
    for name in PRIVATE_DIRS {
        out.push_str(&format!(
            "BindPaths={}:{}\n",
            seat.join(name).display(),
            shared_steamapps.join(name).display()
        ));
    }
    out
}

/// The owner's drop-in: a `background` session, so the monitor's own login stays the one logind
/// and polkit treat as the user's display.
pub(super) fn render_class() -> &'static str {
    "[Service]\nEnvironment=XDG_SESSION_CLASS=background\n"
}

/// Writes `contents` at `path` with `mode` unless it already holds exactly that. `true` when it
/// changed.
pub(super) fn write_if_changed(path: &Path, contents: &str, mode: u32) -> std::io::Result<bool> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    if std::fs::read_to_string(path).is_ok_and(|current| current == contents) {
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

/// Writes the environment file for `seat`. It holds a secret: root reads it before the unit drops
/// privileges, nobody else. The owner's user unit reads it too, so it is the owner's, `0600`.
pub(super) fn write_env(
    seat: &Seat,
    home: &Path,
    box_dir: &Path,
    token: &str,
    owner: Option<(u32, u32)>,
) -> Result<(), BackendError> {
    let io = |what: &str, error: std::io::Error| err("session_files", format!("{what}: {error}"));
    let path = env_path(&seat.account);
    write_if_changed(&path, &render_env(seat, home, box_dir, token), 0o600)
        .map_err(|e| io("write the seat environment file", e))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&path, Some(uid), Some(gid))
            .map_err(|e| io("hand the environment file to its owner", e))?;
    }
    Ok(())
}

/// Writes both files for `seat`, then reloads systemd when the drop-in changed.
pub(super) fn write_unit_files(
    seat: &Seat,
    home: &Path,
    box_dir: &Path,
    token: &str,
    owner: Option<(u32, u32)>,
) -> Result<(), BackendError> {
    let io = |what: &str, error: std::io::Error| err("session_files", format!("{what}: {error}"));
    write_env(seat, home, box_dir, token, owner)?;
    let (name, contents) = if seat.owner {
        ("class.conf", render_class().to_owned())
    } else {
        ("binds.conf", render_binds(box_dir, &seat.id))
    };
    let dropin = dropin_dir(&seat.account).join(name);
    let changed = write_if_changed(&dropin, &contents, 0o644)
        .map_err(|e| io("write the seat unit drop-in", e))?;
    if changed {
        daemon_reload()?;
    }
    Ok(())
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
}

pub(super) fn parse_show(text: &str) -> UnitState {
    let mut state = UnitState::default();
    for line in text.lines() {
        match line.split_once('=') {
            Some(("ActiveState", value)) => state.active = value.to_owned(),
            Some(("SubState", value)) => state.sub = value.to_owned(),
            Some(("Result", value)) => state.result = value.to_owned(),
            _ => {}
        }
    }
    state
}

impl UnitState {
    pub(super) fn runtime(&self) -> RuntimeStatus {
        match self.active.as_str() {
            "active" => RuntimeStatus::running(),
            "activating" if self.sub == "auto-restart" => RuntimeStatus {
                state: RuntimeState::Starting,
                detail: Some(format!("restarting after {}", self.result)),
            },
            "activating" | "reloading" => RuntimeStatus {
                state: RuntimeState::Starting,
                detail: Some("starting".into()),
            },
            "deactivating" => RuntimeStatus {
                state: RuntimeState::Stopping,
                detail: None,
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
        ],
    )?;
    Ok(parse_show(&shown))
}

/// Whether something accepts TCP connections on loopback `port`: the seat host's management API.
pub(super) fn port_open(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
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
            native_port: 9778,
            mgmt_port: 47995,
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            owner: false,
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

    /// The owner's host is the owner's own: no config directory, host name or GameStream
    /// setting from the row, only the contract that moves its ports behind the door.
    #[test]
    fn the_owner_s_env_file_leaves_its_config_and_settings_alone() {
        let text = render_env(
            &owner(),
            Path::new("/home/enrico"),
            Path::new("/var/lib/punktfunk"),
            "ab12",
        );
        assert_eq!(
            text,
            "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
             PUNKTFUNK_SEAT_ID=\"0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_SEAT_OWNER=\"1\"\n\
             PUNKTFUNK_TRUST_DIR=\"/var/lib/punktfunk/trust/0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_PAIRING=\"refused\"\n\
             PUNKTFUNK_NATIVE_PORT=\"9778\"\n\
             PUNKTFUNK_MGMT_BIND=\"127.0.0.1:47995\"\n\
             PUNKTFUNK_MDNS=\"0\"\n\
             PUNKTFUNK_MGMT_TOKEN=ab12\n"
        );
        assert!(!text.contains("CONFIG_DIR"));
        assert!(render_class().contains("XDG_SESSION_CLASS=background"));
    }

    /// The whole seat contract, in the order the doc lists it, with each path under its seat.
    #[test]
    fn the_env_file_carries_the_seat_contract() {
        let text = render_env(
            &seat("Mara"),
            Path::new("/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/home"),
            Path::new("/var/lib/punktfunk"),
            "ab12",
        );
        assert_eq!(
            text,
            "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
             PUNKTFUNK_SEAT_ID=\"0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_CONFIG_DIR=\"/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/home/.config/punktfunk\"\n\
             PUNKTFUNK_TRUST_DIR=\"/var/lib/punktfunk/trust/0123456789abcdef0123456789abcdef\"\n\
             PUNKTFUNK_PAIRING=\"refused\"\n\
             PUNKTFUNK_NATIVE_PORT=\"9778\"\n\
             PUNKTFUNK_MGMT_BIND=\"127.0.0.1:47995\"\n\
             PUNKTFUNK_HOST_NAME=\"Mara\"\n\
             PUNKTFUNK_GAMESTREAM=\"0\"\n\
             PUNKTFUNK_MDNS=\"0\"\n\
             PUNKTFUNK_MGMT_TOKEN=ab12\n"
        );
    }

    /// A seat name is free text: quotes and backslashes are escaped so it can't end the value or
    /// add a line, and `$` and `%` pass through because an environment file expands neither.
    #[test]
    fn a_seat_name_cannot_break_out_of_its_value() {
        let text = render_env(
            &seat(r#"A "B" \ $HOME 100%"#),
            Path::new("/h"),
            Path::new("/b"),
            "ab12",
        );
        let line = text
            .lines()
            .find(|l| l.starts_with("PUNKTFUNK_HOST_NAME="))
            .unwrap();
        assert_eq!(line, r#"PUNKTFUNK_HOST_NAME="A \"B\" \\ $HOME 100%""#);
        assert_eq!(text.lines().count(), 11);
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
    fn the_binds_drop_in_maps_each_private_dir_over_the_shared_one() {
        let id = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            render_binds(Path::new("/var/lib/punktfunk"), &id),
            "[Service]\n\
             BindPaths=/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/compatdata:/var/lib/punktfunk/games/steamapps/compatdata\n\
             BindPaths=/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/shadercache:/var/lib/punktfunk/games/steamapps/shadercache\n\
             BindPaths=/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/downloading:/var/lib/punktfunk/games/steamapps/downloading\n"
        );
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
}
