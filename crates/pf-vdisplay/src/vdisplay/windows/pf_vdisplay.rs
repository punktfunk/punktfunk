//! Windows virtual-display backend for **pf-vdisplay**, punktfunk's IddCx Indirect Display Driver.
//!
//! [`create`](VirtualDisplay::create) adds a virtual monitor at the client's `WxH@Hz` (mode baked
//! into the ADD IOCTL; no EDID seeding), starts the watchdog ping, and the returned
//! [`VirtualOutput`]'s keepalive `Drop` removes it.
//!
//! Control surface: device-interface GUID + `CreateFileW` + `DeviceIoControl`. Wire contract is
//! [`pf_driver_proto::control`] (versioned `#[repr(C)] Pod` structs). See
//! `design/windows-host-rewrite.md`.
//!
//! Lifecycle, CCD isolation, and active-mode forcing live in [`super::manager`] and
//! `pf_win_display::win_display` — a pf-vdisplay `target_id` is a real OS target id. This module
//! owns only GUID, IOCTL codes, request/reply structs, and the version handshake.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, HDEVINFO,
    SETUP_DI_REGISTRY_PROPERTY, SPDRP_DRIVER, SPDRP_HARDWAREID, SPINT_ACTIVE,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use bytemuck::Zeroable;
use pf_driver_proto::{control, encode};

use super::manager::{AddedMonitor, MonitorKey, VdisplayDriver};
use super::{Mode, VirtualDisplay, VirtualOutput};

// Own interface GUID (`PF_VDISPLAY_INTERFACE_GUID_U128`), not SudoVDA's `{e5bcc234-…}` —
// a private GUID is how we refuse to open a real SudoVDA install.
const PF_VDISPLAY_INTERFACE: GUID =
    GUID::from_u128(pf_driver_proto::PF_VDISPLAY_INTERFACE_GUID_U128);

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// The driver's key for a monitor: a per-process counter.
///
/// Two hosts both starting at 1 used to collide, and an `IOCTL_ADD` on a live
/// key makes the driver depart the incumbent — so a second host's first ADD
/// tore down the first host's display. #624 keys the driver's sessions by the
/// requesting process, so identical counters in two hosts no longer meet.
///
/// A fresh key per ADD is also what the preempt-and-recreate path needs: it
/// removes the old monitor, waits up to 400 ms for the departure, then adds
/// regardless, and a reused key turns that last add into the depart-the-
/// incumbent branch mid-churn.
fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// The pf-vdisplay control device. `probe_sync` is the only constructor and opens it without
/// `FILE_FLAG_OVERLAPPED`, so every IOCTL through it completes before `DeviceIoControl` returns.
/// A borrow keeps the handle open; drop closes it.
pub struct ControlDevice(OwnedHandle);

#[cfg(test)]
impl ControlDevice {
    /// A synchronous handle that is not the driver's, for a fake driver that issues no IOCTL.
    pub(crate) fn stand_in() -> Self {
        let exe = std::env::current_exe().expect("locate the test binary");
        Self(
            std::fs::File::open(exe)
                .expect("open the test binary")
                .into(),
        )
    }
}

/// One METHOD_BUFFERED `DeviceIoControl`. Empty `input`/`output` are allowed; `bytemuck` at the
/// call site.
fn ioctl(dev: &ControlDevice, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
    let mut returned = 0u32;
    let inp = (!input.is_empty()).then_some(input.as_ptr() as *const c_void);
    let outp = (!output.is_empty()).then_some(output.as_mut_ptr() as *mut c_void);
    // SAFETY: `dev` is borrowed, so its handle stays open for the call. `inp`/`outp` come from
    // `input`/`output` with those slices' lengths, and both slices outlive the call. The handle is
    // synchronous (`ControlDevice`) and no OVERLAPPED is passed, so the request completes before
    // return and nothing touches either buffer afterwards.
    unsafe {
        DeviceIoControl(
            HANDLE(dev.0.as_raw_handle()),
            code,
            inp,
            input.len() as u32,
            outp,
            output.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .with_context(|| format!("DeviceIoControl(code={code:#x})"))?;
    Ok(returned)
}

/// [`ioctl`] for a control verb that writes no output.
fn ioctl_send(dev: &ControlDevice, code: u32, input: &[u8]) -> Result<()> {
    ioctl(dev, code, input, &mut []).map(|_| ())
}

/// Remove not-present "punktfunk" monitor PDOs that `IddCxMonitorDeparture` leaves behind.
/// Each ghost pins a VidPN target against IddCx's ~16-slot budget; once full, `IOCTL_ADD`
/// returns 0x80070490 (`ERROR_NOT_FOUND`). Best-effort: only `Present==false` AND
/// `Status==Unknown` nodes are removed, so a live session is never touched. Returns how many
/// were removed. Logs found and removed even when both are zero — silence hid a failed reap.
fn reap_ghost_monitors() -> u32 {
    // Presence, not health: `Status -ne 'OK'` would `pnputil /remove-device` a live monitor in a
    // transient problem state. `Status -eq 'Unknown'` guards `Present` reading null (`-not $null`
    // is true and would select every device). Full-path pnputil; `$LASTEXITCODE=1` before launch
    // so a miss cannot look like exit 0. Tokens are locale-invariant.
    const REAP_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $g = @(Get-PnpDevice -Class Monitor | Where-Object { -not $_.Present -and $_.Status -eq 'Unknown' -and $_.FriendlyName -match 'punktfunk' }); \
        $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); \
        $n = 0; foreach ($d in $g) { $LASTEXITCODE = 1; if (Test-Path $pnp) { & $pnp /remove-device $d.InstanceId *> $null }; if ($LASTEXITCODE -eq 0) { $n++ } }; \
        Write-Output ($g.Count.ToString() + ' ' + $n)";
    // Full-path powershell: LocalSystem PATH need not include System32.
    let ps = pf_paths::system32(r"WindowsPowerShell\v1.0\powershell.exe");
    // Bounded: this runs under the manager's `device` mutex (driver open) and under its `state`
    // lock (the ADD slot-exhaustion retry), so a wedged Get-PnpDevice would block every acquire,
    // release and `/display/state`. `output_within` kills the whole tree on the deadline.
    let mut cmd = std::process::Command::new(&ps);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        REAP_PS,
    ]);
    match crate::proc::output_within(&mut cmd, std::time::Duration::from_secs(30)) {
        Ok(o) => {
            let raw = String::from_utf8_lossy(&o.stdout);
            let Some((found, removed)) = parse_reap_output(&raw) else {
                tracing::warn!(
                    output = %raw.trim(),
                    "pf-vdisplay: ghost-monitor reap died before reporting — ghost nodes (if any) still pin IddCx monitor slots"
                );
                return 0;
            };
            if found == 0 {
                tracing::info!("pf-vdisplay: no ghost (not-present) virtual-monitor nodes to reap");
            } else if removed < found {
                tracing::warn!(
                    found,
                    removed,
                    "pf-vdisplay: ghost-monitor reap left ghost nodes behind — the leftovers keep pinning IddCx monitor slots toward the 0x80070490 wedge"
                );
            } else {
                tracing::warn!(
                    reaped = removed,
                    "pf-vdisplay: reaped ghost (not-present) virtual-monitor nodes — IddCx slot-exhaustion prevention"
                );
            }
            removed
        }
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: ghost-monitor reap could not spawn powershell");
            0
        }
    }
}

/// Parse `"<found> <removed>"` from [`reap_ghost_monitors`]. `None` = the script died before
/// reporting (callers treat that as removed-nothing, loudly).
fn parse_reap_output(out: &str) -> Option<(u32, u32)> {
    let mut it = out.split_whitespace().map(str::parse::<u32>);
    match (it.next(), it.next()) {
        (Some(Ok(found)), Some(Ok(removed))) => Some((found, removed)),
        _ => None,
    }
}

