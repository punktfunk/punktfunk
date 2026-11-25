//! Driver IOCTL seam: REMOVE-key type, `add_monitor` reply, and the IOCTL trait.
//!
//! Isolates the driver's wire protocol from the lifecycle. The refcount
//! machine, linger, pinger and CCD/GDI glue stay driver-neutral in
//! [`super::VirtualDisplayManager`]. One implementor:
//! `crate::driver::PfVdisplayDriver`. The trait remains so tests can fake the
//! IOCTL surface.

use super::*;

/// `Session` is the host-chosen live key: a per-process counter, one fresh value per ADD.
/// `Guid` remains unused so the type still says the key is a per-driver choice, not a `u64` by
/// nature.
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
    /// An earlier session's hardware-cursor declare is irrevocable and covers the
    /// whole adapter, not only this target (the driver's `any_declared`). DWM
    /// leaves the pointer out of its frames, so a session without the cursor
    /// channel must self-composite (GDI poller + blend) or stream a cursor-less
    /// desktop.
    pub cursor_excluded: bool,
}

/// `Send + Sync` because the manager (and so the boxed driver) is a
/// `&'static` singleton reached from the pinger and linger threads.
pub(crate) trait VdisplayDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// `reap_orphans` sends `CLEAR_ALL`, which departs every monitor this
    /// process holds; the manager sets it on its first open only, as a reopen
    /// would take down live sessions. Returns the control device, watchdog
    /// seconds, and protocol version.
    fn open(&self, reap_orphans: bool) -> Result<(ControlDevice, u32, u32)>;
    /// Pins the IDD render GPU to `render_luid` when `Some`.
    /// `preferred_monitor_id` `0` = auto. `client_hdr` `None` = the driver's
    /// EDID CTA HDR defaults; `display_mm` `None` = its 96-DPI panel size. Reply LUID is
    /// `IDARG_OUT_MONITORARRIVAL.OsAdapterLuid` — the IddCx DISPLAY adapter,
    /// not the render GPU.
    #[allow(clippy::too_many_arguments)]
    fn add_monitor(
        &self,
        dev: &ControlDevice,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
        client_hdr: Option<pf_frame::HdrMeta>,
        display_mm: Option<(u16, u16)>,
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
    /// Move the driver's diagnostic lines into this process's log; issued after each successful
    /// ping, since the encoder runs in WUDFHost and reports nowhere else. Defaulted to
    /// nothing, so a backend with no such channel needs no stub.
    fn drain_log(&self, dev: &ControlDevice) {
        let _ = dev;
    }
}

/// A [`VdisplayDriver`] with no device behind it, for tests: every ADD fails, every REMOVE
/// succeeds.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    pub(crate) struct FakeDriver;

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
            _display_mm: Option<(u16, u16)>,
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
}

#[cfg(test)]
mod tests {
    use super::fake::FakeDriver;
    use super::*;

    /// Default must err: `resize_in_place` treats `Ok(())` as "the driver
    /// refreshed the advertised modes" and waits out the advertise window before
    /// the re-arrival it should have taken immediately.
    #[test]
    fn the_defaulted_update_modes_reports_not_supported() {
        let d = FakeDriver;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            refresh_mhz: 0,
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
