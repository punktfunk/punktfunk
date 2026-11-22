//! One serving Mac host per config folder: `flock` on `<config_dir>/host.lock`, the holder's pid
//! inside. The lock goes with the holder's last descriptor, at any exit or crash, so a file
//! nobody holds is simply taken. std opens files close-on-exec: children never inherit it.

use rustix::fs::{flock, FlockOperation};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long the login agent's instance waits for a running host to quit: the UI quits an idle
/// host right after it registers the agent.
const AGENT_WAIT: Duration = Duration::from_secs(15);

static HELD: OnceLock<File> = OnceLock::new();

enum Lock {
    Taken(File),
    /// Another process holds it; the pid it wrote, if readable.
    Held(Option<i32>),
}

fn lock_path() -> PathBuf {
    pf_paths::config_dir().join("host.lock")
}

/// Take the lock, waiting up to `wait` for its holder to let go.
fn acquire(path: &Path, wait: Duration) -> std::io::Result<Lock> {
    let deadline = Instant::now() + wait;
    loop {
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)?
        };
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Lock::Taken(file)),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {}
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= deadline {
            let mut pid = String::new();
            let _ = file.read_to_string(&mut pid);
            return Ok(Lock::Held(pid.trim().parse().ok()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Become the serving host, or exit 0: the login agent's instance after waiting out a running
/// host, any other launch at once, asking the holder to show its window.
pub(crate) fn take(agent: bool) {
    let path = lock_path();
    let wait = if agent { AGENT_WAIT } else { Duration::ZERO };
    let lock =
        pf_paths::create_private_dir(&pf_paths::config_dir()).and_then(|()| acquire(&path, wait));
    match lock {
        Ok(Lock::Taken(mut file)) => {
            let _ = file.set_len(0);
            let _ = file.write_all(std::process::id().to_string().as_bytes());
            let _ = HELD.set(file);
        }
        Ok(Lock::Held(pid)) if agent => {
            tracing::info!(
                ?pid,
                "host already serving — the login agent leaves it running"
            );
            std::process::exit(0);
        }
        Ok(Lock::Held(pid)) => {
            tracing::info!(?pid, "host already serving — asked it to show its window");
            wake(pid);
            std::process::exit(0);
        }
        Err(error) => {
            tracing::warn!(%error, lock = %path.display(), "instance lock not taken");
        }
    }
}

/// Ask a running host to show its window, if one holds the lock.
pub(crate) fn wake_holder() {
    if let Ok(Lock::Held(pid)) = acquire(&lock_path(), Duration::ZERO) {
        wake(pid);
    }
}

fn wake(pid: Option<i32>) {
    let Some(pid) = pid.filter(|&p| p > 1) else {
        return;
    };
    // SAFETY: kill takes no pointers. `pid > 1` names one process, never a group or all of them.
    unsafe { libc::kill(pid, libc::SIGUSR1) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds the lock at `PF_TEST_LOCK` for `PF_TEST_HOLD_MS`, as another host would. Run by
    /// [`a_second_host_yields_and_the_agent_s_waits`] in a process of its own.
    #[test]
    #[ignore = "the other process of a lock test"]
    fn holds_the_lock() {
        let Some(path) = std::env::var_os("PF_TEST_LOCK") else {
            return;
        };
        let hold = std::env::var("PF_TEST_HOLD_MS").unwrap().parse().unwrap();
        let Lock::Taken(mut file) = acquire(Path::new(&path), Duration::ZERO).unwrap() else {
            panic!("the lock was free");
        };
        file.write_all(std::process::id().to_string().as_bytes())
            .unwrap();
        std::thread::sleep(Duration::from_millis(hold));
    }

    /// A launch finds the holder's pid; a waiting launch takes over once the holder exits.
    #[test]
    fn a_second_host_yields_and_the_agent_s_waits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hostsys::instance::tests::holds_the_lock"])
            .args(["--ignored", "--test-threads=1"])
            .env("PF_TEST_LOCK", &path)
            .env("PF_TEST_HOLD_MS", "1500")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = holder.id() as i32;
        let started = Instant::now();
        while std::fs::read_to_string(&path).unwrap_or_default() != pid.to_string() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the holder never wrote"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(acquire(&path, Duration::ZERO), Ok(Lock::Held(Some(p))) if p == pid));
        let waited = Instant::now();
        assert!(matches!(acquire(&path, AGENT_WAIT), Ok(Lock::Taken(_))));
        assert!(waited.elapsed() < AGENT_WAIT);
        assert!(holder.wait().unwrap().success());
    }
}
