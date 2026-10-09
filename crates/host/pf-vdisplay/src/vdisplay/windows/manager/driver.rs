//! Driver IOCTL seam: REMOVE-key type, `add_monitor` reply, and the IOCTL trait.
//!
//! Isolates the driver's wire protocol from the lifecycle. The refcount
//! machine, linger, pinger and CCD/GDI glue stay driver-neutral in
//! [`super::VirtualDisplayManager`]. One implementor:
//! `crate::driver::PfVdisplayDriver`. The trait remains so tests can fake the
//! IOCTL surface.

use super::*;

/// `Session` is the host-chosen live key: monotonic for legacy ownership and
/// stable by connector under a seats reservation. `Guid` remains unused so the
/// type still says the key is a per-driver choice, not a `u64` by nature.
#[derive(Clone, Copy)]
pub(crate) enum MonitorKey {
    Guid(windows::core::GUID),
    Session(u64),
}

/// `wudf_pid` is the sealed frame channel's handle-duplication target.
pub(crate) struct AddedMonitor {
    pub key: MonitorKey,
    pub target_id: u32,
    pub luid: LUID,
    pub wudf_pid: u32,
    pub resolved_monitor_id: u32,
    /// This OS target already carries an irrevocable hardware-cursor declare
    /// from an earlier session. DWM excludes the pointer from its frames
    /// forever, so a session without the cursor channel must self-composite
    /// (GDI poller + blend) or stream a cursor-less desktop.
    pub cursor_excluded: bool,
}

/// `Send + Sync` because the manager (and so the boxed driver) is a
/// `&'static` singleton reached from the pinger and linger threads.
pub(crate) trait VdisplayDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// `reap_orphans` permits a global `CLEAR_ALL`; only the first legacy
    /// single-owner open may set it. Reserved seats and handle reopens can
    /// overlap live monitors. Returns the control device, watchdog seconds, and
    /// protocol version.
    fn open(&self, reap_orphans: bool) -> Result<(ControlDevice, u32, u32)>;
    /// Pins the IDD render GPU to `render_luid` when `Some`.
    /// `preferred_monitor_id` `0` = auto. `client_hdr` `None` = the driver's
    /// EDID CTA HDR defaults. Reply LUID is
    /// `IDARG_OUT_MONITORARRIVAL.OsAdapterLuid` — the IddCx DISPLAY adapter,
    /// not the render GPU (that is only in the shared frame header).
    fn add_monitor(
        &self,
        dev: &ControlDevice,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
        client_hdr: Option<pf_frame::HdrMeta>,
        hw_cursor: bool,
    ) -> Result<AddedMonitor>;
    /// In-place resize (`IOCTL_UPDATE_MODES`, protocol v4). The monitor is not
    /// departed; the caller CCD-forces the new mode afterwards. Default errs so
    /// a backend without support takes the re-arrival fallback.
    fn update_modes(&self, dev: &ControlDevice, key: &MonitorKey, mode: Mode) -> Result<()> {
        let _ = (dev, key, mode);
        anyhow::bail!("backend does not support in-place mode updates")
    }
    fn remove_monitor(&self, dev: &ControlDevice, key: &MonitorKey) -> Result<()>;
    /// Issued every `watchdog/3` from the pinger thread.
    fn ping(&self, dev: &ControlDevice) -> Result<()>;
    /// Move the driver's diagnostic lines into this process's log; issued after each ping, since
    /// the encoder runs in WUDFHost and reports nowhere else. Defaulted to nothing, so a backend
    /// with no such channel needs no stub.
    fn drain_log(&self, dev: &ControlDevice) {
        let _ = dev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDriver;

    impl VdisplayDriver for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn open(&self, _reap_orphans: bool) -> Result<(ControlDevice, u32, u32)> {
            anyhow::bail!("fake driver has no control device")
        }
        fn add_monitor(
            &self,
            _dev: &ControlDevice,
            _mode: Mode,
            _render_luid: Option<LUID>,
            _preferred_monitor_id: u32,
            _client_hdr: Option<pf_frame::HdrMeta>,
            _hw_cursor: bool,
        ) -> Result<AddedMonitor> {
            anyhow::bail!("fake driver adds no monitors")
        }
        fn remove_monitor(&self, _dev: &ControlDevice, _key: &MonitorKey) -> Result<()> {
            Ok(())
        }
        fn ping(&self, _dev: &ControlDevice) -> Result<()> {
            Ok(())
        }
    }

    /// Default must err: `resize_in_place` treats `Ok(())` as "the driver
    /// refreshed the advertised modes" and burns the 1.5 s CCD settle before
    /// the re-arrival it should have taken immediately.
    #[test]
    fn the_defaulted_update_modes_reports_not_supported() {
        let d = FakeDriver;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let err = d
            .update_modes(&ControlDevice::stand_in(), &MonitorKey::Session(1), mode)
            .expect_err("the default must not report success");
        assert!(
            err.to_string()
                .contains("does not support in-place mode updates"),
            "unexpected error text: {err:#}"
        );
    }
}
