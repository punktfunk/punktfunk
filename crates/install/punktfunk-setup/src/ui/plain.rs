//! The no-TTY / `--yes` renderer: the sh installer's `==>` / `ok` / `!!` lines.
//!
//! D5: a box with no terminal sees that output byte-for-byte, so CI containers
//! and scripts do not notice a renderer swap. Warnings and the die line stay
//! on stderr for the same reason. The TUI renders the same `Plan` through a
//! different surface; nothing below `ui` knows which one is attached.
//!
//! `Sink::Buffer` is what the golden suite renders into — one stream, so the ordering of
//! warnings against commands is part of what the goldens pin. `Sink::Writer` is the Windows
//! silent path's `/LOG=` file and attached console, same one-stream rule.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

pub type Buffer = Rc<RefCell<String>>;

pub enum Sink {
    Stdio,
    Buffer(Buffer),
    Writer(RefCell<Box<dyn Write>>),
}

pub struct Plain {
    sink: Sink,
    color: bool,
}

impl Plain {
    pub fn stdio(color: bool) -> Self {
        Self {
            sink: Sink::Stdio,
            color,
        }
    }

    /// Both streams into one writer — a log file, an attached console.
    pub fn to_writer(writer: impl Write + 'static, color: bool) -> Self {
        Self {
            sink: Sink::Writer(RefCell::new(Box::new(writer))),
            color,
        }
    }

    /// Both streams into one buffer, uncoloured — the golden suite's renderer.
    pub fn capture() -> (Self, Buffer) {
        let buf: Buffer = Rc::new(RefCell::new(String::new()));
        (
            Self {
                sink: Sink::Buffer(buf.clone()),
                color: false,
            },
            buf,
        )
    }

    fn emit(&self, stderr: bool, line: &str) {
        match &self.sink {
            Sink::Buffer(buf) => {
                buf.borrow_mut().push_str(line);
                buf.borrow_mut().push('\n');
            }
            Sink::Writer(w) => {
                let mut w = w.borrow_mut();
                let _ = writeln!(w, "{line}");
                let _ = w.flush();
            }
            Sink::Stdio if stderr => {
                let mut e = std::io::stderr();
                let _ = writeln!(e, "{line}");
            }
            Sink::Stdio => {
                let mut o = std::io::stdout();
                let _ = writeln!(o, "{line}");
            }
        }
    }

    fn tag(&self, code: &str, mark: &str) -> String {
        if self.color {
            format!("\x1b[1;{code}m{mark}\x1b[0m")
        } else {
            mark.to_string()
        }
    }
}

impl crate::ui::Reporter for Plain {
    fn say(&self, msg: &str) {
        let tag = self.tag("36", "==>");
        self.emit(false, &format!("{tag} {msg}"));
    }

    fn ok(&self, msg: &str) {
        let tag = self.tag("32", "  ok");
        self.emit(false, &format!("{tag} {msg}"));
    }

    fn warn(&self, msg: &str) {
        let tag = self.tag("33", "  !!");
        self.emit(true, &format!("{tag} {msg}"));
    }

    fn die(&self, msg: &str) {
        let tag = self.tag("31", "  xx");
        self.emit(true, &format!("{tag} {msg}"));
    }

    fn plus(&self, cmd: &str) {
        self.emit(false, &format!("  + {cmd}"));
    }

    /// Continuation under a step, indented to the `ok` column.
    fn detail(&self, msg: &str) {
        self.emit(false, &format!("     {msg}"));
    }

    fn blank(&self) {
        self.emit(false, "");
    }

    fn line(&self, msg: &str) {
        self.emit(false, msg);
    }
}
