//! What this host reads of the seat contract, in one place.
//!
//! [`HostRole`] is what the process is: the console host, the door, or a seat's host. The seat
//! supervisor (`pf-seats`) marks each seat host with `PUNKTFUNK_SEAT_SESSION=1` plus a
//! `PUNKTFUNK_SEAT_ID`; the host never learns how those sessions come to exist. No marker is
//! the console host.
//!
//! `serve` fixes the role with [`resolve`]. Any other process reads the environment at each
//! [`role`] call. No other crate reads a role variable.
//! `every_predicate_answers_its_truth_table_row` pins what each role answers. [`ignored`] names
//! each setting that is set and changes nothing, which `serve` warns of.
//!
//! The id is untrusted process input, so it is validated once here rather than
//! at each use: it reaches a device-parameter marker and log lines.
//! `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.

/// What this host process is. A policy that turns on more than one role is an exhaustive
/// `match`, so a new variant fails to compile wherever nobody has decided for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRole {
    /// No marker: the console host.
    Plain,
    /// `serve --door`, on a host that is no seat's: it advertises, pairs,
    /// serves the console and places every connect on a seat, and never streams itself.
    Door,
    /// `PUNKTFUNK_SEAT_SESSION=1`: a supervisor-managed seat owns this host, not the console.
    /// It is never the door, whatever asks for one.
    Seat(SeatRole),
}

/// Whose seat a seat host serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatRole {
    /// A seat of its own. Its desktop follows the seat contract: one virtual screen, never a
    /// real monitor, the box's Seat defaults and no settings of its own
    /// (`design/web-console-structure-2026-10.md` §2.1).
    Contract,
    /// `PUNKTFUNK_SEAT_OWNER=1`: the box owner's own host behind the door, at the desk. It
    /// serves the owner profile and the light-seat profiles that play inside the owner's host.
    /// It reads the row's contract, but the box's session there is the owner's.
    Owner,
    /// The owner's row in the headless session the seat runner starts it in
    /// (`PUNKTFUNK_SEAT_HEADLESS=1`). Its Game Mode is the host's, as a seat's is.
    OwnerHeadless,
}

impl HostRole {
    /// Every role, for a test that walks a policy over all of them.
    pub const ALL: [HostRole; 5] = [
        HostRole::Plain,
        HostRole::Door,
        HostRole::Seat(SeatRole::Contract),
        HostRole::Seat(SeatRole::Owner),
        HostRole::Seat(SeatRole::OwnerHeadless),
    ];

    /// The role these values name. Each is a variable's value, `None` when unset, and a marker
    /// is its exact value `1`. `door_flag` is `serve --door`. The owner and headless markers
    /// mean nothing without the seat marker, and the door means nothing with it: a host a
    /// supervisor started on a seat is never the door.
    pub fn from_env_values(
        seat_session: Option<&str>,
        seat_owner: Option<&str>,
        seat_headless: Option<&str>,
        door_flag: bool,
    ) -> HostRole {
        let on = |value: Option<&str>| value == Some("1");
        let seat = match (on(seat_session), on(seat_owner), on(seat_headless)) {
            (false, _, _) => None,
            (true, false, _) => Some(SeatRole::Contract),
            (true, true, false) => Some(SeatRole::Owner),
            (true, true, true) => Some(SeatRole::OwnerHeadless),
        };
        match (seat, door_flag) {
            (Some(seat), _) => HostRole::Seat(seat),
            (None, true) => HostRole::Door,
            (None, false) => HostRole::Plain,
        }
    }

    fn seat(self) -> Option<SeatRole> {
        match self {
            HostRole::Plain | HostRole::Door => None,
            HostRole::Seat(seat) => Some(seat),
        }
    }

    /// Whether a supervisor-managed seat owns this host rather than the console.
    pub fn is_seat_host(self) -> bool {
        self.seat().is_some()
    }

    /// Whether this seat host is the box owner's own, behind the door.
    pub fn is_owner_seat(self) -> bool {
        match self.seat() {
            Some(SeatRole::Owner | SeatRole::OwnerHeadless) => true,
            Some(SeatRole::Contract) | None => false,
        }
    }

    /// Whether this is a seat host that is not the owner's, whose desktop follows the seat
    /// contract ([`SeatRole::Contract`]).
    pub fn follows_contract(self) -> bool {
        match self.seat() {
            Some(SeatRole::Contract) => true,
            Some(SeatRole::Owner | SeatRole::OwnerHeadless) | None => false,
        }
    }

