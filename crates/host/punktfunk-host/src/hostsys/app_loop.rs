//! The host as a Mac app: a bare launch of the bundle serves, and a serving host runs its work on
//! another thread and gives the main thread to the menu bar UI library, else to a bare
//! `NSApplication` with no Dock icon and no menu bar.
//!
//! CoreGraphics keeps a process's view of the displays current only through this loop. Once
//! the process has configured the displays, a virtual display made after that never lists, and
//! a display's modes read only if it was online at the process's first display read (macOS
//! 27.2). A plain run loop or `dispatch_main` does not do it.
//!
//! The UI library's C ABI is pinned by `testdata/host_ui_stub.c` and this file's tests.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
use std::ffi::{c_char, CStr, CString, OsStr};
use std::path::{Path, PathBuf};

/// The login agent's launchd label, which launchd hands its instance as `XPC_SERVICE_NAME`.
const AGENT_LABEL: &str = "io.unom.punktfunk.host";

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

/// Whether launchd started this process as the login agent.
pub(crate) fn login_agent() -> bool {
    is_agent(std::env::var_os("XPC_SERVICE_NAME").as_deref())
}

fn is_agent(service: Option<&OsStr>) -> bool {
    service == Some(OsStr::new(AGENT_LABEL))
}

/// What a Mac launch does before `work`: a `serve` takes the instance lock and opens its log
/// file, and the UI library may hand it to the login agent first. Returns only when the loop
/// is not wanted; the caller then runs `work` on this thread.
pub(crate) fn start(args: &[String], work: fn()) {
    let first = args.first().map(String::as_str);
    let serves = first == Some("serve");
    let looped = matches!(first, Some("serve" | "punktfunk1-host"))
        && objc2_core_graphics::CGSessionCopyCurrentDictionary().is_some();
    let mut ui = None;
    if serves {
        // A second launch's SIGUSR1 asks for the window and must not end the host; the UI
        // watches it with a dispatch source. A handler, not SIG_IGN: exec resets a handler,
        // so games, hooks and the console start with the signal's default.
        extern "C" fn keep_running(_: libc::c_int) {}
        // SAFETY: the handler does nothing, which is async-signal-safe, and the host thread
        // has not started yet.
        unsafe {
            libc::signal(
                libc::SIGUSR1,
                keep_running as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
        let agent = login_agent();
        ui = looped.then(|| Ui::find(agent)).flatten();
        if let Some(ui) = ui.filter(|_| !agent) {
            // SAFETY: on the main thread, before anything serves, with a context that lives
            // for the process (`Ui::find`); the library returns 0 or 1.
            if unsafe { (ui.prelaunch)(ui.ctx) } == 1 {
                tracing::info!("launch handed to the login agent");
                super::instance::wake_holder();
                std::process::exit(0);
            }
        }
        super::instance::take(agent);
        crate::telemetry::log_file::open();
    }
    if looped {
        run_beside(work, ui);
    }
}

/// Run `work` on its own thread while the main thread, the caller's, runs the UI library or
/// the AppKit loop. The process exits when `work` returns, with 0, or with 101 if it panics.
fn run_beside(work: fn(), ui: Option<Ui>) -> ! {
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
    if let Some(ui) = ui {
        // SAFETY: the main thread, with the host thread started, as the ABI asks; the context
        // lives for the process.
        unsafe { (ui.run)(ui.ctx) };
    } else {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        app.run();
    }
    std::process::exit(0)
}

/// The UI library's ABI version this host speaks.
const UI_ABI: u32 = 1;

/// `pf_host_ui_context`, version 1.
#[repr(C)]
struct UiContext {
    abi_version: u32,
    config_dir: *const c_char,
    version: *const c_char,
    launched_by_login_agent: u8,
}

type PrelaunchFn = unsafe extern "C" fn(*const UiContext) -> i32;
type RunFn = unsafe extern "C" fn(*const UiContext);

/// A loaded UI library. The library is never closed, and the context is leaked.
#[derive(Clone, Copy)]
struct Ui {
    prelaunch: PrelaunchFn,
    run: RunFn,
    ctx: &'static UiContext,
}

impl Ui {
    /// The library `PUNKTFUNK_HOST_UI` names (`off`: none), else the bundle's. A missing default
    /// library is no UI; one that does not load is logged and ignored.
    fn find(agent: bool) -> Option<Ui> {
        let path = match std::env::var_os("PUNKTFUNK_HOST_UI") {
            Some(v) if v == "off" => return None,
            Some(v) => PathBuf::from(v),
            None => {
                let exe = std::env::current_exe().ok()?;
                let path = exe.parent()?.join("../Frameworks/libPunktfunkHostUI.dylib");
                if !path.exists() {
                    return None;
                }
                path
            }
        };
        let ctx = context(&pf_paths::config_dir(), crate::version::get(), agent);
        match Ui::load(&path, ctx) {
            Ok(ui) => Some(ui),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "menu bar UI not loaded");
                None
            }
        }
    }

    fn load(path: &Path, ctx: &'static UiContext) -> Result<Ui, String> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call. Loading runs the library's
        // initializers: the trust the bundle's own Frameworks folder gets.
        let lib = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if lib.is_null() {
            // SAFETY: dlerror returns null or a NUL-terminated string valid until the next dl call.
            let why = unsafe { libc::dlerror() };
            return Err(if why.is_null() {
                "dlopen".into()
            } else {
                // SAFETY: non-null, from dlerror just above, read before any other dl call.
                unsafe { CStr::from_ptr(why) }
                    .to_string_lossy()
                    .into_owned()
            });
        }
        let sym = |name: &CStr| {
            // SAFETY: `lib` is a live handle (never closed) and `name` is NUL-terminated.
            let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
            if p.is_null() {
                Err(format!("no symbol {}", name.to_string_lossy()))
            } else {
                Ok(p)
            }
        };
        let abi = sym(c"pf_host_ui_abi_version")?;
        let prelaunch = sym(c"pf_host_ui_prelaunch")?;
        let run = sym(c"pf_host_ui_run")?;
        // SAFETY: the C ABI types this symbol `uint32_t (void)`.
        let abi: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(abi) };
        // SAFETY: a call with no arguments into the library just loaded.
        let version = unsafe { abi() };
        if version != UI_ABI {
            return Err(format!("ABI version {version}, not {UI_ABI}"));
        }
        Ok(Ui {
            // SAFETY: an ABI version 1 library types these as `PrelaunchFn` and `RunFn`.
            prelaunch: unsafe { std::mem::transmute::<*mut libc::c_void, PrelaunchFn>(prelaunch) },
            // SAFETY: as above.
            run: unsafe { std::mem::transmute::<*mut libc::c_void, RunFn>(run) },
            ctx,
        })
    }
}

