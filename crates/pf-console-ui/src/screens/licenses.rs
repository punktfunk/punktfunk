//! Open-source licences, drawn by the console on every platform that shows the row. The
//! console's own texts are compiled in: Punktfunk's licence and the Geist typeface it
//! embeds, whose OFL has to travel with it. Each host adds what it bundles
//! ([`LicenseSection`]), asked for with [`ConsoleCmd::LoadLicenses`] when the screen opens.
//!
//! A host's notices run to ~14 000 lines, so lines wrap once per width and only the ones
//! on screen are drawn.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, LicenseSection};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox};
use crate::theme::{edge, fg, Fonts, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::shortcuts::{self, Client};
use skia_safe::{Canvas, Rect};

const MIT: &str = include_str!("../../../../LICENSE-MIT");
const APACHE: &str = include_str!("../../../../LICENSE-APACHE");
const GEIST_OFL: &str = include_str!("../../assets/fonts/Geist-OFL.txt");

#[derive(Clone, Copy, PartialEq, Debug)]
enum Style {
    Title,
    Heading,
    /// A control's keys, bold over the line that says what they do.
    Keys,
    Body,
    /// A licence file's own text: smaller and quieter than the prose around it.
    Text,
}

impl Style {
    /// Size, weight and alpha, in design units.
    fn look(self) -> (f64, W, f32) {
        match self {
            Style::Title => (26.0, W::Bold, 1.0),
            Style::Heading => (18.0, W::SemiBold, 0.95),
            Style::Keys => (16.0, W::SemiBold, 1.0),
            Style::Body => (14.0, W::Regular, 0.8),
            Style::Text => (12.0, W::Regular, 0.62),
        }
    }

    fn line_height(self, k: f64) -> f64 {
        self.look().0 * 1.45 * k
    }
}

struct Line {
    text: String,
    style: Style,
}

/// Up and Down scroll this many body lines, Left and Right a page (L1/R1 are the tabs).
/// A fling decays at this rate per second.
const STEP_LINES: f64 = 3.0;
const FLING_DECAY: f64 = 4.0;

/// What the screen scrolls.
#[derive(Clone, Copy, PartialEq)]
enum Doc {
    Licenses,
    /// The stream's keys, chords and gestures for this client ([`pf_client_core::shortcuts`]).
    Controls(Client, bool),
}

/// Scrolling read-only text: the licences, or the stream controls.
pub(crate) struct LicensesScreen {
    doc: Doc,
    /// The host's sections; `None` until they arrive.
    host: Option<Vec<LicenseSection>>,
    lines: Vec<Line>,
    /// Each line's top, plus the total height last, px.
    tops: Vec<f64>,
    /// What `lines` were laid out for: width, scale, and whether the host's part was in.
    laid: Option<(f32, f64, bool)>,
    scroll: f64,
    velocity: f64,
    view_h: f64,
    step: f64,
}

impl LicensesScreen {
    /// Opens asking the host for its sections; the shell hands them over once in.
    pub(crate) fn new(fx: &mut Outbox) -> LicensesScreen {
        fx.cmds.push(ConsoleCmd::LoadLicenses);
        LicensesScreen {
            doc: Doc::Licenses,
            host: None,
            lines: Vec::new(),
            tops: Vec::new(),
            laid: None,
            scroll: 0.0,
            velocity: 0.0,
            view_h: 0.0,
            step: 0.0,
        }
    }

    /// The stream controls this platform has. Every platform has a list today.
    pub(crate) fn controls(ctx: &Ctx) -> Option<LicensesScreen> {
        use crate::platform::Platform;
        let client = match ctx.device.platform {
            Platform::Desktop => Client::Desktop,
            Platform::Android => Client::Android,
            Platform::Apple => Client::Apple,
            Platform::WebOS => Client::WebOS,
            Platform::Web => Client::Web,
            Platform::Tizen => Client::Tizen,
        };
        Some(LicensesScreen {
            doc: Doc::Controls(client, !ctx.device.tv),
            host: None,
            lines: Vec::new(),
            tops: Vec::new(),
            laid: None,
            scroll: 0.0,
            velocity: 0.0,
            view_h: 0.0,
            step: 0.0,
        })
    }

    pub(crate) fn title(&self) -> &'static str {
        match self.doc {
            Doc::Licenses => "Open-source licences",
            Doc::Controls(..) => "Stream controls",
        }
    }

    pub(crate) fn set_host(&mut self, sections: Vec<LicenseSection>) {
        self.host = Some(sections);
    }

    pub(crate) fn waiting(&self) -> bool {
        self.doc == Doc::Licenses && self.host.is_none()
    }

    #[cfg(test)]
    pub(crate) fn scrolled(&self) -> f64 {
        self.scroll
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        _ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let page = (self.view_h - self.step).max(self.step);
        let by = match ev {
            MenuEvent::Back => {
                fx.pop();
                return None;
            }
            MenuEvent::Move(MenuDir::Up) => -STEP_LINES * self.step,
            MenuEvent::Move(MenuDir::Down) => STEP_LINES * self.step,
            MenuEvent::Move(MenuDir::Left) => -page,
            MenuEvent::Move(MenuDir::Right) => page,
            _ => return None,
        };
        self.velocity = 0.0;
        Some(if self.scroll_by(by) {
            MenuPulse::Move
        } else {
            MenuPulse::Boundary
        })
    }

    /// A wheel step scrolls; every other press lands on the page and does nothing.
    pub(crate) fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        if let PointerKind::Scroll { up } = p.kind {
            let by = STEP_LINES * self.step;
            self.velocity = 0.0;
            self.scroll_by(if up { -by } else { by });
        }
        true
    }

    /// A vertical drag moves the text with the finger; its fling coasts.
    pub(crate) fn pan(&mut self, p: Pointer) -> bool {
        match p.kind {
            PointerKind::PanStart { horizontal } => {
                self.velocity = 0.0;
                !horizontal
            }
            PointerKind::Pan { dy, .. } => {
                self.scroll_by(-dy);
                true
            }
            PointerKind::Fling { vy, .. } => {
                self.velocity = -vy;
                true
            }
            _ => false,
        }
    }

    /// `false` when already at that end.
    fn scroll_by(&mut self, by: f64) -> bool {
        let before = self.scroll;
        self.scroll = (self.scroll + by).clamp(0.0, self.max_scroll());
        self.scroll != before
    }

    fn max_scroll(&self) -> f64 {
        (self.tops.last().copied().unwrap_or(0.0) - self.view_h).max(0.0)
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Adjust, "Page"),
            Hint::new(HintKey::Back, "Done"),
        ]
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        _ctx: &mut Ctx,
    ) {
        let (left, right) = (
            f64::from(rect.left) + edge(k),
            f64::from(rect.right) - edge(k),
        );
        let width = (right - left) as f32;
        let key = (width, k, self.host.is_some());
        if self.laid != Some(key) {
            self.layout(fonts, width, k);
            self.laid = Some(key);
        }
        self.view_h = f64::from(rect.height());
        self.step = Style::Body.line_height(k);
        if self.velocity != 0.0 {
            let moved = self.scroll_by(self.velocity * dt);
            self.velocity *= (-FLING_DECAY * dt).exp();
            if !moved || self.velocity.abs() < 20.0 {
                self.velocity = 0.0;
            }
        }
        self.scroll = self.scroll.min(self.max_scroll());

        canvas.save();
        canvas.clip_rect(rect, None, true);
        let top = f64::from(rect.top) - self.scroll;
        let first = self
            .tops
            .partition_point(|t| top + t < f64::from(rect.top) - 40.0 * k);
        for (line, y) in self
            .lines
            .iter()
            .zip(&self.tops)
            .skip(first.saturating_sub(1))
        {
            let y = top + y;
            if y > f64::from(rect.bottom) {
                break;
            }
            let (size, w, alpha) = line.style.look();
            let lh = line.style.line_height(k);
            let baseline = y + lh * 0.72;
            fonts.draw(canvas, &line.text, left, baseline, w, size * k, fg(alpha));
        }
        canvas.restore();
    }

    /// Every logical line, wrapped to `width`: the console's own texts, then the host's.
    fn layout(&mut self, fonts: &Fonts, width: f32, k: f64) {
        let mut source: Vec<Line> = Vec::new();
        let mut push = |style: Style, text: &str| {
            if text.is_empty() {
                source.push(Line {
                    text: String::new(),
                    style,
                });
            }
            for l in text.lines() {
                source.push(Line {
                    text: l.trim_end().to_string(),
                    style,
                });
            }
        };
        if let Doc::Controls(client, touch) = self.doc {
            push(Style::Body, "Press these while a stream is running.");
            for g in shortcuts::groups(client, touch) {
                push(Style::Body, "");
                push(Style::Heading, g.title);
                for i in g.items {
                    push(Style::Keys, i.keys);
                    push(Style::Body, &format!("  {}", i.text));
                }
            }
            return self.wrap_lines(source, fonts, width, k);
        }
        push(
            Style::Title,
            &format!("Punktfunk {}", env!("CARGO_PKG_VERSION")),
        );
        push(
            Style::Body,
            "Punktfunk is licensed under MIT OR Apache-2.0, at your option.",
        );
        push(Style::Body, "");
        push(Style::Heading, "MIT License");
        push(Style::Text, MIT);
        push(Style::Body, "");
        push(Style::Heading, "Apache License 2.0");
        push(Style::Text, APACHE);
        push(Style::Body, "");
        push(Style::Heading, "Bundled font");
        push(
            Style::Body,
            "The Geist typeface, © The Geist Project Authors / Vercel, is licensed under the \
             SIL Open Font License 1.1.",
        );
        push(Style::Text, GEIST_OFL);
        match &self.host {
            Some(sections) => {
                for s in sections {
                    push(Style::Body, "");
                    push(Style::Heading, &s.heading);
                    push(Style::Text, &s.text);
                }
            }
            None => {
                push(Style::Body, "");
                push(Style::Body, "Loading this device's third-party notices…");
            }
        }

        self.wrap_lines(source, fonts, width, k);
    }

    fn wrap_lines(&mut self, source: Vec<Line>, fonts: &Fonts, width: f32, k: f64) {
        self.lines.clear();
        self.tops.clear();
        let mut y = 0.0;
        for line in source {
            let (size, w, _) = line.style.look();
            for text in wrap(&line.text, width, |s| fonts.measure(s, w, size * k)) {
                self.tops.push(y);
                y += line.style.line_height(k);
                self.lines.push(Line {
                    text,
                    style: line.style,
                });
            }
        }
        self.tops.push(y);
    }
}

