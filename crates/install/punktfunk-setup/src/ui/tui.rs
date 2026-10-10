//! Clack-look screens: gutter rail, inline prompts, no alternate screen.
//!
//! The transcript scrolls and stays the audit log, which is the constraint that shapes this
//! file: a frame is only ever repainted while it is still the *live* one (the settings list,
//! a row editor, the intro animation). Once a command has been echoed, nothing overwrites it.
//! That is why the verbose progress view prints one `◆` heading per phase with its commands
//! dim beneath, rather than the collapsing checklist a repaint could draw.
//!
//! The default run collapses to a single live line instead. It repaints, which is allowed
//! precisely because nothing is echoed while it is up: with no command in the scrollback there
//! is nothing a repaint could overwrite. Warnings and failures still print above it, and `-v`
//! restores the transcript.
//!
//! All I/O goes through `term::Terminal`, so `ScriptedTerm` can drive the
//! flow with a key list and assert on the on-screen frame.

use std::cell::RefCell;

use crate::choices::{Action, LAN_BIND, LOOPBACK_BIND};
use crate::facts::Channel;
use crate::ui::logo::{self, Intro, MARK_TEXT_ROWS};
use crate::ui::summary::{Field, Item, Key as UiKey, Screen, Step};
use crate::ui::term::{Key, Terminal};
use crate::ui::theme::{Caps, Colors, Layer, BRAND, LENS};
use crate::ui::Reporter;

const BAR: &str = "│";
const STEP_ACTIVE: &str = "◆";
const STEP_DONE: &str = "◇";
const CURSOR: &str = "▸";
const RADIO_ON: &str = "●";
const RADIO_OFF: &str = "○";
const WARN: &str = "▲";
/// Short enough to type on a couch keyboard, long enough that the console is not open to
/// the LAN behind four characters.
const MIN_PASSWORD: usize = 8;
/// What a systemd EnvironmentFile value cannot carry through unquoting.
const FORBIDDEN: &str = "\"'\\`$";

pub struct Tui<'a> {
    term: RefCell<&'a mut dyn Terminal>,
    caps: Caps,
    /// 0 in tests and under `--demo --fast`, so nothing waits on wall-clock time.
    frame_ms: u64,
    /// `Some` while the run is collapsed to one live line; `None` under `-v`.
    progress: RefCell<Option<Progress>>,
}

/// The live run line. `done` counts phase headings, which `exec` emits one of per phase.
struct Progress {
    total: usize,
    done: usize,
    title: String,
    drawn: usize,
}

impl<'a> Tui<'a> {
    pub fn new(term: &'a mut dyn Terminal, caps: Caps, frame_ms: u64) -> Tui<'a> {
        Tui {
            term: RefCell::new(term),
            caps,
            frame_ms,
            progress: RefCell::new(None),
        }
    }

    fn write(&self, text: &str) {
        self.term.borrow_mut().write(text);
    }

    /// Collapse the run to one repainting line. Nothing is echoed until `end_progress`.
    pub fn begin_progress(&self, total: usize) {
        *self.progress.borrow_mut() = Some(Progress {
            total,
            done: 0,
            title: String::new(),
            drawn: 0,
        });
    }

    /// Take the line down, so the outro starts on a clean row.
    pub fn end_progress(&self) {
        if let Some(p) = self.progress.borrow_mut().take()
            && p.drawn > 0
        {
            self.term.borrow_mut().clear_last_lines(p.drawn);
        }
    }

    fn progress_live(&self) -> bool {
        self.progress.borrow().is_some()
    }

    /// Lift the line so a warning can be printed under it permanently.
    fn clear_progress(&self) {
        if let Some(p) = self.progress.borrow_mut().as_mut()
            && p.drawn > 0
        {
            self.term.borrow_mut().clear_last_lines(p.drawn);
            p.drawn = 0;
        }
    }

    fn repaint_progress(&self) {
        let frame = match self.progress.borrow().as_ref() {
            Some(p) => self.progress_frame(p),
            None => return,
        };
        self.clear_progress();
        self.write(&frame);
        if let Some(p) = self.progress.borrow_mut().as_mut() {
            p.drawn = frame.lines().count();
        }
    }

    fn progress_frame(&self, p: &Progress) -> String {
        const CELLS: usize = 14;
        let filled = (CELLS * p.done)
            .checked_div(p.total)
            .unwrap_or(0)
            .min(CELLS);
        let bar = format!("{}{}", "▓".repeat(filled), "░".repeat(CELLS - filled));
        format!(
            "{}  {} {} {}\n",
            self.bar(),
            self.accent(STEP_ACTIVE),
            self.dim(&bar),
            self.highlight(&format!("{}/{}  {}", p.done, p.total, p.title))
        )
    }

    fn dim(&self, text: &str) -> String {
        if self.caps.colors == Colors::None {
            return text.to_string();
        }
        format!("\x1b[2m{text}\x1b[0m")
    }

    fn accent(&self, text: &str) -> String {
        format!(
            "{}{text}{}",
            self.caps.paint(BRAND, Layer::Fg),
            self.caps.reset()
        )
    }

    fn highlight(&self, text: &str) -> String {
        format!(
            "{}{text}{}",
            self.caps.paint(LENS, Layer::Fg),
            self.caps.reset()
        )
    }

    fn bar(&self) -> String {
        self.dim(BAR)
    }

    /// Truncate to terminal width in printable columns.
    ///
    /// `clear_last_lines` counts physical rows; a wrap makes rewind short and
    /// leftovers pile up. Truncating keeps the two counts equal.
    fn fit(&self, text: &str) -> String {
        let width = usize::from(self.caps.width).max(20);
        let mut out = String::new();
        for line in text.lines() {
            let mut cols = 0usize;
            let mut cut = false;
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    out.push(c);
                    for c in chars.by_ref() {
                        out.push(c);
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    continue;
                }
                if cols == width {
                    cut = true;
                    break;
                }
                out.push(c);
                cols += 1;
            }
            if cut {
                out.push_str(&self.caps.reset());
            }
            out.push('\n');
        }
        out
    }