/// What the cycle DID, not the devnode's PnP status afterwards. A device never touched still
/// reads `OK`, so status-after cannot tell a no-op from a reload.
enum AdapterCycle {
    Reloaded { how: &'static str, status: String },
    NotInstalled,
    Refused(String),
}

/// Reload the pf-vdisplay adapter — in-process `reset-pf-vdisplay.ps1` step 3. A killed WUDFHost
/// can leave the devnode "started" yet hostless (PnP OK, no process, zero interfaces).
///
/// Disable+enable is the script's lever, but that script stops the host first: this process
/// still holds [`DeviceSlot`](super::manager) handles, so disable is expected to refuse.
/// `pnputil /restart-device` reloads a device in use. Failure paths re-enable so a half cycle
/// cannot leave the adapter disabled. Best-effort, ~6 s inside the script.
fn reload_vdisplay_adapter() -> AdapterCycle {
    // `$pin` (prepended below) is the devnode this process last opened: a seats box has several
    // same-named adapters, and the name match — only when nothing was ever opened — would cycle
    // a sibling's live one. Live nodes first: `-First 1` can pick a phantom whose disable and
    // restart both fail. `-ErrorAction Stop` inside `try`, since PnP Status reads `OK` after a
    // refused disable. `$LASTEXITCODE=1` before pnputil so "never ran" ≠ 0 (3010 = needs reboot).
    const CYCLE_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $all = @(); if ($pin) { $all = @(Get-PnpDevice -InstanceId $pin | Where-Object { $_ }) }; \
        if ($all.Count -eq 0) { $all = @(Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -match 'punktfunk Virtual Display' }) }; \
        if ($all.Count -eq 0) { Write-Output 'ABSENT'; exit }; \
        $live = @($all | Where-Object { $_.Present -or $_.Status -ne 'Unknown' } | Sort-Object { $_.Status -ne 'OK' }); \
        if ($live.Count -eq 0) { Write-Output ('REFUSED only phantom (not-present) adapter devnodes remain (' + $all.Count + ') - the device node itself is gone and no reload can revive it; reinstalling the host re-creates it'); exit }; \
        $ad = $live[0]; $id = $ad.InstanceId; $err = ''; \
        try { \
            Disable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop; Start-Sleep -Seconds 2; \
            try { Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop } \
            catch { Start-Sleep -Seconds 2; Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop }; \
            Start-Sleep -Seconds 2; \
            Write-Output ('RELOADED cycle ' + (Get-PnpDevice -InstanceId $id).Status); exit \
        } catch { $err = ($_.Exception.Message -replace '\\s+', ' ') }; \
        $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); $LASTEXITCODE = 1; \
        if (Test-Path $pnp) { & $pnp /restart-device $id *> $null }; \
        $rx = $LASTEXITCODE; \
        if ($rx -eq 0) { Start-Sleep -Seconds 2; \
            Write-Output ('RELOADED restart ' + (Get-PnpDevice -InstanceId $id).Status) } \
        else { Enable-PnpDevice -InstanceId $id -Confirm:$false; \
            Write-Output ('REFUSED devnodes=' + $all.Count + ' live=' + $live.Count + ' status=' + $ad.Status + ' problem=' + $ad.ConfigManagerErrorCode + ' restart_exit=' + $rx + ' ' + $err) }";
    let ps = pf_paths::system32(r"WindowsPowerShell\v1.0\powershell.exe");
    let pin = LAST_INSTANCE_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    let script = format!("$pin='{}'; {CYCLE_PS}", pin.replace('\'', "''"));
    // Bounded: this holds the RECOVERY mutex, so a wedged Disable-PnpDevice would stall the whole
    // recovery ladder. The script's own sleeps total ~6-8 s on the happy path.
    let mut cmd = std::process::Command::new(&ps);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &script,
    ]);
    let out = match crate::proc::output_within(&mut cmd, std::time::Duration::from_secs(60)) {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: adapter reload did not complete");
            return AdapterCycle::Refused(format!("powershell did not complete: {e}"));
        }
    };
    let outcome = classify_reload_output(&out);
    match &outcome {
        AdapterCycle::NotInstalled => {
            tracing::warn!("pf-vdisplay: no adapter devnode to reload — driver not installed");
        }
        AdapterCycle::Reloaded { how, status } => tracing::warn!(
            how,
            %status,
            "pf-vdisplay: reloaded the adapter device (hostless-zombie recovery)"
        ),
        AdapterCycle::Refused(why) => tracing::warn!(
            reason = %why,
            "pf-vdisplay: the adapter devnode exists but the reload was refused — a session \
             recovers only after a host-service restart or a reboot"
        ),
    }
    outcome
}

/// Parse [`reload_vdisplay_adapter`] stdout. Split out so the decoder is testable without a box.
fn classify_reload_output(out: &str) -> AdapterCycle {
    let out = out.trim();
    let (verb, rest) = out.split_once(char::is_whitespace).unwrap_or((out, ""));
    match verb {
        "ABSENT" => AdapterCycle::NotInstalled,
        "RELOADED" => {
            let (how, status) = rest
                .trim()
                .split_once(char::is_whitespace)
                .unwrap_or((rest.trim(), ""));
            // `&'static str` so the two levers stay distinct: `restart` means disable was refused
            // (something still holds the device open).
            let how: &'static str = if how == "restart" {
                "pnputil /restart-device"
            } else {
                "disable+enable"
            };
            AdapterCycle::Reloaded {
                how,
                status: status.trim().to_string(),
            }
        }
        // `REFUSED <reason>`, empty stdout, or anything else: the devnode was not reloaded.
        _ => AdapterCycle::Refused(if rest.trim().is_empty() {
            format!("unexpected adapter-reload output: {out:?}")
        } else {
            rest.trim().to_string()
        }),
    }
}

/// True if `e`'s chain carries 0x80070490 (`ERROR_NOT_FOUND`) — IddCx slot exhaustion. The hex
/// is locale-invariant; the OS message text is not.
fn is_slot_exhaustion_wedge(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("0x80070490")
}

/// Pin the IddCx render GPU to `luid` before `IOCTL_ADD`. On a multi-adapter box this stops DXGI
/// reparenting the virtual output onto a different GPU than the one we encode on (ACCESS_LOST).
/// Callers tolerate `Err`: the driver reports the real render LUID in the shared header anyway.
fn set_render_adapter(dev: &ControlDevice, luid: LUID) -> Result<()> {
    let req = control::SetRenderAdapterRequest {
        luid_low: luid.LowPart,
        luid_high: luid.HighPart,
    };
    ioctl_send(
        dev,
        control::IOCTL_SET_RENDER_ADAPTER,
        bytemuck::bytes_of(&req),
    )
    .context("pf-vdisplay SET_RENDER_ADAPTER")
}

/// Deliver a monitor's hardware-cursor section (`IOCTL_SET_CURSOR_CHANNEL`, proto v5). On IOCTL
/// success the driver owns the handle duplicated into WUDFHost; the caller reaps the remote
/// duplicate on failure so none leaks.
pub fn send_cursor_channel(
    dev: &ControlDevice,
    req: &control::SetCursorChannelRequest,
) -> Result<()> {
    ioctl_send(
        dev,
        control::IOCTL_SET_CURSOR_CHANNEL,
        bytemuck::bytes_of(req),
    )
    .context("pf-vdisplay SET_CURSOR_CHANNEL")
}

/// Flip a live monitor's hardware-cursor declaration (`IOCTL_SET_CURSOR_FORWARD`, proto v6).
/// Fails against a pre-v6 driver; callers log and keep the declared-at-ADD behavior.
pub fn send_cursor_forward(
    dev: &ControlDevice,
    req: &control::SetCursorForwardRequest,
) -> Result<()> {
    ioctl_send(
        dev,
        control::IOCTL_SET_CURSOR_FORWARD,
        bytemuck::bytes_of(req),
    )
    .context("pf-vdisplay SET_CURSOR_FORWARD")
}

/// Open a monitor's in-driver encoder (`IOCTL_SET_ENCODE`, proto v7) and adopt its AU section.
/// Same handle contract as [`send_cursor_channel`]: the section and event VALUES in `req` are
/// already duplicated into WUDFHost, the driver owns them iff the IOCTL succeeds, and the
/// caller reaps them on `Err`. A short reply fails closed: every field feeds the session.
pub fn send_set_encode(
    dev: &ControlDevice,
    req: &encode::SetEncodeRequest,
) -> Result<encode::SetEncodeReply> {
    let mut reply = encode::SetEncodeReply::zeroed();
    let res = ioctl(
        dev,
        encode::IOCTL_SET_ENCODE,
        bytemuck::bytes_of(req),
        bytemuck::bytes_of_mut(&mut reply),
    );
    // The open's own story — which backends were tried, what each refused, the bitrate applied —
    // is written inside WUDFHost. Take it here so it lands beside this host's open line instead
    // of a pinger tick later, or not at all when a failed open tears the session down first.
    drain_driver_log(dev);
    let n = res.context("pf-vdisplay SET_ENCODE")?;
    if (n as usize) < size_of::<encode::SetEncodeReply>() {
        // Typed: the IOCTL completed, so the driver owns the handles the caller duplicated.
        return Err(anyhow::Error::new(encode::ReplyTooShort {
            got: n as usize,
            want: size_of::<encode::SetEncodeReply>(),
        }));
    }
    Ok(reply)
}

/// Take the driver's pending diagnostic lines (`IOCTL_DRAIN_LOG`) and re-emit them into this
/// process's log at the level the driver chose. The encoder lives in WUDFHost, so without this
/// its backend rejections, bitrate retargets and wedges are visible only to a debugger.
///
/// Best-effort and silent on failure: a driver built before the verb answers `STATUS_NOT_FOUND`,
/// which means "no lines", and a lost keepalive is the pinger's to report, not this.
fn drain_driver_log(dev: &ControlDevice) {
    // One pinger tick of driver chatter. 512 lines' worth of ceiling against a 256-line ring, so
    // a drain never leaves a backlog behind that the next tick has to catch up on.
    let mut buf = vec![0u8; 256 * 1024];
    let Ok(n) = ioctl(dev, control::IOCTL_DRAIN_LOG, &[], &mut buf) else {
        return;
    };
    for (level, text) in control::log_lines(&buf[..n as usize]) {
        match level {
            control::LOG_ERROR => tracing::error!(target: "pf_vdisplay::driver", "{text}"),
            control::LOG_WARN => tracing::warn!(target: "pf_vdisplay::driver", "{text}"),
            control::LOG_DEBUG => tracing::debug!(target: "pf_vdisplay::driver", "{text}"),
            _ => tracing::info!(target: "pf_vdisplay::driver", "{text}"),
        }
    }
}

/// One-shot control on a monitor's live in-driver encoder (`IOCTL_ENCODE_CTL`, proto v7).
pub fn send_encode_ctl(dev: &ControlDevice, req: &encode::EncodeCtlRequest) -> Result<()> {
    ioctl_send(dev, encode::IOCTL_ENCODE_CTL, bytemuck::bytes_of(req))
        .with_context(|| format!("pf-vdisplay ENCODE_CTL op {}", req.op))
}

/// RAII SetupAPI device-info list. Every [`open_device`] exit path must destroy it; a driverless
/// box probes repeatedly and a leaked `HDEVINFO` per failed open would accumulate.
struct DevInfoList(HDEVINFO);

impl Drop for DevInfoList {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the live device-info list this wrapper solely owns; destroyed
        // exactly once here.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Device-interface enumeration result. `active`/`inactive` let [`ensure_available`] tell a
/// mid-transition devnode (registered, not started) from a missing one. Only the latter is
/// worth reloading; cycling the former lengthens the outage it is waiting out.
struct Probe {
    /// An open control handle and the driver's `IOCTL_GET_INFO` reply: the device answered.
    opened: Option<(ControlDevice, control::InfoReply)>,
    /// `SPINT_ACTIVE` set — owning device is started.
    active: u32,
    /// `SPINT_ACTIVE` clear — registered, owning device not started.
    inactive: u32,
    last_err: Option<anyhow::Error>,
    /// The open or the handshake did not return within [`PROBE_BUDGET`]: WUDFHost is not
    /// serving requests. Neither absent nor not-ready — only a reload clears it.
    wedged: bool,
}

impl Probe {
    fn wedged() -> Self {
        Probe {
            opened: None,
            active: 0,
            inactive: 0,
            last_err: None,
            wedged: true,
        }
    }