    /// Whether this host runs a seat's own session, whose Game Mode is the host's: a seat's, or
    /// the owner's row in its headless session. Not the owner's host at the desk.
    pub fn runs_seat_session(self) -> bool {
        match self.seat() {
            Some(SeatRole::Contract | SeatRole::OwnerHeadless) => true,
            Some(SeatRole::Owner) | None => false,
        }
    }

    /// Whether this host is the door.
    pub fn is_door(self) -> bool {
        match self {
            HostRole::Door => true,
            HostRole::Plain | HostRole::Seat(_) => false,
        }
    }
}

/// The role and whether pairing is refused, as [`resolve`] fixed them.
static RESOLVED: std::sync::OnceLock<(HostRole, bool)> = std::sync::OnceLock::new();

/// The role variables as they are now. `door_flag` is `serve --door`. A seat host refuses
/// pairing whatever `PUNKTFUNK_PAIRING` says.
fn read(door_flag: bool) -> (HostRole, bool) {
    let var = |name: &str| std::env::var(name).ok();
    let role = HostRole::from_env_values(
        var("PUNKTFUNK_SEAT_SESSION").as_deref(),
        var("PUNKTFUNK_SEAT_OWNER").as_deref(),
        var("PUNKTFUNK_SEAT_HEADLESS").as_deref(),
        door_flag,
    );
    let refused = var("PUNKTFUNK_PAIRING").as_deref() == Some("refused");
    (role, role.is_seat_host() || refused)
}

/// Fixes this process's role for the rest of its life. `serve` calls it once its arguments are
/// read, with whether `--door` is among them. The first call wins.
pub fn resolve(door_flag: bool) {
    RESOLVED.get_or_init(|| read(door_flag));
}

/// What [`resolve`] fixed, else the environment as it is now. A process that never resolves (a
/// management CLI, the Windows service, a test) reads it at each call.
fn current() -> (HostRole, bool) {
    RESOLVED.get().copied().unwrap_or_else(|| read(false))
}

/// This process's role: what [`resolve`] fixed, else the environment as it is now.
pub fn role() -> HostRole {
    current().0
}

/// Each role variable, the one value that sets its marker, and what to say of any other.
const MARKERS: [(&str, &str, &str); 4] = [
    ("PUNKTFUNK_SEAT_SESSION", "1", ONLY_ONE),
    ("PUNKTFUNK_SEAT_OWNER", "1", ONLY_ONE),
    ("PUNKTFUNK_SEAT_HEADLESS", "1", ONLY_ONE),
    ("PUNKTFUNK_PAIRING", "refused", ONLY_REFUSED),
];
const ONLY_ONE: &str = "only the value 1 sets it";
const ONLY_REFUSED: &str = "only the value refused sets it";
const NO_SEAT_MARKER: &str = "PUNKTFUNK_SEAT_SESSION is not 1";
const ON_A_SEAT_HOST: &str = "a seat host is never the door";

/// Each seat-contract setting that is set and changes nothing, with why: a role variable whose
/// value is not its marker; the owner marker, the headless marker or a seat id without the seat
/// marker; the door asked for on a seat host. `values` are the role variables' in [`MARKERS`]
/// order, `None` when unset. Naming a setting here changes no answer.
pub fn ignored_values(
    values: [Option<&str>; 4],
    door_flag: bool,
    seat_id_set: bool,
) -> Vec<(&'static str, &'static str)> {
    let [seat_session, seat_owner, seat_headless, _] = values;
    let mut ignored = Vec::new();
    for (value, (name, marker, why)) in values.into_iter().zip(MARKERS) {
        if value.is_some_and(|value| value != marker) {
            ignored.push((name, why));
        }
    }
    let (stray, why) = if seat_session == Some("1") {
        (vec![("--door", door_flag)], ON_A_SEAT_HOST)
    } else {
        let seat = vec![
            ("PUNKTFUNK_SEAT_OWNER", seat_owner == Some("1")),
            ("PUNKTFUNK_SEAT_HEADLESS", seat_headless == Some("1")),
            ("PUNKTFUNK_SEAT_ID", seat_id_set),
        ];
        (seat, NO_SEAT_MARKER)
    };
    ignored.extend(
        stray
            .into_iter()
            .filter(|(_, set)| *set)
            .map(|(name, _)| (name, why)),
    );
    ignored
}