    /// Word-wrap prose, leaving room for the gutter.
    ///
    /// Row values are truncated instead: a wrapped column stops lining up.
    fn wrap(&self, text: &str) -> Vec<String> {
        let width = usize::from(self.caps.width).saturating_sub(3).max(24);
        let mut lines = Vec::new();
        let mut cur = String::new();
        for word in text.split_whitespace() {
            if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    fn marked(&self) -> bool {
        logo::intro_level(&self.caps, false) != Intro::Plain
    }

    /// Animated or still mark. Returns rows left on screen; `settings` clears
    /// them before drawing its own copy so the two never stack.
    pub fn intro(&self, level: Intro, parts: logo::Parts) -> usize {
        match level {
            Intro::Plain => return 0,
            Intro::Static => self.write(&logo::still(&self.caps, 2, parts)),
            Intro::Animated => {
                for i in 0..logo::FRAMES {
                    let t = i as f32 / (logo::FRAMES - 1) as f32;
                    if i > 0 {
                        self.term.borrow_mut().clear_last_lines(MARK_TEXT_ROWS);
                    }
                    self.write(&logo::render(&logo::frame_parts(t, parts), &self.caps, 2));
                    if self.frame_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(self.frame_ms));
                    }
                }
            }
        }
        MARK_TEXT_ROWS
    }

    /// `already` is intro rows still on screen; the first repaint clears
    /// exactly those so the animated mark is replaced, not pushed down.
    pub fn settings(&self, screen: &mut Screen, already: usize) -> Step {
        self.term.borrow_mut().hide_cursor();
        let mut drawn = already;
        let outcome = loop {
            let frame = self.frame(screen);
            if drawn > 0 {
                self.term.borrow_mut().clear_last_lines(drawn);
            }
            let frame = self.fit(&frame);
            drawn = frame.lines().count();
            self.write(&frame);

            let key = self.key();
            match screen.key(key) {
                Step::Idle => {}
                Step::Edit(field) => {
                    // The editor draws below the list, so the list is redrawn from scratch.
                    self.term.borrow_mut().clear_last_lines(drawn);
                    drawn = 0;
                    self.edit(screen, field);
                }
                done => break done,
            }
        };
        self.term.borrow_mut().show_cursor();
        outcome
    }

    fn key(&self) -> UiKey {
        let key = self.term.borrow_mut().read_key();
        match key {
            Key::Up | Key::Char('k') => UiKey::Up,
            Key::Down | Key::Char('j') => UiKey::Down,
            Key::Enter | Key::Space => UiKey::Enter,
            Key::Cancel | Key::Char('q') => UiKey::Cancel,
            _ => UiKey::Ignored,
        }
    }

