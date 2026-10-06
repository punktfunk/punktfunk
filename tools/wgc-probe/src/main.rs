//! `wgc-probe` — what Windows Graphics Capture does on this box.
//!
//! The spikes of punktfunk-planning `design/windows-wgc-capture.md` §7 as one command, run the
//! way the capture worker will run: a user process a SYSTEM parent started (`as-user`), on
//! `winsta0\default`, with the console user's unelevated token.
//!
//! * `list` — every DXGI output with the host inventory's view of it, and which optional WGC
//!   properties this build has.
//! * `capture` — open one monitor; report stage timings, the first frame, the delivered rate
//!   and each frame's age. `--cursor-ab` asks whether WGC draws the pointer, `--shot` writes
//!   one frame as a BMP, `--touch` copies every frame so the GPU sees a pass's worth of work.
//! * `animate` — only the repainting square, as steady content for another measurement.
//! * `opens` — open and close N times. A stage that takes over 5 s ends the process with 3.
//! * `as-user` — SYSTEM only: run the rest of the command line as the console user.
//! * `as-system` — SYSTEM only: run it as SYSTEM in the console session, as the host runs.
//!
//! Every line is `epoch_ms event key=value …`, so it lines up with `host.log`. Run it with no
//! arguments for the flags.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("wgc-probe is Windows-only (it opens Windows.Graphics.Capture).");
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
fn main() {
    win::main()
}

#[cfg(target_os = "windows")]
mod capture;
#[cfg(target_os = "windows")]
mod spawn;

#[cfg(target_os = "windows")]
mod win {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// One output line: `epoch_ms event detail`.
    pub fn log(event: &str, detail: impl std::fmt::Display) {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        println!("{ms} {event} {detail}");
    }

    /// A failed call as text, HRESULT first: the code is the finding.
    pub fn hr<T>(r: windows::core::Result<T>, what: &str) -> Result<T, String> {
        r.map_err(|e| format!("{what}: {:#010x} {}", e.code().0 as u32, e.message()))
    }

    pub struct Opts {
        pub monitor: String,
        pub secs: u64,
        pub fp16: bool,
        /// Frame rate asked through `MinUpdateInterval` where the build has it. `0` leaves it.
        pub fps: u32,
        pub buffers: i32,
        pub cursor: bool,
        /// Ask for borderless access before clearing the border.
        pub access: bool,
        pub keep_border: bool,
        /// Draw the compose canary when no frame came after this long. `0` never draws it.
        pub canary_ms: u64,
        pub touch: bool,
        pub shot: Option<PathBuf>,
        /// Take the shot this many seconds into the run, not at its end. `0` is the end.
        pub shot_at: u64,
        pub cursor_ab: bool,
        /// Move the pointer one pixel and back before the cursor leg, so it is showing.
        pub nudge: bool,
        /// Dirty one pixel of the monitor every 2 ms, so DWM composes at the refresh rate.
        pub animate: bool,
        pub count: u32,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self {
                monitor: "physical".into(),
                secs: 10,
                fp16: false,
                fps: 60,
                buffers: 2,
                cursor: true,
                access: true,
                keep_border: false,
                canary_ms: 1000,
                touch: false,
                shot: None,
                shot_at: 0,
                cursor_ab: false,
                nudge: false,
                animate: false,
                count: 200,
            }
        }
    }

    fn usage() -> ! {
        eprintln!(
            "usage: wgc-probe list\n       \
             wgc-probe capture --monitor <\\\\.\\DISPLAYn|ours|physical> [--secs N] [--format bgra|fp16]\n                 \
             [--fps N] [--buffers N] [--cursor on|off] [--no-access] [--keep-border]\n                 \
             [--canary-ms N] [--touch] [--animate] [--shot out.bmp [--shot-at N]] [--cursor-ab [--nudge]]\n       \
             wgc-probe opens --monitor <…> [--count N]\n       \
             wgc-probe animate --monitor <…> [--secs N]\n       \
             wgc-probe as-user|as-system --out <log> -- <list|capture|opens …>"
        );
        std::process::exit(2);
    }

    fn parse(args: &[String]) -> Opts {
        let mut o = Opts::default();
        let mut it = args.iter();
        let value =
            |it: &mut std::slice::Iter<String>| it.next().cloned().unwrap_or_else(|| usage());
        let number = |s: String| s.parse::<u64>().unwrap_or_else(|_| usage());
        while let Some(a) = it.next() {
            match a.as_str() {
                "--monitor" => o.monitor = value(&mut it),
                "--secs" => o.secs = number(value(&mut it)),
                "--format" => o.fp16 = value(&mut it) == "fp16",
                "--fps" => o.fps = number(value(&mut it)) as u32,
                "--buffers" => o.buffers = number(value(&mut it)) as i32,
                "--cursor" => o.cursor = value(&mut it) != "off",
                "--no-access" => o.access = false,
                "--keep-border" => o.keep_border = true,
                "--canary-ms" => o.canary_ms = number(value(&mut it)),
                "--touch" => o.touch = true,
                "--shot" => o.shot = Some(PathBuf::from(value(&mut it))),
                "--shot-at" => o.shot_at = number(value(&mut it)),
                "--cursor-ab" => o.cursor_ab = true,
                "--nudge" => o.nudge = true,
                "--animate" => o.animate = true,
                "--count" => o.count = number(value(&mut it)) as u32,
                _ => usage(),
            }
        }
        o
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some(cmd) = args.first() else { usage() };
        let rest = &args[1..];
        let result = match cmd.as_str() {
            "as-user" => crate::spawn::relaunch(rest, false),
            "as-system" => crate::spawn::relaunch(rest, true),
            "list" => {
                crate::spawn::identity();
                crate::capture::list()
            }
            "capture" => {
                crate::spawn::identity();
                crate::capture::capture(&parse(rest))
            }
            "animate" => crate::capture::animate_only(&parse(rest)),
            "opens" => {
                crate::spawn::identity();
                crate::capture::opens(&parse(rest))
            }
            _ => usage(),
        };
        match result {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                log("error", e);
                std::process::exit(1);
            }
        }
    }
}
