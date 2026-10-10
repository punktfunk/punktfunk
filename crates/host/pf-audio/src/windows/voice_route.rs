//! Voice chat on the host (`PUNKTFUNK_AUDIO_VOICE_CHAT=host`), the Windows half.
//!
//! Windows routes per application through the store `mmsys.cpl`'s "App volume and
//! device preferences" page writes: `AudioPolicyConfig::SetPersistedDefaultAudioEndpoint
//! (pid, flow, role, device)`, undocumented like the `IPolicyConfig` default-endpoint
//! write next door. The store is per user and the host is SYSTEM, so the write has to
//! come from the signed-in user's context: a worker thread finds voice-app processes
//! and runs this same binary as the console user, windowless
//! (`punktfunk-host voice-route set …`, [`cli`]), to pin them. A host that is not
//! SYSTEM writes in-process.
//!
//! A pin is keyed by the app, not the pid, and outlives both the process and a host
//! crash. The marker file owes every app a clear before its pin is written, and after,
//! so a pin that lands late is still owed. The helper keeps the operator's own pin in
//! the user's profile and puts it back on clear; a pin the operator moved since is left
//! alone. Owed apps are cleared at session end, at host start, or the next session end
//! the app is running again ([`recover_orphaned`]).
//!
//! The target is the output the operator heard before the session parked the default
//! on the plan's sink ([`super::audio_control::parked_previous_render`]).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How often the worker looks for a voice app launched mid-session.
const RESCAN_EVERY: Duration = Duration::from_secs(5);
/// A pin helper that has not exited in this long is not going to.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a clear waits for a worker still pinning. The marker already owes its apps.
const WORKER_JOIN: Duration = Duration::from_secs(15);
/// An app Windows will not pin (a helper process that never plays audio) is asked again
/// after this, not on every scan.
const REFUSED_RETRY: Duration = Duration::from_secs(60);

/// Serializes the marker's read-modify-write: a worker and a clear can run at once.
static MARKER: Mutex<()> = Mutex::new(());

/// What the pin borrows from the host binary: its process scan, its console-user spawn and
/// its SYSTEM check.
pub struct HostHooks {
    /// `(pid, parent pid, image base name)` of every process, one Toolhelp snapshot.
    pub processes: fn() -> Vec<(u32, u32, String)>,
    /// Run a command line as the console user, windowless; its exit code.
    pub run_hidden_as_user: fn(&str, Duration) -> Result<u32>,
    /// Whether this process runs as LocalSystem.
    pub running_as_system: fn() -> bool,
}

/// Set once at host startup. Unset, no voice app is found, so no helper runs.
pub static HOST_HOOKS: OnceLock<HostHooks> = OnceLock::new();

pub fn wanted() -> bool {
    pf_host_config::config().audio_voice_chat == pf_host_config::VoiceChatRoute::Host
}

/// The capture thread's pins: which output, which apps point at it, and the worker
/// that writes them.
#[derive(Default)]
pub struct VoiceRoute {
    target: Option<String>,
    /// Lowercase exe names pinned to `target`.
    pinned: BTreeSet<String>,
    last_scan: Option<Instant>,
    /// Apps Windows refused, and when: skipped until [`REFUSED_RETRY`] has passed.
    refused: BTreeMap<String, Instant>,
    /// A worker's report for one scan.
    inflight: Option<Receiver<ScanReport>>,
}

/// What one worker scan did: the target it pinned to, the apps it pinned and the apps
/// Windows refused.
struct ScanReport {
    target: String,
    pinned: Vec<String>,
    refused: Vec<String>,
}

impl VoiceRoute {
    /// Point voice apps at `device_id` for this capture. Never blocks: a new target
    /// simply re-pins every app on the next [`tick`](Self::tick).
    pub fn arm(&mut self, device_id: &str) {
        if !wanted() || self.target.as_deref() == Some(device_id) {
            return;
        }
        self.target = Some(device_id.to_owned());
        self.pinned.clear();
        self.refused.clear();
        self.last_scan = None;
        tracing::info!(
            device = device_id,
            "voice chat stays on the host output — pinning voice apps to it"
        );
    }

