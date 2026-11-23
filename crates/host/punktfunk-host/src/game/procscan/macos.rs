//! The macOS matcher: `libproc` and `KERN_PROCARGS2`.
//!
//! Contract and rules live in [`super`]. A start is `proc_bsdinfo`'s start time in microseconds
//! since the Unix epoch, compared only for equality; [`Scanner::now_stamp`] is on that clock.
//! Only this uid's processes are walked. A game is matched by its image path (a game inside an
//! app bundle sits under the bundle, its install dir), by name, or by an argv entry under its
//! dir, as on Linux. macOS shows no other process's environment and Steam starts no reaper here,
//! so an env marker or a bare appid matches nothing. Pin: `tests`.

use super::{ProcRef, START_SLACK_SECS};
use crate::library::DetectSpec;
use std::path::{Path, PathBuf};

/// A start's unit: microseconds.
const MICROS_PER_SEC: f64 = 1_000_000.0;

pub struct Scanner {
    uid: u32,
}

impl Scanner {
    pub fn system() -> Self {
        // SAFETY: a parameterless POSIX call that always succeeds and touches no memory.
        let uid = unsafe { libc::getuid() };
        Scanner { uid }
    }

    /// Seconds since the Unix epoch, the clock a process start is read on. A clock step between
    /// this and a later start read must exceed [`START_SLACK_SECS`] to matter.
    pub fn now_stamp(&self) -> Option<f64> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        now.ok().map(|d| d.as_secs_f64())
    }

    /// This uid's processes matching `spec`, started at or after `min_start` ([`Self::now_stamp`]
    /// seconds; `None` disables that filter). Signals are a union, as on Linux.
    pub fn find(&self, spec: &DetectSpec, min_start: Option<f64>) -> Vec<ProcRef> {
        let want = Want::new(spec);
        if !want.any() {
            return Vec::new();
        }
        let mut args = Args::new();
        let mut out = Vec::new();
        for pid in all_pids() {
            let Some(info) = bsd_info(pid).filter(|i| i.pbi_uid == self.uid) else {
                continue;
            };
            let p = ProcRef {
                pid,
                start: start_of(&info),
            };
            if let Some(min) = min_start {
                if p.start as f64 / MICROS_PER_SEC + START_SLACK_SECS < min {
                    continue; // predates this launch (rule 1)
                }
            }
            if want.matches(image(pid).as_deref(), || args.of(pid)) {
                out.push(p);
            }
        }
        out
    }

    /// Pin a pid the host just spawned to *this* process by reading its start time.
    pub fn resolve(&self, pid: u32) -> Option<ProcRef> {
        let info = bsd_info(pid)?;
        Some(ProcRef {
            pid,
            start: start_of(&info),
        })
    }

    /// Diagnostics only ([`super::names`]). Gone processes yield `?`.
    pub fn name_of(&self, p: ProcRef) -> String {
        let mut buf = [0u8; 2 * libc::MAXCOMLEN + 1];
        // SAFETY: `buf` is a live local of the length passed; the call writes at most that many
        // bytes and returns how many it wrote.
        let n = unsafe { libc::proc_name(p.pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
        match usize::try_from(n) {
            Ok(n) if n > 0 => String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned(),
            _ => "?".into(),
        }
    }

    /// Pid present and start time unchanged, so a recycled pid is never alive (rule 2).
    pub fn alive(&self, procs: &[ProcRef]) -> Vec<ProcRef> {
        procs
            .iter()
            .copied()
            .filter(|p| self.resolve(p.pid).map(|r| r.start) == Some(p.start))
            .collect()
    }

    /// `(pid, parent)` for every own process.
    pub fn parents(&self) -> Vec<(u32, u32)> {
        all_pids()
            .into_iter()
            .filter_map(|pid| bsd_info(pid).map(|i| (pid, i)))
            .filter(|(_, i)| i.pbi_uid == self.uid)
            .map(|(pid, i)| (pid, i.pbi_ppid))
            .collect()
    }
}

/// A spec's path signals, each canonical and as spelled: argv keeps the launcher's spelling.
struct Want<'a> {
    spec: &'a DetectSpec,
    exe: Option<PathBuf>,
    dir: Option<PathBuf>,
}

