//! The present sinks, where a decoded frame leaves the decode loop for glass. [`AscSink`] is
//! the default: the codec renders into an `AImageReader` and each frame goes onto an
//! `ASurfaceControl` layer in a transaction aimed at its own vsync slot. [`SurfaceViewSink`]
//! releases straight to the SurfaceView's window: the fallback for API < 29, ChromeOS, an ASC
//! init failure, a bring-up rung that sheds the reader, or `present_backend=surfaceview`.
//!
//! Both drive the pure policy in [`crate::present`] and own their buffers, their display
//! feedback and their 1 Hz `pf.present` line. The decode loop holds one `Box<dyn Sink>` and
//! calls it at the same points of every pass. The selection sysprops are read here.

mod asc;
mod surface_view;

use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use punktfunk_core::quic::ColorInfo;

pub(super) use asc::AscSink;
pub(super) use surface_view::PresentMeter;
use surface_view::SurfaceViewSink;

use super::async_loop::{Ctx, OutputReady};
use super::surface_control::{Layer, PresentComplete};
use super::vsync::VsyncShared;
use super::DecodeOptions;
use crate::present::verdict::{downrated, Verdict};
use crate::present::PresentPriority;
use crate::sys::sysprop;

/// What routing one pass's outputs did, for the loop's own tallies.
#[derive(Default)]
pub(super) struct Routed {
    /// The HUD's `skipped`: core withheld it, or the store evicted it.
    pub(super) skipped: u64,
    /// Released unrendered from the codec. The ASC sink drops off-glass and counts none.
    pub(super) discarded: u64,
    /// Rendered straight away (the SurfaceView arrival path).
    pub(super) rendered: u64,
}

/// One present backend, as the decode loop drives it.
pub(super) trait Sink {
    /// The ASurfaceControl sink: the rate vote, the render callback, the decode-stage meter and
    /// the completion sender follow from it.
    fn is_asc(&self) -> bool;

    /// The sink paces against the vsync clock (every sink but the SurfaceView arrival path).
    fn needs_clock(&self) -> bool;

    /// Route one pass's decoded outputs, in pts order. `shows` folds each through core's freeze
    /// and must see every output, even one newest-wins discards.
    fn on_outputs(
        &mut self,
        ctx: &Ctx,
        ready: Vec<OutputReady>,
        shows: &mut dyn FnMut(u64) -> bool,
    ) -> Routed;

    /// The decision point, run every pass: present the due frame if the budget is open.
    /// Returns `true` when a frame went to the display.
    fn pump(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>) -> bool;

    /// Once a pass: the HUD counters, and the 1 Hz `pf.present` line with its verdicts
    /// ([`crate::present::verdict`]). `lost`: an AU was lost this pass, so its second is not
    /// unbroken. Acts on `Skipping` itself; returns `ShowsNothing` for the loop to end the run.
    fn flush(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>, lost: bool) -> Option<Verdict>;

    /// A vsync tick arrived this pass.
    fn on_vsync(&mut self, ctx: &Ctx);

    /// One ASurfaceControl transaction completed; `clock_offset` is this pass's.
    fn on_present_complete(&mut self, pc: PresentComplete, clock_offset: i64, ctx: &Ctx);

    /// The picture's size or colour changed: `picture` is the codec's new size, if known, and
    /// `stream` the colour the stream's own header states, which outranks the codec's report.
    fn on_format_changed(
        &mut self,
        ctx: &Ctx,
        picture: Option<(i32, i32)>,
        stream: Option<ColorInfo>,
    );

    /// The re-anchor gate armed: the cadence loop re-anchors on the next frame.
    fn reset_cadence(&mut self);

    /// Return every held buffer and image. The teardown calls it BEFORE `codec.stop()`.
    fn release_all(&mut self, codec: &MediaCodec);

    /// The layer that keeps showing the last frame after the codec is gone, if this sink has one.
    fn into_stale_layer(self: Box<Self>) -> Option<Layer>;
}

