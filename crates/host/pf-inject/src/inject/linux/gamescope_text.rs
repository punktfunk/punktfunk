//! Committed text in a gamescope session, through gamescope's own input method
//! (`gamescope_input_method`, what Steam's on-screen keyboard types with).
//!
//! gamescope types each string against its session keymap: it holds and announces the
//! modifiers a character's level needs, and gives a character the keymap lacks a spare key
//! in a keymap of its own. Upstream's EIS server only presses keys and announces no
//! modifiers, so a Shift-level character (a digit on AZERTY) can't be typed there. Ours
//! announces them from `+pfhdr43`; text takes this one route on every build.
//!
//! The protocol rides gamescope's Wayland socket, which its EIS socket is named after
//! (`<display>-ei`). That is a second connection beside EIS: [`TextPipe::type_char`] waits
//! until gamescope has typed, so a key sent over EIS after it lands after it.

use anyhow::{bail, Context as _, Result};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};

// Inline scanner (no build.rs). Path is relative to CARGO_MANIFEST_DIR.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod proto {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/gamescope-input-method.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/gamescope-input-method.xml");
}

use proto::gamescope_input_method::{self, GamescopeInputMethod};
use proto::gamescope_input_method_manager::GamescopeInputMethodManager;

pub(super) struct TextPipe {
    _conn: Connection,
    queue: EventQueue<State>,
    state: State,
    ime: GamescopeInputMethod,
}

#[derive(Default)]
struct State {
    manager: Option<GamescopeInputMethodManager>,
    seat: Option<WlSeat>,
    /// The last `done` serial; a commit names it or gamescope ignores the commit.
    serial: Option<u32>,
    unavailable: bool,
}

impl TextPipe {
    /// Connect to the display behind gamescope's EIS socket `ei` and make an input method.
    pub fn open(ei: &Path) -> Result<Self> {
        let Some(socket) = display_of(ei) else {
            bail!("{} is not a gamescope EIS socket", ei.display());
        };
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("connect {}", socket.display()))?;
        let conn = Connection::from_socket(stream).context("open the Wayland connection")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        let (Some(manager), Some(seat)) = (state.manager.take(), state.seat.take()) else {
            bail!("gamescope offers no input method on {}", socket.display());
        };
        let ime = manager.create_input_method(&seat, &qh, ());
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        if state.unavailable || state.serial.is_none() {
            bail!("gamescope's input method is unavailable");
        }
        tracing::info!(socket = %socket.display(), "gamescope input method open for text");
        Ok(Self {
            _conn: conn,
            queue,
            state,
            ime,
        })
    }

    /// Type `c` and return once gamescope has.
    pub fn type_char(&mut self, c: char) -> Result<()> {
        let Some(serial) = self.state.serial.filter(|_| !self.state.unavailable) else {
            bail!("gamescope's input method is unavailable");
        };
        self.ime.set_string(c.to_string());
        self.ime.commit(serial);
        self.queue
            .roundtrip(&mut self.state)
            .context("Wayland roundtrip")?;
        Ok(())
    }
}

/// `$XDG_RUNTIME_DIR/gamescope-0` for `$XDG_RUNTIME_DIR/gamescope-0-ei`.
fn display_of(ei: &Path) -> Option<PathBuf> {
    let name = ei.file_name()?.to_str()?.strip_suffix("-ei")?;
    (!name.is_empty()).then(|| ei.with_file_name(name))
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            // Version 1 has everything typing needs.
            match interface.as_str() {
                "gamescope_input_method_manager" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, 1, qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<GamescopeInputMethod, ()> for State {
    fn event(
        state: &mut Self,
        _: &GamescopeInputMethod,
        event: gamescope_input_method::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            gamescope_input_method::Event::Done { serial } => state.serial = Some(serial),
            gamescope_input_method::Event::Unavailable => state.unavailable = true,
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: GamescopeInputMethodManager);

#[cfg(test)]
mod tests {
    use super::display_of;
    use std::path::{Path, PathBuf};

    /// The display is the EIS socket's name without `-ei`, in the same directory.
    #[test]
    fn the_display_is_the_eis_socket_without_its_suffix() {
        let of = |p: &str| display_of(Path::new(p));
        assert_eq!(
            of("/run/user/1000/gamescope-0-ei"),
            Some(PathBuf::from("/run/user/1000/gamescope-0"))
        );
        assert_eq!(of("/run/user/1000/gamescope-0"), None);
        assert_eq!(of("/run/user/1000/-ei"), None);
        assert_eq!(of("/run/user/1000/eis-0"), None);
    }
}
