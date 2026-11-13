//! The D9 step frame over the engine's `WinScreen` (WP2.1): Welcome → Configure → Network →
//! Install → Done. Every rule lives in `punktfunk_setup::platform::windows::screen`; this
//! module only renders it.
//!
//! One component, [`Wizard`]. Every edit, navigation and worker report is a [`Msg`] that
//! `update` applies; the pages are plain functions of `&Wizard` returning a `View`. The install
//! executor runs on a worker thread whose `Reporter` posts each `say`/`plus`/`ok`/`warn` back
//! as a log line: the TUI's dim-command-echo transparency contract, phase checklist included.
//!
//! The stepper draws `run_steps()`, this run's real path, never a ghost Network dot. Each
//! navigation slides the page in from its direction; animations off ⇒ a cut. An installed box
//! opens in manage mode (Reconfigure or Uninstall), and the payload-less `unins000.exe` (D6) is
//! the same page offering only the teardown.

use pf_fluent::motion::Tween;
use pf_fluent::{Palette, Token};
use pf_l10n::{common, tools};
use punktfunk_setup::choices::{LAN_BIND, LOOPBACK_BIND};
use punktfunk_setup::platform::windows::choices::NetworkAnswer;
use punktfunk_setup::platform::windows::demo::WinPreset;
use punktfunk_setup::platform::windows::plan::{self, Artifact};
use punktfunk_setup::platform::windows::screen::{Editor, Field, Section, WinScreen, WizStep};
use punktfunk_setup::platform::windows::{choices::WinChoices, report as win_report};
use punktfunk_setup::ui::Reporter;
use std::rc::Rc;
use windows_reactor::*;

use crate::brand;
use crate::live::{DemoSeams, LiveSeams, RealSeams};
use crate::real::Seams;

mod configure;
mod done;
mod install;
mod network;
mod welcome;

pub use done::PASSWORD_MASK;
use {
    configure::configure_page, done::done_page, install::install_page, network::network_page,
    welcome::welcome_page,
};

/// Long enough that a demo step reads as work happening — the Linux demo's value.
pub const DEMO_LATENCY_MS: u64 = 140;

/// 14 × 16 ms ≈ 220 ms: over before a fast clicker's next click lands.
const SLIDE_FRAMES: u32 = 14;
const SLIDE_TRAVEL: f64 = 36.0;

/// Upcoming dots and the line ahead: a translucent grey that reads on both themes.
const MUTED: Color = Color::argb(0x55, 0x80, 0x80, 0x80);

/// Stepper dot diameter; the line is 2 px and meets the dot's edge. Each step gets `STEP_W`
/// of the header's right side — narrow, the labels still fit at 11 px.
const DOT: f64 = 10.0;
const STEP_W: f64 = 84.0;

/// The lockup intro: the website's 1.3 s at 16 ms a frame.
const LOGO_FRAMES: u32 = 81;
/// The mark's box and the wordmark's width, in px; both scale from the site's geometry.
const MARK: f64 = 112.0;
const WORD: f64 = 236.0;

/// Command echo and the password: a monospace face that ships with Windows.
const MONO: &str = "Consolas";

const WINDOW_W: f64 = 980.0;
const WINDOW_H: f64 = 700.0;

#[derive(Clone, Copy, PartialEq)]
pub enum LineKind {
    Phase,
    Cmd,
    Ok,
    Warn,
    Fail,
    Detail,
    Text,
}

#[derive(Clone, PartialEq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum InstallPhase {
    Idle,
    Running,
    Failed,
    Finished,
}

/// Which tween a [`Msg::Tick`] advances.
#[derive(Clone, Copy, PartialEq)]
pub enum Anim {
    Slide,
    Bar,
    Logo,
}

/// Where the wizard stands and which way it got there (D9: Continue enters from the right,
/// Back from the left). The install thread's jump to Done is a forward move.
#[derive(Clone, Copy, PartialEq)]
pub struct Nav {
    pub step: WizStep,
    pub forward: bool,
}