/// [`ignored_values`] of this process, for `serve` to warn of once at startup.
pub fn ignored(door_flag: bool) -> Vec<(&'static str, &'static str)> {
    let values = MARKERS.map(|(name, ..)| std::env::var(name).ok());
    let seat_id_set = std::env::var_os("PUNKTFUNK_SEAT_ID").is_some();
    ignored_values(
        values.each_ref().map(Option::as_deref),
        door_flag,
        seat_id_set,
    )
}

/// 32 lowercase hexadecimal characters, so the id is safe as a device-parameter
/// marker and in a log line. Any other value is a supervisor bug, not a seat.
pub fn validate_seat_id(raw: &str) -> Result<&str, &'static str> {
    let valid = raw.len() == SEAT_ID_LEN
        && raw
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    valid
        .then_some(raw)
        .ok_or("PUNKTFUNK_SEAT_ID must be 32 lowercase hexadecimal characters")
}

const SEAT_ID_LEN: usize = 32;

/// This process's validated seat id, or `None` on a host that is no seat's: the id belongs to
/// the seat marker and is not read without it. The error is a missing or rejected id, which
/// callers surface rather than ignore.
pub fn seat_id() -> Result<Option<String>, &'static str> {
    if !role().is_seat_host() {
        return Ok(None);
    }
    // A seat without an id would mint audio devnodes the console host also matches.
    let raw = std::env::var_os("PUNKTFUNK_SEAT_ID")
        .ok_or("PUNKTFUNK_SEAT_SESSION=1 without a PUNKTFUNK_SEAT_ID")?;
    let text = raw
        .to_str()
        .ok_or("PUNKTFUNK_SEAT_ID must be valid Unicode")?;
    validate_seat_id(text).map(|id| Some(id.to_owned()))
}

