//! Whether a made seat's Steam opened the libraries the seat shows it.
//!
//! Steam's `content_log.txt` names each listed folder it couldn't open at a library load:
//! `Install folder <path> not mounted.` The check reads the log of every Steam folder in the
//! seat's home ([`shared::STEAM_ROOTS`]) and takes the last load of the unit's run. For each
//! folder of ours it missed, it tries the folder as that Steam does: its user, its groups, its
//! mount namespace. The error that try meets, beside the folder's owner and mode on the host,
//! names the cause. A Steam the library list doesn't reach, a Flatpak's or a Snap's, is named,
//! and a Steam the check can't find is logged with what the seat's user runs.

use super::accounts::Passwd;
use super::{libraries, output, run, safe_open, shared};
use crate::model::SeatId;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// One library load and what Steam logs after it fit many times over.
const TAIL: u64 = 256 * 1024;

const LOAD: &str = "] Loaded Steam library folders configuration: ";

/// At most this many of the seat user's processes are logged when Steam isn't among them.
const LISTED: usize = 8;
/// The characters of each listed process's arguments kept.
const CMDLINE: usize = 200;

/// The last library load of the seat's Steam, when it went wrong.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Missed {
    /// The Steam folder under the seat's home whose log holds the load.
    pub root: &'static str,
    /// The load's local time as Steam logs it, which tells one load from the next.
    pub stamp: String,
    /// The folders of ours it couldn't open.
    pub folders: Vec<String>,
}

impl Missed {
    /// Whether that Steam is one the seat's library list reaches ([`shared::SEAT_STEAMS`]).
    pub(super) fn listed(&self) -> bool {
        shared::SEAT_STEAMS.contains(&self.root)
    }
}

/// What the Steam in `home` that loaded its libraries last missed of seat `id`'s folders. `None`
/// when that Steam opened every folder of ours and is one the list reaches, or none logged a load.
pub(super) fn missed(state: &Path, id: &SeatId, home: &Path) -> Option<Missed> {
    let games = shared::games_dir(state);
    let libraries = libraries::libraries_dir(state, id);
    let ours = |folder: &str| {
        let folder = Path::new(folder);
        folder == games || folder.starts_with(&libraries)
    };
    let mut last: Option<Missed> = None;
    // `.steam/steam` is a link to one of the others, which `safe_open` doesn't follow.
    for root in shared::STEAM_ROOTS
        .into_iter()
        .filter(|root| *root != ".steam/steam")
    {
        let Some(log) = read_log(home, root).ok().flatten() else {
            continue;
        };
        let Some((stamp, folders)) = parse(&log, ours) else {
            continue;
        };
        if last.as_ref().is_none_or(|last| stamp > last.stamp) {
            last = Some(Missed {
                root,
                stamp,
                folders,
            });
        }
    }
    last.filter(|load| !load.folders.is_empty() || !load.listed())
}

/// Whether `missed` is a load of `unit`'s current run, not one the log keeps from the last. A
/// start time that can't be read counts the load in.
pub(super) fn this_run(missed: &Missed, unit: &str) -> bool {
    let args = ["show", "-p", "ActiveEnterTimestamp", "--value", unit];
    let shown = run("systemctl_failed", "systemctl", &args).unwrap_or_default();
    started(&shown).is_none_or(|started| missed.stamp >= started)
}

/// `systemctl show`'s `Fri 2026-11-07 19:34:42 CET` as Steam stamps a line: local, to the second.
fn started(shown: &str) -> Option<String> {
    let mut words = shown.split_whitespace().skip(1);
    let (day, time) = (words.next()?, words.next()?);
    let stamp = format!("{day} {time}");
    let digits = |at: &[usize]| at.iter().all(|&i| stamp.as_bytes()[i].is_ascii_digit());
    (stamp.len() == 19 && digits(&[0, 3, 5, 8, 11, 14, 17, 18])).then_some(stamp)
}

/// The stamp of `log`'s last library load, and the folders `ours` admits it couldn't open.
fn parse(log: &str, ours: impl Fn(&str) -> bool) -> Option<(String, Vec<String>)> {
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
    Some((stamp, folders))
}

