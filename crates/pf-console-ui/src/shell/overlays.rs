//! Modal overlays: connecting, waking, toast, full-screen takeover.

use crate::anim::{approach, springs};
use crate::coverflow::{card_matrix, PERSPECTIVE};
use crate::glyphs::{hint_bar, Hint, HintKey};
use crate::theme::{edge, fg, fill, Fonts, PanelStroke, W};
use skia_safe::{gradient, Canvas, Color4f, Image, PathBuilder, Point, RRect, Rect, TileMode, M44};

use super::{Launching, Shell, ToastMark, BOTTOM_BAND};
use crate::model::{SpeedPhase, SpeedStatus};

/// Kind mark in a 13 dp box.
fn draw_toast_mark(canvas: &Canvas, mark: ToastMark, cx: f64, cy: f64, k: f64, ink: Color4f) {
    let mut p = fill(ink);
    match mark {
        ToastMark::Dot => {
            canvas.draw_circle((cx as f32, cy as f32), (3.4 * k) as f32, &p);
        }
        ToastMark::Check => {
            p.set_style(skia_safe::PaintStyle::Stroke);
            p.set_stroke_width((1.9 * k) as f32);
            p.set_stroke_cap(skia_safe::PaintCap::Round);
            p.set_stroke_join(skia_safe::PaintJoin::Round);
            let r = 5.0 * k;
            let mut path = PathBuilder::new();
            path.move_to(((cx - r) as f32, cy as f32));
            path.line_to(((cx - r * 0.25) as f32, (cy + r * 0.7) as f32));
            path.line_to(((cx + r) as f32, (cy - r * 0.7) as f32));
            canvas.draw_path(&path.detach(), &p);
        }
        ToastMark::Bang => {
            // Stem + dot, not a text "!": at 0.75× k a font bang is two pixels and vanishes.
            let r = 5.4 * k;
            let w = 2.0 * k;
            canvas.draw_rrect(
                skia_safe::RRect::new_rect_xy(
                    Rect::from_xywh(
                        (cx - w / 2.0) as f32,
                        (cy - r) as f32,
                        w as f32,
                        (r * 1.35) as f32,
                    ),
                    (w / 2.0) as f32,
                    (w / 2.0) as f32,
                ),
                &p,
            );
            canvas.draw_circle((cx as f32, (cy + r * 0.72) as f32), (w * 0.62) as f32, &p);
        }
    }
}