    /// Collect the last worker's pins and start the next scan when one is due. Nothing
    /// here touches a process or a snapshot: this runs on the capture thread.
    pub fn tick(&mut self) {
        if let Some(rx) = &self.inflight {
            match rx.try_recv() {
                Ok(report) => {
                    self.inflight = None;
                    // A report for an earlier target: the next scan re-pins those apps.
                    if self.target.as_deref() == Some(report.target.as_str()) {
                        if !report.pinned.is_empty() {
                            tracing::info!(apps = ?report.pinned,
                                "voice-chat apps pinned to the host output");
                        }
                        self.pinned.extend(report.pinned);
                        let now = Instant::now();
                        self.refused
                            .extend(report.refused.into_iter().map(|exe| (exe, now)));
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.inflight = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
            }
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        if self.last_scan.is_some_and(|t| t.elapsed() < RESCAN_EVERY) {
            return;
        }
        self.last_scan = Some(Instant::now());
        self.refused.retain(|_, at| at.elapsed() < REFUSED_RETRY);
        let mut known = self.pinned.clone();
        known.extend(self.refused.keys().cloned());
        let (tx, rx) = channel();
        self.inflight = Some(rx);
        std::thread::Builder::new()
            .name("punktfunk-voice-pin".into())
            .spawn(move || {
                let apps = &pf_host_config::config().audio_voice_apps;
                let (mut pinned, mut refused) = (Vec::new(), Vec::new());
                for (pid, exe) in voice_processes(apps) {
                    if known.contains(&exe) || pinned.contains(&exe) {
                        continue;
                    }
                    let pid = pid.to_string();
                    // A process that never played audio has no pin to read or write. Nothing
                    // is owed for it; the app's next process may be the one that plays.
                    if !run_helper(&["probe", &target, &pid, &exe]) {
                        if !refused.contains(&exe) {
                            refused.push(exe);
                        }
                        continue;
                    }
                    // Owed before the write and after it: a clear that ran in between
                    // removed the entry, and this pin still has to be undone.
                    owe(&exe, &target);
                    let ok = run_helper(&["set", &target, &pid, &exe]);
                    owe(&exe, &target);
                    if ok {
                        refused.retain(|r| *r != exe);
                        pinned.push(exe);
                    } else if !refused.contains(&exe) {
                        refused.push(exe);
                    }
                }
                let _ = tx.send(ScanReport {
                    target,
                    pinned,
                    refused,
                });
            })
            .map_err(|e| tracing::warn!(error = %e, "voice-chat pin worker did not start"))
            .ok();
    }

    /// Put every pinned app back where the operator had it: session end. Blocking — the
    /// capture is stopping. An app that already exited stays owed.
    pub fn clear(&mut self) {
        // A worker still pinning would land its pin after this clear.
        if let Some(rx) = self.inflight.take() {
            let _ = rx.recv_timeout(WORKER_JOIN);
        }
        self.target = None;
        self.last_scan = None;
        self.pinned.clear();
        self.refused.clear();
        recover_orphaned();
    }
}

/// Clear the pins the marker still owes, for every owed app that is running now. Host
/// start and session end; a crash or an app that exited before the clear leaves the
/// marker for the next chance. One helper per app, so one refusal keeps only its own
/// entry.
pub fn recover_orphaned() {
    let _marker = MARKER.lock().unwrap_or_else(|e| e.into_inner());
    let mut owed = read_owed();
    if owed.is_empty() {
        return;
    }
    let apps: Vec<String> = owed.keys().cloned().collect();
    let running = voice_processes(&apps);
    if running.is_empty() {
        tracing::debug!(owed = ?apps, "voice-chat pins owed to apps that are not running");
        return;
    }
    let mut cleared = Vec::new();
    for (pid, exe) in running {
        // A fragment can match an app nobody owes; only owed apps are touched.
        if !owed.contains_key(&exe) {
            continue;
        }
        let target = owed
            .get(&exe)
            .cloned()
            .flatten()
            .unwrap_or_else(|| "-".into());
        if run_helper(&["clear", &target, &pid.to_string(), &exe]) {
            owed.remove(&exe);
            cleared.push(exe);
        }
    }
    write_owed(&owed);
    let left: Vec<&String> = owed.keys().collect();
    tracing::info!(cleared = ?cleared, still_owed = ?left, "voice-chat pins cleared");
}

fn marker_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("voice-route.pinned")
}

/// Owed apps, one `exe<TAB>device-id` line each: the lowercase exe name and the target
/// this host pinned it to. A bare `exe` line (older hosts) has no target: its clear
/// writes "default" whatever the pin is now.
fn read_owed() -> BTreeMap<String, Option<String>> {
    std::fs::read_to_string(marker_path())
        .map(|s| parse_owed(&s))
        .unwrap_or_default()
}

fn parse_owed(s: &str) -> BTreeMap<String, Option<String>> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once('\t') {
            Some((exe, dev)) if !dev.trim().is_empty() => {
                (exe.trim().to_string(), Some(dev.trim().to_string()))
            }
            _ => (l.split('\t').next().unwrap_or(l).trim().to_string(), None),
        })
        .collect()
}

