//! Mode, colour depth, and the output format the driver's encoder opens at.
//!
//! [`DescriptorPoller`] samples the CCD off-thread; this side consumes those
//! samples behind a two-strikes debounce, re-asserts the negotiated depth for a
//! session that must not follow a mid-session flip, and derives the
//! [`PixelFormat`] the stream loop rebuilds its encoder from. The CCD queries
//! here take the display-config lock, so they run at open and on a descriptor
//! change only — never per frame.

use super::*;

impl IddPushCapturer {
    /// Failed pins before [`Self::poll_display_hdr`] backs off (≈2 s at 4 Hz).
    const HDR_PIN_EAGER: u32 = 8;
    /// While backed off, re-pin every this-many-th sample (~4 s at 4 Hz).
    const HDR_PIN_RETRY_EVERY: u64 = 16;

    /// The frame format the driver's encoder takes, from display HDR + session 4:4:4. The
    /// stream loop rebuilds the encoder — a fresh `SET_ENCODE` — whenever this changes.
    pub(super) fn out_format(&self) -> PixelFormat {
        // PyroWave carries planar studio codes; the label follows the display's depth.
        if self.pyrowave {
            return if self.display_hdr {
                PixelFormat::P010
            } else {
                PixelFormat::Nv12
            };
        }
        if self.display_hdr {
            if self.want_444 {
                // Packed RGB; the encoder CSCs to YUV 4:4:4. No subsampling here.
                return PixelFormat::Rgb10a2;
            }
            PixelFormat::P010
        } else if self.ten_bit_sdr {
            PixelFormat::Rgb10a2Sdr
        } else if self.want_444 {
            PixelFormat::Bgra
        } else {
            PixelFormat::Nv12
        }
    }