impl Shell {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::shell) fn draw_overlays(
        &mut self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        dt: f64,
        t: f64,
        fonts: &Fonts,
    ) {
        // Resolved before the chain below: naming the layer Apply writes to reads the
        // whole shell, which the chain's `&mut self.connecting` arm would forbid.
        let pinned_by = self
            .speed
            .as_ref()
            .and_then(|sp| self.speed_pinned_by(&sp.key))
            .map(str::to_string);
        // Connect, wake and the speed test share one full-screen shape. A connect can
        // follow a wake (`sync`) so they share the backdrop and never blink between them.
        let takeover: Option<(f64, bool, String, String, Vec<Hint>)> =
            if let Some(c) = &mut self.connecting {
                c.appear = approach(c.appear, 1.0, dt, 0.07);
                if c.request_access {
                    Some((
                        c.appear,
                        true,
                        "Waiting for approval…".to_string(),
                        format!(
                            "Approve this device in {}'s console or web UI — no PIN needed.",
                            c.title
                        ),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ))
                } else {
                    Some((
                        c.appear,
                        true,
                        format!("Connecting to {}…", c.title),
                        "Starting the stream in this window.".to_string(),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ))
                }
            } else if let Some(sw) = &mut self.seat_wait {
                sw.appear = approach(sw.appear, 1.0, dt, 0.07);
                Some((
                    sw.appear,
                    true,
                    pf_client_core::profiles::waking_line(&sw.name),
                    sw.detail.clone().unwrap_or_default(),
                    vec![Hint::new(HintKey::Back, "Cancel")],
                ))
            } else if let Some(wk) = &self.wake {
                // Service-driven: already settled, no fade-in.
                if wk.timed_out {
                    Some((
                        1.0,
                        false,
                        format!("{} didn't wake", wk.name),
                        "Check its power settings, or wake it manually and try again.".to_string(),
                        vec![
                            Hint::new(HintKey::Confirm, "Try Again"),
                            Hint::new(HintKey::Back, "Cancel"),
                        ],
                    ))
                } else {
                    Some((
                        1.0,
                        true,
                        format!("Waking {}…", wk.name),
                        format!("Waiting for it to come online · {} s", wk.seconds),
                        // Wake-only offers "Stop Waiting"; wake-then-connect is "Cancel".
                        vec![Hint::new(
                            HintKey::Back,
                            if wk.then_connect {
                                "Cancel"
                            } else {
                                "Stop Waiting"
                            },
                        )],
                    ))
                }
            } else if let Some(sp) = &self.speed {
                // Service-driven like the wake card: already settled, no fade-in.
                let close = Hint::new(HintKey::Back, "Close");
                Some(match &sp.phase {
                    SpeedPhase::Connecting => (
                        1.0,
                        true,
                        format!("Testing {}\u{2026}", sp.name),
                        "Connecting.".to_string(),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ),
                    SpeedPhase::Measuring | SpeedPhase::Progress { .. } => (
                        1.0,
                        false,
                        format!("Testing {}\u{2026}", sp.name),
                        "Measuring the link \u{2014} this takes a few seconds.".to_string(),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ),
                    SpeedPhase::Failed(why) => (
                        1.0,
                        false,
                        format!("Couldn't measure {}", sp.name),
                        why.clone(),
                        vec![close],
                    ),
                    SpeedPhase::Done {
                        throughput_kbps,
                        wall,
                        clean,
                        recommended_kbps,
                        findings,
                    } => {
                        let measured = speed_headline(*throughput_kbps, *wall, clean.as_ref());
                        let mut lines: Vec<String> = findings.iter().map(finding_text).collect();
                        let mut hints = Vec::new();
                        match &pinned_by {
                            // Read-only: the default is not the layer this host streams at.
                            Some(name) => lines.push(format!(
                                "\u{201c}{name}\u{201d} sets this host's bitrate \u{2014} \
                                 change it there to use this."
                            )),
                            None => {
                                lines.push(format!(
                                    "{} recommended, leaving headroom for FEC and loss.",
                                    mbps(*recommended_kbps)
                                ));
                                hints.push(Hint::new(HintKey::Confirm, "Set as the default"));
                            }
                        }
                        if findings.iter().any(|f| f.profile.is_some()) {
                            hints.push(Hint::new(HintKey::Secondary, "Use paced delivery"));
                        }
                        hints.push(close);
                        (1.0, false, measured, lines.join("\n"), hints)
                    }
                })
            } else {
                None
            };
        // A burst under way or measured draws its graph, when the speed test is the takeover
        // showing; connecting and failing are words.
        let charted = self
            .speed
            .as_ref()
            .filter(|_| self.connecting.is_none() && self.wake.is_none())
            .filter(|sp| !matches!(sp.phase, SpeedPhase::Connecting | SpeedPhase::Failed(_)));
        match (takeover, charted) {
            (Some((_, _, title, body, hints)), Some(sp)) => {
                let (done, rec) = measured(&sp.phase);
                self.speed_view.step(sp, done, rec, dt);
                self.draw_speed(canvas, (w, h), k, t, fonts, sp, (&title, &body), &hints);
            }
            (Some((appear, spinner, title, body, hints)), _) => {
                let rects = self.draw_takeover(
                    canvas, w, h, k, appear, t, fonts, spinner, &title, &body, &hints,
                );
                // The seat wait's Cancel takes a click; the other takeovers' legends do not.
                if self.seat_wait.is_some() {
                    self.hint_rects = rects;
                }
            }
            (None, _) => {}
        }
        if let Some(l) = &mut self.launching {
            l.appear = approach(l.appear, 1.0, dt, 0.09);
            l.flight.step_spec(1.0, crate::anim::springs::LAUNCH, dt);
            l.flight.settle(1.0, 0.001, 0.01);
        }
        if let Some(l) = &self.launching {
            // The shelf that launched it still holds its decoded poster underneath.
            let poster = (self.stack.last())
                .and_then(crate::screens::Screen::shelf)
                .and_then(|lib| lib.poster(&l.host.id));
            self.draw_launch_hold(canvas, w, h, k, t, fonts, l, poster);
        }

        if self.toast.as_ref().is_some_and(|toast| t - toast.at > 4.0) {
            self.toast = None;
        }
        if let Some(toast) = &mut self.toast {
            let age = t - toast.at;
            if crate::theme::reduce_motion() {
                toast.seat = crate::anim::Spring::rest(1.0);
            } else {
                toast.seat.step_spec(1.0, springs::MODAL, dt);
                toast.seat.settle(1.0, 0.001, 0.01);
            }
            // Seat springs the slide. Fade stays linear: dismissal is a 4 s deadline, not a gesture.
            let slide = toast.seat.pos.clamp(0.0, 1.0);
            let fade = if age > 3.4 {
                (1.0 - (age - 3.4) / 0.6).max(0.0)
            } else {
                1.0
            };
            let alpha = (slide * fade) as f32;
            let (tint, mark) = toast.kind.look();
            let size = 13.0 * k;
            let tw = f64::from(fonts.measure(&toast.text, W::Medium, size));
            let (pad_x, bh) = (16.0 * k, 34.0 * k);
            // Kind mark, then text. Pad is 13 dp (shy of `pad_x`) because the 13 dp mark
            // never fills its box; equal pad leaves the pill left-heavy.
            let (mark_pad, mark_w, gap) = (13.0 * k, 13.0 * k, 9.0 * k);
            let lead = mark_pad + mark_w + gap;
            let bw = lead + tw + pad_x;
            let bx = (w - bw) / 2.0;
            let by = h - BOTTOM_BAND * k - bh - 8.0 * k + (1.0 - slide) * 12.0 * k;
            let rect = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32);
            // Bound the fade layer to the pill. Unbounded `save_layer` is a full-surface
            // offscreen every frame. 12 k outset is stroke slack (no blur to reach further).
            let bounds = rect.with_outset((12.0 * k as f32, 12.0 * k as f32));
            crate::theme::save_layer_alpha(canvas, Some(bounds), alpha);
            canvas.draw_rrect(
                skia_safe::RRect::new_rect_xy(rect, (bh / 2.0) as f32, (bh / 2.0) as f32),
                &fill(crate::theme::shade(0.6)),
            );
            crate::theme::panel(
                canvas,
                rect,
                (bh / 2.0 / k) as f32,
                None,
                PanelStroke::Plain(0.14),
                k as f32,
            );
            let cy = by + bh / 2.0;
            draw_toast_mark(canvas, mark, bx + mark_pad + mark_w / 2.0, cy, k, tint);
            fonts.draw(
                canvas,
                &toast.text,
                bx + lead,
                cy + size * 0.36,
                W::Medium,
                size,
                fg(0.92),
            );
            canvas.restore();
        }
    }
    /// Full-screen connect/wake takeover: aurora, optional spinner, title, one
    /// detail line, own hint row. `appear` = 1.0 when a wake hands off to a connect
    /// so the two never blink. Not a centered card.
    #[allow(clippy::too_many_arguments)]
    fn draw_takeover(
        &self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        appear: f64,
        t: f64,
        fonts: &Fonts,
        spinner: bool,
        title: &str,
        body: &str,
        hints: &[Hint],
    ) -> Vec<(HintKey, Rect)> {
        let cx = w / 2.0;
        // Only while it is arriving: an unbounded layer is a full-screen offscreen per frame,
        // and `appear` is at 1.0 within half a second of a hold that runs for many.
        if appear < 0.999 {
            crate::theme::save_layer_alpha(canvas, None, appear as f32);
        } else {
            canvas.save();
        }
        self.draw_takeover_field(canvas, w, h, t);

        let title_y = h / 2.0 + if spinner { 14.0 * k } else { 0.0 };
        if spinner {
            crate::theme::spinner(canvas, cx, title_y - 52.0 * k, 22.0 * k, t);
        }
        fonts.centered(
            canvas,
            title,
            W::SemiBold,
            23.0 * k,
            fg(1.0),
            cx,
            title_y,
            w * 0.82,
        );
        if !body.is_empty() {
            fonts.centered(
                canvas,
                body,
                W::Regular,
                14.0 * k,
                fg(0.55),
                cx,
                title_y + 32.0 * k,
                w * 0.66,
            );
        }
        let rects = self.draw_takeover_hints(canvas, w, h, k, fonts, hints);
        canvas.restore();
        rects
    }

    /// The speed test while it measures and once it has: the live figure, the burst's
    /// throughput over time, and the words under it. After the Apple client's speed page.
    #[allow(clippy::too_many_arguments)]
    fn draw_speed(
        &self,
        canvas: &Canvas,
        (w, h): (f64, f64),
        k: f64,
        t: f64,
        fonts: &Fonts,
        sp: &SpeedStatus,
        (title, body): (&str, &str),
        hints: &[Hint],
    ) {
        canvas.save();
        self.draw_takeover_field(canvas, w, h, t);
        let (done, rec) = measured(&sp.phase);
        // Measured, the figure is the headline: the caption names the host and the loss.
        let caption = match &sp.phase {
            SpeedPhase::Done { clean: Some(c), .. } => format!(
                "{} \u{b7} {:.1} % loss at {}",
                sp.name,
                c.loss_pct,
                mbps(c.rate_kbps)
            ),
            SpeedPhase::Done { .. } => sp.name.clone(),
            _ => title.to_string(),
        };
        let title = caption.as_str();
        let (cx, cw) = (w / 2.0, (620.0 * k).min(w - 2.0 * edge(k)));
        let ch = (180.0 * k).min(h * 0.34);
        // Caption, figure, chart and body, centred as one block.
        let top = (h - (96.0 * k + ch + 64.0 * k)) / 2.0;
        fonts.centered(canvas, title, W::SemiBold, 15.0 * k, fg(0.62), cx, top, cw);
        let figure = match (done, sp.trace.is_empty()) {
            (None, true) => "\u{2014}".into(),
            _ => mbps(self.speed_view.figure.round() as u32),
        };
        fonts.centered(
            canvas,
            &figure,
            W::Bold,
            44.0 * k,
            fg(1.0),
            cx,
            top + 22.0 * k,
            cw,
        );
        let chart = Rect::from_xywh(
            (cx - cw / 2.0) as f32,
            (top + 96.0 * k) as f32,
            cw as f32,
            ch as f32,
        );
        speed_chart(canvas, fonts, chart, &sp.trace, rec, &self.speed_view, k, t);
        let body_top = f64::from(chart.bottom) + 34.0 * k;
        fonts.centered(
            canvas,
            body,
            W::Regular,
            14.0 * k,
            fg(0.55),
            cx,
            body_top,
            cw,
        );
        self.draw_takeover_hints(canvas, w, h, k, fonts, hints);
        canvas.restore();
    }

    /// The takeover's ground: an opaque aurora — the home field, so this reads as the
    /// console taking over — with a shade pool under the centre so text separates from a
    /// bright field.
    ///
    /// Painted in SURFACE space, like the base aurora it covers: a backdrop that stops at
    /// the safe rect leaves the cutout strip carrying the frame's first aurora with no
    /// vignette over it, which reads as a lighter band with a hard edge. The pool still
    /// centres on the safe rect, because that is where the text it separates sits.
    fn draw_takeover_field(&self, canvas: &Canvas, w: f64, h: f64, t: f64) {
        let (left, top) = self.last_insets;
        let (fw, fh) = (f64::from(self.last_full.0), f64::from(self.last_full.1));
        canvas.save();
        canvas.translate((-left, -top));
        self.draw_aurora(canvas, fw, fh, t, 0.0);
        let mut vignette = crate::theme::shaded();
        let shades = [crate::theme::shade(0.5), crate::theme::shade(0.0)];
        vignette.set_shader(gradient::shaders::radial_gradient(
            (
                Point::new(left + (w / 2.0) as f32, top + (h / 2.0) as f32),
                (fw.max(fh) * 0.42) as f32,
            ),
            &gradient::Gradient::new(
                gradient::Colors::new_evenly_spaced(&shades, TileMode::Clamp, None),
                gradient::Interpolation::default(),
            ),
            None,
        ));
        canvas.draw_rect(Rect::from_wh(fw as f32, fh as f32), &vignette);
        canvas.restore();
    }

    /// The takeover's legend, centered where every console screen's sits. Returns what each
    /// hint's box covers.
    fn draw_takeover_hints(
        &self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        fonts: &Fonts,
        hints: &[Hint],
    ) -> Vec<(HintKey, Rect)> {
        if hints.is_empty() {
            return Vec::new();
        }
        let probe = hint_bar(canvas, fonts, hints, self.glyphs, -10_000.0, -10_000.0, k);
        hint_bar(
            canvas,
            fonts,
            hints,
            self.glyphs,
            w / 2.0 - probe.size.0 / 2.0,
            h - 34.0 * k,
            k,
        )
        .rects
    }

    /// The launch hold: the title's poster on the takeover field, its name and store
    /// beneath, a spinner for the wait. `appear` lifts the poster group in; the field
    /// itself is already on screen from the connect, so it never blinks.
    #[allow(clippy::too_many_arguments)]
    fn draw_launch_hold(
        &self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        t: f64,
        fonts: &Fonts,
        l: &Launching,
        poster: Option<&Image>,
    ) {
        // Fades in rather than replacing the shelf outright: the cover has to be seen
        // LEAVING its tile, which means the tile has to still be there when it does.
        if l.appear < 0.999 {
            crate::theme::save_layer_alpha(canvas, None, l.appear as f32);
        } else {
            canvas.save();
        }
        self.draw_takeover_field(canvas, w, h, t);
        canvas.restore();

        let a = l.appear as f32;
        let rows: Vec<(&String, f64, f32)> = [
            (&l.facts, 15.0, 0.62),
            (&l.developer, 14.0, 0.45),
            (&l.genres, 14.0, 0.45),
        ]
        .into_iter()
        .filter(|(text, _, _)| !text.is_empty())
        .collect();
        // Under the title: the rows, then the spinner line 38 k below the last row, or the
        // download's bar and two lines, 66 k.
        let downloading = l.download.as_ref().filter(|_| !l.connected);
        let tail = if downloading.is_some() { 66.0 } else { 38.0 };
        let rest = (11.0 + rows.iter().map(|(_, size, _)| size + 7.0).sum::<f64>() + tail) * k;
        let lay = launch_layout(w, h, k, rest, |dw| {
            fonts.title_height(&l.title, W::SemiBold, 34.0 * k, fg(a), dw)
        });
        let (cw, ch) = (lay.cw, lay.ch);
        let settled = Rect::from_xywh(lay.cover_x as f32, lay.cover_y as f32, cw as f32, ch as f32);
        // Where it flies from: its shelf tile, or — with no tile to leave — the settled rect
        // a little small, so the arrival still reads as one.
        let start = if l.from.is_empty() {
            settled.with_inset((settled.width() * 0.07, settled.height() * 0.07))
        } else {
            l.from
        };
        let p = l.flight.pos;
        let lerp = |a: f64, b: f64| a + (b - a) * p;

        canvas.save();
        // One full turn on the way over, about the card's own vertical axis and through the
        // same projection the coverflow tilts its side cards with — so the near edge grows
        // and the far one recedes instead of the face merely narrowing.
        let m = card_matrix(
            lerp(f64::from(start.center_x()), f64::from(settled.center_x())),
            lerp(f64::from(start.center_y()), f64::from(settled.center_y())),
            360.0 * p,
            lerp(f64::from(start.width()) / cw.max(1.0), 1.0),
            cw,
            ch,
            PERSPECTIVE * k,
        );
        canvas.concat_44(&M44::row_major(&m));
        let card = Rect::from_wh(cw as f32, ch as f32);
        let corner = (14.0 * k) as f32;
        let mut shadow = fill(crate::theme::shade(0.6));
        shadow.set_mask_filter(skia_safe::MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            (18.0 * k) as f32,
            None,
        ));
        canvas.draw_rrect(
            RRect::new_rect_xy(card.with_offset((0.0, (12.0 * k) as f32)), corner, corner),
            &shadow,
        );
        match poster {
            Some(img) => {
                canvas.save();
                canvas.clip_rrect(RRect::new_rect_xy(card, corner, corner), None, true);
                // Cover-fit: crop the source to the card's aspect, centred.
                let (iw, ih) = (f64::from(img.width()), f64::from(img.height()));
                let scale = (cw / iw).max(ch / ih);
                let (sw, sh) = (cw / scale, ch / scale);
                let src = Rect::from_xywh(
                    ((iw - sw) / 2.0) as f32,
                    ((ih - sh) / 2.0) as f32,
                    sw as f32,
                    sh as f32,
                );
                canvas.draw_image_rect_with_sampling_options(
                    img,
                    Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                    card,
                    crate::theme::art_sampling(),
                    &fill(fg(1.0)),
                );
                canvas.restore();
            }
            None => {
                crate::screens::library::draw_poster_placeholder(canvas, fonts, None, card, k, 1.0)
            }
        }
        canvas.draw_rrect(
            RRect::new_rect_xy(card, corner, corner),
            &crate::theme::stroke(fg(0.14), 1.0),
        );
        canvas.restore();

        // The column, placed from the title's measured height: a wrapped title would
        // otherwise be painted over by the rows placed for one line.
        let (dx, dw) = (lay.col_x, lay.dw);
        fonts.title(
            canvas,
            &l.title,
            W::SemiBold,
            34.0 * k,
            fg(a),
            dx,
            lay.col_y,
            dw,
        );
        let mut y = lay.col_y + lay.title_h + 11.0 * k;
        for (text, size, alpha) in rows {
            fonts.leading(canvas, text, W::Regular, size * k, fg(alpha * a), dx, y, dw);
            y += (size + 7.0) * k;
        }
        match (l.failed.as_deref(), downloading) {
            // Where the spinner was, because the wait is what ended. Brighter than the status
            // line it replaces: this is the one thing on screen the player has to read.
            (Some(why), _) => {
                fonts.leading(
                    canvas,
                    why,
                    W::Regular,
                    12.5 * k,
                    fg(0.85 * a),
                    dx,
                    y + 26.0 * k,
                    dw,
                );
            }
            (None, Some(d)) => {
                // A bar where the spinner was: the wait has a length now, and the player can
                // see it move.
                let bar = Rect::from_xywh(
                    dx as f32,
                    (y + 24.0 * k) as f32,
                    dw.min(360.0 * k) as f32,
                    (4.0 * k) as f32,
                );
                let r = (2.0 * k) as f32;
                canvas.draw_rrect(RRect::new_rect_xy(bar, r, r), &fill(fg(0.14 * a)));
                let (from, width) = match d.fraction() {
                    Some(f) => (0.0, f as f32),
                    // No total: a segment travels the track.
                    None => (((t * 0.6).fract() * 0.75) as f32, 0.25),
                };
                let done = Rect::from_xywh(
                    bar.left + bar.width() * from,
                    bar.top,
                    bar.width() * width,
                    bar.height(),
                );
                canvas.draw_rrect(RRect::new_rect_xy(done, r, r), &fill(fg(0.85 * a)));
                let line = if d.state == "downloading" {
                    format!("Downloading \u{b7} {}", d.line())
                } else {
                    d.line()
                };
                fonts.leading(
                    canvas,
                    &line,
                    W::Regular,
                    12.5 * k,
                    fg(0.7 * a),
                    dx,
                    y + 36.0 * k,
                    dw,
                );
                fonts.leading(
                    canvas,
                    "Leaving won't stop the download.",
                    W::Regular,
                    11.5 * k,
                    fg(0.4 * a),
                    dx,
                    y + 54.0 * k,
                    dw,
                );
            }
            (None, None) => {
                crate::theme::spinner(canvas, dx + 8.0 * k, y + 30.0 * k, 8.0 * k, t);
                fonts.leading(
                    canvas,
                    if l.window_wait {
                        "Waiting for the game's window\u{2026}"
                    } else if l.connected {
                        "Starting the game\u{2026}"
                    } else {
                        "Connecting\u{2026}"
                    },
                    W::Regular,
                    12.5 * k,
                    fg(0.5 * a),
                    dx + 24.0 * k,
                    y + 22.0 * k,
                    dw,
                );
            }
        }
        // Before the dial lands B cancels it, exactly as it does on the connect card;
        // after, the only thing left to ask for is the picture.
        let hint = if l.failed.is_some() {
            Hint::new(HintKey::Confirm, "Show the desktop anyway")
        } else if l.connected {
            Hint::new(HintKey::Confirm, "Show stream")
        } else {
            Hint::new(HintKey::Back, "Cancel")
        };
        self.draw_takeover_hints(canvas, w, h, k, fonts, &[hint]);
    }
}