impl<'a> Want<'a> {
    fn new(spec: &'a DetectSpec) -> Self {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        Want {
            exe: spec.exe.as_deref().map(canonical),
            dir: spec.install_dir.as_deref().map(canonical),
            spec,
        }
    }

    /// Whether anything here can match on this platform.
    fn any(&self) -> bool {
        self.exe.is_some() || self.dir.is_some() || self.spec.process_name.is_some()
    }

    /// The image first, then argv, which `args` reads only when the image decides nothing.
    fn matches(&self, image: Option<&Path>, args: impl FnOnce() -> Vec<Vec<u8>>) -> bool {
        if let Some(image) = image {
            if self.exe.as_deref() == Some(image)
                || self.dir.as_deref().is_some_and(|d| image.starts_with(d))
            {
                return true;
            }
            // Operator-typed fallback, case-insensitive as on Linux.
            let name = image.file_name().and_then(|n| n.to_str());
            if let (Some(want), Some(name)) = (self.spec.process_name.as_deref(), name) {
                if name.eq_ignore_ascii_case(want) {
                    return true;
                }
            }
        }
        if self.exe.is_none() && self.dir.is_none() {
            return false;
        }
        let spelled = |canonical: &Option<PathBuf>, raw: &Option<PathBuf>| -> Vec<Vec<u8>> {
            [canonical.as_deref(), raw.as_deref()]
                .into_iter()
                .flatten()
                .map(|p| p.as_os_str().as_encoded_bytes().to_vec())
                .collect()
        };
        let dirs = spelled(&self.dir, &self.spec.install_dir);
        let exes = spelled(&self.exe, &self.spec.exe);
        args().iter().any(|arg| {
            exes.iter().any(|e| arg == e)
                // A separator after the dir: `/games/x` is not satisfied by `/games/xyz/…`.
                || dirs
                    .iter()
                    .any(|d| arg.strip_prefix(d.as_slice()).is_some_and(|r| r.first() == Some(&b'/')))
        })
    }
}

/// One reusable `KERN_PROCARGS2` buffer for a scan, `kern.argmax` long.
struct Args {
    buf: Vec<u8>,
}

impl Args {
    fn new() -> Self {
        Args { buf: Vec::new() }
    }

    /// `pid`'s argv, empty when it is gone or not ours to read.
    fn of(&mut self, pid: u32) -> Vec<Vec<u8>> {
        if self.buf.is_empty() {
            self.buf = vec![0; arg_max()];
        }
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
        let mut len = self.buf.len();
        // SAFETY: `mib` names three live ints; `buf` is a live buffer of `len` bytes, and the call
        // writes at most that many and stores the count written back into `len`.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                self.buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Vec::new();
        }
        parse_procargs(&self.buf[..len.min(self.buf.len())])
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect()
    }
}

/// `kern.argmax`, the most `KERN_PROCARGS2` returns: 1 MiB when it can't be read.
fn arg_max() -> usize {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut max: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: `mib` names two live ints; `max` is a live int of `len` bytes the call fills.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut max).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    usize::try_from(max)
        .ok()
        .filter(|&m| rc == 0 && m > 0)
        .unwrap_or(1 << 20)
}

/// `KERN_PROCARGS2`'s argv: an `argc`, the exec path, NUL padding, then `argc` strings. What
/// follows them is the environment, which is never read.
fn parse_procargs(buf: &[u8]) -> Vec<&[u8]> {
    let Some((argc, rest)) = buf.split_first_chunk::<4>() else {
        return Vec::new();
    };
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).unwrap_or(0);
    let Some(path_end) = rest.iter().position(|&b| b == 0) else {
        return Vec::new();
    };
    let rest = &rest[path_end..];
    let first = rest.iter().position(|&b| b != 0).unwrap_or(rest.len());
    rest[first..].split(|&b| b == 0).take(argc).collect()
}

/// `pid`'s executable as the kernel resolved it, `None` when it is gone or not ours.
fn image(pid: u32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is a live buffer of the length passed; the call writes at most that many
    // bytes and returns how many it wrote.
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
    buf.truncate(n.min(buf.len()));
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// A start in microseconds since the Unix epoch.
fn start_of(info: &libc::proc_bsdinfo) -> u64 {
    info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
}

/// Every pid on the box, read twice as large as the count the first call reports so a burst of
/// new processes between the calls still fits.
fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer of length 0 asks only for the count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(n) = usize::try_from(n) else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; n * 2 + 16];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as i32;
    // SAFETY: `pids` is a live buffer of `bytes` bytes; the call fills at most that and returns
    // the number of pids written.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let got = usize::try_from(got).unwrap_or(0).min(pids.len());
    pids[..got]
        .iter()
        .filter_map(|&p| u32::try_from(p).ok().filter(|&p| p > 0))
        .collect()
}