    /// Re-assert the session's NEGOTIATED colour depth on the display, settling like `open` does.
    ///
    /// A monitor that re-arrives mid-stream (`re_add`: REMOVE then ADD, for a mode outside the
    /// frozen advertised list) comes back with advanced colour OFF whatever the session
    /// negotiated. It carries the client's HDR volume in its EDID, so it still looks like an HDR
    /// display — but composition drops to BGRA while the encoder was opened for FP16, and the
    /// driver's pool then refuses every frame with no way to recover. Returns the depth the
    /// display actually settled at, which is what the caller must open the encoder for.
    pub(super) fn pin_negotiated_depth(&self) -> bool {
        let want = self.want_hdr;
        let set = pf_win_display::win_display::set_advanced_color(self.ccd, want);
        let settle = Instant::now();
        while settle.elapsed() < Duration::from_millis(250) {
            if pf_win_display::win_display::advanced_color_enabled(self.ccd) == Some(want) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let observed = pf_win_display::win_display::advanced_color_enabled(self.ccd);
        let got = observed.unwrap_or(want && set);
        if got == want {
            tracing::info!(
                target_id = self.target_id,
                want_hdr = want,
                settle_ms = settle.elapsed().as_millis() as u64,
                "IDD push: re-asserted the negotiated depth after the monitor re-arrived"
            );
        } else {
            tracing::error!(
                target_id = self.target_id,
                want_hdr = want,
                observed_hdr = ?observed,
                set_advanced_color_returned = set,
                "IDD push: the re-arrived monitor would NOT take the negotiated depth - the \
                 encoder opens for what it actually composes, so the stream's depth will not \
                 match the negotiation"
            );
        }
        got
    }

    /// Re-open the encoder when two consecutive poller samples agree on a new descriptor
    /// (~½ s), so a topology re-probe blip never costs a session rebuild.
    pub(super) fn poll_display_hdr(&mut self) {
        let (mut now, seq) = self.desc_poller.snapshot();
        if seq == self.desc_seq {
            return;
        }
        self.desc_seq = seq;
        self.refresh_sdr_white_scale();
        // Exclusive-watchdog reassert in flight: a sample here is the transient eviction.
        if pf_win_display::topology_churn::held() {
            self.pending_desc = None;
            return;
        }
        // Re-assert negotiated depth instead of following a mid-session flip:
        // PyroWave plane formats are fixed; an SDR session must not promote to
        // P010 PQ. HDR H.26x is not pinned — its encoder re-opens on a flip.
        if (self.pyrowave || !self.want_hdr) && now.hdr != self.want_hdr {
            let want = self.want_hdr;
            if self.hdr_pin_failures < Self::HDR_PIN_EAGER
                || self.desc_seq % Self::HDR_PIN_RETRY_EVERY == 0
            {
                // OBSERVE the flip; never assert it. Substituting the DESIRED state for the
                // observed one breaks in both directions on a display that cannot be flipped:
                // the encoder opens for a depth the driver does not compose, and every frame
                // is wrong until the poller corrects it.
                let requested = pf_win_display::win_display::set_advanced_color(self.ccd, want);
                let observed = pf_win_display::win_display::advanced_color_enabled(self.ccd);
                // A failed READ is not evidence of a failed flip — keep the poller's sample then.
                now.hdr = observed.unwrap_or(now.hdr);
                if now.hdr != want {
                    self.hdr_pin_failures = self.hdr_pin_failures.saturating_add(1);
                    if !self.hdr_pin_warned {
                        self.hdr_pin_warned = true;
                        tracing::error!(
                            target_id = self.target_id,
                            want_hdr = want,
                            observed_hdr = ?observed,
                            set_advanced_color_returned = requested,
                            pyrowave = self.pyrowave,
                            "IDD push: could not pin the display to the NEGOTIATED depth — following what \
                             it actually composes instead (a physical display forcing HDR, or a driver that \
                             refuses the flip). The stream's depth will not match the negotiation; the \
                             encoder's caps cross-check reports the truth to the client"
                        );
                    }
                } else {
                    self.hdr_pin_failures = 0;
                }
            }
        } else {
            // No mismatch (or the session follows flips): a later refusal starts eager again.
            self.hdr_pin_failures = 0;
        }
        let current = DisplayDescriptor {
            hdr: self.display_hdr,
            width: self.width,
            height: self.height,
        };
        if now == current {
            self.pending_desc = None;
            return;
        }
        // Samples name the topology generation they were observed under (immunity plan WP10 item
        // 5): two strikes straddling a finished topology transaction are two different desktops,
        // never one confirmed change.
        let topo_gen = pf_win_display::topology_churn::generation();
        if self.pending_desc != Some(now) || self.pending_desc_gen != topo_gen {
            // First strike — act only when a second consecutive sample agrees.
            self.pending_desc = Some(now);
            self.pending_desc_gen = topo_gen;
            return;
        }
        self.pending_desc = None;
        tracing::info!(
            target_id = self.target_id,
            from = format!("{}x{} hdr={}", self.width, self.height, self.display_hdr),
            to = format!("{}x{} hdr={}", now.width, now.height, now.hdr),
            "IDD push: display descriptor changed — the next frame re-opens the driver's encoder"
        );
        self.display_hdr = now.hdr;
        self.width = now.width;
        self.height = now.height;
        self.refresh_sdr_white_scale();
        self.refresh_cursor_origin();
    }

    /// Re-read where this monitor sits on the desktop, from the display actor's snapshot (no
    /// CCD call on this thread), and re-stamp the cursor section — a mode change or an HDR
    /// re-arrival moves it. `None` keeps the last value, as the poller does.
    pub(super) fn refresh_cursor_origin(&mut self) {
        let Some((x, y, _, _)) = pf_win_display::display_events::snapshot().source_rect(self.ccd)
        else {
            return;
        };
        if let Some(cs) = self.cursor_shared.as_mut() {
            cs.set_origin((x, y));
        }
    }

    /// Where DWM places SDR white on this HDR desktop (2.5× = 200 nits at the Windows default),
    /// stamped into the cursor section for the driver's blend. Read from the display actor's
    /// snapshot, so it follows the SDR brightness slider without a CCD call on this thread.
    pub(super) fn refresh_sdr_white_scale(&mut self) {
        let Some(cs) = self.cursor_shared.as_ref() else {
            return;
        };
        if !self.display_hdr {
            cs.set_sdr_white_scale(0.0);
            return;
        }
        let queried = pf_win_display::display_events::snapshot()
            .target(self.ccd)
            .and_then(|t| t.sdr_white_level)
            .map(|level| level as f32 / 1000.0);
        let scale = queried.unwrap_or(self.sdr_white_scale);
        cs.set_sdr_white_scale(scale);
        if scale != self.sdr_white_scale || !self.sdr_white_logged {
            self.sdr_white_logged = true;
            tracing::info!(
                target_id = self.target_id,
                queried = ?queried,
                applied = scale,
                "cursor composite: HDR SDR-white scale (1.0 = 80 nits; None keeps the prior value)"
            );
        }
        self.sdr_white_scale = scale;
    }
}