/// The session's presentation intent, or the `debug.punktfunk.present_priority` override:
/// `latency`, `smooth` or `smooth:N` (N = 1..=3). An on-device A/B without a settings round trip.
pub(super) fn priority(opts: &DecodeOptions) -> PresentPriority {
    match sysprop(c"debug.punktfunk.present_priority").as_deref() {
        Some("latency") => PresentPriority::Latency,
        Some(s) if s.starts_with("smooth") => PresentPriority::resolve(
            1,
            s.strip_prefix("smooth:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        ),
        _ => PresentPriority::resolve(opts.present_priority, opts.smooth_buffer),
    }
}

/// Whether the latency cushion covers the late tail (`debug.punktfunk.cushion_tail`, default
/// on): a jitter spike then costs its own height for a second instead of a dropped frame.
pub(super) fn cushion_tail() -> bool {
    sysprop(c"debug.punktfunk.cushion_tail").is_none_or(|v| v != "0")
}

/// The sink this codec presents through: the ASC sink bring-up created, else the SurfaceView
/// timeline presenter, or its arrival path under `debug.punktfunk.presenter=arrival`. `window`
/// and `color` give the SurfaceView window the negotiated colour before the first frame.
pub(super) fn select(
    asc: Option<AscSink>,
    opts: &DecodeOptions,
    refresh_hz: u32,
    window: &NativeWindow,
    color: &ColorInfo,
) -> Box<dyn Sink> {
    if let Some(a) = asc {
        return Box::new(a);
    }
    if sysprop(c"debug.punktfunk.presenter").as_deref() == Some("arrival") {
        log::info!("decode: presenter = arrival (sysprop) — legacy immediate release");
        return Box::new(SurfaceViewSink::arrival(window, color));
    }
    let priority = priority(opts);
    log::info!(
        "decode: presenter = timeline ({})",
        match priority {
            PresentPriority::Latency => "lowest latency".to_string(),
            PresentPriority::Smooth { buffer } => format!("smoothness, buffer {buffer}"),
        }
    );
    Box::new(SurfaceViewSink::timeline(
        window,
        color,
        priority,
        refresh_hz,
        opts.panel_hz,
    ))
}

/// The down-rated panel rule ([`downrated`]) as a sink follows it, pump by pump, logged on entry
/// and on exit.
struct Downrate {
    /// The mode-table rate Kotlin asked for, as a period; 0 keeps the rule off.
    asked_period_ns: i64,
    on: bool,
}

impl Downrate {
    /// `panel_hz`: [`DecodeOptions::panel_hz`], 0 = unknown.
    fn new(panel_hz: i32) -> Downrate {
        Downrate {
            asked_period_ns: if panel_hz > 0 {
                1_000_000_000 / i64::from(panel_hz)
            } else {
                0
            },
            on: false,
        }
    }

    /// Whether this pump's `panel_period_ns` (0 = unknown) is down-rated.
    fn follow(&mut self, panel_period_ns: i64) -> bool {
        let on = downrated(panel_period_ns, self.asked_period_ns);
        if on != self.on {
            self.on = on;
            if on {
                log::info!(
                    "presenter: panel period {:.2} ms under the asked {:.2} ms — every frame goes \
                     to SurfaceFlinger",
                    panel_period_ns as f64 / 1e6,
                    self.asked_period_ns as f64 / 1e6
                );
            } else {
                log::info!(
                    "presenter: panel back at {:.2} ms — pacing resumes",
                    panel_period_ns as f64 / 1e6
                );
            }
        }
        on
    }
}

/// Whether the ASurfaceControl backend is selected: ON everywhere but ChromeOS. ARC latches the
/// child layer and signals its present fence, yet Chrome can leave it undrawn, and nothing on
/// the Android side can tell. `debug.punktfunk.present_backend` overrides both ways: `surfaceview`
/// forces the SurfaceView presenter, `asc` forces this backend on ChromeOS.
pub(crate) fn asc_backend_selected(chromeos: bool) -> bool {
    match sysprop(c"debug.punktfunk.present_backend").as_deref() {
        Some("surfaceview") => false,
        Some("asc") => true,
        _ => !chromeos,
    }
}
