//! The Linux matcher: `/proc`.
//!
//! Contract and rules live in [`super`]. This module only reads `/proc`. `stat`'s `comm` can
//! contain spaces and parentheses, so fields are counted from the last `)`.

use super::{ProcRef, START_SLACK_SECS};
use crate::library::DetectSpec;
use std::path::{Path, PathBuf};

/// Cap on one `cmdline`/`environ` read. `ARG_MAX` is the practical bound; without this a scan can
/// allocate without bound.
const MAX_PROC_BLOB: u64 = 512 * 1024;

/// `/proc` reader. Root, uid, and clock are parameters so tests can feed a fixture tree.
pub struct Scanner {
    root: PathBuf,
    /// Own-uid filter. `None` is tests-only and considers every process.
    uid: Option<u32>,
    ticks_per_sec: f64,
}

impl Scanner {
    pub fn system() -> Self {
        // SAFETY: a parameterless POSIX call that always succeeds and touches no memory.
        let uid = unsafe { libc::getuid() };
        // SAFETY: `sysconf` reads a static system limit by name; no memory of ours is involved, and a
        // non-positive answer (which the caller below handles) is its documented failure signal.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        Self {
            root: PathBuf::from("/proc"),
            uid: Some(uid),
            // Non-positive sysconf would poison every start-time comparison; 100 is the usual Linux CLK_TCK.
            ticks_per_sec: if ticks > 0 { ticks as f64 } else { 100.0 },
        }
    }

    /// Seconds since boot on the process-start timeline ([`super::launch_stamp`]). `None` if
    /// `/proc/uptime` is unreadable — that disables the start-time filter, it does not reject everyone.
    pub fn now_stamp(&self) -> Option<f64> {
        let text = std::fs::read_to_string(self.root.join("uptime")).ok()?;
        text.split_whitespace().next()?.parse().ok()
    }

    /// Processes matching any of `spec`'s signals, started at or after `min_start` (seconds since
    /// boot; `None` disables that filter). Signals are a union: appid, env marker, exact exe, or
    /// install dir each independently qualify.
    pub fn find(&self, spec: &DetectSpec, min_start: Option<f64>) -> Vec<ProcRef> {
        if spec.is_empty() {
            return Vec::new();
        }
        // Canonicalize once: a game reached through a symlink would miss its own image path.
        let dir = spec
            .install_dir
            .as_deref()
            .map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));
        let exe = spec
            .exe
            .as_deref()
            .map(|e| e.canonicalize().unwrap_or_else(|_| e.to_path_buf()));
        let steam_tok = spec.steam_appid.map(|id| format!("AppId={id}"));

        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let dir_path = e.path();
            if let Some(uid) = self.uid {
                let owned = std::fs::metadata(&dir_path)
                    .map(|m| {
                        use std::os::unix::fs::MetadataExt;
                        m.uid() == uid
                    })
                    .unwrap_or(false);
                if !owned {
                    continue;
                }
            }
            let Some(start_ticks) = self.start_ticks(&dir_path) else {
                continue;
            };
            if let Some(min) = min_start {
                let started = start_ticks as f64 / self.ticks_per_sec;
                if started + START_SLACK_SECS < min {
                    continue; // predates this launch — never ours (rule 1)
                }
            }
            if self.matches(
                &dir_path,
                spec,
                steam_tok.as_deref(),
                exe.as_deref(),
                dir.as_deref(),
            ) {
                out.push(ProcRef {
                    pid,
                    start: start_ticks,
                });
            }
        }
        out
    }

    /// Pin a pid the host just spawned to *this* process by reading its start time.
    ///
    /// The only safe entry into rule 2 from a bare pid: the caller spawned it and resolves it
    /// immediately, so the number cannot have been recycled. Downstream re-checks via [`Self::alive`].
    pub fn resolve(&self, pid: u32) -> Option<ProcRef> {
        let start = self.start_ticks(&self.root.join(pid.to_string()))?;
        Some(ProcRef { pid, start })
    }

    /// Diagnostics only ([`super::names`]). Gone processes yield `?`.
    pub fn name_of(&self, p: ProcRef) -> String {
        std::fs::read_to_string(self.root.join(p.pid.to_string()).join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into())
    }

    /// Pid present and start time unchanged, so a recycled pid is never alive (rule 2).
    pub fn alive(&self, procs: &[ProcRef]) -> Vec<ProcRef> {
        procs
            .iter()
            .copied()
            .filter(|p| self.start_ticks(&self.root.join(p.pid.to_string())) == Some(p.start))
            .collect()
    }

    /// `(pid, parent)` for every own process, from `stat`.
    pub fn parents(&self) -> Vec<(u32, u32)> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
                if let Some(uid) = self.uid {
                    use std::os::unix::fs::MetadataExt;
                    if std::fs::metadata(e.path()).ok()?.uid() != uid {
                        return None;
                    }
                }
                let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
                let tail = &stat[stat.rfind(')')? + 1..];
                let ppid = tail.split_whitespace().nth(1)?.parse().ok()?;
                Some((pid, ppid))
            })
            .collect()
    }

    fn matches(
        &self,
        dir_path: &Path,
        spec: &DetectSpec,
        steam_tok: Option<&str>,
        exe: Option<&Path>,
        install_dir: Option<&Path>,
    ) -> bool {
        let image = std::fs::read_link(dir_path.join("exe")).ok();
        if let Some(want) = exe {
            if image.as_deref() == Some(want) {
                return true;
            }
        }
        if let Some(dir) = install_dir {
            if image.as_deref().is_some_and(|i| i.starts_with(dir)) {
                return true;
            }
        }
        // Operator-typed fallback. Case-insensitive: missing the game costs more than a rare case collision.
        if let Some(want) = spec.process_name.as_deref() {
            let named = image
                .as_deref()
                .and_then(|i| i.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(want));
            if named {
                return true;
            }
        }

        // Image path misses Proton/Wine titles (image is the runtime) and Steam's launch reaper (argv only).
        let cmdline = read_capped(&dir_path.join("cmdline"));
        if let Some(cmdline) = cmdline.as_deref() {
            if let Some(tok) = steam_tok {
                // Both tokens, exact: `AppId=57` must not satisfy 570. Skip the pre-launch payloads —
                // Steam wraps shader pre-caching and Proton's install-script step in the same
                // `SteamLaunch AppId=` reaper, and adopting one treats its exit as the game exiting.
                let mut launch = false;
                let mut appid = false;
                let mut prelaunch = false;
                for arg in cmdline.split(|&b| b == 0) {
                    let name = program_name(arg);
                    if arg == b"SteamLaunch" {
                        launch = true;
                    } else if arg == tok.as_bytes() {
                        appid = true;
                    } else if name == b"fossilize_replay" || name == b"iscriptevaluator.exe" {
                        prelaunch = true;
                    }
                }
                if launch && appid && !prelaunch {
                    return true;
                }
            }
            // argv keeps the launcher's spelling; the image is canonical. Through a link (Fedora
            // Atomic's `/home` → `var/home`) the two differ, so either spelling counts here.
            if let Some(dir) = install_dir {
                // Path separator after the directory so `/games/x` is not satisfied by `/games/xyz/…`.
                // (`Path::starts_with` on the image already compares whole components.)
                let dirs = [dir, spec.install_dir.as_deref().unwrap_or(dir)];
                let under_dir = |arg: &[u8]| {
                    dirs.iter().any(|d| {
                        arg.strip_prefix(d.as_os_str().as_encoded_bytes())
                            .is_some_and(|rest| rest.first() == Some(&b'/'))
                    })
                };
                if cmdline.split(|&b| b == 0).any(under_dir) {
                    return true;
                }
            }
            if let Some(want) = exe {
                let exes = [want, spec.exe.as_deref().unwrap_or(want)];
                let is_exe =
                    |arg: &[u8]| exes.iter().any(|e| arg == e.as_os_str().as_encoded_bytes());
                if cmdline.split(|&b| b == 0).any(is_exe) {
                    return true;
                }
            }
        }

        // Last: reading another process's environment is the most invasive of these. Matched and discarded — never logged.
        if let Some(marker) = &spec.env_marker {
            if let Some(env) = read_capped(&dir_path.join("environ")) {
                let want: Vec<u8> = match &marker.value {
                    Some(v) => format!("{}={v}", marker.key).into_bytes(),
                    None => format!("{}=", marker.key).into_bytes(),
                };
                let hit = env.split(|&b| b == 0).any(|kv| match marker.value {
                    Some(_) => kv == want.as_slice(),
                    None => kv.starts_with(want.as_slice()),
                });
                if hit {
                    return true;
                }
            }
        }
        false
    }

    /// `/proc/<pid>/stat` field 22 (`starttime`), or `None` for a zombie: the entry outlives the
    /// process until its parent waits, and every caller means "gone" by then. `comm` is
    /// parenthesized and may contain spaces and parentheses, so fields are counted from the last
    /// `)`: the next token is field 3 (`state`), which puts `starttime` at index 19 of the remainder.
    fn start_ticks(&self, dir_path: &Path) -> Option<u64> {
        let stat = std::fs::read_to_string(dir_path.join("stat")).ok()?;
        let tail = &stat[stat.rfind(')')? + 1..];
        let mut fields = tail.split_whitespace();
        if matches!(fields.next()?, "Z" | "X") {
            return None;
        }
        fields.nth(18)?.parse().ok()
    }
}