    fn frame(&self, screen: &Screen) -> String {
        let mut out = String::new();
        let bar = self.bar();
        // The rule is decoration, so it yields to the terminal rather than forcing a wrap.
        let rule_cols = usize::from(self.caps.width).saturating_sub(6).min(58);
        let rule = self.dim(&"─".repeat(rule_cols));
        // The mark is part of the frame, so muting follows the Components row live.
        if self.marked() {
            let parts = logo::Parts {
                host: screen.choices.components.host,
                client: screen.choices.components.client,
            };
            out.push_str(&logo::render(&logo::frame_parts(1.0, parts), &self.caps, 2));
        }
        out.push_str(&format!("{bar}\n"));
        out.push_str(&format!(
            "{}  {}\n",
            self.accent(STEP_ACTIVE),
            self.highlight(&screen.title())
        ));
        out.push_str(&format!("{bar}  {}\n", self.dim(&screen.facts.docs_page)));
        out.push_str(&format!("{bar}\n"));

        for (index, item) in screen.items.iter().enumerate() {
            let on = index == screen.cursor;
            let mark = if on { self.accent(CURSOR) } else { " ".into() };
            match item {
                Item::Row(field) => {
                    let label = format!("{:<25}", Screen::label(*field));
                    let value = screen.value(*field);
                    let text = if on {
                        format!("{}{}", self.highlight(&label), value)
                    } else {
                        format!("{label}{}", self.dim(&value))
                    };
                    out.push_str(&format!("{bar}  {mark} {text}\n"));
                }
                Item::Go => {
                    let label = if screen.manage_mode() {
                        "Apply these changes"
                    } else {
                        "Install now with these settings"
                    };
                    out.push_str(&format!("{bar}  {mark} {}\n", self.highlight(label)));
                    out.push_str(&format!("{bar}  {rule}\n"));
                }
                Item::Uninstall => out.push_str(&format!(
                    "{bar}  {mark} {}\n",
                    self.dim("Uninstall — remove the packages and the repo")
                )),
            }
        }
        out.push_str(&format!("{bar}\n"));
        out.push_str(&format!(
            "{bar}  {}\n",
            self.dim("↑↓ move · Enter select/edit · q quit — nothing runs until you confirm")
        ));
        out
    }

    fn edit(&self, screen: &mut Screen, field: Field) {
        let why = Screen::why(field);
        match field {
            Field::Channel => {
                let current = usize::from(screen.choices.channel == Channel::Canary);
                if let Some(pick) = self.choose(why, &["stable", "canary"], current) {
                    screen.set_channel(if pick == 0 {
                        Channel::Stable
                    } else {
                        Channel::Canary
                    });
                }
            }
            Field::Components => {
                let c = screen.choices.components;
                let current = match (c.host, c.client) {
                    (true, false) => 0,
                    (true, true) => 1,
                    _ => 2,
                };
                let options = [
                    "The PC I stream from (host)",
                    "Both (host + client)",
                    "The device I play on (client)",
                ];
                if let Some(pick) = self.choose(why, &options, current) {
                    screen.set_components(pick != 2, pick != 0);
                }
            }
            other => {
                let current = usize::from(!self.current_bool(screen, other));
                if let Some(pick) = self.choose(why, &["yes", "no"], current) {
                    screen.set_bool(other, pick == 0);
                }
            }
        }
    }

    fn current_bool(&self, screen: &Screen, field: Field) -> bool {
        let c = &screen.choices;
        match field {
            Field::Group => c.punktfunk_group,
            Field::Gamestream => c.gamestream,
            Field::Clipboard => c.clipboard,
            Field::Linger => c.linger,
            Field::Start => c.start,
            _ => false,
        }
    }

    /// Prompt prose: each input line wrapped on its own, so a command keeps its line.
    fn prose(&self, text: &str) -> String {
        let mut out = String::new();
        let mut first = true;
        for line in text.lines() {
            for wrapped in self.wrap(line) {
                let lead = if first {
                    self.accent(STEP_ACTIVE)
                } else {
                    self.bar()
                };
                out.push_str(&format!("{lead}  {wrapped}\n"));
                first = false;
            }
        }
        out
    }