/// What the window is started with: the preset (canned or probed) and the seams its install
/// runs on.
#[derive(Clone, PartialEq)]
pub struct WizardInput {
    pub preset: WinPreset,
    pub seams: Seams,
}

#[derive(Clone)]
pub enum Msg {
    /// Welcome's option cards: Custom (`true`) or Recommended.
    Choose(bool),
    Continue(WizStep),
    Back(WizStep),
    Uninstall,
    Reconfigure,
    Toggle(Field, bool),
    TriState(Field, Option<usize>),
    BindChoice(Option<usize>),
    BindAddress(String),
    Password(String),
    Regenerate,
    Reveal(bool),
    RevealDone(bool),
    Copy,
    Network(Option<usize>),
    Log(LogLine),
    Installed(bool),
    Tick(Anim),
    Scheme(ColorScheme),
    /// Windows changed its accent or contrast.
    Colors(Palette),
    Exit(i32),
}

pub struct Wizard {
    preset: WinPreset,
    seams: Seams,
    screen: WinScreen,
    nav: Nav,
    install: InstallPhase,
    /// This run tears down instead of installing: the uninstaller exe, or Uninstall chosen on
    /// the manage Welcome.
    uninstall: bool,
    /// Custom setup: the Configure step is on this run's path. Off, the defaults install
    /// (Recommended); Reconfigure on an installed box always turns it on.
    custom: bool,
    /// The password row's Show/Hide state.
    reveal: bool,
    /// Done's password card: masked until Show, and whether Copy has fired.
    reveal_done: bool,
    copied: bool,
    log: Vec<LogLine>,
    palette: Palette,
    /// Keeps [`Msg::Colors`] arriving while the window lives.
    _colors: Option<pf_fluent::Watch>,
    slide: Tween,
    /// The install page's progress bar, eased 0 → 1 across the plan's phases.
    bar: Tween,
    /// The Welcome lockup's intro, 0 → 1 once on mount (the website's orbit + slide-in).
    logo: Tween,
}

/// The install page as a `Reporter`: each line posts back to the window as a [`Msg::Log`].
struct ChannelReporter {
    out: ComponentCompletion<Msg>,
}

impl ChannelReporter {
    fn push(&self, kind: LineKind, text: &str) {
        let _ = self.out.clone().complete(Msg::Log(LogLine {
            kind,
            text: text.to_string(),
        }));
    }
}

impl Reporter for ChannelReporter {
    fn say(&self, msg: &str) {
        self.push(LineKind::Phase, msg);
    }
    fn ok(&self, msg: &str) {
        self.push(LineKind::Ok, msg);
    }
    fn warn(&self, msg: &str) {
        self.push(LineKind::Warn, msg);
    }
    fn die(&self, msg: &str) {
        self.push(LineKind::Fail, msg);
    }
    fn plus(&self, cmd: &str) {
        self.push(LineKind::Cmd, cmd);
    }
    fn detail(&self, msg: &str) {
        self.push(LineKind::Detail, msg);
    }
    fn line(&self, msg: &str) {
        self.push(LineKind::Text, msg);
    }
    fn blank(&self) {
        self.push(LineKind::Text, "");
    }
}

impl Component for Wizard {
    type Input = WizardInput;
    type Message = Msg;

    fn create(input: &WizardInput, cx: &ComponentContext<Self>) -> Self {
        let preset = input.preset.clone();
        let mut choices = WinChoices::derive(&preset.facts, preset.artifact);
        // The fresh-host password row arrives pre-filled (D9): real RNG, 24 hex chars. It
        // travels via an ACL'd temp file, never argv.
        if preset.artifact == Artifact::Host
            && preset.facts.installed.is_none()
            && !preset.facts.web_password_present
            && let Ok(pw) = punktfunk_setup::platform::windows::sys::random_hex(12)
        {
            choices.web_password = Some(pw);
        }
        let screen = WinScreen::new(preset.facts.clone(), choices, preset.artifact);
        Wizard {
            uninstall: preset.uninstall,
            preset,
            seams: input.seams.clone(),
            screen,
            nav: Nav {
                step: WizStep::Welcome,
                forward: true,
            },
            install: InstallPhase::Idle,
            custom: false,
            reveal: false,
            reveal_done: false,
            copied: false,
            log: Vec::new(),
            palette: Palette::system(),
            _colors: {
                let out = cx.completion();
                pf_fluent::watch(move || {
                    let _ = out.clone().complete(Msg::Colors(Palette::system()));
                })
            },
            slide: Tween::settled(1.0),
            bar: Tween::settled(0.0),
            logo: Tween::start(0.0, 1.0, LOGO_FRAMES, Msg::Tick(Anim::Logo), cx),
        }
    }