fn format_owed(owed: &BTreeMap<String, Option<String>>) -> String {
    owed.iter()
        .map(|(exe, dev)| match dev {
            Some(d) => format!("{exe}\t{d}\n"),
            None => format!("{exe}\n"),
        })
        .collect()
}

/// Temp file plus rename: a crash mid-write must not truncate what is owed.
fn write_owed(owed: &BTreeMap<String, Option<String>>) {
    let path = marker_path();
    if owed.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Err(e) = pf_paths::replace_file(&path, format_owed(owed).as_bytes()) {
        tracing::warn!(error = %e, "voice-chat pin marker not written — a crash would leave the pins");
    }
}

fn owe(exe: &str, target: &str) {
    let _marker = MARKER.lock().unwrap_or_else(|e| e.into_inner());
    let mut owed = read_owed();
    if owed.get(exe).cloned().flatten().as_deref() != Some(target) {
        owed.insert(exe.to_string(), Some(target.to_string()));
        write_owed(&owed);
    }
}

/// `(pid, lowercase exe name)`: every process of each voice app, in this host's session.
/// One Toolhelp snapshot ([`HostHooks::processes`]). Never on the capture thread.
///
/// The pin is keyed by the app, so one pid that answers is enough, but it may not be the
/// first: an Electron app plays from a child process. Another session's process is left
/// out: the console user's helper can't pin it.
fn voice_processes(apps: &[String]) -> Vec<(u32, String)> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let Some(host) = HOST_HOOKS.get() else {
        return Vec::new();
    };
    let session_of = |pid: u32| {
        let mut s = 0u32;
        // SAFETY: `s` is a live local out-param for this synchronous call.
        unsafe { ProcessIdToSessionId(pid, &mut s) }
            .ok()
            .map(|()| s)
    };
    let ours = session_of(std::process::id());
    (host.processes)()
        .into_iter()
        .filter_map(|(pid, _, exe)| {
            let exe = exe.to_ascii_lowercase();
            (pf_host_config::voice_app_matches([exe.as_str()], apps)
                && (ours.is_none() || session_of(pid) == ours))
                .then_some((pid, exe))
        })
        .collect()
}

/// Run `voice-route <args>` as the console user, windowless, and wait for its verdict.
/// In-process only when this host is not SYSTEM: SYSTEM's own write lands in SYSTEM's
/// store, where no app of the user's looks.
fn run_helper(args: &[&str]) -> bool {
    // Unset, `voice_processes` found no pid to get here with.
    let Some(host) = HOST_HOOKS.get() else {
        return false;
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "voice-chat pin: own executable path unknown");
            return false;
        }
    };
    let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
    let cmdline = format!("\"{}\" voice-route {}", exe.display(), quoted.join(" "));
    match (host.run_hidden_as_user)(&cmdline, HELPER_TIMEOUT) {
        Ok(0) => true,
        // A probe refusal is an app with no audio yet: expected, and retried later.
        Ok(code) if args.first() == Some(&"probe") => {
            tracing::debug!(
                code,
                app = args.get(3).copied(),
                "voice-chat app not pinnable yet"
            );
            false
        }
        Ok(code) => {
            tracing::warn!(
                code,
                "voice-chat pin helper refused — the apps stay where they are"
            );
            false
        }
        Err(spawn_err) if !(host.running_as_system)() => {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            match cli(&owned) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        spawn = %format!("{spawn_err:#}"),
                        "voice-chat pin not written — the apps stay where they are"
                    );
                    false
                }
            }
        }
        Err(spawn_err) => {
            tracing::warn!(error = %format!("{spawn_err:#}"),
                "voice-chat pin helper did not run — the apps stay where they are");
            false
        }
    }
}

