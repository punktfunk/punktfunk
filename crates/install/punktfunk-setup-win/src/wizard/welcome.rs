//! Welcome: the lockup, the Recommended / Custom choice, and the manage-mode buttons.

use super::*;
use std::time::Duration;

/// One of the two setup options on Welcome: a radio in a card, the whole card tappable. The
/// chosen one shows an accent ring drawn OVER the card (a stroke-only border, so taps pass
/// through) that fades in and out — the card itself never changes size, so nothing shifts.
fn option_card(
    w: &Wizard,
    cx: &ViewContext<'_, Wizard>,
    custom: bool,
    title: &str,
    detail: &str,
) -> Grid {
    let selected = w.custom == custom;
    let card = Border::new()
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(CornerRadius::uniform(10.0))
        .padding(Thickness::new(16.0, 12.0, 16.0, 14.0))
        .on_pointer_released(cx.callback(move |_| Msg::Choose(custom)))
        .content(
            StackPanel::new().spacing(2.0).children((
                RadioButton::new()
                    .content(title)
                    .group_name("setup")
                    .is_checked(selected)
                    .on_checked(cx.callback(move |_| Msg::Choose(custom))),
                note(w, detail)
                    .font_size(12.5)
                    .margin(Thickness::new(30.0, 0.0, 0.0, 0.0)),
            )),
        );
    let ring = Border::new()
        .border_brush(ThemeBrush::Accent)
        .border_thickness(Thickness::uniform(2.0))
        .corner_radius(CornerRadius::uniform(10.0))
        .opacity(if selected { 1.0 } else { 0.0 })
        .opacity_transition(Duration::from_millis(180));
    Grid::new().children((card, ring))
}

