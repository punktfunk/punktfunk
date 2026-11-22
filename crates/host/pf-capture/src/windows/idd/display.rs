//! Mode, colour depth, and the output format the driver's encoder opens at.
//!
//! [`DescriptorPoller`] samples the display off-thread; this side steps the depth pin
//! ([`crate::windows::depth_pin`]) over those samples and runs what it decides over the
//! capturer's [`pf_win_display::colour::DisplayColour`]: a pin back to the negotiated depth,
//! the encoder's re-open, the WCG refusal latch. It derives the [`PixelFormat`] the stream
//! loop rebuilds its encoder from. The CCD queries here take the display-config lock, so they
//! run at open and on a descriptor change only — never per frame.

use super::*;
use crate::windows::depth_pin::{pin_wcg, reassert_hdr, Composed, PinAction, Sample};
use std::collections::VecDeque;

/// Applies a [`crate::windows::depth_pin::wcg_verdict`] to the process-wide latch, the only
/// place it is written.
pub(super) fn apply_wcg_verdict(verdict: PinAction) {
    match verdict {
        PinAction::LatchWcgRefused => super::WCG_REFUSED.store(true, Ordering::Relaxed),
        PinAction::ClearWcgRefused => super::WCG_REFUSED.store(false, Ordering::Relaxed),
        _ => {}
    }
}

impl IddPushCapturer {
    /// The frame format the driver's encoder takes, from display HDR or wide colour + session
    /// 4:4:4. The stream loop rebuilds the encoder — a fresh `SET_ENCODE` — whenever this
    /// changes.
    pub(super) fn out_format(&self) -> PixelFormat {
        let (wanted, composed) = (self.depth.wanted(), self.depth.composed());
        // SDR wide colour: the driver reads FP16 under an SDR transfer. Its own label, so a flip
        // in or out of wide colour re-opens the encoder.
        if composed.wcg && !composed.hdr {
            return PixelFormat::RgbaF16;
        }
        // PyroWave carries planar studio codes; the label follows the display's depth.
        if wanted.pyrowave {
            return if composed.hdr {
                PixelFormat::P010
            } else {
                PixelFormat::Nv12
            };
        }
        if composed.hdr {
            if self.want_444 {
                // Packed RGB; the encoder CSCs to YUV 4:4:4. No subsampling here.
                return PixelFormat::Rgb10a2;
            }
            PixelFormat::P010
        } else if wanted.ten_bit_sdr {
            PixelFormat::Rgb10a2Sdr
        } else if self.want_444 {
            PixelFormat::Bgra
        } else {
            PixelFormat::Nv12
        }
    }

    /// Re-assert the session's negotiated depth and wide colour after a re-arrival, and adopt
    /// what it settled at: the encoder opens for what the display composes, or the driver's
    /// pool refuses every frame of an FP16 encoder over a BGRA surface.
    pub(super) fn pin_negotiated_depth(&mut self) {
        let colour = &*self.colour;
        let hdr = reassert_hdr(colour, self.ccd, self.target_id, self.depth.wanted().hdr);
        let wcg = !hdr && {
            let (got, verdict) = pin_wcg(colour, self.ccd, self.target_id, self.depth.wcg());
            if let Some(verdict) = verdict {
                apply_wcg_verdict(verdict);
            }
            got
        };
        self.depth.step(Sample::Rearrived(Composed { hdr, wcg }));
    }

    /// Step the depth pin over the poller's latest sample and run what it asks: a pin back to
    /// the negotiated depth, or, after two agreeing samples, the encoder's re-open.
    pub(super) fn poll_display_hdr(&mut self) {
        let (desc, seq) = self.desc_poller.snapshot();
        let mut queue = VecDeque::from(self.depth.step(Sample::Polled {
            seq,
            desc,
            held: pf_win_display::topology_churn::held(),
            mode: (self.width, self.height),
        }));
        while let Some(action) = queue.pop_front() {
            let next = match action {
                PinAction::RefreshSdrWhite => {
                    self.refresh_sdr_white_scale();
                    continue;
                }
                PinAction::SetAdvancedColour(on) => Sample::Pinned {
                    set: self.colour.set_advanced_color(self.ccd, on),
                    mode: self.colour.color_mode(self.ccd),
                },
                PinAction::SetWcg(on) => Sample::Pinned {
                    set: self.colour.set_wcg(self.ccd, on),
                    mode: self.colour.color_mode(self.ccd),
                },
                PinAction::ReadGeneration => {
                    Sample::Generation(pf_win_display::topology_churn::generation())
                }
                PinAction::Warn { observed, set } => {
                    let wanted = self.depth.wanted();
                    tracing::error!(
                        target_id = self.target_id,
                        want_hdr = wanted.hdr,
                        want_wcg = self.depth.wcg(),
                        observed = ?observed,
                        set_returned = set,
                        pyrowave = wanted.pyrowave,
                        "IDD push: could not pin the display to the negotiated depth — following what \
                         it actually composes instead (a physical display forcing HDR, or a driver that \
                         refuses the flip). The stream's depth will not match the negotiation, \
                         and nothing tells the client"
                    );
                    continue;
                }
                PinAction::ReopenEncoder { from, to } => {
                    tracing::info!(
                        target_id = self.target_id,
                        from = format!(
                            "{}x{} hdr={} wcg={}",
                            from.width, from.height, from.hdr, from.wcg
                        ),
                        to = format!("{}x{} hdr={} wcg={}", to.width, to.height, to.hdr, to.wcg),
                        "IDD push: display descriptor changed — the next frame re-opens the driver's encoder"
                    );
                    self.width = to.width;
                    self.height = to.height;
                    self.refresh_sdr_white_scale();
                    continue;
                }
                PinAction::LatchWcgRefused | PinAction::ClearWcgRefused => {
                    apply_wcg_verdict(action);
                    continue;
                }
            };
            queue.extend(self.depth.step(next));
        }
    }

    /// Where DWM places SDR white on this HDR desktop (2.5× = 200 nits at the Windows default),
    /// stamped into the cursor section for the driver's blend. Read from the display actor's
    /// snapshot, so it follows the SDR brightness slider without a CCD call on this thread.
    pub(super) fn refresh_sdr_white_scale(&mut self) {
        let Some(cs) = self.cursor_shared.as_ref() else {
            return;
        };
        if !self.depth.composed().hdr {
            cs.set_sdr_white_scale(0.0);
            return;
        }
        let queried = self
            .colour
            .snapshot()
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