/// Where the launch hold puts its cover and text column, in layout pixels.
struct LaunchLayout {
    cover_x: f64,
    cover_y: f64,
    cw: f64,
    ch: f64,
    col_x: f64,
    col_y: f64,
    dw: f64,
    title_h: f64,
}

/// Landscape: a 2:3 cover and a text column beside it, one centred pair. Portrait, or a
/// window too narrow for the pair: the cover on top, the column under it, the cover shrunk
/// before anything leaves the screen. `title_h(dw)` measures the title at a column width;
/// `rest` is the height of everything under the title.
fn launch_layout(w: f64, h: f64, k: f64, rest: f64, title_h: impl Fn(f64) -> f64) -> LaunchLayout {
    let margin = (w * 0.06).max(24.0 * k);
    let gap = (w * 0.04).min(56.0 * k);
    let ch = (h * 0.62).min(460.0 * k);
    let cw = ch * 2.0 / 3.0;
    let dw = (w * 0.34).clamp(260.0 * k, 460.0 * k);
    if w >= h && cw + gap + dw <= w - 2.0 * margin {
        let cover_x = (w - (cw + gap + dw)) / 2.0;
        let cover_y = (h - ch) / 2.0;
        let title_h = title_h(dw);
        // Centred on the cover's height: a block hung from the top of a card this
        // tall reads as fallen off it.
        let col_y = cover_y + (ch - title_h - rest).max(0.0) / 2.0;
        return LaunchLayout {
            cover_x,
            cover_y,
            cw,
            ch,
            col_x: cover_x + cw + gap,
            col_y,
            dw,
            title_h,
        };
    }
    let dw = w - 2.0 * margin;
    let title_h = title_h(dw);
    let text = title_h + rest;
    // The hint row owns the bottom band; the stack is centred in what is left.
    let avail = h - margin - BOTTOM_BAND * k;
    let ch = (h * 0.4)
        .min(460.0 * k)
        .min(avail - gap - text)
        .min(dw * 1.5)
        .max(0.0);
    let cw = ch * 2.0 / 3.0;
    let cover_y = margin + (avail - margin - (ch + gap + text)).max(0.0) / 2.0;
    LaunchLayout {
        cover_x: (w - cw) / 2.0,
        cover_y,
        cw,
        ch,
        col_x: margin,
        col_y: cover_y + ch + gap,
        dw,
        title_h,
    }
}

