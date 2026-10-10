//! Terminal seam: keys in, frames out.
//!
//! The TUI draws here, not through cliclack: `Select` cannot mark an item
//! unselectable, prompts go to stderr (which would split the command-echo
//! transcript), and the prompt loop is private and refuses a non-tty so
//! nothing it draws can be tested in-process.
//!
//! Keys come from `/dev/tty`, not stdin. Under `curl | sh` stdin is the
//! script — the same trap the sh installer opens `/dev/tty` to dodge.
//!
//! `ScriptedTerm` answers a queued key list and keeps the on-screen frame
//! so goldens can assert on it.

use std::collections::VecDeque;
#[cfg(unix)]
use std::fs::File;

use console::Term;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    /// Text entry only; every other prompt reads it as `Other`.
    Backspace,
    /// Esc, Ctrl-C, `q`, or a terminal that has stopped answering.
    Cancel,
    Char(char),
    Other,
}

pub trait Terminal {
    fn write(&mut self, text: &str);
    fn read_key(&mut self) -> Key;
    /// No alternate screen: the transcript scrolls and scrollback is the audit log.
    fn clear_last_lines(&mut self, n: usize);
    fn width(&self) -> u16;
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
}

pub struct ConsoleTerm {
    term: Term,
}

impl ConsoleTerm {
    /// `None` with no controlling terminal. In a container or under a service
    /// `/dev/tty` exists but opens ENXIO, so probing `-r`/`-w` is not enough.
    #[cfg(unix)]
    pub fn open() -> Option<ConsoleTerm> {
        let read = File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let write = read.try_clone().ok()?;
        let term = Term::read_write_pair(read, write);
        term.is_term().then_some(ConsoleTerm { term })
    }

    /// Windows has no `/dev/tty` and no `curl | sh` stdin to route around; the
    /// process console is the terminal.
    #[cfg(not(unix))]
    pub fn open() -> Option<ConsoleTerm> {
        let term = Term::stdout();
        term.is_term().then_some(ConsoleTerm { term })
    }
}

impl Terminal for ConsoleTerm {
    fn write(&mut self, text: &str) {
        let _ = self.term.write_str(text);
    }

    fn read_key(&mut self) -> Key {
        match self.term.read_key() {
            Ok(console::Key::ArrowUp) => Key::Up,
            Ok(console::Key::ArrowDown) => Key::Down,
            Ok(console::Key::ArrowLeft) => Key::Left,
            Ok(console::Key::ArrowRight) => Key::Right,
            Ok(console::Key::Enter) => Key::Enter,
            Ok(console::Key::Char(' ')) => Key::Space,
            Ok(console::Key::Backspace | console::Key::Del) => Key::Backspace,
            Ok(console::Key::Escape | console::Key::CtrlC) => Key::Cancel,
            Ok(console::Key::Char(c)) => Key::Char(c),
            Ok(_) => Key::Other,
            // A terminal that stopped answering must end the loop, never spin it.
            Err(_) => Key::Cancel,
        }
    }

    fn clear_last_lines(&mut self, n: usize) {
        let _ = self.term.clear_last_lines(n);
    }

    fn width(&self) -> u16 {
        self.term.size().1
    }

    fn hide_cursor(&mut self) {
        let _ = self.term.hide_cursor();
    }

    fn show_cursor(&mut self) {
        let _ = self.term.show_cursor();
    }
}

pub struct ScriptedTerm {
    pub keys: VecDeque<Key>,
    pub out: String,
    pub width: u16,
    /// Every frame in order, so a golden can assert on more than the last.
    pub frames: Vec<String>,
}

impl ScriptedTerm {
    pub fn new(keys: &[Key]) -> ScriptedTerm {
        ScriptedTerm {
            keys: keys.iter().copied().collect(),
            out: String::new(),
            width: 100,
            frames: Vec::new(),
        }
    }

    pub fn screen(&self) -> &str {
        &self.out
    }
}

impl Terminal for ScriptedTerm {
    fn write(&mut self, text: &str) {
        self.out.push_str(text);
        self.frames.push(text.to_string());
    }

    /// An exhausted script cancels rather than blocking, so a wrong test hangs nothing.
    fn read_key(&mut self) -> Key {
        self.keys.pop_front().unwrap_or(Key::Cancel)
    }

    fn clear_last_lines(&mut self, n: usize) {
        for _ in 0..n {
            match self.out.trim_end_matches('\n').rfind('\n') {
                Some(at) => self.out.truncate(at + 1),
                None => self.out.clear(),
            }
        }
    }

    fn width(&self) -> u16 {
        self.width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_lines_rewinds_the_captured_frame() {
        let mut t = ScriptedTerm::new(&[]);
        t.write("one\ntwo\nthree\n");
        t.clear_last_lines(2);
        assert_eq!(t.screen(), "one\n");
    }

    #[test]
    fn clearing_more_lines_than_exist_empties_the_screen() {
        let mut t = ScriptedTerm::new(&[]);
        t.write("only\n");
        t.clear_last_lines(5);
        assert_eq!(t.screen(), "");
    }

    #[test]
    fn an_exhausted_script_cancels_instead_of_blocking() {
        let mut t = ScriptedTerm::new(&[Key::Down]);
        assert_eq!(t.read_key(), Key::Down);
        assert_eq!(t.read_key(), Key::Cancel);
        assert_eq!(t.read_key(), Key::Cancel);
    }
}