    fn update(&mut self, msg: Msg, cx: &ComponentContext<Self>) {
        match msg {
            Msg::Choose(custom) => self.choose(custom),
            Msg::Continue(cur) => self.advance(cur, cx),
            Msg::Back(cur) => self.back(cur, cx),
            Msg::Uninstall => {
                self.uninstall = true;
                self.advance(WizStep::Welcome, cx);
            }
            Msg::Reconfigure => {
                self.custom = true;
                self.advance(WizStep::Welcome, cx);
            }
            Msg::Toggle(field, on) => self.screen.set_bool(field, on),
            Msg::TriState(field, choice) => match choice {
                Some(0) => self.screen.keep_box_setting(field),
                Some(1) => self.screen.set_bool(field, true),
                Some(_) => self.screen.set_bool(field, false),
                None => {}
            },
            Msg::BindChoice(choice) => {
                let bind = match choice {
                    None => return,
                    Some(0) => None,
                    Some(1) => Some(LOOPBACK_BIND.to_string()),
                    Some(2) => Some(LAN_BIND.to_string()),
                    Some(_) => Some(String::new()),
                };
                self.screen.set_bind(bind);
            }
            Msg::BindAddress(addr) => self.screen.set_bind(Some(addr)),
            Msg::Password(pw) => self.screen.set_password(pw),
            Msg::Regenerate => {
                if let Ok(pw) = punktfunk_setup::platform::windows::sys::random_hex(12) {
                    self.screen.set_password(pw);
                }
            }
            Msg::Reveal(on) => self.reveal = on,
            Msg::RevealDone(on) => self.reveal_done = on,
            Msg::Copy => {
                if let Some(pw) = &self.screen.choices.web_password {
                    done::copy_to_clipboard(pw);
                    self.copied = true;
                }
            }
            Msg::Network(choice) => {
                let answer = match choice {
                    None => return,
                    Some(0) => NetworkAnswer::MakePrivate(self.network_name().unwrap_or_default()),
                    Some(1) => NetworkAnswer::OpenPublicRules,
                    Some(_) => NetworkAnswer::Skip,
                };
                self.screen.set_network(answer);
            }
            Msg::Log(line) => {
                self.log.push(line);
                self.retarget_bar(cx);
            }
            Msg::Installed(ok) => {
                if ok {
                    self.install = InstallPhase::Finished;
                    self.retarget_bar(cx);
                    self.go(WizStep::Done, true, cx);
                } else {
                    self.install = InstallPhase::Failed;
                }
            }
            Msg::Tick(anim) => match anim {
                Anim::Slide => self.slide.tick(Msg::Tick(anim), cx),
                Anim::Bar => self.bar.tick(Msg::Tick(anim), cx),
                Anim::Logo => self.logo.tick(Msg::Tick(anim), cx),
            },
            Msg::Scheme(scheme) => self.palette = self.palette.with_scheme(scheme),
            Msg::Colors(p) => self.palette = p.with_scheme(self.palette.scheme),
            Msg::Exit(code) => std::process::exit(code),
        }
    }