/// `punktfunk-host voice-route probe|set <device-id> <pid> <exe>` / `clear <device-id|-> <pid> <exe>`:
/// the process that reads or writes one app's pins, in whichever user context it was started in.
///
/// `probe` only reads: it fails for a process Windows will not pin, one that never played audio.
/// `set` saves the app's current pins in the user's profile the first time, then pins it.
/// `clear` puts a saved pin back on each role still pinned to `device-id`, and leaves a role
/// the operator moved since. `-` (an older marker) clears every role to "default".
pub fn cli(args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: punktfunk-host voice-route probe|set <device-id> <pid> <exe> | \
                         clear <device-id|-> <pid> <exe>";
    let arg = |i: usize| args.get(i).map(String::as_str).context(USAGE);
    let (verb, device, pid, exe) = (arg(0)?, arg(1)?, arg(2)?, arg(3)?.to_ascii_lowercase());
    let pid: u32 = pid.parse().context(USAGE)?;
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let policy = AudioPolicyConfig::activate()?;
    let mut saved = read_saved();
    match verb {
        "probe" => policy.get_persisted_render(pid).map(|_| ()),
        "set" => {
            let ours = mmdevapi_path(device);
            if !saved.contains_key(&exe) {
                let current = policy.get_persisted_render(pid)?;
                saved.insert(exe.clone(), original_pins(current, &ours));
                write_saved(&saved)?;
            }
            policy.set_persisted_render(pid, [Some(ours.as_str()); 3])
        }
        "clear" => {
            let original = saved.remove(&exe).unwrap_or_default();
            let current = policy.get_persisted_render(pid)?;
            let ours = (device != "-").then(|| mmdevapi_path(device));
            let restore = restored_pins(current, ours.as_deref(), &original);
            policy.set_persisted_render(pid, restore.each_ref().map(|r| r.as_deref()))?;
            write_saved(&saved)
        }
        _ => bail!("{USAGE}"),
    }
}

/// What an app's roles were pinned to before this host: `None` is "default". A role that
/// already carries our pin (a crash left it) saves as "default", never as the operator's.
fn original_pins(current: [Option<String>; 3], ours: &str) -> [Option<String>; 3] {
    current.map(|c| c.filter(|c| !c.eq_ignore_ascii_case(ours)))
}

/// Per role, what a clear writes: the saved pin where the role still carries ours (or every
/// role, with no `ours`), the current value where the operator moved it since.
fn restored_pins(
    current: [Option<String>; 3],
    ours: Option<&str>,
    original: &[Option<String>; 3],
) -> [Option<String>; 3] {
    let mut out = current;
    for (role, cur) in out.iter_mut().enumerate() {
        let still_ours = match ours {
            Some(o) => cur.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(o)),
            None => true,
        };
        if still_ours {
            *cur = original[role].clone();
        }
    }
    out
}

/// The operator's own pins, per app, in the user's profile: `exe<TAB>role0<TAB>role1<TAB>role2`,
/// an empty field for "default". Written by the helper, which runs as that user.
fn saved_path() -> Result<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unset")?;
    Ok(std::path::PathBuf::from(base)
        .join("punktfunk")
        .join("voice-route.saved"))
}

fn read_saved() -> BTreeMap<String, [Option<String>; 3]> {
    saved_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_saved(&s))
        .unwrap_or_default()
}

fn parse_saved(s: &str) -> BTreeMap<String, [Option<String>; 3]> {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut f = l.split('\t');
            let exe = f.next().unwrap_or_default().trim().to_string();
            let role =
                |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(String::from);
            (exe, [role(f.next()), role(f.next()), role(f.next())])
        })
        .collect()
}

fn format_saved(saved: &BTreeMap<String, [Option<String>; 3]>) -> String {
    saved
        .iter()
        .map(|(exe, roles)| {
            let r = |i: usize| roles[i].as_deref().unwrap_or("");
            format!("{exe}\t{}\t{}\t{}\n", r(0), r(1), r(2))
        })
        .collect()
}

fn write_saved(saved: &BTreeMap<String, [Option<String>; 3]>) -> Result<()> {
    let path = saved_path()?;
    if saved.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    pf_paths::replace_file(&path, format_saved(saved).as_bytes())
        .with_context(|| format!("replace {}", path.display()))
}

