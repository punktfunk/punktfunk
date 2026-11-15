//! Realtek's low-delay decode mode. A Realtek decoder returns each picture when the next AU is
//! fed; its own ACodec reads the MediaFormat key [`KEY`] and turns it into the decoder's
//! `videoLowDelayMode`, which returns a picture on its own AU. Another ACodec never looks the
//! key up, so the framework library is read for the key's name first: no name, no key.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::codec_format::{contains, is_realtek};

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