    /// No instance of any kind. With an adapter present this is a hostless WUDFHost crash; with
    /// none, the driver is not installed. Waiting alone will not fix either.
    fn is_absent(&self) -> bool {
        !self.wedged && self.opened.is_none() && self.active == 0 && self.inactive == 0
    }

    /// Why no handle came back, naming what was seen. "0 interfaces" and "1 inactive" are
    /// different diagnoses; call only on a miss.
    fn into_error(self) -> anyhow::Error {
        if self.wedged {
            return anyhow::anyhow!(
                "pf-vdisplay control device did not answer within {PROBE_BUDGET:?} — its \
                 WUDFHost process is wedged; an adapter reload or a reboot clears it"
            );
        }
        let seen = format!("{} active, {} inactive", self.active, self.inactive);
        if self.opened.is_some() {
            return anyhow::anyhow!("pf-vdisplay device interface opened ({seen})");
        }
        match self.last_err {
            Some(e) => e.context(format!("no openable pf-vdisplay device interface ({seen})")),
            None => anyhow::anyhow!(
                "no pf-vdisplay device interface found ({seen}) — is the pf-vdisplay driver \
                 installed and its device started?"
            ),
        }
    }

    fn into_result(mut self) -> Result<(ControlDevice, control::InfoReply)> {
        match self.opened.take() {
            Some(o) => Ok(o),
            None => Err(self.into_error()),
        }
    }
}

/// Open the pf-vdisplay control device. Safe and owning: no caller obligation, close is `Drop`.
fn open_device() -> Result<ControlDevice> {
    probe_device().into_result().map(|(h, _)| h)
}

/// Bound on one open + `IOCTL_GET_INFO`. Both are sub-second; the driver's own silence watchdog
/// is 3 s, so a device that has not answered in 10 s has already departed every monitor and an
/// adapter reload costs nothing.
const PROBE_BUDGET: Duration = Duration::from_secs(10);

/// Probe workers abandoned past their budget and still blocked in the driver. While non-zero,
/// [`probe_device`] reports wedged without spawning another — each would only add a thread that
/// unblocks together with the first, when a reload kills the host process.
static WEDGED_PROBES: AtomicUsize = AtomicUsize::new(0);

/// [`probe_sync`] on a worker, abandoned after [`PROBE_BUDGET`]. `CreateFileW` and
/// `DeviceIoControl` on a hung WUDFHost never return and take no timeout; the calling thread
/// must not be the one that waits.
fn probe_device() -> Probe {
    bounded_probe(PROBE_BUDGET, probe_sync)
}

/// [`probe_device`] with the probe and its budget as parameters, so a test can wedge one.
fn bounded_probe(budget: Duration, probe: fn() -> Probe) -> Probe {
    if WEDGED_PROBES.load(Ordering::SeqCst) > 0 {
        return Probe::wedged();
    }
    // Rendezvous: a `send` after the receiver gave up blocks until `rx` drops, then fails, so
    // the worker's decrement can never precede the increment below.
    let (tx, rx) = std::sync::mpsc::sync_channel(0);
    let spawned = std::thread::Builder::new()
        .name("vdisplay-probe".into())
        .spawn(move || {
            if tx.send(probe()).is_err() {
                WEDGED_PROBES.fetch_sub(1, Ordering::SeqCst);
            }
        });
    if spawned.is_err() {
        return probe(); // no thread to be had — unbounded is still better than no probe
    }
    match rx.recv_timeout(budget) {
        Ok(p) => p,
        Err(_) => {
            WEDGED_PROBES.fetch_add(1, Ordering::SeqCst);
            tracing::error!(
                budget_s = budget.as_secs(),
                "pf-vdisplay control device did not answer — WUDFHost is WEDGED; sessions get \
                 audio and no video until the adapter reloads"
            );
            Probe::wedged()
        }
    }
}

/// [`open_device`], reporting what was found rather than only success.
/// The instance id of the devnode this process last opened a control interface on. A seats box
/// carries several same-named adapters, so a reload must name ours, not the first healthy one.
static LAST_INSTANCE_ID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// `\\?\ROOT#DISPLAY#0000#{guid}` → `ROOT\DISPLAY\0000`: the interface symbolic link carries the
/// instance id with its separators swapped, ahead of the interface class.
fn instance_id_from_path(path: &str) -> Option<String> {
    let rest = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))?;
    let (id, _) = rest.split_once("#{")?;
    Some(id.replace('#', "\\"))
}

/// Enumerate, open the first active interface, and complete the version handshake — one
/// synchronous unit so [`probe_device`] can bound it. "Openable" means the driver answered.
fn probe_sync() -> Probe {
    let mut probe = Probe {
        opened: None,
        active: 0,
        inactive: 0,
        last_err: None,
        wedged: false,
    };
    // SAFETY: SetupAPI enumeration; the returned list is solely owned by the RAII wrapper.
    let hdev = match unsafe {
        SetupDiGetClassDevsW(
            Some(&PF_VDISPLAY_INTERFACE),
            PCWSTR::null(),
            None,
            DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
        )
    }
    .context("SetupDiGetClassDevsW(pf-vdisplay) — is the pf-vdisplay driver installed?")
    {
        Ok(h) => DevInfoList(h),
        Err(e) => {
            probe.last_err = Some(e);
            return probe;
        }
    };

    // Every instance, not index 0: after an upgrade a Code-10 node can sit at 0 while the live
    // interface is later. First `SPINT_ACTIVE` + openable wins.
    for index in 0..64u32 {
        let mut idata = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: `hdev.0` is the live list; `idata` is a valid, size-stamped out-param.
        if unsafe {
            SetupDiEnumDeviceInterfaces(hdev.0, None, &PF_VDISPLAY_INTERFACE, index, &mut idata)
        }
        .is_err()
        {
            break; // ERROR_NO_MORE_ITEMS — no further candidates
        }
        if idata.Flags & SPINT_ACTIVE == 0 {
            probe.inactive += 1;
            continue;
        }
        probe.active += 1;
        let mut required = 0u32;
        // SAFETY: sizing call — null buffer plus a valid `required` out-param; the expected
        // ERROR_INSUFFICIENT_BUFFER "failure" is ignored and only `required` is consumed.
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(hdev.0, &idata, None, 0, Some(&mut required), None)
        };
        // Against the struct's size, not `u32`: `cbSize` below is
        // `size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()`.
        if (required as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
            continue; // sizing failed — never stamp a cbSize through an under-sized buffer
        }
        // `u64`, not `u8`: the buffer is written as `SP_DEVICE_INTERFACE_DETAIL_DATA_W` (4-byte
        // align); `Vec<u8>` only promises 1.
        let mut buf = vec![0u64; (required as usize).div_ceil(size_of::<u64>())];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        // SAFETY: `buf` is ≥ `required` bytes and 8-aligned (so also 4). `detail` aliases `buf`
        // only in this iteration; `DevicePath` is read before `buf` drops. The path is a RAW
        // place projection so it keeps the whole allocation's provenance: `DevicePath` is
        // `[u16; 1]` (FAM stub), so `.as_ptr()` would tag two bytes while `CreateFileW` reads
        // the full NUL-terminated path — everything past `[0]` OOB, and the compiler may fold
        // the zero-init into an empty device name.
        let opened = unsafe {
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(hdev.0, &idata, Some(detail), required, None, None)
                .context("SetupDiGetDeviceInterfaceDetailW(pf-vdisplay)")
                .and_then(|()| {
                    CreateFileW(
                        PCWSTR((&raw const (*detail).DevicePath).cast::<u16>()),
                        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    )
                    .context("CreateFileW(pf-vdisplay device)")
                })
        };
        match opened {
            Ok(h) => {
                // The devnode behind this interface, for a later reload while it is hostless
                // and has no interface left to ask. SAFETY: `detail` still aliases `buf`, and
                // `DevicePath` is the NUL-terminated path the call above filled in.
                let path =
                    unsafe { PCWSTR((&raw const (*detail).DevicePath).cast::<u16>()).to_string() };
                if let Some(id) = path.ok().and_then(|p| instance_id_from_path(&p)) {
                    *LAST_INSTANCE_ID.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                }
                // SAFETY: `h` is the handle `CreateFileW` just returned to this call and nothing
                // else holds it; `OwnedHandle` is the single owner that closes it on drop. It was
                // opened without `FILE_FLAG_OVERLAPPED`, as `ControlDevice` requires.
                let device = ControlDevice(unsafe { OwnedHandle::from_raw_handle(h.0 as _) });
                // A handle that opens but does not answer is a miss like any other: the next
                // interface may be the live one.
                match get_info(&device) {
                    Ok(info) => {
                        probe.opened = Some((device, info));
                        return probe;
                    }
                    Err(e) => probe.last_err = Some(e),
                }
            }
            // Raced-away device — remember the error, try the next interface.
            Err(e) => probe.last_err = Some(e),
        }
    }
    probe
}

/// `IOCTL_GET_INFO` on a freshly opened control handle. Fails closed on a short reply:
/// `protocol_version` gates host behaviour and zeros from an under-written buffer must not pass.
fn get_info(device: &ControlDevice) -> Result<control::InfoReply> {
    let mut info_buf = [0u8; size_of::<control::InfoReply>()];
    let n = ioctl(device, control::IOCTL_GET_INFO, &[], &mut info_buf)
        .context("pf-vdisplay IOCTL_GET_INFO (version handshake)")?;
    if (n as usize) < size_of::<control::InfoReply>() {
        anyhow::bail!(
            "pf-vdisplay IOCTL_GET_INFO returned {n} bytes, expected {}",
            size_of::<control::InfoReply>()
        );
    }
    Ok(bytemuck::pod_read_unaligned(
        &info_buf[..size_of::<control::InfoReply>()],
    ))
}

/// The installed driver speaks a protocol this host cannot drive. Typed so a session can name
/// the remedy — install the matching pair — instead of reporting an IOCTL error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverOutdated {
    pub driver: u32,
    pub host: u32,
}

