//! Realtek's low-delay decode mode. A Realtek decoder returns each picture when the next AU is
//! fed; its own ACodec reads the MediaFormat key [`KEY`] and turns it into the decoder's
//! `videoLowDelayMode`, which returns a picture on its own AU. Another ACodec never looks the
//! key up, so the framework library is read for the key's name first: no name, no key.
//! [`census`] logs what else the box names, for a switch this module does not know yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::codec_format::{contains, is_realtek, marked_strings};
use crate::sys::sysprop;

/// The key, as Realtek's `ACodec::configureCodec` looks it up.
pub(super) const KEY: &str = "video-lowdelay-mode";

#[cfg(target_pointer_width = "64")]
const LIB: &str = "lib64";
#[cfg(not(target_pointer_width = "64"))]
const LIB: &str = "lib";

/// A configure with the key failed in this process: it is not set again.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// Whether the file at `path` holds `needle`; `None` when it cannot be read.
fn holds(path: &str, needle: &str) -> Option<bool> {
    std::fs::read(path)
        .ok()
        .map(|bytes| contains(&bytes, needle.as_bytes()))
}

/// `(framework reads the key, decoder has the mode)`, read once: ACodec lives in
/// `libstagefright.so`, the mode's name in the vendor's OMX decoder. `None` is unreadable.
pub(super) fn probe() -> (Option<bool>, Option<bool>) {
    static FOUND: OnceLock<(Option<bool>, Option<bool>)> = OnceLock::new();
    *FOUND.get_or_init(|| {
        (
            holds(&format!("/system/{LIB}/libstagefright.so"), KEY),
            holds(
                &format!("/vendor/{LIB}/libOMX.realtek.video.dec.so"),
                "videoLowDelayMode",
            ),
        )
    })
}

/// Whether `codec_name` is configured with [`KEY`]: a Realtek decoder under a framework that
/// reads the key, until a configure with it fails.
pub(super) fn low_delay(codec_name: &str) -> bool {
    is_realtek(codec_name) && probe().0 == Some(true) && !REFUSED.load(Ordering::Relaxed)
}

/// A configure or start with the key failed.
pub(super) fn refuse() {
    REFUSED.store(true, Ordering::Relaxed);
}

/// What marks a string in a framework library as Realtek's own.
const FRAMEWORK_MARKS: [&str; 9] = [
    "rtk", "RTK", "Rtk", "ealtek", "EALTEK", "owdelay", "owDelay", "ow-delay", "ow_delay",
];
/// The decoder's own libraries also name its indices, switches and properties.
const DECODER_MARKS: [&str; 5] = ["eorder", "OMX_DEC_", "vendor.omx", "atency", "persist."];

/// One census line for a library: its marked strings, or why it cannot be read.
fn library_line(path: &str, marks: &[&str]) -> String {
    match std::fs::read(path) {
        Ok(bytes) => format!("{path}: {:?}", marked_strings(&bytes, marks, 32)),
        Err(e) => format!("{path}: {e}"),
    }
}

/// What this box tells an app about its Realtek decoder, one line each: the marked strings of
/// the framework and decoder libraries, the decoder's env file and its properties. A switch the
/// decoder has shows up here by name. Read once.
pub(super) fn census() -> &'static [String] {
    static LINES: OnceLock<Vec<String>> = OnceLock::new();
    LINES.get_or_init(|| {
        let mut lines: Vec<String> = [
            "libstagefright.so",
            "libstagefright_omx.so",
            "libmedia_omx.so",
        ]
        .iter()
        .map(|lib| library_line(&format!("/system/{LIB}/{lib}"), &FRAMEWORK_MARKS))
        .collect();
        let vendor = format!("/vendor/{LIB}");
        let marks = [&FRAMEWORK_MARKS[..], &DECODER_MARKS[..]].concat();
        match std::fs::read_dir(&vendor) {
            Ok(dir) => {
                let mut names: Vec<String> = dir
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| {
                        ["OMX", "RTK", "rtk", "wrap", "vpu"]
                            .iter()
                            .any(|m| n.contains(m))
                    })
                    .collect();
                names.sort();
                lines.push(format!("{vendor}: {names:?}"));
                let video = |n: &&String| n.contains("dec") && !n.contains("audio");
                for name in names.iter().filter(video).take(6) {
                    lines.push(library_line(&format!("{vendor}/{name}"), &marks));
                }
            }
            Err(e) => lines.push(format!("{vendor}: {e}")),
        }
        lines.push(match std::fs::read_to_string("/vendor/etc/env.txt") {
            Ok(env) => format!(
                "/vendor/etc/env.txt: {:?}",
                env.lines().take(40).collect::<Vec<_>>()
            ),
            Err(e) => format!("/vendor/etc/env.txt: {e}"),
        });
        let props = [
            c"persist.rtk.LowDelay",
            c"persist.vendor.rtk.LowDelay",
            c"rtk.omx.freerun",
            c"vendor.omx.freerun",
            c"vendor.omx.hevc_performance_mode",
            c"persist.vendor.rtk.video.maxAcqCnt",
        ]
        .map(|p| format!("{}={}", p.to_string_lossy(), sysprop(p).unwrap_or_default()));
        lines.push(format!("properties: {}", props.join(" ")));
        lines
    })
}

/// The probe as the log says it, for a Realtek decoder's start line.
pub(super) fn describe() -> String {
    let word = |found: Option<bool>| match found {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unreadable",
    };
    let (framework, decoder) = probe();
    format!(
        "framework reads `{KEY}`: {}, decoder names the mode: {}",
        word(framework),
        word(decoder)
    )
}