/// The box's config dir a seat host reads its trust from (`PUNKTFUNK_TRUST_DIR`): the
/// identity, the pairing store, `profiles.json` and the device display overlays. Read only;
/// the box host is their one writer. `None` on a host that is no seat's, as for [`seat_id`],
/// and for a relative path.
pub fn trust_dir() -> Option<std::path::PathBuf> {
    if !role().is_seat_host() {
        return None;
    }
    std::env::var_os("PUNKTFUNK_TRUST_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// The file a Windows seat host writes into its config dir just before it drops its own session's
/// connection: the time, in unix milliseconds as decimal text.
pub const SEAT_DISCONNECT_MARK: &str = "disconnect-mark";

/// How long after it was written a disconnect mark still excuses a drop.
const DISCONNECT_MARK_LIFE: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether a [`SEAT_DISCONNECT_MARK`]'s text excuses the drop of a connection that began at
/// `since`: its time is at or after `since` and at most 30 s before `now`. A mark from before the
/// connection began belongs to an earlier one.
pub fn mark_excuses_drop(
    mark: &str,
    since: std::time::SystemTime,
    now: std::time::SystemTime,
) -> bool {
    let Some(marked) = mark.trim().parse().ok().and_then(|millis| {
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(millis))
    }) else {
        return false;
    };
    marked >= since
        && now
            .duration_since(marked)
            .is_ok_and(|age| age <= DISCONNECT_MARK_LIFE)
}

/// Where a host reads the box's library from (`PUNKTFUNK_LIBRARY_DIR`): `library*.json`,
/// `library-metadata/`, and on Windows the plugin manifests and grants an `exec` entry resolves
/// against. A Windows seat names the box's config dir; on Linux the door and every seat but the
/// owner's name the supervisor's copy of the owner's library. Read only; the box's host, or the
/// owner's, is their one writer. `None` on every other host, and for a relative path.
pub fn box_library_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_LIBRARY_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// A Windows seat's own `steam.exe` (`PUNKTFUNK_SEAT_STEAM`), which may not be copied yet. A seat
/// host starts Steam from nowhere else. `None` on every other host, and for a relative path.
pub fn seat_steam() -> Option<std::path::PathBuf> {
    std::env::var_os("PUNKTFUNK_SEAT_STEAM")
        .map(std::path::PathBuf::from)
        .filter(|exe| exe.is_absolute())
}

/// Where this host reads its library: [`box_library_dir`], else its own config dir. Play stats
/// are never here; they stay in [`crate::config_dir`].
pub fn library_dir() -> std::path::PathBuf {
    box_library_dir().unwrap_or_else(crate::config_dir)
}

/// Whether devices pair with the box and never with this host: every seat host, and any other
/// host with `PUNKTFUNK_PAIRING=refused`. A knock is refused, a PIN window never opens.
/// [`resolve`] fixes it with the role.
pub fn pairing_refused() -> bool {
    current().1
}

/// The seat supervisor's pad broker: a seat host asks here for the pads it cannot make itself.
pub const PADS_SOCKET: &str = "/run/punktfunk/pads.sock";

/// The supervisor's uinput proxy for `account`, a CUSE device under `/dev`; the seat's unit
/// binds it over `/dev/uinput`. Sysfs names it with `!` for each `/`.
pub fn uinput_devname(account: &str) -> String {
    format!("punktfunk/uinput-{account}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mark excuses a drop only between the connection's start and 30 s after it was written.
    #[test]
    fn a_disconnect_mark_excuses_only_its_own_connection_for_30_s() {
        use std::time::{Duration, UNIX_EPOCH};
        let since = UNIX_EPOCH + Duration::from_secs(1_000);
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        let mark = |secs: u64| (secs * 1_000).to_string();
        assert!(mark_excuses_drop(&mark(1_005), since, at(1_010)));
        assert!(mark_excuses_drop(
            &format!("{}\r\n", mark(1_000)),
            since,
            at(1_030)
        ));
        assert!(
            !mark_excuses_drop(&mark(999), since, at(1_001)),
            "before the connection"
        );
        assert!(!mark_excuses_drop(&mark(1_005), since, at(1_036)), "stale");
        assert!(
            !mark_excuses_drop(&mark(1_005), since, at(1_004)),
            "from the future"
        );
        assert!(!mark_excuses_drop("", since, at(1_010)));
        assert!(!mark_excuses_drop("soon", since, at(1_010)));
        assert!(!mark_excuses_drop(&u64::MAX.to_string(), since, at(1_010)));
    }

    /// What each predicate answers, by the markers set. Inputs: `S` `PUNKTFUNK_SEAT_SESSION=1`,
    /// `O` `PUNKTFUNK_SEAT_OWNER=1`, `H` `PUNKTFUNK_SEAT_HEADLESS=1`, `D` `serve --door`, `P`
    /// `PUNKTFUNK_PAIRING=refused`. Answers, in [`answers`]' order:
    /// `s` seat host, `o` owner seat, `c` follows the contract, `r` runs a seat session, `d` door,
    /// `p` pairing refused, `e` a missing seat id is an error.
    const TRUTH: [(&str, &str); 32] = [
        ("-----", "-------"),
        ("----P", "-----p-"),
        ("---D-", "----d--"),
        ("---DP", "----dp-"),
        ("--H--", "-------"),
        ("--H-P", "-----p-"),
        ("--HD-", "----d--"),
        ("--HDP", "----dp-"),
        ("-O---", "-------"),
        ("-O--P", "-----p-"),
        ("-O-D-", "----d--"),
        ("-O-DP", "----dp-"),
        ("-OH--", "-------"),
        ("-OH-P", "-----p-"),
        ("-OHD-", "----d--"),
        ("-OHDP", "----dp-"),
        ("S----", "s-cr-pe"),
        ("S---P", "s-cr-pe"),
        ("S--D-", "s-cr-pe"),
        ("S--DP", "s-cr-pe"),
        ("S-H--", "s-cr-pe"),
        ("S-H-P", "s-cr-pe"),
        ("S-HD-", "s-cr-pe"),
        ("S-HDP", "s-cr-pe"),
        ("SO---", "so---pe"),
        ("SO--P", "so---pe"),
        ("SO-D-", "so---pe"),
        ("SO-DP", "so---pe"),
        ("SOH--", "so-r-pe"),
        ("SOH-P", "so-r-pe"),
        ("SOHD-", "so-r-pe"),
        ("SOHDP", "so-r-pe"),
    ];

    /// Each role variable in [`TRUTH`]'s input order: its name, the value that sets its marker,
    /// and a value that must not.
    const VARS: [(&str, &str, &str); 4] = [
        ("PUNKTFUNK_SEAT_SESSION", "1", "true"),
        ("PUNKTFUNK_SEAT_OWNER", "1", "0"),
        ("PUNKTFUNK_SEAT_HEADLESS", "1", "yes"),
        ("PUNKTFUNK_PAIRING", "refused", "1"),
    ];

    /// Set on the child only: `flag` for a `serve --door` process.
    const CHILD: &str = "PF_ROLE_TRUTH_CHILD";
    const MARK: &str = "role-answers=";
    /// Every variable unset, set and near-missed, with and without the flag.
    const COMBINATIONS: usize = 3 * 3 * 3 * 3 * 2;

    fn answers() -> String {
        let role = role();
        let answers = [
            ('s', role.is_seat_host()),
            ('o', role.is_owner_seat()),
            ('c', role.follows_contract()),
            ('r', role.runs_seat_session()),
            ('d', role.is_door()),
            ('p', pairing_refused()),
            ('e', seat_id().is_err()),
        ];
        answers
            .iter()
            .map(|(letter, on)| if *on { *letter } else { '-' })
            .collect()
    }

    /// The role comes from the process environment, so each combination runs this test again as
    /// a child with that environment. The child prints its answers twice: read from the
    /// environment, then as [`resolve`] fixes them.
    #[test]
    fn every_predicate_answers_its_truth_table_row() {
        if let Some(door) = std::env::var_os(CHILD) {
            println!("{MARK}{}", answers());
            resolve(door == "flag");
            println!("{MARK}{}", answers());
            return;
        }
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        std::thread::scope(|scope| {
            for worker in 0..workers {
                scope.spawn(move || (worker..COMBINATIONS).step_by(workers).for_each(check));
            }
        });
    }

    /// This test binary run again as `test` alone, with no seat-contract variable set.
    fn child(test: &str) -> std::process::Command {
        let module = module_path!().split_once("::").expect("crate::module").1;
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child
            .args(["--exact", &format!("{module}::{test}"), "--nocapture"])
            .env(CHILD, "none")
            .env_remove("PUNKTFUNK_SEAT_ID");
        for (name, ..) in VARS {
            child.env_remove(name);
        }
        child
    }

    /// What a child printed after each [`MARK`], to the end of that line.
    fn printed(child: &mut std::process::Command) -> Vec<String> {
        let out = child.output().expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let answers = stdout.split(MARK).skip(1);
        answers
            .map(|rest| rest.lines().next().unwrap_or_default().to_owned())
            .collect()
    }

    /// Bit 0 of `combination` is the flag; the rest is a base-3 digit per variable.
    fn check(combination: usize) {
        let flag = combination % 2 == 1;
        let mut child = child("every_predicate_answers_its_truth_table_row");
        child.env(CHILD, if flag { "flag" } else { "none" });
        let mut digits = combination / 2;
        let mut set = [false; 4];
        for (at, (name, on, near_miss)) in VARS.iter().enumerate() {
            match digits % 3 {
                0 => child.env_remove(name),
                1 => child.env(name, on),
                _ => child.env(name, near_miss),
            };
            set[at] = digits % 3 == 1;
            digits /= 3;
        }
        let row = |door_flag: bool| {
            let [seat, owner, headless, pairing] = set;
            let inputs: String = "SOHDP"
                .chars()
                .zip([seat, owner, headless, door_flag, pairing])
                .map(|(letter, on)| if on { letter } else { '-' })
                .collect();
            let row = TRUTH.iter().find(|(row, _)| *row == inputs);
            row.map(|(_, answers)| *answers)
        };
        // Only `resolve` knows the flag.
        let rows: Vec<&str> = [row(false), row(flag)].into_iter().flatten().collect();
        assert_eq!(
            printed(&mut child),
            rows,
            "combination {combination}, markers {set:?}"
        );
    }

    /// Without the seat marker the id is not read at all. With it, a missing or malformed one
    /// is an error.
    #[test]
    fn a_seat_id_counts_only_with_the_seat_marker() {
        if std::env::var_os(CHILD).is_some() {
            let answer = match seat_id() {
                Ok(None) => "none",
                Ok(Some(_)) => "some",
                Err(_) => "error",
            };
            println!("{MARK}{answer}");
            return;
        }
        let id = "0123456789abcdef0123456789abcdef";
        // (seat marker, id) → what `seat_id` answers
        let table = [
            (None, None, "none"),
            (None, Some(id), "none"),
            (None, Some("seat 1"), "none"),
            (Some("true"), Some(id), "none"),
            (Some("1"), None, "error"),
            (Some("1"), Some("seat 1"), "error"),
            (Some("1"), Some(id), "some"),
        ];
        for (marker, id, answer) in table {
            let mut child = child("a_seat_id_counts_only_with_the_seat_marker");
            child.envs(marker.map(|value| ("PUNKTFUNK_SEAT_SESSION", value)));
            child.envs(id.map(|value| ("PUNKTFUNK_SEAT_ID", value)));
            assert_eq!(
                printed(&mut child),
                [answer],
                "marker {marker:?}, id {id:?}"
            );
        }
    }

    #[test]
    fn a_setting_that_changes_nothing_is_named_with_why() {
        let (seat, owner, headless, pairing) = (VARS[0].0, VARS[1].0, VARS[2].0, VARS[3].0);
        // A console host, a door, and the two seats a supervisor starts: nothing to say.
        assert!(ignored_values([None; 4], false, false).is_empty());
        assert!(ignored_values([None; 4], true, false).is_empty());
        let row = [Some("1"), None, Some("1"), Some("refused")];
        assert!(ignored_values(row, false, true).is_empty());
        let owner_row = [Some("1"), Some("1"), Some("1"), Some("refused")];
        assert!(ignored_values(owner_row, false, true).is_empty());

        let near_misses = [Some("true"), Some("0"), Some("yes"), Some("1")];
        assert_eq!(
            ignored_values(near_misses, false, false),
            [
                (seat, ONLY_ONE),
                (owner, ONLY_ONE),
                (headless, ONLY_ONE),
                (pairing, "only the value refused sets it"),
            ]
        );
        assert_eq!(
            ignored_values([None, Some("1"), Some("1"), None], false, true),
            [
                (owner, NO_SEAT_MARKER),
                (headless, NO_SEAT_MARKER),
                ("PUNKTFUNK_SEAT_ID", NO_SEAT_MARKER),
            ]
        );
        assert_eq!(
            ignored_values([Some("1"), None, None, None], true, true),
            [("--door", ON_A_SEAT_HOST)]
        );
    }

    /// [`ignored`] reads each variable under its own name.
    #[test]
    fn the_settings_that_change_nothing_come_from_the_environment() {
        const TEST: &str = "the_settings_that_change_nothing_come_from_the_environment";
        if let Some(door) = std::env::var_os(CHILD) {
            let settings: Vec<_> = ignored(door == "flag").into_iter().map(|s| s.0).collect();
            println!("{MARK}{}", settings.join(" "));
            return;
        }
        let mut near_misses = child(TEST);
        near_misses.envs(VARS.map(|(name, _, near_miss)| (name, near_miss)));
        near_misses.env("PUNKTFUNK_SEAT_ID", "stray");
        let named = VARS.map(|(name, ..)| name).join(" ") + " PUNKTFUNK_SEAT_ID";
        assert_eq!(printed(&mut near_misses), [named]);

        let mut seat = child(TEST);
        seat.env(CHILD, "flag").env(VARS[0].0, "1");
        assert_eq!(printed(&mut seat), ["--door"]);
    }

    /// A policy test walks [`HostRole::ALL`], so it holds each role the markers name, once.
    #[test]
    fn all_holds_each_role_the_markers_name() {
        let mut named = Vec::new();
        for markers in 0..16u8 {
            let value = |bit: u8| (markers & bit != 0).then_some("1");
            let role = HostRole::from_env_values(value(1), value(2), value(4), markers & 8 != 0);
            if !named.contains(&role) {
                named.push(role);
            }
        }
        assert_eq!(named.len(), HostRole::ALL.len());
        assert!(HostRole::ALL.iter().all(|role| named.contains(role)));
    }

    #[test]
    fn seat_ids_reject_unsafe_marker_and_log_text() {
        for raw in [
            "",
            "550E8400E29B41D4A716446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "seat 1",
            "seat/1",
            "seat\n1",
            "séat-1",
            "0123456789abcdefg123456789abcdef",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_seat_id(raw).is_err(), "{raw:?}");
        }
        assert_eq!(
            validate_seat_id("0123456789abcdef0123456789abcdef"),
            Ok("0123456789abcdef0123456789abcdef")
        );
    }
}
