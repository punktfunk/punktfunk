//! Whether a made seat's Steam opened the libraries the seat shows it.
//!
//! Steam's `content_log.txt` names each listed folder it couldn't open at a library load:
//! `Install folder <path> not mounted.` The check reads the last load, and for each folder of
//! ours it missed, tries the folder as that Steam does: its user, its groups, its mount
//! namespace. The error that try meets, beside the folder's owner and mode on the host, names
//! the cause.

use super::accounts::Passwd;
use super::{libraries, output, safe_open, shared};
use crate::model::SeatId;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// One library load and what Steam logs after it fit many times over.
const TAIL: u64 = 256 * 1024;

const LOAD: &str = "] Loaded Steam library folders configuration: ";

/// The seat's folders its Steam's last library load couldn't open.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Missed {
    /// The load's time as Steam logs it, which tells one load from the next.
    pub stamp: String,
    pub folders: Vec<String>,
}

/// What the Steam in `home` missed of seat `id`'s folders at its last library load. `None` when
/// it opened them all, or has no log.
pub(super) fn missed(state: &Path, id: &SeatId, home: &Path) -> Option<Missed> {
    let games = shared::games_dir(state);
    let libraries = libraries::libraries_dir(state, id);
    let ours = |folder: &str| {
        let folder = Path::new(folder);
        folder == games || folder.starts_with(&libraries)
    };
    parse(&read_log(home).ok()??, ours)
}

/// The folders `ours` admits that `log` says its last library load couldn't open.
fn parse(log: &str, ours: impl Fn(&str) -> bool) -> Option<Missed> {
    let at = log.rfind(LOAD)?;
    let line = log[..at].rfind('\n').map_or(0, |end| end + 1);
    let stamp = log[line..at].trim_start_matches('[').to_owned();
    let mut folders: Vec<String> = Vec::new();
    for line in log[at..].lines().skip(1) {
        let text = line.split_once("] ").map_or(line, |(_, text)| text);
        let folder = text
            .strip_prefix("Install folder ")
            .and_then(|rest| rest.strip_suffix(" not mounted."));
        let new = |folder: &&str| ours(folder) && !folders.iter().any(|seen| seen == folder);
        if let Some(folder) = folder.filter(new) {
            folders.push(folder.to_owned());
        }
    }
    (!folders.is_empty()).then_some(Missed { stamp, folders })
}

/// The tail of the Steam log in `home`, which the seat's user writes: opened without following
/// a link.
fn read_log(home: &Path) -> std::io::Result<Option<String>> {
    let names = [".local", "share", "Steam", "logs"];
    let Some(logs) = safe_open::walk(safe_open::dir(home)?, &names)? else {
        return Ok(None);
    };
    let Some((mut file, meta)) = safe_open::file_at(&logs, "content_log.txt", u64::MAX)? else {
        return Ok(None);
    };
    file.seek(SeekFrom::Start(meta.len().saturating_sub(TAIL)))?;
    let mut bytes = Vec::new();
    file.take(TAIL).read_to_end(&mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Logs, for each folder in `missed`, what keeps the seat's Steam from it.
pub(super) fn explain(id: &SeatId, seat: &Passwd, missed: &Missed) {
    let steam = client(Path::new("/proc"), seat.uid);
    for folder in &missed.folders {
        let host = on_host(Path::new(folder));
        let Some(steam) = &steam else {
            let steam = "exited";
            tracing::warn!(seat = %id, folder, host, steam, "seat Steam library folder out of reach");
            continue;
        };
        tracing::warn!(
            seat = %id,
            folder,
            host,
            as_steam = %open_as(steam, folder),
            steam_pid = steam.pid,
            steam_cgroup = %steam.cgroup,
            steam_gid = steam.gid,
            steam_groups = ?steam.groups,
            seat_gid = seat.gid,
            "seat Steam library folder out of reach"
        );
    }
}

/// The seat's running Steam client, as `/proc` shows it.
#[derive(Debug, PartialEq, Eq)]
struct Client {
    pid: u32,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
    cgroup: String,
}

/// The Steam client `uid` runs: `ubuntu12_32/steam`, the binary that writes the content log.
fn client(proc: &Path, uid: u32) -> Option<Client> {
    std::fs::read_dir(proc).ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().to_str()?.parse().ok()?;
        let dir = entry.path();
        let cmdline = std::fs::read(dir.join("cmdline")).ok()?;
        let argv0 = cmdline.split(|byte| *byte == 0).next()?;
        if !argv0.ends_with(b"ubuntu12_32/steam") {
            return None;
        }
        let status = std::fs::read_to_string(dir.join("status")).ok()?;
        let ids = |name: &str| -> Vec<u32> {
            let line = status.lines().find_map(|line| line.strip_prefix(name));
            let ids = line.unwrap_or("").split_whitespace();
            ids.filter_map(|id| id.parse().ok()).collect()
        };
        if ids("Uid:").first() != Some(&uid) {
            return None;
        }
        let cgroup = std::fs::read_to_string(dir.join("cgroup")).unwrap_or_default();
        Some(Client {
            pid,
            uid,
            gid: *ids("Gid:").first()?,
            groups: ids("Groups:"),
            cgroup: cgroup
                .lines()
                .find_map(|l| l.strip_prefix("0::"))
                .unwrap_or("")
                .to_owned(),
        })
    })
}