/// `Windows.Media.Internal.AudioPolicyConfig`'s factory, by slot: IUnknown, IInspectable,
/// nineteen volume-group and chat methods this host never calls, then the three we do.
#[repr(C)]
struct IAudioPolicyConfigFactoryVtbl {
    query_interface: unsafe extern "system" fn(
        *mut c_void,
        *const windows::core::GUID,
        *mut *mut c_void,
    ) -> windows::core::HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    /// GetIids, GetRuntimeClassName, GetTrustLevel.
    _inspectable: [*const c_void; 3],
    _reserved: [*const c_void; 19],
    /// `(pid, EDataFlow, ERole, HSTRING device)`; a null HSTRING clears the pin.
    set_persisted_default_audio_endpoint: unsafe extern "system" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        windows::core::Ref<'_, windows::core::HSTRING>,
    ) -> windows::core::HRESULT,
    get_persisted_default_audio_endpoint: unsafe extern "system" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        *mut windows::core::HSTRING,
    ) -> windows::core::HRESULT,
    clear_all_persisted_application_default_endpoints:
        unsafe extern "system" fn(*mut c_void) -> windows::core::HRESULT,
}

// No header exists; calls go by slot. A field added above `set_persisted…` would invoke a
// different method, so the slot indexes and the table size are pinned here.
const _: () = {
    use std::mem::{offset_of, size_of};
    type P = *const c_void;
    assert!(offset_of!(IAudioPolicyConfigFactoryVtbl, _inspectable) == 3 * size_of::<P>());
    assert!(offset_of!(IAudioPolicyConfigFactoryVtbl, _reserved) == 6 * size_of::<P>());
    assert!(
        offset_of!(
            IAudioPolicyConfigFactoryVtbl,
            set_persisted_default_audio_endpoint
        ) == 25 * size_of::<P>()
    );
    assert!(
        offset_of!(
            IAudioPolicyConfigFactoryVtbl,
            get_persisted_default_audio_endpoint
        ) == 26 * size_of::<P>()
    );
    assert!(
        offset_of!(
            IAudioPolicyConfigFactoryVtbl,
            clear_all_persisted_application_default_endpoints
        ) == 27 * size_of::<P>()
    );
    assert!(size_of::<IAudioPolicyConfigFactoryVtbl>() == 28 * size_of::<P>());
    // The HSTRING handle is passed by value: one pointer.
    assert!(size_of::<windows::core::Ref<windows::core::HSTRING>>() == size_of::<P>());
};

/// The IID changed in Windows 11 21H2; both name the same table.
const IID_WIN11: windows::core::GUID =
    windows::core::GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
const IID_WIN10: windows::core::GUID =
    windows::core::GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

/// A live `IAudioPolicyConfigFactory`; the `IUnknown` inside releases it on drop.
#[repr(transparent)]
#[derive(Clone)]
struct AudioPolicyConfig(windows::core::IUnknown);

// SAFETY: one COM pointer (transparent over `IUnknown`) to an object whose table starts with
// `IAudioPolicyConfigFactoryVtbl`, the layout the asserts pin, under either IID.
unsafe impl windows::core::Interface for AudioPolicyConfig {
    type Vtable = IAudioPolicyConfigFactoryVtbl;
    const IID: windows::core::GUID = IID_WIN11;
}

impl AudioPolicyConfig {
    /// The newer IID is tried first.
    fn activate() -> Result<AudioPolicyConfig> {
        use windows::core::{IInspectable, Interface, HSTRING};
        use windows::Win32::System::WinRT::RoGetActivationFactory;
        let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
        // SAFETY: `class` is a live HSTRING; the factory is an owned IInspectable released by
        // its Drop.
        let factory: IInspectable = unsafe { RoGetActivationFactory(&class) }
            .map_err(|e| anyhow!("RoGetActivationFactory(AudioPolicyConfig): {e}"))?;
        for iid in [IID_WIN11, IID_WIN10] {
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: QueryInterface on a live factory; `raw` is a live local out-param.
            if unsafe { factory.query(&iid, &mut raw) }.is_ok() && !raw.is_null() {
                // SAFETY: `raw` is one owned reference to an object with this table; `from_raw`
                // adopts it.
                return Ok(unsafe { AudioPolicyConfig::from_raw(raw) });
            }
        }
        bail!("IAudioPolicyConfigFactory: neither the Windows 11 nor the Windows 10 interface answered")
    }