    fn view(&self, _input: &WizardInput, cx: &mut ViewContext<'_, Self>) -> View {
        cx.window_title(tools::wizard_window_title());
        // One size: every page is laid out for 980×700, so min = max pins the frame.
        let mut visuals = WindowVisuals::new()
            .backdrop(WindowBackdrop::Mica)
            .constraints(WindowConstraints {
                min_width: Some(WINDOW_W),
                min_height: Some(WINDOW_H),
                max_width: Some(WINDOW_W),
                max_height: Some(WINDOW_H),
            });
        if let Some(icon) = brand::icon_path() {
            visuals = visuals.icon(icon.display().to_string());
        }
        cx.window_visuals(visuals);
        let scheme = cx.callback(Msg::Scheme);
        cx.on_color_scheme(scheme);
        match self.nav.step {
            WizStep::Welcome => welcome_page(self, cx),
            WizStep::Configure => configure_page(self, cx),
            WizStep::Network => network_page(self, cx),
            WizStep::Install => install_page(self, cx),
            WizStep::Done => done_page(self, cx),
        }
    }
}

impl Wizard {
    /// This run's step list. An uninstall run has nothing to configure — Welcome states what
    /// is about to happen and Install is the teardown. Recommended (the default) skips
    /// Configure; the Network step still materializes on its own trigger.
    fn run_steps(&self) -> Vec<WizStep> {
        if self.uninstall {
            vec![WizStep::Welcome, WizStep::Install, WizStep::Done]
        } else {
            self.screen
                .steps()
                .into_iter()
                .filter(|s| self.custom || *s != WizStep::Configure)
                .collect()
        }
    }

    /// What Continue will do from `cur`: install now, or one more step first.
    fn go_label(&self, cur: WizStep) -> &'static str {
        let next = self
            .run_steps()
            .into_iter()
            .skip_while(|s| *s != cur)
            .nth(1);
        if next == Some(WizStep::Install) {
            tools::wizard_install()
        } else {
            tools::wizard_continue()
        }
    }

    /// The public network the Network step names: the one already chosen, else the first.
    /// `None` when the box has no Public network, which the step's sentences word whole.
    fn network_name(&self) -> Option<String> {
        match &self.screen.choices.network {
            NetworkAnswer::MakePrivate(n) if !n.is_empty() => Some(n.clone()),
            _ => self
                .screen
                .facts
                .public_networks()
                .first()
                .map(|n| n.name.clone()),
        }
    }

    /// Pick Custom or Recommended. Recommended installs the defaults, so it drops what an
    /// earlier Custom pass edited on Configure; the network answer and the password stay.
    fn choose(&mut self, custom: bool) {
        if !custom {
            let s = &self.screen;
            let mut defaults = WinChoices::derive(&s.facts, self.preset.artifact);
            defaults.web_password = s.choices.web_password.clone();
            defaults.network = s.choices.network.clone();
            self.screen = WinScreen::new(s.facts.clone(), defaults, self.preset.artifact);
        }
        self.custom = custom;
    }

    fn go(&mut self, step: WizStep, forward: bool, cx: &ComponentContext<Self>) {
        self.nav = Nav { step, forward };
        self.slide = Tween::start(0.0, 1.0, SLIDE_FRAMES, Msg::Tick(Anim::Slide), cx);
    }

    /// Continue: move to the step after `cur` on this run's real path.
    fn advance(&mut self, cur: WizStep, cx: &ComponentContext<Self>) {
        let next = self
            .run_steps()
            .into_iter()
            .skip_while(|s| *s != cur)
            .nth(1)
            .unwrap_or(WizStep::Done);
        if next == WizStep::Network && self.screen.choices.network == NetworkAnswer::Skip {
            // The step opens on its recommended answer (D12: recommended first); the radio
            // edits it from there.
            let name = self
                .screen
                .facts
                .public_networks()
                .first()
                .map(|n| n.name.clone())
                .unwrap_or_default();
            self.screen.set_network(NetworkAnswer::MakePrivate(name));
        }
        if next == WizStep::Install {
            if self.install != InstallPhase::Idle {
                return;
            }
            self.start_install(cx);
        }
        self.go(next, true, cx);
    }

    fn back(&mut self, cur: WizStep, cx: &ComponentContext<Self>) {
        let steps = self.run_steps();
        if let Some(i) = steps.iter().position(|s| *s == cur)
            && i > 0
        {
            self.go(steps[i - 1], false, cx);
        }
    }

    /// The bar's target: one phase heading per plan phase reaches the log, so the target is
    /// the phase count over the plan's.
    fn retarget_bar(&mut self, cx: &ComponentContext<Self>) {
        let total = plan::build(
            &self.screen.facts,
            &self.screen.effective_choices(),
            self.preset.artifact,
            self.uninstall,
        )
        .phases
        .len()
        .max(1);
        let seen = self
            .log
            .iter()
            .filter(|l| l.kind == LineKind::Phase)
            .count();
        let target = match self.install {
            InstallPhase::Idle => 0.0,
            InstallPhase::Finished => 1.0,
            _ => (seen as f64 - 0.5).max(0.0) / total as f64,
        };
        if target != self.bar.target() {
            self.bar = Tween::start(
                self.bar.eased(),
                target,
                SLIDE_FRAMES,
                Msg::Tick(Anim::Bar),
                cx,
            );
        }
    }

    fn start_install(&mut self, cx: &ComponentContext<Self>) {
        self.install = InstallPhase::Running;
        let preset = self.preset.clone();
        let screen = self.screen.clone();
        let uninstall = self.uninstall;
        let seams = self.seams.clone();
        let out = cx.completion();
        std::thread::spawn(move || {
            let choices = screen.effective_choices();
            let built = plan::build(&screen.facts, &choices, preset.artifact, uninstall);
            let ui = ChannelReporter { out: out.clone() };
            let live = match &seams {
                Seams::Demo { latency_ms } => LiveSeams::Demo(DemoSeams::new(&preset, *latency_ms)),
                Seams::Real { root, version } => {
                    LiveSeams::Real(RealSeams::new(root.as_deref(), version))
                }
            };
            let exec = live.executor(&ui, false, false, choices.web_password.clone());
            let ok = match exec.execute(&built) {
                Ok(()) => true,
                Err(failed) => {
                    ui.die(&failed.0);
                    false
                }
            };
            let _ = out.complete(Msg::Installed(ok));
        });
    }
}

