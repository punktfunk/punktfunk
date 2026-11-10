//! JNI seam for "Send logs to host" and the log file behind it. Every line the
//! [`crate::RingTee`] logcat tee writes lands in the client log ring and in `client.log` under
//! the app's files dir, so a run that crashed leaves its last lines for the next launch to send.
//! The UPLOAD stays on the Kotlin side — its mTLS OkHttp client (`mtlsHttpClient`, the
//! library/art path) already owns HTTPS-to-the-pinned-host on this platform, and
//! `logring::send_to_host`'s ureq agent is deliberately desktop-only.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JObject, JString};
use jni::sys::jint;
use jni::EnvUnowned;
use pf_client_core::logring::{self, MAX_BYTES};

const CURRENT: &str = "client.log";
const PREVIOUS: &str = "client.log.1";

/// The open log file: `None` until Kotlin names its dir, and again after a write fails.
static FILE: Mutex<Option<LogFile>> = Mutex::new(None);

/// `client.log`, written a line at a time with no buffer to lose when the process dies. At
/// [`MAX_BYTES`] it becomes `client.log.1`, replacing the one before, so the pair always holds
/// one full bundle and never much more.
struct LogFile {
    dir: PathBuf,
    file: File,
    len: u64,
}

impl LogFile {
    /// Append to `dir`'s `client.log`, keeping what earlier runs left there.
    fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let file = Self::open_current(dir)?;
        let len = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_owned(),
            file,
            len,
        })
    }

    fn open_current(dir: &Path) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(CURRENT))
    }

    fn append(&mut self, line: &str) -> io::Result<()> {
        let line = format!("{line}\n");
        self.file.write_all(line.as_bytes())?;
        self.len += line.len() as u64;
        if self.len >= MAX_BYTES as u64 {
            fs::rename(self.dir.join(CURRENT), self.dir.join(PREVIOUS))?;
            self.file = Self::open_current(&self.dir)?;
            self.len = 0;
        }
        Ok(())
    }

    /// Both files, newest first, opened for a bundle. Opened under the sink's lock, so a
    /// rotation cannot land between the two; reading them needs no lock.
    fn open_both(&self) -> io::Result<Vec<File>> {
        let mut files = Vec::new();
        for name in [CURRENT, PREVIOUS] {
            match File::open(self.dir.join(name)) {
                Ok(file) => files.push(file),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(files)
    }

    /// The newest [`MAX_BYTES`] of both files behind `header`, oldest line first.
    #[cfg(test)]
    fn render(&self, header: &str) -> io::Result<String> {
        render_files(self.open_both()?, header)
    }
}

/// The newest [`MAX_BYTES`] of `files` (newest file first) behind `header`, oldest line first.
fn render_files(files: Vec<File>, header: &str) -> io::Result<String> {
    let mut budget = MAX_BYTES as u64;
    let mut cut = false;
    let mut parts = Vec::new();
    for mut file in files {
        let len = file.metadata()?.len();
        let want = len.min(budget);
        cut |= want < len;
        budget -= want;
        file.seek(SeekFrom::Start(len - want))?;
        let mut bytes = Vec::new();
        file.take(want).read_to_end(&mut bytes)?;
        parts.push(bytes);
    }
    parts.reverse();
    let mut text = parts.concat();
    let mut out = format!("{header}\n");
    if cut {
        // The cut lands mid-line; the bundle starts at the next whole one.
        let start = text
            .iter()
            .position(|&b| b == b'\n')
            .map_or(text.len(), |i| i + 1);
        text.drain(..start);
        out.push_str("… older lines trimmed …\n");
    }
    out.push_str(&String::from_utf8_lossy(&text));
    Ok(out)
}

fn sink() -> MutexGuard<'static, Option<LogFile>> {
    FILE.lock().unwrap_or_else(|e| e.into_inner())
}

/// One line to the log file, when one is open. A failed write closes it, and the bundle falls
/// back to the ring.
#[cfg(target_os = "android")]
pub(crate) fn append(line: &str) {
    let mut sink = sink();
    if sink.as_mut().is_some_and(|file| file.append(line).is_err()) {
        *sink = None;
    }
}

/// The dir the log file is open in: `None` before Kotlin names it, and after a write failed.
#[cfg(target_os = "android")]
pub(crate) fn dir() -> Option<PathBuf> {
    sink().as_ref().map(|file| file.dir.clone())
}