/// The headline once there is an answer: what the link carries, and the clean round's loss
/// and jitter at its rate. Without a round (an older host) the figure stands alone — a
/// blast's loss is not the link's.
fn speed_headline(
    ceiling_kbps: u32,
    wall: bool,
    clean: Option<&crate::model::CleanRound>,
) -> String {
    let carries = if wall {
        format!("Link carries {}.", mbps(ceiling_kbps))
    } else {
        format!("Link carries at least {}.", mbps(ceiling_kbps))
    };
    match clean {
        Some(c) => format!(
            "{carries} At {}: {:.1} % loss, {:.1} ms jitter",
            mbps(c.rate_kbps),
            c.loss_pct,
            f64::from(c.jitter_us) / 1000.0
        ),
        None => format!("{} measured", mbps(ceiling_kbps)),
    }
}

/// One finding in words ([`pf_client_core::findings::text`]). The offered profile is the
/// Secondary hint, not a sentence here.
pub(crate) fn finding_text(f: &crate::model::FindingRow) -> String {
    pf_client_core::findings::text(f.id, f.numbers)
}

/// The measured rate and its recommendation, once there is an answer.
fn measured(phase: &SpeedPhase) -> (Option<u32>, Option<u32>) {
    match *phase {
        SpeedPhase::Done {
            throughput_kbps,
            recommended_kbps,
            ..
        } => (Some(throughput_kbps), Some(recommended_kbps)),
        _ => (None, None),
    }
}