impl DriverOutdated {
    /// v7 replaced the video transport outright, so the floor equals this host's version: a
    /// v6 driver has no encoder to open and a future one may have moved the section again.
    fn check(driver: u32) -> Option<Self> {
        let host = pf_driver_proto::PROTOCOL_VERSION;
        (driver < pf_driver_proto::MIN_DRIVER_PROTOCOL_VERSION || driver > host)
            .then_some(DriverOutdated { driver, host })
    }
}

impl std::fmt::Display for DriverOutdated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pf-vdisplay driver outdated: the driver speaks protocol {}, this host needs {} — \
             install the matching host + driver (they ship in one installer)",
            self.driver, self.host
        )
    }
}

impl std::error::Error for DriverOutdated {}

/// pf-vdisplay IOCTL surface behind [`VirtualDisplayManager`](super::manager::VirtualDisplayManager).
/// Wire contract: `pf_driver_proto::control` (versioned, hard-checked).
pub(crate) struct PfVdisplayDriver;

impl VdisplayDriver for PfVdisplayDriver {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    fn open(&self, reap_orphans: bool) -> Result<(ControlDevice, u32, u32)> {
        // Brief re-probe, no adapter reload. `ensure_available` already ran; leftover is a race.
        // `hw_cursor_capable` also lands here mid-handshake. Reloading would deadlock:
        // `ensure_device` calls us holding the manager `device` mutex (`RECOVERY` lock order).
        // The probe already ran `IOCTL_GET_INFO`; a wedged host is an error here, never a wait.
        // Owned from the probe on, so every `?` below closes the device.
        let (device, info) = wait_for_interface(BRIEF_RETRY, false).0?;
        // Hard version check: a mismatch must not proceed to corrupt the IOCTL stream.
        if let Some(outdated) = DriverOutdated::check(info.protocol_version) {
            return Err(anyhow::Error::new(outdated));
        }
        let watchdog_s = info.watchdog_timeout_s.max(1);
        // Only log of the negotiated watchdog; pinger cadence is `watchdog/3`.
        tracing::info!(
            "pf-vdisplay protocol {} (watchdog timeout {}s)",
            info.protocol_version,
            watchdog_s
        );
        // CLEAR_ALL needs sole ownership of the device. A reopen races sessions this process
        // still believes live, and under a seats reservation another host owns monitors here.
        if !reap_orphans {
            reap_ghost_monitors();
            return Ok((device, watchdog_s, info.protocol_version));
        }
        if ioctl_send(&device, control::IOCTL_CLEAR_ALL, &[]).is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        } else {
            tracing::warn!("pf-vdisplay IOCTL_CLEAR_ALL failed on startup (continuing)");
        }
        // CLEAR_ALL cannot remove OS-side not-present "Generic Monitor (Punktfunk)" PDOs.
        // Reap those so a restart starts with a clean IddCx slot budget.
        reap_ghost_monitors();
        Ok((device, watchdog_s, info.protocol_version))
    }

    fn add_monitor(
        &self,
        dev: &ControlDevice,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
        client_hdr: Option<pf_frame::HdrMeta>,
        hw_cursor: bool,
    ) -> Result<AddedMonitor> {
        let session_id = next_session_id();
        // EDID CTA HDR block; all-zero = unknown → driver defaults (also what a driver that
        // reads only the legacy 24-byte prefix does).
        let (max_luminance_nits, max_frame_avg_nits, min_luminance_millinits) = client_hdr
            .map(|m| pf_frame::hdr::vdisplay_luminance_fields(&m))
            .unwrap_or((0, 0, 0));
        if max_luminance_nits > 0 {
            tracing::info!(
                max_luminance_nits,
                max_frame_avg_nits,
                min_luminance_millinits,
                "pf-vdisplay ADD: advertising the client display's HDR volume in the monitor EDID"
            );
        }
        let add = control::AddRequest {
            session_id,
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            preferred_monitor_id,
            max_luminance_nits,
            max_frame_avg_nits,
            min_luminance_millinits,
            // v5: driver declares an IddCx hardware cursor (DWM stops compositing the pointer).
            // Zero toward older drivers is harmless — host sets this only when proto ≥ 5.
            hw_cursor: hw_cursor as u32,
        };
        // Opt-in; non-fatal: the driver reports the real render LUID in the shared header.
        if let Some(luid) = render_luid {
            match set_render_adapter(dev, luid) {
                Ok(()) => tracing::info!(
                    luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "pf-vdisplay SET_RENDER_ADAPTER: pinned IDD render GPU"
                ),
                Err(e) => tracing::warn!(
                    "pf-vdisplay SET_RENDER_ADAPTER failed (continuing on the natural adapter): {e:#}"
                ),
            }
        }
        let mut out = [0u8; size_of::<control::AddReply>()];
        let add_res = ioctl(dev, control::IOCTL_ADD, bytemuck::bytes_of(&add), &mut out);
        let add_res = match add_res {
            Err(e) if is_slot_exhaustion_wedge(&e) => {
                // Ghost PDOs exhausted the IddCx slot pool (0x80070490). Reap and retry so the
                // wedge self-heals instead of hard-failing every session.
                let reaped = reap_ghost_monitors();
                tracing::warn!(
                    reaped,
                    "pf-vdisplay ADD wedged (0x80070490 ERROR_NOT_FOUND) — reaped ghost monitor nodes, retrying ADD"
                );
                // pnputil is durable; VidPN slot reclaim is async and can lag the return.
                // 5 × 300 ms, no re-reap (~1.5 s worst case, wedge path only).
                let mut res = Err(anyhow::anyhow!("pf-vdisplay ADD retry loop did not run"));
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    res = ioctl(dev, control::IOCTL_ADD, bytemuck::bytes_of(&add), &mut out);
                    if res.is_ok() {
                        break;
                    }
                }
                res
            }
            other => other,
        };
        let n = add_res.with_context(|| {
            format!(
                "pf-vdisplay ADD {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })?;
        // Fail closed on a short reply — `target_id`/`wudf_pid`/`luid` feed OpenProcess.
        // Legacy size, not the full struct: an old driver writes only the prefix; `out` is
        // zeroed so the missing `cursor_excluded` tail reads 0 (unknown/clean).
        if (n as usize) < control::ADD_REPLY_LEGACY_SIZE {
            // IOCTL succeeded: the driver already created the monitor and took a slot. Bailing
            // without REMOVE leaks it; ~16 leaks wedge later ADDs at 0x80070490.
            let req = control::RemoveRequest { session_id };
            let undo = ioctl_send(dev, control::IOCTL_REMOVE, bytemuck::bytes_of(&req));
            match undo {
                Ok(_) => tracing::warn!(
                    session_id,
                    "pf-vdisplay ADD returned a short reply — removed the monitor it had already \
                     created so its IddCx slot is not leaked"
                ),
                Err(e) => tracing::error!(
                    session_id,
                    error = %format!("{e:#}"),
                    "pf-vdisplay ADD returned a short reply AND the compensating REMOVE failed — \
                     this monitor's IddCx slot is leaked until the driver is cycled"
                ),
            }
            anyhow::bail!(
                "pf-vdisplay ADD returned {n} bytes, expected at least {}",
                control::ADD_REPLY_LEGACY_SIZE
            );
        }
        // `pod_read_unaligned`, not `from_bytes`: `out` has no 4-byte alignment guarantee.
        let reply: control::AddReply =
            bytemuck::pod_read_unaligned(&out[..size_of::<control::AddReply>()]);
        let luid = LUID {
            LowPart: reply.adapter_luid_low,
            HighPart: reply.adapter_luid_high,
        };
        tracing::info!(
            target_id = reply.target_id,
            adapter_luid = %format_args!("{:#x}", luid.LowPart),
            wudf_pid = reply.wudf_pid,
            cursor_excluded = reply.cursor_excluded != 0,
            "pf-vdisplay monitor created {}x{}@{}",
            mode.width,
            mode.height,
            mode.refresh_hz
        );
        // Did the driver honor the preferred (stable) monitor id? 0 = ignored; a mismatch
        // means Windows will not reapply this client's saved per-monitor config this session.
        if preferred_monitor_id != 0 {
            if reply.resolved_monitor_id == preferred_monitor_id {
                tracing::info!(
                    monitor_id = preferred_monitor_id,
                    "pf-vdisplay: per-client monitor id honored (stable identity → saved config persists)"
                );
            } else {
                tracing::warn!(
                    preferred = preferred_monitor_id,
                    resolved = reply.resolved_monitor_id,
                    "pf-vdisplay: preferred monitor id NOT honored (live-id collision, or a pre-Phase-2 \
                     driver) — per-client config persistence degraded to auto identity this session"
                );
            }
        }
        // `reply.adapter_luid` is the IddCx *display* adapter (`OsAdapterLuid`), not the
        // render GPU — it cannot validate SET_RENDER_ADAPTER. Real render LUID is in the
        // shared frame header; the IDD-push capturer rebinds on a mismatch.
        Ok(AddedMonitor {
            key: MonitorKey::Session(session_id),
            target_id: reply.target_id,
            luid,
            wudf_pid: reply.wudf_pid,
            resolved_monitor_id: reply.resolved_monitor_id,
            cursor_excluded: reply.cursor_excluded != 0,
        })
    }

    fn update_modes(&self, dev: &ControlDevice, key: &MonitorKey, mode: Mode) -> Result<()> {
        let MonitorKey::Session(session_id) = key else {
            anyhow::bail!("pf-vdisplay: unexpected monitor key kind");
        };
        let req = control::UpdateModesRequest {
            session_id: *session_id,
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            _reserved: 0,
        };
        ioctl_send(dev, control::IOCTL_UPDATE_MODES, bytemuck::bytes_of(&req)).with_context(|| {
            format!(
                "pf-vdisplay UPDATE_MODES {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })
    }

    fn remove_monitor(&self, dev: &ControlDevice, key: &MonitorKey) -> Result<()> {
        let MonitorKey::Session(session_id) = key else {
            anyhow::bail!("pf-vdisplay: unexpected monitor key kind");
        };
        let req = control::RemoveRequest {
            session_id: *session_id,
        };
        ioctl_send(dev, control::IOCTL_REMOVE, bytemuck::bytes_of(&req))
    }

    fn ping(&self, dev: &ControlDevice) -> Result<()> {
        ioctl_send(dev, control::IOCTL_PING, &[])
    }

    fn drain_log(&self, dev: &ControlDevice) {
        drain_driver_log(dev);
    }
}

/// Windows pf-vdisplay backend. Lifecycle lives in
/// [`VirtualDisplayManager`](super::manager::VirtualDisplayManager); this only carries the
/// connecting client's fingerprint so the manager can assign a stable per-client monitor id.
pub struct PfVdisplayDisplay {
    /// Connecting client's cert fingerprint (`None` = anonymous/GameStream → auto id).
    client_fp: Option<[u8; 32]>,
    /// Client HDR volume (`None` = unknown/SDR → driver EDID defaults). Advertised in the
    /// created monitor's EDID so host apps tone-map to the client's panel.
    client_hdr: Option<pf_frame::HdrMeta>,
    /// Declare an IddCx hardware cursor. Honored only when the handshake reported proto ≥ 5.
    hw_cursor: bool,
    /// Deliberate-quit flag (`None` = linger policy). A user "stop" tears the monitor down
    /// immediately instead of lingering.
    quit: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// `mode_conflict: join` admitted this session: share the live display it named.
    join_live: bool,
}

impl PfVdisplayDisplay {
    pub fn new() -> Result<Self> {
        super::manager::init(Box::new(PfVdisplayDriver)).open_backend()?;
        Ok(Self {
            client_fp: None,
            client_hdr: None,
            hw_cursor: false,
            quit: None,
            join_live: false,
        })
    }
}

impl VirtualDisplay for PfVdisplayDisplay {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn set_client_hdr(&mut self, hdr: Option<pf_frame::HdrMeta>) {
        self.client_hdr = hdr;
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn set_quit_flag(&mut self, quit: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.quit = Some(quit);
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    /// A joiner gets a reference on the display it was admitted to, and no display of its own.
    /// With nothing left to join it creates, as every other backend does.
    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        if self.join_live {
            if let Some(shared) = super::manager::vdm().join(mode, self.client_fp) {
                return Ok(shared);
            }
        }
        super::manager::vdm().acquire(
            mode,
            self.client_fp,
            self.client_hdr,
            self.hw_cursor,
            self.quit.clone(),
        )
    }
}

pub fn probe() -> Result<()> {
    open_device().map(|_| ())
}

/// One bounded probe for the diagnostics row: no reload, no lock, nothing cached. A wedged
/// host answers within [`PROBE_BUDGET`] the first time and at once after. No interface on an
/// [`adapter_installed`] box reads `Installed`: the next connect starts or reloads it.
pub fn health() -> crate::DriverHealth {
    use crate::DriverHealth;
    let probe = probe_device();
    if probe.wedged {
        return DriverHealth::Wedged;
    }
    if let Some((_, info)) = &probe.opened {
        return match DriverOutdated::check(info.protocol_version) {
            Some(DriverOutdated { driver, host }) => DriverHealth::Outdated { driver, host },
            None => DriverHealth::Ok {
                protocol: info.protocol_version,
            },
        };
    }
    if probe.is_absent() {
        return if adapter_installed() {
            DriverHealth::Installed
        } else {
            DriverHealth::Absent
        };
    }
    DriverHealth::NotReady {
        detail: format!("{:#}", probe.into_error()),
    }
}

/// A present Display devnode with a `pf_vdisplay` hardware ID and a driver bound: installed,
/// whether or not its control interface is up. A node without a driver (Code 28) is not.
fn adapter_installed() -> bool {
    // SAFETY: SetupAPI enumeration; the returned list is solely owned by the RAII wrapper.
    let Ok(set) = (unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )
    }) else {
        return false;
    };
    let set = DevInfoList(set);
    let prop = |did: &SP_DEVINFO_DATA, prop: SETUP_DI_REGISTRY_PROPERTY| {
        let mut buf = [0u8; 1024];
        let mut req = 0u32;
        // SAFETY: live set and element; the buffer length travels with the slice.
        unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                did,
                prop,
                None,
                Some(&mut buf),
                Some(&mut req),
            )
        }
        .ok()?;
        let units: Vec<u16> = buf[..(req as usize).min(buf.len())]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units).to_ascii_lowercase())
    };
    (0..)
        .map_while(|i| {
            let mut did = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: live set; `did` is a valid, size-stamped out-param.
            unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut did) }
                .ok()
                .map(|()| did)
        })
        .any(|did| {
            prop(&did, SPDRP_HARDWAREID).is_some_and(|ids| ids.contains("pf_vdisplay"))
                && prop(&did, SPDRP_DRIVER).is_some()
        })
}

