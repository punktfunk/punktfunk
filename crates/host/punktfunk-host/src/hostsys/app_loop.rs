//! The host as a Mac app: a bare launch of the bundle serves, and a serving host runs its work
//! on another thread and gives the main thread to `NSApplication`, with no Dock icon and no
//! menu bar.
//!
//! CoreGraphics keeps a process's view of the displays current only through this loop. Once
//! the process has configured the displays, a virtual display made after that never lists, and
//! a display's modes read only if it was online at the process's first display read (macOS
//! 27.2). A plain run loop or `dispatch_main` does not do it.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
use std::ffi::OsStr;
use std::path::Path;

/// The command line a launch runs: `argv`, except that a bare launch of the app serves.
pub(crate) fn launch_args(argv: Vec<String>, exe: &Path) -> Vec<String> {
    if argv.is_empty() && bundle_of(exe).is_some() {
        return vec!["serve".into()];
    }
    argv
}

/// The `.app` whose `Contents/MacOS` holds `exe`.
pub(crate) fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe
        .parent()
        .filter(|d| d.file_name() == Some(OsStr::new("MacOS")))?;
    let contents = macos
        .parent()
        .filter(|d| d.file_name() == Some(OsStr::new("Contents")))?;
    contents
        .parent()
        .filter(|d| d.extension() == Some(OsStr::new("app")))
}

/// What a Mac launch does before `work`. Returns only when the loop is not wanted: the command
/// serves no streams, or no desktop session is there to connect to. The caller then runs `work`
/// on this thread.
pub(crate) fn start(args: &[String], work: fn()) {
    let first = args.first().map(String::as_str);
    let looped = matches!(first, Some("serve" | "punktfunk1-host"))
        && objc2_core_graphics::CGSessionCopyCurrentDictionary().is_some();
    if looped {
        run_beside(work);
    }
}

/// Run `work` on its own thread while the main thread, the caller's, runs the AppKit loop. The
/// process exits when `work` returns, with 0, or with 101 if it panics.
fn run_beside(work: fn()) -> ! {
    let mtm = MainThreadMarker::new().expect("run the AppKit loop from the main thread");
    std::thread::Builder::new()
        .name("host".into())
        // The main thread's size: `work` is what `main` ran before.
        .stack_size(8 << 20)
        .spawn(move || {
            let code = match std::panic::catch_unwind(work) {
                Ok(()) => 0,
                Err(_) => 101,
            };
            std::process::exit(code)
        })
        .expect("spawn the host thread");
    // An app without a window is App Nap's to throttle; a streaming host must not be.
    let reason = NSString::from_str("Serving game streams");
    let _activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &reason,
    );
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.run();
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::args;

    /// Only a bare launch from inside an app bundle serves; every other command line is as given.
    #[test]
    fn a_bare_launch_of_the_app_serves() {
        let app = Path::new("/Applications/Punktfunk Host.app/Contents/MacOS/punktfunk-host");
        let loose = Path::new("/usr/local/bin/punktfunk-host");
        let not_app = Path::new("/opt/Host/Contents/MacOS/punktfunk-host");
        let rel = Path::new("./X.app/Contents/MacOS/punktfunk-host");
        for (argv, exe, want) in [
            (&[][..], app, &["serve"][..]),
            (&[], rel, &["serve"]),
            (&[], loose, &[]),
            (&[], not_app, &[]),
            (&["ctl", "status"], app, &["ctl", "status"]),
            (&["serve", "--no-mdns"], app, &["serve", "--no-mdns"]),
            (&["--help"], app, &["--help"]),
        ] {
            assert_eq!(launch_args(args(argv), exe), args(want), "{argv:?} {exe:?}");
        }
        assert_eq!(
            bundle_of(app),
            Some(Path::new("/Applications/Punktfunk Host.app"))
        );
    }
}