// --- rendering ---------------------------------------------------------------------------

/// A rounded, bordered surface in the theme's card colours — the client shell's card.
fn card(child: impl Into<View>) -> Border {
    Border::new()
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(CornerRadius::uniform(10.0))
        .padding(Thickness::new(18.0, 12.0, 18.0, 12.0))
        .content(child)
}

/// A wrapping secondary-text line.
fn note(w: &Wizard, text: impl AsRef<str>) -> TextBlock {
    TextBlock::new()
        .text(text)
        .text_wrapping(TextWrapping::Wrap)
        .foreground(w.palette.get(Token::SecondaryText))
}

/// The Install step is the teardown on an uninstall run, and its dot says so.
fn step_title(step: WizStep, uninstall: bool) -> &'static str {
    match step {
        WizStep::Install if uninstall => tools::wizard_step_uninstall(),
        _ => step.title(),
    }
}

/// The D9 stepper: dots joined by a line, filled through the current step in brand violet,
/// hollow ahead. Each step owns one equal column: a half-segment either side of its dot,
/// stopping at the dot's edge, so the line meets every dot and the first/last halves are
/// simply invisible.
fn stepper(w: &Wizard, steps: &[WizStep], pos: usize) -> Grid {
    let n = steps.len();
    let half = |lit: bool, hidden: bool, column: i32| {
        Rectangle::new()
            .fill(if lit {
                Brush::from(ThemeBrush::Accent)
            } else {
                Brush::from(MUTED)
            })
            .height(2.0)
            .opacity(if hidden { 0.0 } else { 1.0 })
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(column)
    };
    let cells: Vec<View> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let dot = if i <= pos {
                Ellipse::new().fill(ThemeBrush::Accent)
            } else {
                Ellipse::new().stroke(MUTED).stroke_thickness(1.5)
            };
            let line = Grid::new()
                .columns([GridLength::STAR, GridLength::Auto, GridLength::STAR])
                .children((
                    half(i > 0 && i <= pos, i == 0, 0),
                    dot.width(DOT).height(DOT).grid_column(1),
                    half(i < pos, i + 1 == n, 2),
                ));
            let label = TextBlock::new()
                .text(step_title(*step, w.uninstall))
                .font_size(11.0)
                .horizontal_alignment(HorizontalAlignment::Center);
            let label = if i == pos {
                label.font_weight(FontWeight::SEMI_BOLD)
            } else {
                label.foreground(w.palette.get(Token::SecondaryText))
            };
            StackPanel::new()
                .spacing(4.0)
                .grid_column(i as i32)
                .children((line, label))
                .into()
        })
        .collect();
    Grid::new()
        .columns(std::iter::repeat_n(GridLength::STAR, n))
        .children(cells)
}