/// Greedy word wrap. A word wider than `width` on its own is cut by characters, so no
/// text is ever clipped. An empty line stays one empty line.
fn wrap(text: &str, width: f32, measure: impl Fn(&str) -> f32) -> Vec<String> {
    if text.is_empty() || measure(text) <= width {
        return vec![text.to_string()];
    }
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let candidate = if line.is_empty() {
            format!("{indent}{word}")
        } else {
            format!("{line} {word}")
        };
        if measure(&candidate) <= width {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        // Alone and still too wide: cut it.
        let mut piece = String::new();
        for c in word.chars() {
            piece.push(c);
            if measure(&piece) > width && piece.chars().count() > 1 {
                piece.pop();
                out.push(std::mem::take(&mut piece));
                piece.push(c);
            }
        }
        line = piece;
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unit per character, so widths read as column counts.
    fn cols(s: &str) -> f32 {
        s.chars().count() as f32
    }

    #[test]
    fn a_short_line_stays_whole() {
        assert_eq!(wrap("MIT License", 40.0, cols), vec!["MIT License"]);
        assert_eq!(wrap("", 40.0, cols), vec![""]);
    }

    #[test]
    fn a_long_line_breaks_between_words_and_keeps_its_indent() {
        let got = wrap("    the quick brown fox jumps", 14.0, cols);
        assert_eq!(got, vec!["    the quick", "brown fox", "jumps"]);
        assert!(got.iter().all(|l| cols(l) <= 14.0));
    }

    #[test]
    fn a_word_too_wide_for_the_line_is_cut_not_clipped() {
        let url = "https://example.com/a/very/long/path";
        let got = wrap(url, 10.0, cols);
        assert!(got.iter().all(|l| cols(l) <= 10.0));
        assert_eq!(got.concat(), url);
    }

    #[test]
    fn opening_asks_the_host_for_its_sections() {
        let mut fx = Outbox::default();
        let mut s = LicensesScreen::new(&mut fx);
        assert_eq!(fx.cmds, vec![ConsoleCmd::LoadLicenses]);
        assert!(s.waiting());
        s.set_host(vec![LicenseSection {
            heading: "Third-party software".into(),
            text: "serde — MIT".into(),
        }]);
        assert!(!s.waiting());
    }
}
