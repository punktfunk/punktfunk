//! The Android system calls the native client shares: system properties, thread nice values and
//! the monotonic clock. Each syscall and its SAFETY proof lives here once.

use std::ffi::CStr;

/// An Android system property, trimmed; `None` when unset or blank. Always `None` off Android.
pub(crate) fn sysprop(name: &CStr) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        const PROP_VALUE_MAX: usize = 92;
        let mut buf = [0u8; PROP_VALUE_MAX];
        // SAFETY: `name` is NUL-terminated and `buf` holds PROP_VALUE_MAX bytes, the most the
        // call writes.
        let n = unsafe { libc::__system_property_get(name.as_ptr(), buf.as_mut_ptr().cast()) };
        prop_value(buf.get(..usize::try_from(n).ok()?)?)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = name;
        None
    }
}

/// `ro.build.version.sdk`; 0 when unreadable.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn sdk_int() -> u32 {
    sysprop(c"ro.build.version.sdk")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// What `__system_property_get` writes in place of a read-only property longer than its
/// buffer, as a build fingerprint can be.
const TOO_LONG: &str = "Must use __system_property_read_callback() to read";

/// A raw property value as [`sysprop`] returns it. The too-long notice is no value.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn prop_value(raw: &[u8]) -> Option<String> {
    let value = String::from_utf8_lossy(raw);
    let value = value.trim();
    (!value.is_empty() && value != TOO_LONG).then(|| value.to_owned())
}

/// The calling thread's kernel tid.
#[cfg(target_os = "android")]
pub(crate) fn gettid() -> i32 {
    // SAFETY: `gettid` takes no arguments and cannot fail.
    unsafe { libc::gettid() }
}

/// Set one thread's nice value; `None` is the calling thread. `PRIO_PROCESS` with a tid targets
/// that one task, the idiom `Process.setThreadPriority` uses. The platform may refuse.
#[cfg(target_os = "android")]
pub(crate) fn set_thread_nice(tid: Option<i32>, nice: i32) -> std::io::Result<()> {
    let who = tid.unwrap_or(0) as libc::id_t;
    // SAFETY: `setpriority` takes no pointers; a refusal comes back as -1, never UB.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, who, nice) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `CLOCK_MONOTONIC` now in nanoseconds (`System.nanoTime` basis): the clock AChoreographer,
/// `releaseOutputBufferAtTime`, the render callback and AAudio timestamps all use.
#[cfg(target_os = "android")]
pub(crate) fn now_monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    // `time_t` and `c_long` are 32-bit on armv7 and 64-bit on arm64, so the cast is required on
    // one shipping ABI and redundant on the other; `:kit:cargoNdkClippy` lints both.
    #[allow(
        clippy::unnecessary_cast,
        reason = "required on 32-bit ABIs; redundant only on 64-bit"
    )]
    {
        ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
    }
}

#[cfg(test)]
mod tests {
    use super::prop_value;

    #[test]
    fn prop_value_trims_and_reads_blank_as_unset() {
        assert_eq!(prop_value(b"arrival ").as_deref(), Some("arrival"));
        assert_eq!(prop_value(b" 1\n").as_deref(), Some("1"));
        assert_eq!(prop_value(b" \t"), None);
        assert_eq!(prop_value(b""), None);
        assert_eq!(
            prop_value(b"Must use __system_property_read_callback() to read"),
            None
        );
    }
}