/// `PROC_PIDTBSDINFO` of `pid`, `None` when it is gone, a zombie, or not ours to read.
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `proc_bsdinfo` is plain integers and byte arrays, for which all zeroes is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live local of `size` bytes; the call writes at most that and returns
    // the count it wrote.
    let n = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (n == size && info.pbi_status != libc::SZOMB).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test process: pinned, alive, named, and the child of its parent.
    #[test]
    fn reads_its_own_process() {
        let scanner = Scanner::system();
        let me = scanner.resolve(std::process::id()).expect("resolve self");
        assert_eq!(scanner.alive(&[me]), [me]);
        let recycled = ProcRef {
            start: me.start + 1,
            ..me
        };
        assert!(scanner.alive(&[recycled]).is_empty(), "a recycled pid");
        assert_ne!(scanner.name_of(me), "?");
        let parent = std::os::unix::process::parent_id();
        assert!(scanner.parents().contains(&(me.pid, parent)));
    }

    #[test]
    fn a_gone_pid_resolves_to_nothing() {
        assert_eq!(Scanner::system().resolve(i32::MAX as u32), None);
    }

    fn spec(exe: Option<&str>, dir: Option<&str>, name: Option<&str>) -> DetectSpec {
        DetectSpec {
            exe: exe.map(PathBuf::from),
            install_dir: dir.map(PathBuf::from),
            process_name: name.map(str::to_string),
            ..Default::default()
        }
    }

    fn hit(s: &DetectSpec, image: &str, args: &[&str]) -> bool {
        let args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
        Want::new(s).matches(Some(Path::new(image)), || args)
    }

    const CELESTE: &str = "/Games/Celeste.app/Contents/MacOS/Celeste";

    #[test]
    fn a_game_in_an_app_bundle_matches_by_exe_bundle_and_name() {
        let by_exe = spec(Some(CELESTE), None, None);
        assert!(hit(&by_exe, CELESTE, &[]));
        assert!(!hit(
            &by_exe,
            "/Games/Celeste.app/Contents/MacOS/Helper",
            &[]
        ));
        let by_bundle = spec(None, Some("/Games/Celeste.app"), None);
        assert!(hit(&by_bundle, CELESTE, &[]));
        assert!(hit(
            &by_bundle,
            "/Games/Celeste.app/Contents/Helpers/crash",
            &[]
        ));
        assert!(!hit(
            &by_bundle,
            "/Applications/Safari.app/Contents/MacOS/Safari",
            &[]
        ));
        let by_name = spec(None, None, Some("celeste"));
        assert!(hit(&by_name, CELESTE, &[]));
        assert!(!hit(
            &by_name,
            "/Games/Celeste.app/Contents/MacOS/celeste-helper",
            &[]
        ));
    }

    #[test]
    fn install_dir_does_not_match_a_sibling_with_the_same_prefix() {
        let s = spec(None, Some("/Games/x"), None);
        assert!(!hit(&s, "/Games/xyz/run", &[]));
        assert!(!hit(&s, "/bin/sh", &["sh", "/Games/xyz/run.sh"]));
        assert!(hit(&s, "/bin/sh", &["sh", "/Games/x/run.sh"]));
    }

    /// A shell or interpreter runs the game: its argv names it, its image does not.
    #[test]
    fn a_wrapped_game_matches_by_argv_only() {
        let s = spec(Some("/Games/x/game.py"), Some("/Games/x"), None);
        assert!(hit(
            &s,
            "/usr/bin/python3",
            &["python3", "/Games/x/game.py"]
        ));
        let exe_only = spec(Some("/Games/x/game.py"), None, None);
        assert!(hit(
            &exe_only,
            "/usr/bin/python3",
            &["python3", "/Games/x/game.py"]
        ));
        assert!(!hit(
            &exe_only,
            "/usr/bin/python3",
            &["python3", "/Games/x/game.py.bak"]
        ));
        // A name alone never reads argv.
        let name = spec(None, None, Some("game.py"));
        assert!(!hit(
            &name,
            "/usr/bin/python3",
            &["python3", "/Games/x/game.py"]
        ));
    }

    #[test]
    fn signals_macos_cannot_read_match_nothing() {
        let steam_only = DetectSpec {
            steam_appid: Some(570),
            ..Default::default()
        };
        assert!(!Want::new(&steam_only).any());
        assert!(Scanner::system().find(&steam_only, None).is_empty());
        let env_only = DetectSpec::default().with_env("HEROIC_APP_NAME", None);
        assert!(Scanner::system().find(&env_only, None).is_empty());
    }

    #[test]
    fn procargs_yields_argv_and_never_the_environment() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sleep\0\0\0\0sleep\0-n\0/Games/x\0SECRET=1\0HOME=/u\0");
        let argv = parse_procargs(&buf);
        assert_eq!(argv, [&b"sleep"[..], b"-n", b"/Games/x"]);
        assert!(parse_procargs(&[1, 0]).is_empty(), "truncated");
        let mut no_args = 0i32.to_ne_bytes().to_vec();
        no_args.extend_from_slice(b"/bin/x\0\0ENV=1\0");
        assert!(parse_procargs(&no_args).is_empty());
    }

    /// A copied `sleep` run from a temp dir: found by its dir, its exe and an argv entry, and
    /// not by a launch stamp taken after it started.
    #[test]
    fn finds_a_real_process_it_just_started() {
        let td = tempfile::tempdir().expect("tempdir");
        let game = td.path().join("sleep");
        std::fs::copy("/bin/sleep", &game).expect("copy a stand-in game binary");
        let s = Scanner::system();
        let before = s.now_stamp().expect("the clock reads");
        let mut child = std::process::Command::new(&game)
            .arg("20")
            .spawn()
            .expect("spawn the fake game");
        let pid = child.id();
        let found =
            |spec: &DetectSpec, at: f64| s.find(spec, Some(at)).iter().any(|p| p.pid == pid);
        let dir = DetectSpec::dir(td.path());
        assert!(found(&dir, before), "by its install dir");
        assert!(found(&DetectSpec::exe(&game), before), "by its exe");
        assert!(
            !found(&dir, s.now_stamp().unwrap() + 60.0),
            "a launch after it started"
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(!found(&dir, before), "gone once reaped");
    }
}