/// The incoming page's horizontal offset at `progress`: forward starts to the right and
/// travels left, back the mirror.
pub fn slide_offset(forward: bool, progress: f64) -> f64 {
    let off = (1.0 - progress) * SLIDE_TRAVEL;
    if forward {
        off
    } else {
        -off
    }
}

/// Stepper · (title · content · button bar). The stepper stays put; the rest is the page that
/// slides in.
fn frame(w: &Wizard, step: WizStep, content: impl Into<View>, buttons: Vec<View>) -> View {
    let steps = w.run_steps();
    let pos = steps.iter().position(|s| *s == step).unwrap_or(0);
    // One header row: the step title left, the stepper right and narrow.
    let head = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .margin(Thickness::new(0.0, 0.0, 0.0, 20.0))
        .grid_row(0)
        .children((
            TextBlock::new()
                .text(step_title(step, w.uninstall))
                .font_size(26.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(0),
            stepper(w, &steps, pos)
                .width(steps.len() as f64 * STEP_W)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(1),
        ));
    let bar = Border::new()
        .border_brush(w.palette.get(Token::DividerStroke))
        .border_thickness(Thickness::new(0.0, 1.0, 0.0, 0.0))
        .padding(Thickness::new(0.0, 14.0, 0.0, 0.0))
        .margin(Thickness::new(0.0, 16.0, 0.0, 0.0))
        .grid_row(1)
        .content(
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .horizontal_alignment(HorizontalAlignment::Right)
                .children(buttons),
        );
    let progress = w.slide.eased();
    let page = Border::new()
        .opacity(progress)
        .translation(slide_offset(w.nav.forward, progress), 0.0)
        .grid_row(1)
        .content(
            Grid::new()
                .rows([GridLength::STAR, GridLength::Auto])
                .children((Border::new().grid_row(0).content(content), bar)),
        );
    Grid::new()
        .rows([GridLength::Auto, GridLength::STAR])
        .margin(Thickness::new(36.0, 26.0, 36.0, 24.0))
        .children((head, page))
        .into()
}

fn continue_button(cx: &ViewContext<'_, Wizard>, cur: WizStep, label: &str) -> View {
    Button::new()
        .content(label)
        .style(ButtonStyle::Accent)
        .min_width(124.0)
        .on_click(cx.message(Msg::Continue(cur)))
        .into()
}

fn back_button(cx: &ViewContext<'_, Wizard>, cur: WizStep) -> View {
    Button::new()
        .content(common::common_back())
        .min_width(92.0)
        .on_click(cx.message(Msg::Back(cur)))
        .into()
}

/// The real window over a preset (canned or probed) and the seams its install runs on.
pub fn run(preset: WinPreset, seams: Seams) -> Result<(), impl std::fmt::Debug> {
    pf_l10n::set_languages(&pf_l10n::os_languages());
    // Self-contained: the runtime DLLs sit beside the exe (build.rs), which reactor finds
    // without an installed framework.
    App::run_component_with_policy::<Wizard>(
        WizardInput { preset, seams },
        WindowPolicy::new()
            .title(tools::wizard_window_title())
            .client_size(WINDOW_W, WINDOW_H),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_enters_from_the_right_and_back_from_the_left() {
        assert!(slide_offset(true, 0.0) > 0.0);
        assert!(slide_offset(false, 0.0) < 0.0);
        assert_eq!(slide_offset(true, 1.0), 0.0);
    }
}