/// The tail of the log of the Steam at `root` in `home`, which the seat's user writes: opened
/// without following a link.
fn read_log(home: &Path, root: &str) -> std::io::Result<Option<String>> {
    let names: Vec<&str> = root.split('/').chain(["logs"]).collect();
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

/// Logs which Steam loaded and how it runs, then, for each folder in `missed`, what keeps it from
/// it.
pub(super) fn explain(id: &SeatId, seat: &Passwd, missed: &Missed) {
    let steam_root = missed.root;
    if !missed.listed() {
        tracing::warn!(seat = %id, steam_root, "seat Steam without the host's library list");
    }
    let processes = processes(Path::new("/proc"), seat.uid);
    let Some(steam) = processes.iter().find(|process| process.is_client()) else {
        let steam_like: Vec<String> = (processes.iter())
            .filter(|process| process.cmdline.to_lowercase().contains("steam"))
            .take(LISTED)
            .map(|process| {
                let cmdline: String = process.cmdline.chars().take(CMDLINE).collect();
                format!("{} {} {cmdline}", process.pid, process.cgroup)
            })
            .collect();
        tracing::warn!(
            seat = %id,
            steam_root,
            folders = ?missed.folders,
            steam = "not found",
            seat_processes = processes.len(),
            ?steam_like,
            "seat Steam missed library folders"
        );
        return;
    };
    tracing::warn!(
        seat = %id,
        steam_root,
        steam_pid = steam.pid,
        steam_binary = %steam.argv0,
        steam_cgroup = %steam.cgroup,
        steam_gid = steam.gid,
        steam_groups = ?steam.groups,
        seat_gid = seat.gid,
        "seat Steam missed library folders"
    );
    for folder in &missed.folders {
        let host = on_host(Path::new(folder));
        let as_steam = open_as(steam, folder);
        tracing::warn!(seat = %id, folder, host, %as_steam, "seat Steam library folder out of reach");
    }
}

/// A process of the seat's user, as `/proc` shows it.
#[derive(Debug, PartialEq, Eq)]
struct Process {
    pid: u32,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
    argv0: String,
    /// Its arguments joined by spaces.
    cmdline: String,
    cgroup: String,
}

impl Process {
    /// The Steam client, the binary that writes the content log: `steam` in `ubuntu12_32`, or
    /// `ubuntu12_64` for a 64-bit client.
    fn is_client(&self) -> bool {
        let binary = Path::new(&self.argv0);
        let dir = binary.parent().and_then(Path::file_name);
        binary.file_name() == Some("steam".as_ref())
            && dir.is_some_and(|dir| dir.to_string_lossy().starts_with("ubuntu12_"))
    }
}

/// Every process whose real user is `uid`.
fn processes(proc: &Path, uid: u32) -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return Vec::new();
    };
    let process = |entry: std::fs::DirEntry| {
        let pid = entry.file_name().to_str()?.parse().ok()?;
        let dir = entry.path();
        let status = std::fs::read_to_string(dir.join("status")).ok()?;
        let ids = |name: &str| -> Vec<u32> {
            let line = status.lines().find_map(|line| line.strip_prefix(name));
            let ids = line.unwrap_or("").split_whitespace();
            ids.filter_map(|id| id.parse().ok()).collect()
        };
        if ids("Uid:").first() != Some(&uid) {
            return None;
        }
        let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default();
        let args: Vec<String> = (cmdline.split(|byte| *byte == 0))
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let cgroup = std::fs::read_to_string(dir.join("cgroup")).unwrap_or_default();
        let cgroup = cgroup.lines().find_map(|line| line.strip_prefix("0::"));
        Some(Process {
            pid,
            uid,
            gid: *ids("Gid:").first()?,
            groups: ids("Groups:"),
            argv0: args.first().cloned().unwrap_or_default(),
            cmdline: args.join(" "),
            cgroup: cgroup.unwrap_or("").to_owned(),
        })
    };
    entries.flatten().filter_map(process).collect()
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
fn open_as(steam: &Process, folder: &str) -> String {
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
        let (stamp, folders) = parse(LOG, ours).expect("a load");
        assert_eq!(stamp, "2026-11-07 13:14:16");
        assert_eq!(
            folders,
            ["/var/libraries/01c1"],
            "ours, once, from the last load"
        );
    }

    #[test]
    fn a_load_that_opened_every_folder_of_ours_missed_nothing() {
        let log = format!("{LOG}[2026-11-07 14:00:00{LOAD}/h/steamapps/libraryfolders.vdf\n");
        let (stamp, folders) = parse(&log, ours).expect("a load");
        assert_eq!((stamp.as_str(), folders.len()), ("2026-11-07 14:00:00", 0));
        assert_eq!(parse("", ours), None, "no load");
    }

    /// Every Steam folder's log is read, and the latest load wins: a Flatpak Steam that ran last
    /// is named though it missed nothing, as its list never had the host's libraries.
    #[test]
    fn the_steam_that_loaded_last_is_the_one_judged() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state) = (temp.path().join("home"), temp.path().join("state"));
        let id = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let games = shared::games_dir(&state);
        let log = |root: &str, stamp: &str, missed: &str| {
            let logs = home.join(root).join("logs");
            std::fs::create_dir_all(&logs).unwrap();
            let mut text = format!("[{stamp}{LOAD}/x/libraryfolders.vdf\n");
            if !missed.is_empty() {
                text += &format!("[{stamp}] Install folder {missed} not mounted.\n");
            }
            std::fs::write(logs.join("content_log.txt"), text).unwrap();
        };
        log(
            ".local/share/Steam",
            "2026-11-07 19:37:06",
            &games.to_string_lossy(),
        );
        let found = missed(&state, &id, &home).expect("its own copy missed the games");
        assert_eq!((found.root, found.listed()), (".local/share/Steam", true));
        assert_eq!(found.folders, [games.to_string_lossy()]);
        log(
            ".var/app/com.valvesoftware.Steam/.local/share/Steam",
            "2026-11-07 20:01:00",
            "",
        );
        let found = missed(&state, &id, &home).expect("the Flatpak ran last");
        assert_eq!(
            found.root,
            ".var/app/com.valvesoftware.Steam/.local/share/Steam"
        );
        assert!(!found.listed() && found.folders.is_empty());
        log(".local/share/Steam", "2026-11-07 20:05:00", "");
        assert_eq!(
            missed(&state, &id, &home),
            None,
            "its own copy opened everything last"
        );
    }

    #[test]
    fn a_unit_start_reads_as_a_steam_stamp() {
        let stamp = started("Fri 2026-11-07 19:34:42 CET\n");
        assert_eq!(stamp.as_deref(), Some("2026-11-07 19:34:42"));
        assert!(
            "2026-11-07 19:34:41" < stamp.as_deref().unwrap(),
            "the last run's load"
        );
        assert_eq!(started(""), None, "a unit that never ran");
        assert_eq!(started("n/a"), None);
    }

    #[test]
    fn steam_is_the_seat_user_s_client_binary() {
        let proc = tempfile::tempdir().unwrap();
        let process = |pid: &str, cmdline: &str, uid: u32| {
            let dir = proc.path().join(pid);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("cmdline"), cmdline.replace(' ', "\0") + "\0").unwrap();
            let status = format!(
                "Name:\tsteam\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t946\t946\t946\t946\nGroups:\t977 946 \n"
            );
            std::fs::write(dir.join("status"), status).unwrap();
            std::fs::write(dir.join("cgroup"), "0::/user.slice/app-steam.scope\n").unwrap();
        };
        process("10", "/bin/bash /usr/bin/steam -silent", 946);
        process(
            "11",
            "/h/.local/share/Steam/ubuntu12_32/steam -silent",
            1000,
        );
        process("12", "/h/.local/share/Steam/ubuntu12_64/steam -silent", 946);
        let seat = processes(proc.path(), 946);
        assert_eq!(seat.len(), 2, "the seat user's only");
        let found = seat.iter().find(|p| p.is_client()).expect("its Steam");
        assert_eq!((found.pid, found.gid), (12, 946));
        assert_eq!(found.groups, [977, 946]);
        assert_eq!(found.cgroup, "/user.slice/app-steam.scope");
        let launcher = seat.iter().find(|p| p.pid == 10).unwrap();
        assert!(
            !launcher.is_client(),
            "a script named steam runs under its shell"
        );
        assert_eq!(launcher.cmdline, "/bin/bash /usr/bin/steam -silent");
    }
}