pub fn is_available() -> bool {
    open_device().is_ok()
}

const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// How long a registered-but-not-ready interface may come up before a reload. Wake-from-sleep:
/// D0 re-registers the interface while resume is still running; probing once would
/// disable/enable an adapter that was seconds from ready.
const NOT_READY_GRACE: Duration = Duration::from_secs(15);

/// Absent-interface settle before reload. Short (hostless does not self-heal) but non-zero so
/// a resume that briefly de-registers is not met with device surgery.
const ABSENT_SETTLE: Duration = Duration::from_secs(3);

/// Arrival window after a reload. 15 s: PnP is contended right after wake; 4 s missed it.
const ARRIVAL_AFTER_RELOAD: Duration = Duration::from_secs(15);

/// Ceiling on the whole wait. Without it, not-ready + reload + arrival can approach a minute.
const TOTAL_BUDGET: Duration = Duration::from_secs(30);

/// Budget when the caller must not stall: re-probe only, no reload. [`VdisplayDriver::open`]
/// and `hw_cursor_capable` (a handshake bool) must not hold Welcome for tens of seconds.
const BRIEF_RETRY: Duration = Duration::from_secs(3);

/// Serializes recovery so N racing sessions perform one adapter reload, not N interleaved
/// ones that tear down the stack the others wait on.
///
/// Taken only by [`ensure_available`] (no manager lock). Order is `RECOVERY` → `device`:
/// `invalidate_cached_device` takes `device` while this is held. [`VdisplayDriver::open`]
/// runs *inside* `device` and must never take this lock or the orders invert and deadlock.
static RECOVERY: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`is_available`] with self-heal and patience after wake. Returns the reason on failure
/// rather than a bare `false` (callers must not flatten that into "driver not installed").
pub fn ensure_available() -> Result<()> {
    // Guard is `()`; a poison must not wedge every later session out of recovery.
    let (result, reloaded) = {
        let _serialize = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
        wait_for_interface(NOT_READY_GRACE, true)
    };
    // Reload tore the stack; a handle cached during the arrival window is dead. Usually
    // a no-op: recovery already released the manager's reference so PnP can proceed.
    if reloaded {
        super::manager::invalidate_cached_device(
            "the pf-vdisplay adapter was reloaded (hostless-zombie recovery)",
        );
    }
    result.map(|_| ())
}

/// The `DriverCycle` recovery rung (plan §2.5): reap a dead or blocked WUDFHost and reload the
/// adapter, so the session's rebuild reopens against a fresh host process. Reuses the
/// hostless-zombie reload — no second lever. Never joins the WUDFHost and never opens the
/// control device: [`reload_vdisplay_adapter`] shells out with its own timeouts and
/// `invalidate_cached_device` drops the handle without waiting on a drain, so the control plane
/// stays live through the seconds the display is black. The caller rebuilds the pipeline after.
pub fn force_driver_cycle() -> Result<()> {
    let _serialize = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
    // Release our own control handle first: an open handle vetoes the PnP disable/restart.
    super::manager::invalidate_cached_device(
        "driver cycle: releasing the control handle before the adapter reload",
    );
    match reload_vdisplay_adapter() {
        AdapterCycle::Reloaded { .. } => Ok(()),
        AdapterCycle::NotInstalled => {
            anyhow::bail!(
                "driver cycle: no pf-vdisplay adapter devnode — the driver is not installed"
            )
        }
        AdapterCycle::Refused(why) => {
            anyhow::bail!("driver cycle: adapter devnode reload refused ({why})")
        }
    }
}

/// [`force_driver_cycle`] for a session still building its pipeline. Refused while another
/// session holds a virtual monitor, since the reload blanks every one; the caller's own
/// retry-hold lease is the one reference allowed.
pub fn force_driver_cycle_if_sole() -> Result<()> {
    let refs: u32 = super::manager::snapshot()
        .iter()
        .filter(|s| s.state == "active")
        .map(|s| s.sessions)
        .sum();
    anyhow::ensure!(
        refs <= 1,
        "driver cycle skipped: another session holds a virtual monitor"
    );
    force_driver_cycle()
}