/// Bytes: an argv entry is not required to be UTF-8.
fn program_name(arg: &[u8]) -> &[u8] {
    match arg.iter().rposition(|&b| b == b'/') {
        Some(i) => &arg[i + 1..],
        None => arg,
    }
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_PROC_BLOB).read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::DetectSpec;

    struct FakeProc {
        pid: u32,
        start: u64,
        state: &'static str,
        exe: Option<PathBuf>,
        cmdline: Vec<&'static str>,
        environ: Vec<&'static str>,
    }

    impl FakeProc {
        fn new(pid: u32, start: u64) -> Self {
            Self {
                pid,
                start,
                state: "S",
                exe: None,
                cmdline: Vec::new(),
                environ: Vec::new(),
            }
        }
        fn zombie(mut self) -> Self {
            self.state = "Z";
            self
        }
        fn exe(mut self, p: impl Into<PathBuf>) -> Self {
            self.exe = Some(p.into());
            self
        }
        fn cmdline(mut self, args: &[&'static str]) -> Self {
            self.cmdline = args.to_vec();
            self
        }
        fn environ(mut self, kvs: &[&'static str]) -> Self {
            self.environ = kvs.to_vec();
            self
        }
    }

    fn fake_proc_root(uptime: f64, procs: &[FakeProc]) -> tempfile::TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        std::fs::write(td.path().join("uptime"), format!("{uptime} 1000.0\n")).unwrap();
        // Non-pid entries must be skipped, not parsed.
        std::fs::create_dir_all(td.path().join("self")).unwrap();
        std::fs::write(td.path().join("cmdline"), b"").unwrap();
        for p in procs {
            let dir = td.path().join(p.pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            // `starttime` is field 22: after the last `)`, index 0 is `state` (field 3), so it lands at 19.
            // Comm is hostile on purpose (`evil ) name`) — naive splitting on spaces would get this wrong.
            let mut tail = vec!["0".to_string(); 20];
            tail[0] = p.state.to_string();
            tail[19] = p.start.to_string();
            std::fs::write(
                dir.join("stat"),
                format!("{} (evil ) name) {}\n", p.pid, tail.join(" ")),
            )
            .unwrap();
            if !p.cmdline.is_empty() {
                let mut blob = Vec::new();
                for a in &p.cmdline {
                    blob.extend_from_slice(a.as_bytes());
                    blob.push(0);
                }
                std::fs::write(dir.join("cmdline"), blob).unwrap();
            }
            if !p.environ.is_empty() {
                let mut blob = Vec::new();
                for a in &p.environ {
                    blob.extend_from_slice(a.as_bytes());
                    blob.push(0);
                }
                std::fs::write(dir.join("environ"), blob).unwrap();
            }
            if let Some(exe) = &p.exe {
                std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
            }
        }
        td
    }

    fn scanner(root: &Path) -> Scanner {
        Scanner {
            root: root.to_path_buf(),
            uid: None, // fixture owner is the test user; do not couple the test to it
            ticks_per_sec: 100.0,
        }
    }

    fn pids(mut v: Vec<ProcRef>) -> Vec<u32> {
        v.sort_by_key(|p| p.pid);
        v.into_iter().map(|p| p.pid).collect()
    }

    #[test]
    fn empty_spec_matches_nothing() {
        let td = fake_proc_root(100.0, &[FakeProc::new(1, 100).exe("/games/x/run")]);
        let s = scanner(td.path());
        assert!(s.find(&DetectSpec::default(), None).is_empty());
    }

    #[test]
    fn matches_exact_exe_and_install_dir() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(10, 50_000).exe("/games/hades/Hades"),
                FakeProc::new(11, 50_000).exe("/games/hades/tools/crash.bin"),
                FakeProc::new(12, 50_000).exe("/usr/bin/firefox"),
            ],
        );
        let s = scanner(td.path());
        assert_eq!(
            pids(s.find(&DetectSpec::exe("/games/hades/Hades"), None)),
            vec![10]
        );
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/hades"), None)),
            vec![10, 11]
        );
    }

    #[test]
    fn matches_a_bare_process_name_against_the_image_name_only() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(20, 50_000).exe("/opt/retroarch/bin/RetroArch"),
                FakeProc::new(21, 50_000).exe("/usr/bin/retroarch-assets-helper"),
                FakeProc::new(22, 50_000).exe("/home/p/retroarch/launcher.sh"),
            ],
        );
        let s = scanner(td.path());
        let spec = DetectSpec {
            process_name: Some("retroarch".into()),
            ..Default::default()
        };
        assert_eq!(pids(s.find(&spec, None)), vec![20]);
    }

    #[test]
    fn install_dir_does_not_match_a_sibling_with_the_same_prefix() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(90, 50_000).exe("/games/xyz/other"),
                FakeProc::new(91, 50_000).cmdline(&["wrapper", "/games/xyz/other"]),
            ],
        );
        let s = scanner(td.path());
        assert!(s.find(&DetectSpec::dir("/games/x"), None).is_empty());
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/xyz"), None)),
            vec![90, 91]
        );
    }

    #[test]
    fn matches_proton_style_game_only_in_the_cmdline() {
        // Image is the Proton runtime; the game appears only as an argument.
        let td = fake_proc_root(
            1000.0,
            &[FakeProc::new(20, 50_000)
                .exe("/steam/runtime/proton")
                .cmdline(&["proton", "waitforexitandrun", "/games/elden/eldenring.exe"])],
        );
        let s = scanner(td.path());
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/elden"), None)),
            vec![20]
        );
    }

    #[test]
    fn matches_a_cmdline_spelled_through_a_linked_home() {
        // Fedora Atomic: the launcher records `/home/u/…`, which canonicalizes to `/var/home/u/…`.
        let fs = tempfile::tempdir().expect("tempdir");
        let real = fs.path().join("var/home/u/Games/Crysis");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("Crysis.exe"), b"").unwrap();
        std::os::unix::fs::symlink(fs.path().join("var/home"), fs.path().join("home")).unwrap();
        let dir = fs.path().join("home/u/Games/Crysis");
        let arg: &'static str = Box::leak(format!("{}/Crysis.exe", dir.display()).into_boxed_str());
        let td = fake_proc_root(
            1000.0,
            &[FakeProc::new(20, 50_000)
                .exe("/steam/runtime/proton")
                .cmdline(&["proton", "waitforexitandrun", arg])],
        );
        let s = scanner(td.path());
        assert_eq!(pids(s.find(&DetectSpec::dir(&dir), None)), vec![20]);
        assert_eq!(pids(s.find(&DetectSpec::exe(arg), None)), vec![20]);
    }

    #[test]
    fn matches_steam_reaper_exactly() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(30, 50_000).cmdline(&[
                    "reaper",
                    "SteamLaunch",
                    "AppId=570",
                    "--",
                    "dota",
                ]),
                FakeProc::new(31, 50_000).cmdline(&[
                    "reaper",
                    "SteamLaunch",
                    "AppId=57",
                    "--",
                    "other",
                ]),
                FakeProc::new(32, 50_000).cmdline(&["reaper", "SteamLaunch", "--", "shader"]),
                FakeProc::new(33, 50_000).cmdline(&["something", "AppId=570"]),
            ],
        );
        let s = scanner(td.path());
        assert_eq!(pids(s.find(&DetectSpec::steam(570), None)), vec![30]);
        assert_eq!(pids(s.find(&DetectSpec::steam(57), None)), vec![31]);
    }

    #[test]
    fn steam_install_script_evaluator_is_not_the_game() {
        // Balatro's real launch on .21: the evaluator reaper carries the appid and outlives the
        // shim window, so adopting it ended the session 3 s before Steam created the game.
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(37, 50_000).cmdline(&[
                    "/home/p/.steam/ubuntu12_32/reaper",
                    "SteamLaunch",
                    "AppId=2379780",
                    "Install=1",
                    "--",
                    "/home/p/.steam/steamapps/common/Proton - Experimental/proton",
                    "run",
                    "/home/p/.steam/legacycompat/iscriptevaluator.exe",
                    "legacycompat\\evaluatorscript_2379780.vdf",
                ]),
                FakeProc::new(38, 50_000).cmdline(&[
                    "/home/p/.steam/ubuntu12_32/reaper",
                    "SteamLaunch",
                    "AppId=2379780",
                    "--",
                    "/home/p/.steam/steamapps/common/Proton - Experimental/proton",
                    "waitforexitandrun",
                    "/home/p/.steam/steamapps/common/Balatro/Balatro.exe",
                ]),
            ],
        );
        let s = scanner(td.path());
        assert_eq!(pids(s.find(&DetectSpec::steam(2_379_780), None)), vec![38]);
    }

    #[test]
    fn steam_shader_pre_caching_is_not_the_game() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(35, 50_000).cmdline(&[
                    "/home/p/.steam/ubuntu12_32/reaper",
                    "SteamLaunch",
                    "AppId=252950",
                    "--",
                    "/home/p/.steam/steamapps/common/SteamLinuxRuntime/fossilize_replay",
                    "/home/p/.steam/steamapps/shadercache/252950/fozpipelinesv6/steamapprun_pipeline_cache.foz",
                ]),
                FakeProc::new(36, 50_000).cmdline(&[
                    "/home/p/.steam/ubuntu12_32/reaper",
                    "SteamLaunch",
                    "AppId=252950",
                    "--",
                    "/home/p/.steam/steamapps/common/Proton/proton",
                    "waitforexitandrun",
                    "/home/p/.steam/steamapps/common/rocketleague/RocketLeague.exe",
                ]),
            ],
        );
        let s = scanner(td.path());
        assert_eq!(pids(s.find(&DetectSpec::steam(252_950), None)), vec![36]);
    }

    #[test]
    fn matches_env_marker_by_exact_value_or_presence() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(40, 50_000).environ(&["HOME=/home/u", "HEROIC_APP_NAME=Quail"]),
                FakeProc::new(41, 50_000).environ(&["HEROIC_APP_NAME=OtherGame"]),
                FakeProc::new(42, 50_000).environ(&["HOME=/home/u"]),
            ],
        );
        let s = scanner(td.path());
        let exact = DetectSpec::default().with_env("HEROIC_APP_NAME", Some("Quail".to_string()));
        assert_eq!(pids(s.find(&exact, None)), vec![40]);
        let any = DetectSpec::default().with_env("HEROIC_APP_NAME", None);
        assert_eq!(pids(s.find(&any, None)), vec![40, 41]);
    }

    #[test]
    fn never_adopts_a_process_that_predates_the_launch() {
        // ticks/sec is 100: pid 50 started at t=500 (already open), pid 51 at t=950; launch is t=900.
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(50, 50_000).exe("/games/hades/Hades"),
                FakeProc::new(51, 95_000).exe("/games/hades/Hades"),
            ],
        );
        let s = scanner(td.path());
        let spec = DetectSpec::dir("/games/hades");
        assert_eq!(pids(s.find(&spec, Some(900.0))), vec![51]);
        assert_eq!(pids(s.find(&spec, None)), vec![50, 51]);
    }

    #[test]
    fn start_slack_tolerates_tick_granularity() {
        // 89_950 ticks / 100 = 899.5 s — 0.5 s before launch 900, inside START_SLACK_SECS.
        let td = fake_proc_root(1000.0, &[FakeProc::new(60, 89_950).exe("/games/x/run")]);
        let s = scanner(td.path());
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/x"), Some(900.0))),
            vec![60]
        );
        // Launch 960 vs start 899.5 — a minute early is outside slack.
        assert!(s.find(&DetectSpec::dir("/games/x"), Some(960.0)).is_empty());
    }

    #[test]
    fn alive_rejects_a_recycled_pid() {
        let td = fake_proc_root(1000.0, &[FakeProc::new(70, 50_000).exe("/games/x/run")]);
        let s = scanner(td.path());
        let same = ProcRef {
            pid: 70,
            start: 50_000,
        };
        let recycled = ProcRef {
            pid: 70,
            start: 99_999,
        };
        let gone = ProcRef {
            pid: 71,
            start: 50_000,
        };
        assert_eq!(s.alive(&[same]), vec![same]);
        assert!(s.alive(&[recycled]).is_empty());
        assert!(s.alive(&[gone]).is_empty());
    }

    /// Spawn a real process to cover kernel `/proc` fields, the uid filter, and uptime matching.
    /// Fixture tests cannot prove those values agree with the running kernel. Wait on `/proc/<pid>`.
    #[test]
    fn finds_a_real_process_it_just_started() {
        // Run a copied binary from the install dir; a wrapper leaves no trace there.
        // Keep the basename because multi-call coreutils dispatches on `argv[0]`.
        let td = tempfile::tempdir().expect("tempdir");
        let game = td.path().join("sleep");
        let sleep = std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
            .map(|dir| dir.join("sleep"))
            .find(|path| path.is_file())
            .expect("sleep is on PATH");
        std::fs::copy(sleep, &game).expect("copy a stand-in game binary");
        let s = Scanner::system();
        let before = s.now_stamp().expect("real /proc/uptime is readable");

        let mut child = std::process::Command::new(&game)
            .arg("20")
            .spawn()
            .expect("spawn the fake game");
        let proc = std::path::PathBuf::from(format!("/proc/{}", child.id()));
        for _ in 0..50 {
            if proc.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(proc.exists(), "/proc/{} never appeared", child.id());

        let found = s.find(&DetectSpec::dir(td.path()), Some(before));
        assert!(
            found.iter().any(|p| p.pid == child.id()),
            "scanning the real /proc did not find pid {} under {}; found {found:?}",
            child.id(),
            td.path().display()
        );
        assert!(!s.alive(&found).is_empty());
        assert!(s
            .find(&DetectSpec::exe(&game), Some(before))
            .iter()
            .any(|p| p.pid == child.id()));

        // Stamp 60 s after now: this process started before it, so rule 1 must exclude it.
        let after = s.now_stamp().unwrap() + 60.0;
        assert!(s.find(&DetectSpec::dir(td.path()), Some(after)).is_empty());

        let _ = child.kill();
        let _ = child.wait();
        for _ in 0..50 {
            if !proc.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(s.find(&DetectSpec::dir(td.path()), Some(before)).is_empty());
        assert!(s.alive(&found).is_empty());
    }

    #[test]
    fn parses_uptime_and_hostile_comm() {
        let td = fake_proc_root(4321.5, &[FakeProc::new(80, 1234)]);
        let s = scanner(td.path());
        assert_eq!(s.now_stamp(), Some(4321.5));
        assert_eq!(s.start_ticks(&td.path().join("80")), Some(1234));
    }

    /// A killed child nobody has waited on keeps its `/proc` entry as a zombie. It must read
    /// as gone everywhere: a registry that counted it adopted a corpse on every relaunch (#1065).
    #[test]
    fn a_zombie_is_gone() {
        let td = fake_proc_root(
            100.0,
            &[
                FakeProc::new(7, 100).exe("/games/x/run"),
                FakeProc::new(8, 100).exe("/games/x/run").zombie(),
            ],
        );
        let s = scanner(td.path());
        let spec = DetectSpec {
            exe: Some("/games/x/run".into()),
            ..Default::default()
        };
        assert_eq!(pids(s.find(&spec, None)), vec![7]);
        let both = [
            ProcRef { pid: 7, start: 100 },
            ProcRef { pid: 8, start: 100 },
        ];
        assert_eq!(pids(s.alive(&both)), vec![7]);
    }
}
