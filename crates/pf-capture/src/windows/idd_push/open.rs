//! One-shot IDD-push construction: display depth, the WUDFHost duplication
//! broker, and the cursor opt-in.
//!
//! Types that exist only here: [`SharedObjectSa`], shared with the AU section's
//! own create. Steady-state capture (`try_consume`, pollers, `Capturer`) lives
//! in `capturer`. A `#[path]` child sees the parent's private items through
//! `use super::*`. Evidence: `design/idd-push-security.md`.

use super::*;

/// `SECURITY_ATTRIBUTES` for the unnamed AU/cursor section and event objects: SDDL
/// `D:P(A;;GA;;;SY)`, protected, `bInheritHandle: false`.
///
/// The driver never opens by name; it receives duplicated handles (access travels
/// with the handle). See `design/idd-push-security.md`.
///
/// RAII over the `LocalAlloc` descriptor. `sa.lpSecurityDescriptor` points at
/// `psd`; [`as_ptr`](Self::as_ptr) only lends a borrow, so the attributes cannot
/// outlive this value. Moving is fine — the pointer targets the heap, not a field.
pub(super) struct SharedObjectSa {
    sa: SECURITY_ATTRIBUTES,
    psd: PSECURITY_DESCRIPTOR,
}

impl SharedObjectSa {
    pub(super) fn new() -> Result<Self> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the `w!()` literal is the SDDL source; the call writes its
        // `LocalAlloc` descriptor into this live `psd`; `?` rejects failure
        // before `psd` is read.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;;GA;;;SY)"),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
            .context("build SDDL for IDD-push shared objects")?;
        }
        Ok(Self {
            sa: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd.0,
                bInheritHandle: false.into(),
            },
            psd,
        })
    }

    /// Borrowed from this owner; the descriptor must outlive the create call.
    pub(super) fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.sa
    }
}

impl Drop for SharedObjectSa {
    fn drop(&mut self) {
        // SAFETY: `psd` is the descriptor this value's constructor allocated and
        // nothing else owns it. `LocalFree` runs once (`Drop` once; `as_ptr` only
        // lends a borrow of `sa`).
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.psd.0)));
        }
    }
}