/// The folder's owner and mode, or why the host can't stat it.
fn on_host(folder: &Path) -> String {
    match std::fs::metadata(folder) {
        Ok(meta) => format!("{:o} {}:{}", meta.mode() & 0o7777, meta.uid(), meta.gid()),
        Err(error) => error.to_string(),
    }
}

/// What listing `folder` meets as `steam` does: its ids and groups, through its root. The
/// supervisor holds no `CAP_SYS_PTRACE`; only Steam's own ids may enter `/proc/<pid>/root`.
fn open_as(steam: &Client, folder: &str) -> String {
    let (uid, gid) = (steam.uid.to_string(), steam.gid.to_string());
    let groups: Vec<String> = steam.groups.iter().map(u32::to_string).collect();
    let groups = groups.join(",");
    let path = format!("/proc/{}/root{folder}", steam.pid);
    let mut args = vec!["--reuid", &uid, "--regid", &gid];
    match groups.as_str() {
        "" => args.push("--clear-groups"),
        list => args.extend(["--groups", list]),
    }
    args.extend(["--", "ls", "--", &path]);
    match output("setpriv", &args) {
        Ok(out) if out.status.success() => "opens now".to_owned(),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr.trim().rsplit(": ").next().unwrap_or("").to_owned()
        }
        Err(error) => format!("setpriv: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
[2026-11-07 10:27:28] Loaded Steam library folders configuration: /h/steamapps/libraryfolders.vdf
[2026-11-07 10:27:28] Install folder /var/games not mounted.
[2026-11-07 13:14:16] Client version: 1792618285
[2026-11-07 13:14:16] Loaded Steam library folders configuration: /h/steamapps/libraryfolders.vdf
[2026-11-07 13:14:16] Loaded 0 apps from install folder \"/h/steamapps\\appmanifest_*.acf\".
[2026-11-07 13:14:16] Install folder /var/libraries/01c1 not mounted.
[2026-11-07 13:14:16] Install folder /mnt/other not mounted.
[2026-11-07 13:14:16] Triggered async write of Steam library folders configuration (5 libraries).
[2026-11-07 13:14:17] Install folder /var/libraries/01c1 not mounted.
";

    fn ours(folder: &str) -> bool {
        folder.starts_with("/var/")
    }

    #[test]
    fn the_last_load_names_the_folders_of_ours_it_missed() {
        let missed = parse(LOG, ours).expect("a folder was missed");
        assert_eq!(missed.stamp, "2026-11-07 13:14:16");
        assert_eq!(
            missed.folders,
            ["/var/libraries/01c1"],
            "ours, once, from the last load"
        );
    }

    #[test]
    fn a_load_that_opened_every_folder_of_ours_missed_nothing() {
        let log = format!("{LOG}[2026-11-07 14:00:00{LOAD}/h/steamapps/libraryfolders.vdf\n");
        assert_eq!(parse(&log, ours), None);
        assert_eq!(parse("", ours), None, "no load, nothing missed");
    }

    #[test]
    fn steam_is_found_by_its_client_binary_and_user() {
        let proc = tempfile::tempdir().unwrap();
        let process = |pid: &str, argv0: &str, uid: u32| {
            let dir = proc.path().join(pid);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("cmdline"), format!("{argv0}\0-silent\0")).unwrap();
            let status = format!(
                "Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t946\t946\t946\t946\nGroups:\t977 946 \n"
            );
            std::fs::write(dir.join("status"), status).unwrap();
            std::fs::write(
                dir.join("cgroup"),
                "0::/user.slice/punktfunk-seat@pf-seat-1.service\n",
            )
            .unwrap();
        };
        process("10", "/bin/bash", 946);
        process("11", "/h/.local/share/Steam/ubuntu12_32/steam", 1000);
        process("12", "/h/.local/share/Steam/ubuntu12_32/steam", 946);
        let found = client(proc.path(), 946).expect("the seat user's Steam");
        assert_eq!((found.pid, found.gid), (12, 946));
        assert_eq!(found.groups, [977, 946]);
        assert_eq!(found.cgroup, "/user.slice/punktfunk-seat@pf-seat-1.service");
        assert_eq!(client(proc.path(), 947), None);
    }
}