/// A rate as people read it: tenths under 10 Mb/s, whole megabits above, gigabits past 1000.
fn mbps(kbps: u32) -> String {
    match kbps {
        0..10_000 => format!("{:.1} Mb/s", f64::from(kbps) / 1_000.0),
        10_000..1_000_000 => format!("{} Mb/s", kbps / 1_000),
        _ => format!("{:.1} Gb/s", f64::from(kbps) / 1_000_000.0),
    }
}

/// Past its last sample, a live line runs on at most this long: a late poll holds it, it does
/// not run away.
const RUN_ON_S: f64 = 0.4;

/// The speed chart as drawn this frame. Samples land a few times a second, so the line runs
/// on to now, its head glides onto each new sample, and the scales and figure ease.
#[derive(Default)]
pub(in crate::shell) struct SpeedView {
    /// The test this view follows; another starts it over.
    key: Option<String>,
    samples: usize,
    /// Seconds since measuring began that the line reaches.
    now: f64,
    /// The rate at the head of the line, and the headline figure, kbps.
    head: f64,
    figure: f64,
    x_max: f64,
    y_max: f64,
    /// The round scale the gridlines are labelled with.
    y_round: u32,
}

impl SpeedView {
    /// Chase `sp` by `dt`; `done` is the measured rate, `rec` the recommendation.
    pub(in crate::shell) fn step(
        &mut self,
        sp: &SpeedStatus,
        done: Option<u32>,
        rec: Option<u32>,
        dt: f64,
    ) {
        if self.key.as_deref() != Some(sp.key.as_str()) || sp.trace.len() < self.samples {
            let (x, y) = chart_scale(&[], 0);
            *self = SpeedView {
                key: Some(sp.key.clone()),
                x_max: f64::from(x),
                y_max: f64::from(y),
                y_round: y,
                ..SpeedView::default()
            };
        }
        self.samples = sp.trace.len();
        let (last_s, last) = sp
            .trace
            .last()
            .map_or((0.0, 0), |&(s, v)| (f64::from(s), v));
        let to = match (done, sp.trace_start) {
            (None, Some(t0)) => t0.elapsed().as_secs_f64().clamp(last_s, last_s + RUN_ON_S),
            _ => last_s,
        };
        self.now = approach(self.now, to, dt, 0.05);
        self.head = approach(self.head, f64::from(last), dt, 0.08);
        self.figure = approach(self.figure, f64::from(done.unwrap_or(last)), dt, 0.1);
        let peak = sp.trace.iter().map(|p| p.1).chain(rec).max().unwrap_or(0);
        let (_, y) = chart_scale(&sp.trace, peak);
        self.y_round = y;
        self.x_max = approach(self.x_max, self.now.max(2.0).ceil(), dt, 0.15);
        self.y_max = approach(self.y_max, f64::from(y), dt, 0.15);
    }