impl IddPushCapturer {
    /// Open the IDD-push capturer. Success attaches `keepalive` (the capturer
    /// owns the virtual display). Failure returns it so the caller retires or
    /// retries the monitor — this function never tears the display down.
    ///
    /// There is no fallback capture path: the driver is the only Windows video
    /// source, and its encoder opens later, at `SET_ENCODE`.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        want_hdr: bool,
        ten_bit_sdr: bool,
        want_444: bool,
        pyrowave: bool,
        keepalive: Box<dyn Send>,
        cursor_sender: Option<crate::CursorChannelSender>,
        cursor_forward: Option<crate::CursorForwardSender>,
        forwards_to_client: bool,
    ) -> std::result::Result<Self, (anyhow::Error, Box<dyn Send>)> {
        // Idempotent: first capturer starts it so stall logs can correlate DWM holes
        // with OS display events for the session's life.
        pf_win_display::display_events::spawn_once();
        match Self::open_inner(
            target,
            preferred,
            want_hdr,
            ten_bit_sdr,
            want_444,
            pyrowave,
            cursor_sender,
            cursor_forward,
            forwards_to_client,
        ) {
            Ok(mut me) => {
                me._keepalive = keepalive;
                Ok(me)
            }
            Err(e) => Err((e, keepalive)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_inner(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        want_hdr: bool,
        ten_bit_sdr: bool,
        want_444: bool,
        pyrowave: bool,
        cursor_sender: Option<crate::CursorChannelSender>,
        cursor_forward: Option<crate::CursorForwardSender>,
        // This session negotiated client-side cursor drawing (`HOST_CAP_CURSOR`). False ⇒ the
        // pointer can only reach the wire through the driver pool's blend.
        forwards_to_client: bool,
    ) -> Result<Self> {
        let (pw, ph, _hz) = preferred
            .context("IDD push needs the negotiated mode (WxH) to size the encoder's input")?;
        // The complete CCD identity every display-global helper below selects paths by (the
        // packed LUID in the capture target is the IddCx display adapter's).
        let ccd =
            pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id);
        // Follow the display's ACTUAL current resolution when it differs from the negotiated
        // mode: a fullscreen game can hold the virtual display at a different one (especially
        // across a reconnect), and the driver's encoder must open for what DWM composes.
        let (w, h) = pf_win_display::win_display::active_resolution(ccd).unwrap_or((pw, ph));
        if (w, h) != (pw, ph) {
            tracing::info!(
                target_id = target.target_id,
                negotiated = format!("{pw}x{ph}"),
                actual = format!("{w}x{h}"),
                "IDD push: opening at the display's actual mode (differs from negotiated)"
            );
        }
        // Composition is FP16 scRGB in advanced-color, BGRA otherwise. A 10-bit client
        // enables HDR here and the descriptor poller tracks mid-session flips. An SDR
        // client forces advanced color OFF and stays pinned, so the driver's encoder
        // cannot emit in-band PQ to a client that asked for SDR: PyroWave's CSC reads
        // 8-bit BGRA only, and H.26x would emit P010 + PQ.
        if !want_hdr {
            let _ = pf_win_display::win_display::set_advanced_color(ccd, false);
            let settle = Instant::now();
            while settle.elapsed() < Duration::from_millis(250) {
                if pf_win_display::win_display::advanced_color_enabled(ccd) == Some(false) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if pf_win_display::win_display::advanced_color_enabled(ccd) == Some(true) {
                tracing::error!(
                    target = target.target_id,
                    pyrowave,
                    "IDD push: SDR session but advanced color (HDR) could NOT be turned off on the \
                     virtual display (a physical display forcing HDR?) — PyroWave will likely fail \
                     its first frame; H.26x would emit PQ the SDR-only client never asked for"
                );
            } else {
                tracing::info!(
                    target = target.target_id,
                    pyrowave,
                    settle_ms = settle.elapsed().as_millis() as u64,
                    "IDD push: SDR-negotiated session — advanced color forced OFF (SDR/BGRA composition)"
                );
            }
        }
        // 10-bit: take the depth from the successful set, not the CCD poll. A 250 ms
        // poll can still read SDR while the driver already composes FP16.
        let enabled_hdr = want_hdr && pf_win_display::win_display::set_advanced_color(ccd, true);
        if enabled_hdr {
            // Poll CCD instead of a fixed sleep; 250 ms ceiling. A timeout still takes
            // `enabled_hdr` — the set succeeded, and the driver's retained pool slot absorbs
            // a lagging compose flip.
            let hdr_settle = Instant::now();
            while hdr_settle.elapsed() < Duration::from_millis(250) {
                if pf_win_display::win_display::advanced_color_enabled(ccd) == Some(true) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            tracing::debug!(
                target_id = target.target_id,
                settle_ms = hdr_settle.elapsed().as_millis() as u64,
                "IDD push: advanced-color (HDR) enable settle"
            );
        }
        // A failed open-time read defaults to SDR (unless the 10-bit path enabled HDR above) —
        // there is no "last known" yet; the descriptor poller corrects a wrong guess mid-session.
        // Keep the raw observation so the error below can say whether the read reported OFF or
        // failed outright — "we asked, it said no" and "we could not tell" have different fixes.
        let observed_hdr = pf_win_display::win_display::advanced_color_enabled(ccd);
        let display_hdr = want_hdr && (enabled_hdr || observed_hdr.unwrap_or(false));
        // Negotiated 10-bit but advanced color did not enable: the driver composes SDR and its
        // encoder emits 8-bit BT.709 while Welcome said HDR. Loud — every frame of this session
        // is wrong until the descriptor poller sees HDR.
        if want_hdr && !display_hdr {
            tracing::error!(
                target = target.target_id,
                want_hdr = true,
                set_advanced_color_returned = enabled_hdr,
                observed_hdr = ?observed_hdr,
                "IDD push: 10-bit HDR was negotiated but enabling advanced color on the \
                 virtual display FAILED — encoding 8-bit SDR while the client was told HDR \
                 (check the display driver / Windows HDR support on this box). \
                 observed_hdr=Some(false) ⇒ the display reports advanced colour OFF after the \
                 set; None ⇒ the CCD read itself failed"
            );
        }
        // The duplication target for the AU and cursor sections, and the driver-death probe.
        let broker = ChannelBroker::open(target.wudf_pid)?;

        // CursorShm create + deliver. Non-fatal: without it the driver never
        // declares a hardware cursor, so this session has no pointer to forward
        // and none to blend.
        let cursor_shared = cursor_sender.as_ref().and_then(|send_cursor| {
            match cursor::CursorShared::create(ccd) {
                Ok(cs) => {
                    // Shared helper: also re-delivers after a driver monitor re-arrival.
                    deliver_cursor_channel(&broker, target.target_id, &cs, send_cursor)
                        .then_some(cs)
                }
                Err(e) => {
                    tracing::warn!(
                        "cursor section creation failed — the driver will not declare a \
                         hardware cursor, so this session cannot forward the pointer: {e:#}"
                    );
                    None
                }
            }
        });
        // Sticky hardware-cursor declare from an earlier session keeps the pointer out of DWM's
        // frames. Force the pool's blend whenever no CLIENT draws one. Not `cursor_shared` alone:
        // a channel is delivered TO an excluded target as the blend's shape source, so that test
        // cancelled the rescue with the very channel opened for it.
        let composite_forced =
            target.cursor_excluded && (!forwards_to_client || cursor_shared.is_none());
        if composite_forced {
            tracing::info!(
                target_id = target.target_id,
                forwards_to_client,
                have_channel = cursor_shared.is_some(),
                "target carries an irrevocable hardware-cursor declare and no client draws the \
                 pointer this session — the driver's pool blends it. have_channel=false ⇒ the \
                 blend has no shape source either"
            );
        }
        // Same gate as the live channel. IddCx cannot deliver masked/monochrome
        // (`cursor_poll.rs`), so the GDI poller is the client's shape source.
        let cursor_poll = (cursor_shared.is_some() || composite_forced).then(|| {
            let rect = pf_win_display::win_display::source_desktop_rect(ccd).unwrap_or((
                0,
                0,
                i32::MAX,
                i32::MAX,
            ));
            cursor_poll::CursorPoller::spawn(ccd, rect)
        });
        // Previous session may have died on the secure desktop with desired
        // state `false`; delivery would then start undeclared. Fresh sessions
        // start declared; `poll_secure_desktop` re-disables if still locked. A
        // forced-composite session starts the other way: the driver blends from
        // the first frame.
        if let (Some(_), Some(fwd)) = (cursor_shared.as_ref(), cursor_forward.as_ref()) {
            if let Err(e) = fwd(!composite_forced) {
                tracing::debug!("cursor-forward reset at open failed: {e:#}");
            }
        }

        tracing::info!(
            target_id = target.target_id,
            wudf_pid = target.wudf_pid,
            mode = format!("{w}x{h}"),
            display_hdr,
            want_hdr,
            ten_bit_sdr,
            want_444,
            "IDD push(host): virtual display bound; the driver encodes what DWM composes"
        );
        // The diagnostic posture, once per session: active probes and an ETW session alter the
        // very path a disturbance report describes, so every report needs this A/B label.
        match super::diag_dir() {
            Some(dir) => tracing::info!(
                au_dump_dir = %dir.display(),
                "IDD push: PUNKTFUNK_IDD_DIAG is ON — micro-probes, the DxgKrnl ETW session and \
                 the access-unit dump are running for this session"
            ),
            None => tracing::info!(
                "IDD push: diagnostics off (set PUNKTFUNK_IDD_DIAG=1 for probes, DxgKrnl ETW and \
                 an access-unit dump)"
            ),
        }
        let mut me = Self {
            target_id: target.target_id,
            ccd,
            source_seq: 0,
            driver_source_seq: 0,
            delivered: None,
            recovery: super::recovery::Supervisor::new(Instant::now()),
            recovered_outage: None,
            pending_stage: None,
            pending_fault: None,
            encoder: None,
            broker,
            width: w,
            height: h,
            want_hdr,
            ten_bit_sdr,
            display_hdr,
            hdr_pin_warned: false,
            hdr_pin_failures: 0,
            want_444,
            pyrowave,
            desc_poller: DescriptorPoller::spawn(
                ccd,
                DisplayDescriptor {
                    hdr: display_hdr,
                    width: w,
                    height: h,
                },
            ),
            desc_seq: 0,
            pending_desc: None,
            pending_desc_gen: 0,
            recovering_since: None,
            last_fresh: Instant::now(),
            drain_seq: 0,
            last_drain: Instant::now(),
            last_liveness: Instant::now(),
            last_kick: Instant::now(),
            stall_watch: StallWatch::new(),
            max_hb_age_us: 0,
            cursor: CursorWitness::new(Instant::now()),
            probes: super::diag_dir().map(|_| super::probes::acquire()),
            etw: super::diag_dir().and_then(|_| super::dxgkrnl_etw::acquire()),
            cursor_shared,
            cursor_poll,
            cursor_forward,
            cursor_sender,
            secure_active: false,
            composite_cursor: composite_forced,
            composite_forced,
            cursor_shm_latched: false,
            sdr_white_scale: 2.5,
            sdr_white_logged: false,
            // Taken at open so the display cannot idle off under the session;
            // held until the capturer drops.
            _display_wake: pf_frame::session_tuning::DisplayWakeRequest::new(),
            // Placeholder. `open()` attaches the real keepalive only on success
            // so a failed open can hand it back.
            _keepalive: Box::new(()),
        };
        // Stamp both REALTIME GPU-priority opt-ins once per session. Stall
        // WARNs repeat them only when they fire, so a quiet stalling log
        // would otherwise omit the posture.
        tracing::info!(
            rt_gpu_driver = super::stall::rt_gpu_driver_posture(),
            rt_gpu_host = super::stall::rt_gpu_host_posture(),
            "GPU-priority posture for this capture session"
        );
        // The driver's blend needs this and session 0 cannot query it. No-op on SDR.
        me.refresh_sdr_white_scale();
        Ok(me)
    }
}
