//! The AppKit loop on macOS: a serving host runs its work on another thread and gives the main
//! thread to `NSApplication`, with no Dock icon and no menu bar.
//!
//! CoreGraphics keeps a process's view of the displays current only through this loop. Once
//! the process has configured the displays, a virtual display made after that never lists, and
//! a display's modes read only if it was online at the process's first display read (macOS
//! 27.2). A plain run loop or `dispatch_main` does not do it.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

/// Whether this command runs the loop: it serves streams, and a desktop session is there to
/// connect to.
pub(crate) fn wanted() -> bool {
    let serves = matches!(
        std::env::args().nth(1).as_deref(),
        Some("serve" | "punktfunk1-host")
    );
    serves && objc2_core_graphics::CGSessionCopyCurrentDictionary().is_some()
}

/// Run `work` on its own thread while the main thread, the caller's, runs the AppKit loop. The
/// process exits when `work` returns, with 0, or with 101 if it panics.
pub(crate) fn run_beside(work: fn()) -> ! {
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