    /// The samples as drawn: the last one at the head's rate, the line run on to now.
    fn points(&self, trace: &[(f32, u32)]) -> Vec<(f32, u32)> {
        let mut out = trace.to_vec();
        let head = self.head.round() as u32;
        if let Some(last) = out.last_mut() {
            last.1 = head;
            if self.now > f64::from(last.0) {
                out.push((self.now as f32, head));
            }
        }
        out
    }
}

/// The chart's reach: whole seconds, at least two, and a round rate over the peak with room
/// above it, so the gridline labels read as round numbers.
fn chart_scale(trace: &[(f32, u32)], peak: u32) -> (f32, u32) {
    let x_max = trace.last().map_or(2.0, |p| p.0).max(2.0).ceil();
    let want = f64::from(peak.max(1_000)) * 1.05;
    let mag = 10f64.powf(want.log10().floor());
    let step = [1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0]
        .into_iter()
        .map(|m| m * mag)
        .find(|v| *v >= want)
        .unwrap_or(10.0 * mag);
    (x_max, step as u32)
}

/// Trace points in `r`, from the origin: measuring starts at nothing.
fn chart_points(trace: &[(f32, u32)], r: Rect, x_max: f32, y_max: f32) -> Vec<Point> {
    let at = |(s, kbps): (f32, u32)| {
        let y = r.bottom - r.height() * (kbps as f32 / y_max).min(1.0);
        Point::new(r.left + r.width() * (s / x_max).min(1.0), y)
    };
    std::iter::once((0.0, 0))
        .chain(trace.iter().copied())
        .map(at)
        .collect()
}