/// The brand lockup — the mark above the "funk" wordmark — with the website's one-shot
/// intro (pfweb `BrandMark.tsx` / `Wordmark.tsx`) at progress `t` (0 → 1 over 1.3 s):
/// the two circles orbit once on an axis into the screen, scaling in antiphase with a
/// small diagonal sway that vanishes at rest, the lens highlight fades in over the tail,
/// and the letters slide in from the right, staggered. Everything lands on the static mark,
/// in the accent's `shades`.
fn lockup(t: f64, shades: brand::Shades) -> View {
    use std::f64::consts::{FRAC_1_SQRT_2, TAU};
    // The site's geometry: circles r=194.41 in a 1000 box, cropped to the mark's own box.
    const R: f64 = 194.41;
    const BOX: f64 = 583.776;
    const ORIGIN: (f64, f64) = (208.627, 208.443);
    let k = MARK / BOX;
    let angle = TAU * (1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(4));
    let circle = |cx: f64, cy: f64, side: f64, color: Color| -> (f64, View) {
        let z = side * angle.sin() * 0.34;
        let p = 1.05 / (1.05 - z);
        let mag = side * 0.06 * (angle.cos() - 1.0);
        let (tx, ty) = (
            mag * -FRAC_1_SQRT_2 * 1000.0 * p,
            mag * FRAC_1_SQRT_2 * 1000.0 * p,
        );
        let d = 2.0 * R * k * p;
        let x = (cx - ORIGIN.0 + tx) * k - d / 2.0;
        let y = (cy - ORIGIN.1 + ty) * k - d / 2.0;
        (
            p,
            Ellipse::new()
                .fill(color)
                .width(d)
                .height(d)
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(Thickness::new(x, y, 0.0, 0.0))
                .into(),
        )
    };
    let light = circle(403.037, 597.262, 1.0, shades.light);
    let deep = circle(597.808, 402.853, -1.0, shades.deep);
    // The nearer circle paints on top; the crisp overlap only reads once they settle.
    let mut layers: Vec<View> = if light.0 <= deep.0 {
        vec![light.1, deep.1]
    } else {
        vec![deep.1, light.1]
    };
    let h = ((t - 0.6) / 0.346).clamp(0.0, 1.0);
    let h = 1.0 - (1.0 - h).powi(3);
    let size = MARK * (0.6 + 0.4 * h);
    layers.push(
        Image::new()
            .source_svg(brand::lens(shades.lens))
            .stretch(Stretch::Uniform)
            .width(size)
            .height(size)
            .opacity(h)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    );
    let mark = Grid::new().width(MARK).height(MARK).children(layers);

    // Letters: each slides in from its own width, 0.55 s from 0.15 s + 90 ms per letter.
    const LETTER_W: [f64; 4] = [108.54, 133.33, 142.87, 141.79];
    let wk = WORD / 579.0;
    let wh = 136.0 * wk;
    let letters: Vec<View> = (0..4)
        .map(|i| {
            let start = (0.15 + 0.09 * i as f64) / 1.3;
            let e = ((t - start) / (0.55 / 1.3)).clamp(0.0, 1.0);
            let e = 1.0 - (1.0 - e).powi(5);
            let off = (1.0 - e) * LETTER_W[i] * wk;
            Image::new()
                .source_svg(brand::letter(shades.letters, i))
                .stretch(Stretch::Uniform)
                .width(WORD)
                .height(wh)
                .opacity(e)
                .margin(Thickness::new(off, 0.0, -off, 0.0))
                .into()
        })
        .collect();
    let word = Grid::new().width(WORD).height(wh).children(letters);
    StackPanel::new()
        .spacing(10.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .children((mark, word))
        .into()
}

pub(super) fn welcome_page(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> View {
    let uninstall = || -> View {
        Button::new()
            .content(tools::wizard_uninstall())
            .on_click(cx.message(Msg::Uninstall))
            .into()
    };
    // Manage mode (D9): an installed box re-titles Welcome. The uninstaller exe (D6) offers
    // the teardown only; the installer offers Reconfigure — the upgrade path — or Uninstall.
    let installed = w.screen.facts.installed_for(w.preset.artifact);
    let subtitle = installed.map(|inst| match (&inst.version, w.preset.artifact) {
        (Some(v), Artifact::Host) => tools::wizard_installed_host_version(v),
        (Some(v), Artifact::Client) => tools::wizard_installed_client_version(v),
        (None, Artifact::Host) => tools::wizard_installed_host().to_string(),
        (None, Artifact::Client) => tools::wizard_installed_client().to_string(),
    });
    let fresh_install = !w.preset.uninstall && installed.is_none();
    let (sentence, buttons): (&str, Vec<View>) = if w.preset.uninstall {
        (tools::wizard_welcome_uninstall(), vec![uninstall()])
    } else if !fresh_install {
        (
            tools::wizard_welcome_manage(),
            vec![
                uninstall(),
                Button::new()
                    .content(tools::wizard_reconfigure())
                    .style(ButtonStyle::Accent)
                    .min_width(124.0)
                    .on_click(cx.message(Msg::Reconfigure))
                    .into(),
            ],
        )
    } else {
        (
            match w.preset.artifact {
                Artifact::Host => tools::wizard_welcome_host(),
                Artifact::Client => tools::wizard_welcome_client(),
            },
            vec![continue_button(
                cx,
                WizStep::Welcome,
                w.go_label(WizStep::Welcome),
            )],
        )
    };
    let mut children: Vec<View> = vec![lockup(w.logo.linear(), brand::Shades::of(&w.palette))];
    if let Some(subtitle) = subtitle {
        children.push(
            note(w, subtitle)
                .font_size(16.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    children.push(
        note(w, sentence)
            .font_size(15.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .max_width(520.0)
            .into(),
    );
    if fresh_install {
        let (recommended, custom) = match w.preset.artifact {
            Artifact::Host => (
                tools::wizard_recommended_host(),
                tools::wizard_custom_host(),
            ),
            Artifact::Client => (
                tools::wizard_recommended_client(),
                tools::wizard_custom_client(),
            ),
        };
        children.push(
            Grid::new()
                .columns([GridLength::STAR, GridLength::STAR])
                .column_spacing(12.0)
                .max_width(720.0)
                .margin(Thickness::new(0.0, 10.0, 0.0, 0.0))
                .children((
                    option_card(w, cx, false, tools::wizard_recommended(), recommended)
                        .grid_column(0),
                    option_card(w, cx, true, tools::wizard_custom(), custom).grid_column(1),
                ))
                .into(),
        );
    }
    let content = StackPanel::new()
        .spacing(16.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .children(children);
    frame(w, WizStep::Welcome, content, buttons)
}