    /// The password step: a fresh host install must not end with the console asking for
    /// something the user never saw mentioned. `None` keeps the generated one — backing out
    /// of either prompt is that same answer, never a cancelled install.
    pub fn web_password(&self, url: &str, read_cmd: &str) -> Option<String> {
        let prompt = format!(
            "The web console ({url}) asks for a password.\nOne is generated for you unless you set your own. Print it any time with:\n{read_cmd}"
        );
        let options = ["Use the generated password", "Set my own now"];
        match self.choose(&prompt, &options, 0)? {
            0 => None,
            _ => self.ask_secret(),
        }
    }

    /// Where the console listens. Asked, not derived: the answer decides who on the network can
    /// reach the pairing screen, and until now nobody was ever told it was every device.
    ///
    /// `None` on Esc leaves `current` standing, so backing out of a re-run never moves a console
    /// someone already placed. "Any address" is deliberately not an option: a console on the open
    /// internet is a route the operator builds on purpose, not one a list offers them.
    pub fn web_bind(&self, ip: &str, current: &str) -> Option<String> {
        let prompt =
            "Where should the web console be reachable?\nIt is how you pair devices and change every setting.";
        let options = [
            "This machine only (https://127.0.0.1:47992)",
            &format!("This local network (https://{ip}:47992)"),
            "A specific address (e.g. a VPN interface)",
        ];
        // The cursor starts on what the box already answers, so Enter is always "leave it".
        let initial = match current {
            LOOPBACK_BIND => 0,
            LAN_BIND => 1,
            _ => 2,
        };
        match self.choose(prompt, &options, initial)? {
            0 => Some(LOOPBACK_BIND.to_string()),
            1 => Some(LAN_BIND.to_string()),
            _ => self.ask_line(
                "Type the address the console should listen on. It has to be an address this machine already has.",
                "an IPv4 or IPv6 address · Esc leaves it unchanged",
                "Enter accepts · Esc leaves it unchanged",
                |typed| typed.parse::<std::net::IpAddr>().is_ok(),
            ),
        }
    }

    /// Inline text entry for the console password. `None` on Esc — the caller reads that as
    /// "generate one after all", so there is no way to leave here with nothing set.
    fn ask_secret(&self) -> Option<String> {
        self.ask_line(
            "Type the console password. It is stored on this box only, and shown here as you type.",
            &format!("at least {MIN_PASSWORD} characters · Esc generates one instead"),
            "Enter accepts · Esc generates one instead",
            |typed| typed.chars().count() >= MIN_PASSWORD,
        )
    }

    /// One line of typed text, echoed. `accept` gates Enter and picks which hint shows; Esc
    /// returns `None`, which every caller reads as "keep the default", never as a cancel.
    fn ask_line(
        &self,
        prose: &str,
        hint_short: &str,
        hint_ok: &str,
        accept: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let mut typed = String::new();
        let mut drawn = 0usize;
        loop {
            let bar = self.bar();
            let short = !accept(&typed);
            let hint = if short { hint_short } else { hint_ok };
            let mut frame = format!("{bar}\n");
            frame.push_str(&self.prose(prose));
            frame.push_str(&format!(
                "{bar}  {} {}\n",
                self.accent(CURSOR),
                self.highlight(&typed)
            ));
            frame.push_str(&format!("{bar}  {}\n", self.dim(hint)));
            frame.push_str(&format!("{bar}\n"));
            if drawn > 0 {
                self.term.borrow_mut().clear_last_lines(drawn);
            }
            let frame = self.fit(&frame);
            drawn = frame.lines().count();
            self.write(&frame);

            let key = self.term.borrow_mut().read_key();
            match key {
                // The file is read as a systemd EnvironmentFile, which unquotes and
                // unescapes: a quote or a backslash in the value would not survive it.
                Key::Char(c) if c.is_ascii_graphic() && !FORBIDDEN.contains(c) => typed.push(c),
                Key::Backspace => {
                    typed.pop();
                }
                Key::Enter if !short => {
                    self.term.borrow_mut().clear_last_lines(drawn);
                    return Some(typed);
                }
                Key::Cancel => {
                    self.term.borrow_mut().clear_last_lines(drawn);
                    return None;
                }
                _ => {}
            }
        }
    }