/// `NativeBridge.nativeOpenLogFile(dir)` — from now on every log line also lands in `dir`'s
/// `client.log`. Once per process, before anything worth keeping is logged.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeOpenLogFile<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dir: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let dir = dir.try_to_string(env)?;
        match LogFile::open(Path::new(&dir)) {
            Ok(file) => *sink() = Some(file),
            Err(e) => log::warn!("log file did not open in {dir}: {e}"),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeRenderLogs(header): String` — the newest [`MAX_BYTES`] of the log file,
/// earlier runs included, as one text bundle, oldest first, prefixed by `header` (the Kotlin
/// side's identity line). With no file open it is the ring. Never empty (the header line is
/// always present); reads the file, so call it off the main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRenderLogs<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    header: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| {
        let header: String = header.try_to_string(env)?;
        // Every log line takes the sink's lock: hold it for the two opens, not the read.
        let files = sink().as_ref().and_then(|file| file.open_both().ok());
        let file = files.and_then(|files| render_files(files, &header).ok());
        env.new_string(file.unwrap_or_else(|| logring::render(&header)))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeLogCrash(text)` — each line of `text` as an ERROR `pf.crash` record, on
/// disk before this returns: an uncaught exception's stack, written just before the process dies.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLogCrash<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    text: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        for line in text.try_to_string(env)?.lines() {
            log::error!(target: "pf.crash", "{line}");
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeLogExit(summary, reason, trace)` — how the previous run ended: Android's
/// `ApplicationExitInfo` as one `pf.exit` line, then what its `trace` holds for that `reason`
/// ([`crate::crash::trace_lines`]) as `pf.crash` lines. `trace` is empty when Android kept none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLogExit<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    summary: JString<'local>,
    reason: jint,
    trace: JByteArray<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        log::info!(target: "pf.exit", "previous run ended: {}", summary.try_to_string(env)?);
        for line in crate::crash::trace_lines(reason, &env.convert_byte_array(&trace)?) {
            log::error!(target: "pf.crash", "{line}");
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeLogWifiLink(reason, rssiDbm, txMbps, rxMbps, freqMhz, standard)` — one
/// `pf.wifi` line in the ring. Kotlin reads the link and decides when (`WifiLinkLog`); its own
/// `Log` reaches logcat but never a "Send logs" bundle. `-1` is unknown; `freqMhz` ≤ 0 is off Wi-Fi.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLogWifiLink<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    reason: JString<'local>,
    rssi_dbm: jint,
    tx_mbps: jint,
    rx_mbps: jint,
    freq_mhz: jint,
    standard: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let reason = reason.try_to_string(env)?;
        log::info!(
            target: "pf.wifi",
            "link reason={reason} rssiDbm={rssi_dbm} txMbps={tx_mbps} rxMbps={rx_mbps} \
             freqMhz={freq_mhz} standard={}",
            wifi_standard(standard)
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeLogDisplay(line)` — one `pf.display` line in the ring: the displays and
/// fold features a stream sees, so a bundle says which dual-screen shape a device reports.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeLogDisplay<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    line: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let line = line.try_to_string(env)?;
        log::info!(target: "pf.display", "{line}");
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `WifiInfo.getWifiStandard()` by its IEEE name (`ScanResult.WIFI_STANDARD_*`).
fn wifi_standard(code: jint) -> &'static str {
    match code {
        1 => "legacy",
        4 => "11n",
        5 => "11ac",
        6 => "11ax",
        7 => "11ad",
        8 => "11be",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-logs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_next_run_sends_the_last_runs_lines() {
        let dir = scratch("reopen");
        LogFile::open(&dir).unwrap().append("first run").unwrap();
        let mut file = LogFile::open(&dir).unwrap();
        file.append("second run").unwrap();
        assert_eq!(file.render("hdr").unwrap(), "hdr\nfirst run\nsecond run\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotation_keeps_one_full_bundle_of_whole_lines() {
        let dir = scratch("rotate");
        let mut file = LogFile::open(&dir).unwrap();
        let count = 20_000; // ~2.1 MB: two rotations
        for i in 0..count {
            file.append(&format!("{i:06} {}", "x".repeat(99))).unwrap();
        }
        let bundle = file.render("hdr").unwrap();
        let mut lines = bundle.lines();
        assert_eq!(lines.next(), Some("hdr"));
        assert_eq!(lines.next(), Some("… older lines trimmed …"));
        let kept: Vec<usize> = lines
            .map(|line| {
                assert_eq!(line.len(), 106, "a whole line");
                line[..6].parse().unwrap()
            })
            .collect();
        assert_eq!(kept, (count - kept.len()..count).collect::<Vec<_>>());
        assert!(
            kept.len() > MAX_BYTES / 107 - 1,
            "a full bundle: {}",
            kept.len()
        );
        assert!(fs::metadata(dir.join(CURRENT)).unwrap().len() < MAX_BYTES as u64);
        assert!(fs::metadata(dir.join(PREVIOUS)).unwrap().len() <= MAX_BYTES as u64 + 107);
        fs::remove_dir_all(&dir).unwrap();
    }
}