    /// Write `pid`'s render pin on each role (console, multimedia, communications): a device
    /// interface path, or `None` for "default". All three roles: a voice app may open its call
    /// audio on the communications role, and a pin that skipped it would leave exactly the
    /// voices in the stream.
    fn set_persisted_render(&self, pid: u32, paths: [Option<&str>; 3]) -> Result<()> {
        use windows::core::{Interface, Ref, HSTRING};
        for (role, path) in paths.iter().enumerate() {
            // Empty is the null HSTRING: "default", as the Sound settings page writes.
            let hs = path.map(HSTRING::from).unwrap_or_default();
            // SAFETY: live factory; the call borrows `hs`, which outlives it. eRender = 0;
            // eConsole..eCommunications = 0..=2.
            let hr = unsafe {
                (self.vtable().set_persisted_default_audio_endpoint)(
                    self.as_raw(),
                    pid,
                    0,
                    role as u32,
                    Ref::from(&hs),
                )
            };
            hr.ok().map_err(|e| {
                anyhow!("SetPersistedDefaultAudioEndpoint(pid {pid}, role {role}): {e}")
            })?;
        }
        Ok(())
    }

    /// `pid`'s current render pin per role, as the device interface path; `None` is "default".
    fn get_persisted_render(&self, pid: u32) -> Result<[Option<String>; 3]> {
        use windows::core::{Interface, HSTRING};
        let mut out: [Option<String>; 3] = Default::default();
        for (role, slot) in out.iter_mut().enumerate() {
            let mut hs = HSTRING::new();
            // SAFETY: live factory; `hs` is a null HSTRING the call overwrites with one owned
            // reference (null = "default"), which its Drop releases.
            let hr = unsafe {
                (self.vtable().get_persisted_default_audio_endpoint)(
                    self.as_raw(),
                    pid,
                    0,
                    role as u32,
                    &mut hs,
                )
            };
            hr.ok().map_err(|e| {
                anyhow!("GetPersistedDefaultAudioEndpoint(pid {pid}, role {role}): {e}")
            })?;
            let path = hs.to_string_lossy();
            *slot = (!path.is_empty()).then_some(path);
        }
        Ok(out)
    }
}

/// The device-interface path the store keys on: the MMDevice id inside the
/// `SWD\MMDEVAPI` prefix and the render interface class.
fn mmdevapi_path(device_id: &str) -> String {
    format!("\\\\?\\SWD#MMDEVAPI#{device_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str =
        "\\\\?\\SWD#MMDEVAPI#{0.0.0.00000000}.{aaaa}#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
    const HEADSET: &str =
        "\\\\?\\SWD#MMDEVAPI#{0.0.0.00000000}.{bbbb}#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

    #[test]
    fn store_path_wraps_the_endpoint_id() {
        assert_eq!(mmdevapi_path("{0.0.0.00000000}.{aaaa}"), OURS);
    }

    /// Older hosts wrote bare exe names; they still parse, as "clear to default".
    #[test]
    fn owed_marker_round_trips_and_reads_old_lines() {
        let mut owed = BTreeMap::new();
        owed.insert(
            "discord.exe".to_string(),
            Some("{0.0.0.00000000}.{aaaa}".to_string()),
        );
        owed.insert("teamspeak.exe".to_string(), None);
        assert_eq!(parse_owed(&format_owed(&owed)), owed);
        let old = parse_owed("discord.exe\n\n  mumble.exe \n");
        assert_eq!(old.get("discord.exe"), Some(&None));
        assert_eq!(old.get("mumble.exe"), Some(&None));
    }

    #[test]
    fn saved_pins_round_trip_with_default_roles() {
        let mut saved = BTreeMap::new();
        saved.insert(
            "discord.exe".to_string(),
            [Some(HEADSET.to_string()), None, Some(HEADSET.to_string())],
        );
        assert_eq!(parse_saved(&format_saved(&saved)), saved);
    }

    /// A pin a crash left behind is ours, not the operator's, and saves as "default".
    #[test]
    fn our_own_pin_never_saves_as_the_original() {
        let current = [Some(OURS.to_lowercase()), Some(HEADSET.to_string()), None];
        assert_eq!(
            original_pins(current, OURS),
            [None, Some(HEADSET.to_string()), None]
        );
    }

    /// Clear restores the saved pin only where ours still stands; a moved role stays moved.
    #[test]
    fn clear_restores_only_roles_still_pinned_to_us() {
        let original = [Some(HEADSET.to_string()), None, None];
        let current = [
            Some(OURS.to_string()),
            Some("elsewhere".to_string()),
            Some(OURS.to_string()),
        ];
        assert_eq!(
            restored_pins(current.clone(), Some(OURS), &original),
            [
                Some(HEADSET.to_string()),
                Some("elsewhere".to_string()),
                None
            ]
        );
        // No target (an older marker): every role goes back.
        assert_eq!(restored_pins(current, None, &original), original);
    }
}
