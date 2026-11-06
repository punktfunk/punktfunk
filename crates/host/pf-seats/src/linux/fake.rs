//! A stand-in for the programs the backend runs (`systemctl`, `loginctl`, `getent`, …): a test
//! installs one, and each call is recorded and answered without running anything, so a test can
//! say exactly what the backend asked of the machine.

use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{ExitStatus, Output};

type Answer = Box<dyn Fn(&str, &[&str]) -> (i32, String)>;

struct Fake {
    calls: Vec<String>,
    answer: Answer,
}

thread_local! {
    static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
}

/// Keeps a fake installed on this thread; dropping it takes the fake away.
pub(super) struct Installed;

impl Drop for Installed {
    fn drop(&mut self) {
        FAKE.with(|fake| *fake.borrow_mut() = None);
    }
}

/// Answers every program with `answer`'s `(exit code, stdout)` on this thread until the guard
/// drops.
pub(super) fn install(answer: impl Fn(&str, &[&str]) -> (i32, String) + 'static) -> Installed {
    let fake = Fake {
        calls: Vec::new(),
        answer: Box::new(answer),
    };
    FAKE.with(|slot| *slot.borrow_mut() = Some(fake));
    Installed
}

/// Every call since [`install`], as `program arg arg`.
pub(super) fn calls() -> Vec<String> {
    FAKE.with(|fake| {
        fake.borrow()
            .as_ref()
            .map(|f| f.calls.clone())
            .unwrap_or_default()
    })
}

/// The recorded answer for `program`, or `None` where no fake is installed.
pub(super) fn answer(program: &str, args: &[&str]) -> Option<Output> {
    FAKE.with(|fake| {
        let mut fake = fake.borrow_mut();
        let fake = fake.as_mut()?;
        fake.calls.push(
            format!("{program} {}", args.join(" "))
                .trim_end()
                .to_owned(),
        );
        let (code, stdout) = (fake.answer)(program, args);
        Some(Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        })
    })
}