/// The context every UI call gets, leaked: its strings must outlive the never-returning run.
fn context(config_dir: &Path, version: &str, agent: bool) -> &'static UiContext {
    let leak = |s: &str| CString::new(s).unwrap_or_default().into_raw().cast_const();
    Box::leak(Box::new(UiContext {
        abi_version: UI_ABI,
        config_dir: leak(&config_dir.to_string_lossy()),
        version: leak(version),
        launched_by_login_agent: u8::from(agent),
    }))
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
        assert!(is_agent(Some(OsStr::new(AGENT_LABEL))));
        for other in [
            None,
            Some("application.io.unom.punktfunk.host.1234"),
            Some(""),
        ] {
            assert!(!is_agent(other.map(OsStr::new)), "{other:?}");
        }
    }

    /// `testdata/host_ui_stub.c` built with `defines`, or `None` without a C compiler.
    fn stub(dir: &Path, name: &str, defines: &[&str]) -> Option<PathBuf> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/host_ui_stub.c");
        let out = dir.join(format!("{name}.dylib"));
        let status = std::process::Command::new("cc")
            .args(["-dynamiclib", "-Wall", "-Werror"])
            .args(defines)
            .arg(&src)
            .arg("-o")
            .arg(&out)
            .status();
        match status {
            Ok(s) => {
                assert!(s.success(), "the stub UI library did not build");
                Some(out)
            }
            Err(e) => {
                eprintln!("skipping: no C compiler ({e})");
                None
            }
        }
    }

    /// A library is taken only whole and at ABI 1, and its prelaunch sees the context.
    #[test]
    fn the_ui_library_loads_only_at_its_abi() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), "9.9.9", true);
        let Some(good) = stub(dir.path(), "good", &["-DSTUB_PRELAUNCH=1"]) else {
            return;
        };
        let ui = Ui::load(&good, ctx).expect("a whole ABI 1 library loads");
        // SAFETY: the stub's prelaunch reads the leaked context and appends one line to a file.
        assert_eq!(unsafe { (ui.prelaunch)(ui.ctx) }, 1);
        let log = std::fs::read_to_string(dir.path().join("ui-stub.log")).unwrap();
        assert!(
            log.starts_with("prelaunch abi=1 version=9.9.9 agent=1"),
            "{log}"
        );

        let v2 = stub(dir.path(), "v2", &["-DSTUB_ABI=2"]).unwrap();
        assert!(Ui::load(&v2, ctx).err().unwrap().contains("ABI version 2"));
        let partial = stub(dir.path(), "partial", &["-DSTUB_NO_RUN"]).unwrap();
        assert!(Ui::load(&partial, ctx)
            .err()
            .unwrap()
            .contains("pf_host_ui_run"));
        assert!(Ui::load(&dir.path().join("absent.dylib"), ctx).is_err());
    }
}