    /// Inline radio list. `None` if the user backed out.
    fn choose(&self, prompt: &str, options: &[&str], initial: usize) -> Option<usize> {
        let mut cursor = initial.min(options.len().saturating_sub(1));
        let mut drawn = 0usize;
        loop {
            let mut frame = String::new();
            let bar = self.bar();
            frame.push_str(&format!("{bar}\n"));
            frame.push_str(&self.prose(prompt));
            for (index, option) in options.iter().enumerate() {
                let (glyph, text) = if index == cursor {
                    (self.accent(RADIO_ON), self.highlight(option))
                } else {
                    (self.dim(RADIO_OFF), self.dim(option))
                };
                frame.push_str(&format!("{bar}  {glyph} {text}\n"));
            }
            frame.push_str(&format!("{bar}\n"));
            if drawn > 0 {
                self.term.borrow_mut().clear_last_lines(drawn);
            }
            let frame = self.fit(&frame);
            drawn = frame.lines().count();
            self.write(&frame);

            // Bound before the match: an arm that clears lines needs the RefCell back, and a
            // match scrutinee holds its borrow for the whole match.
            let key = self.term.borrow_mut().read_key();
            match key {
                Key::Up | Key::Char('k') => {
                    cursor = (cursor + options.len() - 1) % options.len();
                }
                Key::Down | Key::Char('j') => cursor = (cursor + 1) % options.len(),
                Key::Enter | Key::Space => {
                    // No collapsed transcript line: the row below already holds the answer.
                    self.term.borrow_mut().clear_last_lines(drawn);
                    return Some(cursor);
                }
                Key::Cancel | Key::Char('q') => {
                    self.term.borrow_mut().clear_last_lines(drawn);
                    return None;
                }
                _ => {}
            }
        }
    }

    pub fn outro(&self, lines: &[String]) {
        self.write(&format!("{}\n", self.bar()));
        for line in lines {
            self.write(&format!("{}  {line}\n", self.dim("└")));
        }
    }

    pub fn failure(&self, message: &str) {
        self.write(&format!("{}\n", self.bar()));
        let mark = if self.caps.colors == Colors::None {
            WARN.to_string()
        } else {
            format!("\x1b[31m{WARN}\x1b[0m")
        };
        self.write(&format!("{mark}  {message}\n"));
    }
}

/// Same `Plan`, reported through the rail instead of the plain transcript.
impl Reporter for Tui<'_> {
    fn say(&self, msg: &str) {
        if self.progress_live() {
            if let Some(p) = self.progress.borrow_mut().as_mut() {
                p.done += 1;
                p.title = msg.to_string();
            }
            self.repaint_progress();
            return;
        }
        self.write(&format!("{}\n", self.bar()));
        self.write(&format!(
            "{}  {}\n",
            self.accent(STEP_ACTIVE),
            self.highlight(msg)
        ));
    }

    fn ok(&self, msg: &str) {
        if self.progress_live() {
            return;
        }
        self.write(&format!("{}  {} {msg}\n", self.bar(), self.dim(STEP_DONE)));
    }

    /// A warning outlives the run, so it is printed above the live line, never into it.
    fn warn(&self, msg: &str) {
        let mark = if self.caps.colors == Colors::None {
            WARN.to_string()
        } else {
            format!("\x1b[33m{WARN}\x1b[0m")
        };
        let live = self.progress_live();
        if live {
            self.clear_progress();
        }
        self.write(&format!("{}  {mark} {msg}\n", self.bar()));
        if live {
            self.repaint_progress();
        }
    }

    fn die(&self, msg: &str) {
        self.end_progress();
        self.failure(msg);
    }

    fn plus(&self, cmd: &str) {
        if self.progress_live() {
            return;
        }
        self.write(&format!(
            "{}    {}\n",
            self.bar(),
            self.dim(&format!("+ {cmd}"))
        ));
    }

    fn detail(&self, msg: &str) {
        if self.progress_live() {
            return;
        }
        self.write(&format!("{}    {}\n", self.bar(), self.dim(msg)));
    }

    fn line(&self, msg: &str) {
        if self.progress_live() {
            return;
        }
        self.write(&format!("{}  {msg}\n", self.bar()));
    }

    fn blank(&self) {
        if self.progress_live() {
            return;
        }
        self.write(&format!("{}\n", self.bar()));
    }
}

pub fn action_of(step: Step) -> Option<Action> {
    match step {
        Step::Run(action) => Some(action),
        _ => None,
    }
}