/// The burst's throughput over time in `r`, as `view` has it this frame: a smoothed line
/// over a fading fill, a faint grid at round rates that slides as the scale eases, and once
/// measured the recommendation as a dashed green rule. While it measures, the head pulses.
#[allow(clippy::too_many_arguments)]
fn speed_chart(
    canvas: &Canvas,
    fonts: &Fonts,
    r: Rect,
    trace: &[(f32, u32)],
    rec: Option<u32>,
    view: &SpeedView,
    k: f64,
    t: f64,
) {
    let kf = k as f32;
    let (x_max, y_max) = (view.x_max as f32, view.y_max as f32);
    let grid = crate::theme::stroke(fg(0.16), kf);
    for i in 0..=2u32 {
        let value = view.y_round / 2 * i;
        let y = r.bottom - r.height() * value as f32 / y_max;
        if y < r.top - 1.0 {
            continue;
        }
        canvas.draw_line((r.left, y), (r.right, y), &grid);
        if i > 0 {
            let label = mbps(value);
            let base = f64::from(y) - 6.0 * k;
            fonts.draw(
                canvas,
                &label,
                f64::from(r.left),
                base,
                W::Medium,
                11.0 * k,
                fg(0.45),
            );
        }
    }
    let axis = f64::from(r.bottom) + 18.0 * k;
    fonts.draw(
        canvas,
        "0 s",
        f64::from(r.left),
        axis,
        W::Medium,
        11.0 * k,
        fg(0.45),
    );
    let end = format!("{:.0} s", view.x_max.round());
    let end_w = f64::from(fonts.measure(&end, W::Medium, 11.0 * k));
    let end_x = f64::from(r.right) - end_w;
    fonts.draw(canvas, &end, end_x, axis, W::Medium, 11.0 * k, fg(0.45));
    if trace.is_empty() {
        if rec.is_none() {
            crate::theme::spinner(
                canvas,
                f64::from(r.center_x()),
                f64::from(r.center_y()),
                18.0 * k,
                t,
            );
        }
        return;
    }
    let pts = chart_points(&view.points(trace), r, x_max, y_max);
    // Through the midpoints, so the line bends where the samples turn and never overshoots.
    let curve = |path: &mut PathBuilder| {
        path.move_to(pts[0]);
        for pair in pts.windows(2).skip(1) {
            let mid = Point::new((pair[0].x + pair[1].x) / 2.0, (pair[0].y + pair[1].y) / 2.0);
            path.quad_to(pair[0], mid);
        }
        path.line_to(*pts.last().expect("the origin at least"));
    };
    let last = *pts.last().expect("the origin at least");
    let mut area = PathBuilder::new();
    curve(&mut area);
    area.line_to((last.x, r.bottom));
    area.line_to((pts[0].x, r.bottom));
    area.close();
    let mut fade = fill(fg(1.0));
    fade.set_shader(gradient::shaders::linear_gradient(
        (Point::new(0.0, r.top), Point::new(0.0, r.bottom)),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&[fg(0.30), fg(0.0)], TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_path(&area.detach(), &fade);
    let mut line = PathBuilder::new();
    curve(&mut line);
    let mut ink = crate::theme::stroke(fg(0.95), 2.5 * kf);
    ink.set_stroke_cap(skia_safe::paint::Cap::Round);
    ink.set_stroke_join(skia_safe::paint::Join::Round);
    canvas.draw_path(&line.detach(), &ink);
    match rec {
        Some(rec) => {
            let y = r.bottom - r.height() * (rec as f32 / y_max).min(1.0);
            let mut rule = crate::theme::stroke(crate::theme::ONLINE_GREEN, 1.5 * kf);
            rule.set_path_effect(skia_safe::PathEffect::dash(&[6.0 * kf, 4.0 * kf], 0.0));
            canvas.draw_line((r.left, y), (r.right, y), &rule);
            let label = format!("Recommended {}", mbps(rec));
            let lw = f64::from(fonts.measure(&label, W::SemiBold, 12.0 * k));
            let (x, base) = (f64::from(r.right) - lw, f64::from(y) - 6.0 * k);
            let green = crate::theme::ONLINE_GREEN;
            fonts.draw(canvas, &label, x, base, W::SemiBold, 12.0 * k, green);
        }
        None => {
            // The newest sample breathes while more are coming.
            let pulse = (t * 1.4).fract() as f32;
            let halo = fg(0.5 * (1.0 - pulse));
            canvas.draw_circle(last, (4.0 + 10.0 * pulse) * kf, &fill(halo));
            canvas.draw_circle(last, 4.0 * kf, &fill(fg(1.0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new sample glides the head up rather than jumping to it, the scale eases toward its
    /// round step, and once measured the view rests exactly on the samples.
    #[test]
    fn the_speed_chart_eases_onto_each_sample() {
        let mut sp = SpeedStatus::new("aa11".into(), "Box".into());
        sp.trace = vec![(0.25, 100_000)];
        let mut view = SpeedView::default();
        for _ in 0..60 {
            view.step(&sp, None, None, 1.0 / 60.0);
        }
        sp.trace.push((0.5, 400_000));
        view.step(&sp, None, None, 1.0 / 60.0);
        assert!(
            view.head > 100_000.0 && view.head < 400_000.0,
            "glides: {}",
            view.head
        );
        assert!(view.y_max < 450_000.0, "the scale eases: {}", view.y_max);
        let (done, rec) = (Some(400_000), Some(280_000));
        for _ in 0..240 {
            view.step(&sp, done, rec, 1.0 / 60.0);
        }
        assert!((view.head - 400_000.0).abs() < 1.0);
        assert!((view.figure - 400_000.0).abs() < 1.0);
        assert_eq!(
            view.points(&sp.trace),
            sp.trace,
            "measured, the line is the samples"
        );
    }

    /// Rates read as people say them, and the chart's scale lands on round numbers over the
    /// peak, starting from the origin.
    #[test]
    fn the_speed_chart_scales_to_round_numbers() {
        assert_eq!(mbps(8_500), "8.5 Mb/s");
        assert_eq!(mbps(876_000), "876 Mb/s");
        assert_eq!(mbps(1_250_000), "1.2 Gb/s");
        let trace = [(0.5, 300_000), (1.0, 850_000), (2.4, 870_000)];
        let (x_max, y_max) = chart_scale(&trace, 870_000);
        assert_eq!((x_max, y_max), (3.0, 1_000_000));
        assert_eq!(chart_scale(&[], 0), (2.0, 1_500));
        let r = Rect::from_xywh(0.0, 0.0, 300.0, 100.0);
        let pts = chart_points(&trace, r, x_max, y_max as f32);
        assert_eq!(pts.len(), 4);
        assert_eq!(pts[0], Point::new(0.0, 100.0));
        assert!(
            (pts[2].y - 15.0).abs() < 0.01,
            "850 of 1000 Mb/s: {}",
            pts[2].y
        );
    }

    /// Two title lines at 34 k, the paragraph's line height.
    fn two_lines(k: f64) -> impl Fn(f64) -> f64 {
        move |_| 2.0 * 41.0 * k
    }

    fn inside(l: &LaunchLayout, w: f64, h: f64, rest: f64) {
        assert!(l.cover_x >= 0.0, "cover left {}", l.cover_x);
        assert!(l.cover_x + l.cw <= w, "cover right {}", l.cover_x + l.cw);
        assert!(
            l.col_x >= 0.0 && l.col_x + l.dw <= w,
            "column {}..{}",
            l.col_x,
            l.dw
        );
        assert!(
            l.col_y + l.title_h + rest <= h,
            "text bottom {}",
            l.col_y + l.title_h + rest
        );
        assert!(
            l.cover_y >= 0.0 && l.cover_y + l.ch <= h,
            "cover {}..{}",
            l.cover_y,
            l.ch
        );
    }

    /// A portrait phone: the pair does not fit side by side, so the cover sits on top of
    /// the column instead of past the left edge.
    #[test]
    fn portrait_stacks_the_cover_over_the_column() {
        let (w, h, k) = (1440.0, 3216.0, 3.0);
        let rest = 120.0 * k;
        let l = launch_layout(w, h, k, rest, two_lines(k));
        inside(&l, w, h, rest);
        assert!(l.cover_y + l.ch <= l.col_y, "column starts under the cover");
        assert!(l.dw > w * 0.8, "the column takes the width: {}", l.dw);
        assert!(l.ch > 0.3 * h, "the cover keeps its size: {}", l.ch);
    }

    /// A landscape screen keeps the pair, centred, with the column past the cover.
    #[test]
    fn landscape_keeps_the_pair() {
        let (w, h, k) = (1920.0, 1080.0, 1.35);
        let rest = 120.0 * k;
        let l = launch_layout(w, h, k, rest, two_lines(k));
        inside(&l, w, h, rest);
        assert!(l.col_x >= l.cover_x + l.cw, "column beside the cover");
        assert!(
            (l.cover_x - (w - (l.col_x + l.dw))).abs() < 1.0,
            "pair is centred"
        );
    }

    /// A narrow landscape window that cannot hold the pair stacks too, on screen.
    #[test]
    fn a_narrow_window_never_puts_the_cover_off_screen() {
        let (w, h, k) = (400.0, 380.0, 0.75);
        let rest = 120.0 * k;
        let l = launch_layout(w, h, k, rest, two_lines(k));
        inside(&l, w, h, rest);
        assert!(l.cover_y + l.ch <= l.col_y);
    }
}