/// Wait for a control interface that answers; reload if `reload` and the devnode looks hostless
/// or wedged. Returns the handle with its handshake reply (so the manager's open can keep both)
/// and whether a reload ran.
///
/// Three states want different treatment:
/// * **Not ready** — instances registered, none active (or open refused). The devnode is
///   coming up; reloading lengthens the outage.
/// * **Absent** — no instance. Hostless WUDFHost crash; only a reload clears it.
/// * **Wedged** — the interface opens but the host never answers. Waiting cannot help; the
///   reload kills the host process, which fails the blocked request and clears the state.
///
/// Probe, wait out not-ready, reload absent after [`ABSENT_SETTLE`] or wedged at once, then
/// [`ARRIVAL_AFTER_RELOAD`]. A reload is still attempted at the end of `not_ready_grace`
/// (wedged not-ready). No adapter devnode fails immediately.
fn wait_for_interface(
    not_ready_grace: Duration,
    reload: bool,
) -> (Result<(ControlDevice, control::InfoReply)>, bool) {
    let started = Instant::now();
    let mut deadline = started + not_ready_grace;
    let mut absent_since: Option<Instant> = None;
    let mut reloaded = false;
    loop {
        let mut probe = probe_device();
        if let Some(opened) = probe.opened.take() {
            if reloaded || started.elapsed() > PROBE_INTERVAL {
                tracing::info!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    reloaded,
                    "pf-vdisplay: control interface available"
                );
            }
            return (Ok(opened), reloaded);
        }
        // A wedge is not waited out. Without the reload lever there is nothing to do but say so.
        if probe.wedged && !reload {
            return (Err(probe.into_error()), reloaded);
        }
        // Reset by any sighting: flicker between absent and not-ready is a transition.
        if probe.is_absent() {
            if absent_since.is_none() && reload {
                // Drop the manager's control-handle ref now so `ABSENT_SETTLE` drains outstanding
                // `Arc` clones; an open handle vetoes PnP disable/restart. Gated on `reload`:
                // `BRIEF_RETRY` runs inside the `device` mutex (taking it again deadlocks) and
                // never reloads.
                super::manager::invalidate_cached_device(
                    "control interface absent — releasing the host's own device handle ahead of a \
                     possible adapter reload",
                );
            }
            absent_since.get_or_insert_with(Instant::now);
        } else {
            absent_since = None;
        }
        let absent_long_enough = absent_since.is_some_and(|t| t.elapsed() >= ABSENT_SETTLE);
        if reload && !reloaded && (probe.wedged || absent_long_enough || Instant::now() >= deadline)
        {
            // Not-ready never took the absent-sighting release; drop the ref now (idempotent).
            super::manager::invalidate_cached_device(
                "adapter reload imminent — releasing the host's own device handle (open handles \
                 veto the PnP cycle)",
            );
            match reload_vdisplay_adapter() {
                // No adapter at all — waiting cannot conjure a driver.
                AdapterCycle::NotInstalled => {
                    let e = Err(probe.into_error()).context(
                        "no punktfunk virtual-display adapter devnode exists — the driver is not \
                         installed",
                    );
                    return (e, reloaded);
                }
                AdapterCycle::Refused(why) => {
                    let e = Err(probe.into_error()).context(format!(
                        "pf-vdisplay adapter devnode reload refused ({why})"
                    ));
                    return (e, reloaded);
                }
                AdapterCycle::Reloaded { .. } => {
                    reloaded = true;
                    absent_since = None;
                    deadline = (Instant::now() + ARRIVAL_AFTER_RELOAD).min(started + TOTAL_BUDGET);
                }
            }
        }
        if Instant::now() >= deadline {
            let e = Err(probe.into_error()).context(format!(
                "the pf-vdisplay control interface did not appear within {:?}{}",
                started.elapsed(),
                if reloaded {
                    " (including an adapter reload)"
                } else {
                    ""
                }
            ));
            return (e, reloaded);
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// A probe past its budget is reported wedged and later probes fail fast without a second
    /// worker; the count clears when the abandoned worker returns, and probing resumes. A count
    /// stuck at one would fail every session forever after one hang.
    #[test]
    fn an_abandoned_probe_fails_fast_until_it_returns() {
        use std::sync::atomic::AtomicBool;
        static RELEASE: AtomicBool = AtomicBool::new(false);
        fn stuck() -> Probe {
            while !RELEASE.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            Probe {
                opened: None,
                active: 1,
                inactive: 0,
                last_err: None,
                wedged: false,
            }
        }
        let budget = Duration::from_millis(50);
        let started = Instant::now();
        assert!(bounded_probe(budget, stuck).wedged);
        assert!(started.elapsed() >= budget);
        let started = Instant::now();
        assert!(
            bounded_probe(budget, stuck).wedged,
            "must fail fast while one is abandoned"
        );
        assert!(
            started.elapsed() < budget,
            "a fail-fast probe must not wait out a budget"
        );
        assert_eq!(WEDGED_PROBES.load(Ordering::SeqCst), 1);

        RELEASE.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while WEDGED_PROBES.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            WEDGED_PROBES.load(Ordering::SeqCst),
            0,
            "the returned worker must clear it"
        );
        let probe = bounded_probe(budget, stuck);
        assert!(!probe.wedged && probe.active == 1);
    }

    /// Every ADD needs its own key: the preempt path adds after a removal that
    /// may not have landed, and a reused key would depart the incumbent.
    #[test]
    fn session_ids_are_fresh_per_add() {
        let a = next_session_id();
        let b = next_session_id();
        assert_ne!(a, b);
        assert!(b > a);
    }

    /// A refusal must decode as a refusal, carrying its reason. PnP Status after a refused
    /// disable still reads `OK` and must never decode as `Reloaded`.
    #[test]
    fn a_refused_reload_is_not_reported_as_a_reload() {
        let refused =
            classify_reload_output("REFUSED This device cannot be disabled because it is in use.");
        match refused {
            AdapterCycle::Refused(why) => {
                assert!(why.contains("in use"), "the reason must survive: {why:?}")
            }
            other => panic!("a refused reload decoded as {}", variant(&other)),
        }
        // A bare device status must never decode as a reload.
        for stale in ["OK", "Error", "Unknown"] {
            assert!(
                matches!(classify_reload_output(stale), AdapterCycle::Refused(_)),
                "{stale:?} is a device status, not a reload outcome"
            );
        }
    }

    /// A refusal must carry its evidence (counts, Status, problem code, restart exit) and a
    /// phantom-only state must decode as refused — reload cannot revive a gone device.
    #[test]
    fn a_refusal_keeps_its_evidence() {
        let why = match classify_reload_output(
            "REFUSED devnodes=2 live=1 status=OK problem=0 restart_exit=3010 Generic failure",
        ) {
            AdapterCycle::Refused(why) => why,
            other => panic!("expected Refused, got {}", variant(&other)),
        };
        for token in ["devnodes=2", "live=1", "status=OK", "restart_exit=3010"] {
            assert!(why.contains(token), "{token} must survive: {why:?}");
        }
        assert!(matches!(
            classify_reload_output(
                "REFUSED only phantom (not-present) adapter devnodes remain (2) - the device node \
                 itself is gone and no reload can revive it; reinstalling the host re-creates it"
            ),
            AdapterCycle::Refused(why) if why.contains("phantom")
        ));
    }

    /// `NotInstalled` fails fast, `Reloaded` earns the arrival window, and `restart` means
    /// disable was refused (something still holds the device open).
    #[test]
    fn reload_outcomes_decode() {
        assert!(matches!(
            classify_reload_output("ABSENT"),
            AdapterCycle::NotInstalled
        ));
        match classify_reload_output("RELOADED cycle OK") {
            AdapterCycle::Reloaded { how, status } => {
                assert_eq!(how, "disable+enable");
                assert_eq!(status, "OK");
            }
            other => panic!("expected Reloaded, got {}", variant(&other)),
        }
        match classify_reload_output("RELOADED restart OK\r\n") {
            AdapterCycle::Reloaded { how, status } => {
                assert_eq!(how, "pnputil /restart-device");
                assert_eq!(status, "OK");
            }
            other => panic!("expected Reloaded, got {}", variant(&other)),
        }
        // Empty stdout: un-reloaded, so `Refused`, not a silent success.
        assert!(matches!(
            classify_reload_output("   "),
            AdapterCycle::Refused(_)
        ));
    }

    /// Found and removed travel separately so a leftover ghost is loud. A single number or
    /// a script that died before reporting must not decode as a reap.
    #[test]
    fn reap_output_decodes_found_and_removed() {
        assert_eq!(parse_reap_output("3 3\r\n"), Some((3, 3)));
        assert_eq!(
            parse_reap_output("4 0"),
            Some((4, 0)),
            "pnputil unlaunchable"
        );
        assert_eq!(parse_reap_output("0 0"), Some((0, 0)), "clean box");
        for dead in ["5", "", "   ", "garbage", "OK"] {
            assert_eq!(
                parse_reap_output(dead),
                None,
                "{dead:?} is not a reap report"
            );
        }
    }

    /// `is_absent` is what decides wait vs. surgery. Registered-but-inactive is mid-transition
    /// (wake); reloading under it lengthens the outage.
    #[test]
    fn only_a_total_absence_counts_as_absent() {
        let probe = |active, inactive| Probe {
            opened: None,
            active,
            inactive,
            last_err: None,
            wedged: false,
        };
        assert!(probe(0, 0).is_absent(), "no instances at all = absent");
        assert!(
            !Probe::wedged().is_absent(),
            "a wedged host saw nothing, yet it is not a missing device"
        );
        assert!(
            !probe(0, 1).is_absent(),
            "a registered-but-inactive instance is a device coming up, not a missing one"
        );
        assert!(
            !probe(1, 0).is_absent(),
            "an active instance we merely failed to open is not a missing device"
        );
        // Diagnostic names what was seen — collapsing these into "is the driver installed?"
        // sends recovery down the wrong path.
        assert!(probe(0, 2).into_error().to_string().contains("2 inactive"));
    }

    fn variant(c: &AdapterCycle) -> &'static str {
        match c {
            AdapterCycle::Reloaded { .. } => "Reloaded",
            AdapterCycle::NotInstalled => "NotInstalled",
            AdapterCycle::Refused(_) => "Refused",
        }
    }

    /// Hardware round trip (`#[ignore]`): open → create → hold → drop (REMOVE). Under the
    /// guard so the drop tears down NOW: with the box's real keep-alive the teardown lingers
    /// 10 s and the next case in the file starts on a still-isolated desktop (measured on .173:
    /// `live_force_extend` red on its precondition, the desktop restored seconds later).
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_create_drop() {
        let _policy = ExclusiveTopology::force();
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        assert_eq!(vout.preferred_mode, Some((1920, 1080, 60)));
        thread::sleep(Duration::from_secs(3));
        drop(vout); // REMOVE + stop the pinger
    }

    /// Spike S5 (`#[ignore]`): arm the driver's in-process encode probe on a fresh 1080p60
    /// monitor and read its tally. Needs a driver built with `--features encode-probe`.
    /// `PF_PROBE_BACKEND` (nvenc|amf|qsv|pyrowave|mf), `PF_PROBE_CODEC` (h264|hevc|av1|pyrowave),
    /// `PF_PROBE_INPUT` (default|nv12), `PF_PROBE_FRAMES` (300) pick the run.
    /// `PF_PROBE_HDR=1` takes the 10-bit PQ input and `PF_PROBE_444=1` the full-chroma one. The
    /// run puts the virtual display into the colour mode its depth needs and prints what stuck —
    /// a probe fed the wrong surface format fails at `fmt` rather than encoding something else.
    #[test]
    #[ignore = "needs an encode-probe pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_encode_probe() {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let backend = env("PF_PROBE_BACKEND", "nvenc");
        let codec = env("PF_PROBE_CODEC", "hevc");
        let input = env("PF_PROBE_INPUT", "default");
        let want_hdr = env("PF_PROBE_HDR", "0") == "1";
        let want_444 = env("PF_PROBE_444", "0") == "1";
        let frames: u32 = env("PF_PROBE_FRAMES", "300")
            .parse()
            .expect("PF_PROBE_FRAMES");
        let id = |names: &[&str], v: &str, what: &str| -> u32 {
            let i = names.iter().position(|n| *n == v);
            i.unwrap_or_else(|| panic!("{what}={v:?} is not one of {names:?}")) as u32 + 1
        };
        let req = control::EncodeProbeRequest {
            target_id: 0,
            backend: id(
                &["nvenc", "amf", "qsv", "pyrowave", "mf"],
                &backend,
                "PF_PROBE_BACKEND",
            ),
            codec: id(
                &["h264", "hevc", "av1", "pyrowave"],
                &codec,
                "PF_PROBE_CODEC",
            ),
            input: match input.as_str() {
                "default" => 0,
                "nv12" => 1,
                other => panic!("PF_PROBE_INPUT={other:?} is not default|nv12"),
            },
            frames,
            bitrate_kbps: 20_000,
            fps: 60,
            flags: (if want_hdr { control::PROBE_FLAG_HDR } else { 0 })
                | (if want_444 { control::PROBE_FLAG_444 } else { 0 }),
        };

        let _policy = ExclusiveTopology::force();
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        let wc = vout.win_capture.as_ref().expect("no capture target");
        let target_id = wc.target_id;
        // The probe converts from DWM's surface as it comes, so the run's depth decides the
        // display's colour mode: FP16 under advanced colour, BGRA without it. The host service is
        // stopped for this test, so nobody else sets it — set it here and report what stuck.
        let key = pf_win_display::win_display::CcdTargetKey::new(wc.adapter_luid, target_id);
        let set_ok = pf_win_display::win_display::set_advanced_color(key, want_hdr);
        thread::sleep(Duration::from_millis(750));
        let color_on = pf_win_display::win_display::advanced_color_enabled(key);
        println!("probe hdr-state: want={want_hdr} set_ok={set_ok} enabled={color_on:?}");
        // The swap-chain assign and the first composes settle, as the other live cases wait.
        thread::sleep(Duration::from_secs(3));
        let req = control::EncodeProbeRequest { target_id, ..req };
        let dev = open_device().expect("open the pf-vdisplay control device");
        ioctl_send(
            &dev,
            control::IOCTL_ENCODE_PROBE_ARM,
            bytemuck::bytes_of(&req),
        )
        .expect("IOCTL_ENCODE_PROBE_ARM — is the driver built with --features encode-probe?");

        // DWM presents only what something dirties: a 1 px pointer wiggle keeps frames flowing
        // on the (isolated, so pointer-holding) virtual display.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let jiggle = {
            let stop = stop.clone();
            thread::spawn(move || {
                use windows::Win32::Foundation::POINT;
                use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
                let mut p = POINT::default();
                // SAFETY: `p` is a valid out-param.
                if unsafe { GetCursorPos(&mut p) }.is_err() {
                    return;
                }
                let mut flip = false;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    flip = !flip;
                    // SAFETY: plain integer coordinates, restored on the next flip.
                    let _ = unsafe { SetCursorPos(p.x + i32::from(flip), p.y) };
                    thread::sleep(Duration::from_millis(4));
                }
                // SAFETY: restores the position observed above.
                let _ = unsafe { SetCursorPos(p.x, p.y) };
            })
        };
        let started = Instant::now();
        let reply = loop {
            thread::sleep(Duration::from_millis(100));
            let mut out = [0u8; size_of::<control::EncodeProbeReply>()];
            let n = ioctl(&dev, control::IOCTL_ENCODE_PROBE_STATUS, &[], &mut out)
                .expect("IOCTL_ENCODE_PROBE_STATUS");
            assert_eq!(n as usize, out.len(), "short STATUS reply");
            let r: control::EncodeProbeReply = bytemuck::pod_read_unaligned(&out);
            if r.state >= 3 || started.elapsed() > Duration::from_secs(60) {
                break r;
            }
        };
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = jiggle.join();
        let name = String::from_utf8_lossy(&reply.name)
            .trim_end_matches('\0')
            .to_string();
        println!(
            "encode probe: backend={backend} codec={codec} input={input} flags={:#x} state={} frames={} aus={} \
             bytes={} open_us={} first_au_us={} mean_us={} max_us={} drops={} error={} name={name}",
            req.flags,
            reply.state,
            reply.frames_submitted,
            reply.aus,
            reply.bytes,
            reply.open_us,
            reply.first_au_us,
            reply.mean_submit_to_au_us,
            reply.max_submit_to_au_us,
            reply.drops,
            reply.error
        );
        // WUDFHost's temp dir — `C:\Windows\Temp` under the SCM, the LocalService profile otherwise.
        let file = format!("pfvd-probe-{backend}-{codec}.bin");
        for dir in [
            r"C:\Windows\Temp",
            r"C:\Windows\ServiceProfiles\LocalService\AppData\Local\Temp",
        ] {
            let path = format!(r"{dir}\{file}");
            match std::fs::metadata(&path) {
                Ok(m) => println!("encode probe: output {path} ({} bytes)", m.len()),
                Err(_) => println!("encode probe: no output at {path}"),
            }
        }
        drop(vout); // REMOVE before the asserts so a red run never leaks the monitor
        assert_eq!(
            reply.state, 3,
            "probe did not finish: state={} error={} name={name}",
            reply.state, reply.error
        );
        assert!(
            reply.aus + reply.drops + 2 >= frames,
            "aus={} < frames({frames}) - drops({}) - 2",
            reply.aus,
            reply.drops
        );
    }

    /// Forces `Topology::Exclusive` **and `KeepAlive::Off`** for the duration of a case and puts
    /// the operator's real policy back on drop — including when the case panics.
    ///
    /// The isolate branch this file's Phase-3 cases exercise runs ONLY under `Exclusive`, and a
    /// real install is usually configured otherwise (`topology_action(client)` returns
    /// `effective_topology(client)` as soon as ANY policy is configured). `KeepAlive::Off` is equally
    /// load-bearing: every post-teardown assertion here needs the lease drop to actually tear the
    /// monitor down — under the default 10 s linger the probe races the reaper, and under the
    /// gaming-rig `forever` the group restore never runs at all (measured on .173: both members
    /// PINNED, panel left dark, test red for a policy reason). Note this writes the host's
    /// `display-settings.json`; the guard is what makes that safe to do on a real box.
    struct ExclusiveTopology(crate::policy::DisplayPolicy);

    impl ExclusiveTopology {
        fn force() -> Self {
            let original = crate::policy::prefs().get();
            let mut forced = original.clone();
            forced.preset = crate::policy::Preset::Custom; // explicit fields are ignored otherwise
            forced.topology = crate::policy::Topology::Exclusive;
            forced.keep_alive = crate::policy::KeepAlive::Off;
            crate::policy::prefs()
                .set(forced)
                .expect("force Topology::Exclusive + KeepAlive::Off for this case");
            assert_eq!(
                crate::effective_topology(None),
                crate::policy::Topology::Exclusive,
                "the forced policy did not resolve to Exclusive"
            );
            Self(original)
        }
    }

    impl Drop for ExclusiveTopology {
        fn drop(&mut self) {
            if let Err(e) = crate::policy::prefs().set(self.0.clone()) {
                eprintln!("WARNING: display policy not restored: {e}");
            }
        }
    }

    /// Run `f` on a worker and give up after `budget`. A hang inside `create` skipped every
    /// `Drop` and leaked IddCx slots; a bounded wait lets the harness exit so the driver can reap.
    fn within<T: Send + 'static>(
        budget: Duration,
        what: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(budget) {
            Ok(v) => v,
            Err(_) => panic!(
                "{what} did not finish within {budget:?} — failing rather than hanging, so the \
                 harness can exit and the driver can reap. Check for a leaked punktfunk monitor \
                 before the next run."
            ),
        }
    }

    /// When the first member's isolate fails, a later member's isolate must be adopted as the
    /// group's restore snapshot — otherwise it deactivates the operator's panels with nothing
    /// able to put them back. `FAIL_NEXT_ISOLATES` fails the first isolate on real hardware.
    ///
    /// After both members tear down, the operator's external panel must be active again.
    /// Two members need two distinct client fingerprints (`slot_id_for` keys on them).
    /// Needs `Topology::Exclusive`. If the desk stays dark, recover from the console with
    /// `SetDisplayConfig(… SDC_USE_DATABASE_CURRENT|SDC_APPLY)`; `SDC_TOPOLOGY_EXTEND`
    /// will not do it with a single connected display (rc=31).
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_a_failed_first_isolate_is_recovered_by_adopting_the_next() {
        // Tracing is the only account of the adoption arm / dark-desk backstop; a bare harness
        // has no subscriber.
        init_test_tracing();
        assert!(
            std::env::var("PUNKTFUNK_NO_ISOLATE").is_err(),
            "PUNKTFUNK_NO_ISOLATE forces Topology::Extend — this case needs Exclusive"
        );
        let _topology = ExclusiveTopology::force();
        let physicals_before = active_physicals();
        assert!(
            !physicals_before.is_empty(),
            "no external physical panel is active, so 'the panel came back' cannot be observed — \
             power the display on first (a TV in standby reads as Code 45 / zero CCD paths)"
        );
        println!("physicals before          : {physicals_before:?}");

        super::super::manager::FAIL_NEXT_ISOLATES.store(1, std::sync::atomic::Ordering::Relaxed);

        let mut vd1 = PfVdisplayDisplay::new().expect("open pf-vdisplay (member 1)");
        vd1.set_client_identity(Some([0xA1; 32]));
        let out1 = vd1
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create member 1");
        thread::sleep(Duration::from_secs(2));
        // Member 1's isolate was injected to fail. If the panel is already dark here, IddCx
        // auto-activation poisoned member 2's snapshot at birth. If still lit, the break is
        // downstream (adoption never fired, or restore/backstop could not re-light).
        let physicals_after_m1 = active_physicals();
        println!(
            "after member 1 (isolate INJECTED to fail): {:?}",
            active_targets()
        );
        println!("physicals after member 1  : {physicals_after_m1:?}  <- poisoned-at-birth probe");

        let mut vd2 = PfVdisplayDisplay::new().expect("open pf-vdisplay (member 2)");
        vd2.set_client_identity(Some([0xB2; 32]));
        let out2 = vd2
            .create(Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            })
            .expect("create member 2");
        thread::sleep(Duration::from_secs(2));
        let during = active_physicals();
        println!(
            "after member 2 (isolate REAL)            : {:?}",
            active_targets()
        );
        println!("physicals during                        : {during:?}");

        // Seam must have been consumed — otherwise no isolate ran and a pass proves nothing.
        assert_eq!(
            super::super::manager::FAIL_NEXT_ISOLATES.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the injected isolate failure was never consumed — no isolate ran, so this run proves \
             nothing (is the topology really Exclusive?)"
        );

        drop(out2);
        drop(out1);
        // Bounded poll, not a fixed sleep: the reaper tick + restore + async PnP removal stack up
        // to a box-dependent settle (the old 6 s undershot the default linger outright).
        let physicals_after = wait_for_physicals(Duration::from_secs(20)).unwrap_or_default();
        println!("physicals after teardown  : {physicals_after:?}");
        assert!(
            !physicals_after.is_empty(),
            "the operator's physical panel was left DEACTIVATED after teardown (sweep §5 3.2). \
             Active targets now: {:?}.\n\
             Which candidate this run implicates — read it off the poisoned-at-birth probe above:\n\
             * physicals after member 1 was EMPTY ({m1_empty}) -> the snapshot member 2 adopted was \
             already poisoned: the panel went dark at member 1's create (IddCx auto-activation), so \
             the adopted topology records 'panel off' and restoring it faithfully restores darkness. \
             Adoption is working; the SNAPSHOT SOURCE is the defect.\n\
             * physicals after member 1 was NON-empty -> poisoning is excluded; the break is \
             downstream. Check the trace for 'adopting this member's' (the adoption arm) and for \
             'no external physical display active after the restore' (the dark-desk backstop). A \
             missing adoption line means teardown's restore was never gated on; a backstop line \
             followed by a non-zero force-EXTEND rc means the remedy itself failed.",
            active_targets(),
            m1_empty = physicals_after_m1.is_empty()
        );
    }

    /// What `/display/monitors` answers on Windows. Read-only against a live host.
    #[test]
    #[ignore = "hardware: reads the live display topology"]
    fn live_windows_monitor_enumeration_reports_the_physical_screens() {
        let ms = crate::monitors::list_windows().expect("list_windows");
        for m in &ms {
            println!(
                "connector={:<14} enabled={:<5} managed={:<5} primary={:<5} {:>5}x{:<5} @{:>3}Hz  \
                 pos=({},{})  {:?}",
                m.connector,
                m.enabled,
                m.managed,
                m.primary,
                m.width,
                m.height,
                m.refresh_mhz / 1000,
                m.x,
                m.y,
                m.description
            );
        }
        assert!(!ms.is_empty(), "no monitors enumerated at all");
        assert!(
            ms.iter().any(|m| !m.managed),
            "every enumerated head is one of OURS — the operator's physical screen is still missing"
        );
    }

    /// Active display targets as `(target_id, friendly)`. A count cannot tell "physical still
    /// lit" from "physical deactivated, virtual took its place" — both are `1` on a single panel.
    fn active_targets() -> Vec<(u32, String)> {
        pf_win_display::win_display::target_inventory()
            .into_iter()
            .filter(|t| t.active)
            .map(|t| (t.target_id, format!("{} [{}]", t.friendly, t.tech)))
            .collect()
    }

    /// Surface manager/backend `tracing` on stdout for a live case. Decision points (isolate
    /// ladder, snapshot adoption, dark-desk backstop) have no other account. `with_test_writer`
    /// routes through the harness. Idempotent: the global default can be set once per process.
    /// `RUST_LOG` still wins; default is `debug` for our crates.
    fn init_test_tracing() {
        use tracing_subscriber::{fmt, EnvFilter};
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("pf_vdisplay=debug,pf_win_display=debug"));
        let _ = fmt().with_env_filter(filter).with_test_writer().try_init();
    }

    fn active_physicals() -> Vec<(u32, String)> {
        pf_win_display::win_display::target_inventory()
            .into_iter()
            .filter(|t| t.active && t.external_physical)
            .map(|t| (t.target_id, format!("{} [{}]", t.friendly, t.tech)))
            .collect()
    }

    /// Poll for the operator's panel to come back, up to `budget`. Teardown is timer-driven even
    /// at `KeepAlive::Off` (the linger reaper ticks at 500 ms), the CCD restore then settles, and
    /// PnP removal is async — a fixed sleep undershoots on a slow box and wastes time on a fast
    /// one. `Some(panel set)` the moment one is active; `None` on budget.
    fn wait_for_physicals(budget: Duration) -> Option<Vec<(u32, String)>> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let p = active_physicals();
            if !p.is_empty() {
                return Some(p);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// `SDC_TOPOLOGY_EXTEND` needs something to extend ACROSS — which is the state its callers
    /// are in, and why an isolated probe of the preset misleads.
    ///
    /// `force_extend_topology` both de-clones a fresh IddCx monitor and serves as
    /// `restore_displays_ccd`'s dark-desk backstop. Probed alone with one connected display it
    /// returns rc=31 ERROR_GEN_FAILURE (nothing to extend across) and reads inert; this case is
    /// the on-glass measurement that it works where it actually fires — active paths
    /// `1 -> (virtual up) 1 -> (after force-EXTEND) 2`, both real call sites running with the
    /// virtual still present (the restore fires BEFORE the REMOVE). The same run caught the clone
    /// hazard live: only the forced EXTEND gave the arriving virtual its own active path.
    ///
    /// ⚠️ Residual: a restore that fails once the virtual is already gone is back to one
    /// connected display, where EXTEND returns 31 and cannot re-light anything.
    ///
    /// Reports the counts rather than pinning a topology — which answer is "correct" depends on
    /// the box. It does assert the desk is not left with zero active paths.
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_force_extend_with_a_virtual_display_present() {
        init_test_tracing();
        // Pin the lifecycle like the adoption case: without `KeepAlive::Off` the dropped monitor
        // lingers (or pins, on a gaming-rig box) into the next case AND the teardown assertions
        // below sample before any restore ran.
        let _policy = ExclusiveTopology::force();
        let before = active_targets();
        assert!(
            !active_physicals().is_empty(),
            "no external physical panel is active at the start — power the display on first \
             (a TV in standby reads as Code 45 / zero CCD paths); active now: {before:?}"
        );
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        thread::sleep(Duration::from_secs(2));
        let with_virtual = active_targets();
        let physicals_with_virtual = active_physicals();
        pf_win_display::win_display::force_extend_topology();
        thread::sleep(Duration::from_secs(2));
        let after_extend = active_targets();
        drop(vout);
        // Bounded poll, not a fixed sleep — same settle stack as the adoption case above.
        let panel_back = wait_for_physicals(Duration::from_secs(20));
        let after_drop = active_targets();
        println!("force-EXTEND on glass, ACTIVE TARGETS at each step:");
        println!("  before          : {before:?}");
        println!("  virtual up      : {with_virtual:?}   (physicals: {physicals_with_virtual:?})");
        println!("  after force-EXT : {after_extend:?}");
        println!("  virtual dropped : {after_drop:?}");
        assert!(
            !after_drop.is_empty(),
            "the desk was left with NO active display path after the teardown"
        );
        assert!(
            panel_back.is_some(),
            "the operator's physical panel was left DEACTIVATED after teardown: {after_drop:?}"
        );
    }

    /// Live in-place resize (`#[ignore]`, needs a v4 driver and the host service stopped).
    /// Create at one mode, acquire the same slot at another: UPDATE_MODES path. Success is
    /// the same OS target id plus the committed active resolution.
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_inplace_resize() {
        // Surface manager/backend tracing; a bare harness has no subscriber.
        init_test_tracing();
        // `None` = CCD query failed in this session — a "never activated" verdict would be
        // an artifact of the test context.
        let active0 = pf_win_display::win_display::count_other_active(&[]);
        println!("spike: CCD active paths visible before create: {active0:?}");
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let first = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        let t1 = first
            .win_capture
            .as_ref()
            .expect("no capture target")
            .target_id;
        thread::sleep(Duration::from_secs(2)); // let the activation/settle fully quiesce
                                               // A window-drag-shaped mode the ADD never advertised.
        let t0 = std::time::Instant::now();
        let second = vd
            .create(Mode {
                width: 2356,
                height: 1332,
                refresh_hz: 60,
            })
            .expect("in-place resize acquire");
        let resize_ms = t0.elapsed().as_millis();
        let wc2 = second.win_capture.as_ref().expect("no capture target");
        let t2 = wc2.target_id;
        let in_place = t1 == t2;
        let active = pf_win_display::win_display::active_resolution(
            pf_win_display::win_display::CcdTargetKey::new(wc2.adapter_luid, wc2.target_id),
        );
        println!(
            "in-place resize spike: in_place={in_place} (target {t1} -> {t2}) took {resize_ms} ms, \
             active resolution now {active:?}"
        );
        assert_eq!(
            active,
            Some((2356, 1332)),
            "the new mode did not become the active resolution"
        );
        assert!(
            in_place,
            "the resize fell back to re-arrival (target id changed) — UPDATE_MODES path not taken"
        );
        drop(second);
        drop(first);
    }
}
