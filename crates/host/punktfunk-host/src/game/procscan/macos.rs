//! The macOS reader: `libproc`.
//!
//! Contract and rules live in [`super`]. A start is `proc_bsdinfo`'s start time in microseconds
//! since the Unix epoch, compared only for equality. Only this uid's processes are walked. No
//! spec match and no launch stamp: nothing on macOS watches a launch.

use super::ProcRef;

pub struct Scanner {
    uid: u32,
}

impl Scanner {
    pub fn system() -> Self {
        // SAFETY: a parameterless POSIX call that always succeeds and touches no memory.
        let uid = unsafe { libc::getuid() };
        Scanner { uid }
    }

    /// Pin a pid the host just spawned to *this* process by reading its start time.
    pub fn resolve(&self, pid: u32) -> Option<ProcRef> {
        let info = bsd_info(pid)?;
        Some(ProcRef {
            pid,
            start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
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
}
