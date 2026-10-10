//! Nested X11 splash so a fresh headless gamescope composites (and thus captures).
//!
//! gamescope pushes a PipeWire buffer only on composite, and composites only a
//! client that paints. The parent `spawn` wrapper backgrounds this process
//! before exec'ing the nested app. [`run`] maps a full-screen window and damages
//! it at [`TICK`] until the X connection dies.
//!
//! `--steam` composites only windows whose appid is in the root
//! `GAMESCOPECTRL_BASELAYER_APPID` list. The splash sets `STEAM_GAME` to
//! [`STEAM_UI_APPID`] and seeds that list iff empty; Steam's rewrite at game
//! launch is the handover. Without `--steam` any mapped painting window
//! composites and the atoms are inert.
//!
//! Unfocused paints schedule no composite. The process does not exit on its own;
//! gamescope's reaper, or an X error, is the only stop.

use anyhow::{Context, Result};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ConnectionExt, CreateGCAux, CreateWindowAux, PropMode, Rectangle,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Steam client UI appid (not a game). `--steam` treats this id as the overlay client.
const STEAM_UI_APPID: u32 = 769;

const BG: u32 = 0x0d0d0d;

/// 400 ms ≈ 2.5 fps. Each paint is the damage that makes gamescope push a capture buffer.
const TICK: Duration = Duration::from_millis(400);

/// Hidden `gamescope-splash` subcommand. Blocks until DISPLAY is unreachable or X dies.
pub(crate) fn run() -> Result<()> {
    let (conn, screen_num) = connect_with_retry()?;
    let screen = &conn.setup().roots[screen_num];
    let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().background_pixel(BG),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Punktfunk",
    )?;
    // Seed the baselayer only while empty; Steam's rewrite at game launch is the handover.
    let steam_game = conn.intern_atom(false, b"STEAM_GAME")?.reply()?.atom;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        steam_game,
        AtomEnum::CARDINAL,
        &[STEAM_UI_APPID],
    )?;
    let baselayer = conn
        .intern_atom(false, b"GAMESCOPECTRL_BASELAYER_APPID")?
        .reply()?
        .atom;
    let existing = conn
        .get_property(false, screen.root, baselayer, AtomEnum::CARDINAL, 0, 4)?
        .reply()?;
    if existing.value_len == 0 {
        conn.change_property32(
            PropMode::REPLACE,
            screen.root,
            baselayer,
            AtomEnum::CARDINAL,
            &[STEAM_UI_APPID],
        )?;
    }
    conn.map_window(win)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(BG))?;
    conn.flush()?;
    tracing::info!(
        w,
        h,
        seeded_baselayer = existing.value_len == 0,
        "gamescope splash: mapped"
    );

    // Server-side fills are the damage; the background pixel handles exposures, so there is no
    // event loop. A failed request means the session (or its Xwayland) is gone.
    let bar_w = 120i16;
    let bar_h = 8i16;
    let bar = Rectangle {
        x: (w as i16) / 2 - bar_w / 2,
        y: (h as i16) / 2 - bar_h / 2,
        width: bar_w as u16,
        height: bar_h as u16,
    };
    let mut t: u32 = 0;
    loop {
        let phase = (t % 8) as i32;
        let tri = if phase < 4 { phase } else { 8 - phase };
        let grey = 0x1a + 0x0c * tri as u32;
        let colour = (grey << 16) | (grey << 8) | grey;
        conn.change_gc(gc, &ChangeGCAux::new().foreground(colour))?;
        conn.poly_fill_rectangle(win, gc, &[bar])?;
        conn.flush()
            .context("gamescope splash: X connection lost")?;
        t = t.wrapping_add(1);
        std::thread::sleep(TICK);
    }
}

/// 10 s matches the capture first-frame timeout. Waiting longer cannot save the stream.
const CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// `x11rb::connect` has no timeout. An Xwayland that accepts the socket and never
/// answers setup blocks forever, and a deadline consulted only on `Err` is never
/// reached. The retry therefore runs on a worker and the budget is `recv_timeout`.
/// A worker still stuck in `connect` is abandoned; joining it would hang this process.
fn connect_with_retry() -> Result<(RustConnection, usize)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pf-splash-x11-connect".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + CONNECT_BUDGET;
            loop {
                match x11rb::connect(None) {
                    Ok(ok) => {
                        let _ = tx.send(Ok(ok));
                        return;
                    }
                    Err(e) if std::time::Instant::now() >= deadline => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            }
        })
        .context("gamescope splash: start the X connect thread")?;
    // One second past the worker deadline so a slow-but-finished connect still wins.
    match rx.recv_timeout(CONNECT_BUDGET + Duration::from_secs(1)) {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(e).context("gamescope splash: connect to the session DISPLAY"),
        Err(_) => {
            tracing::warn!(
                secs = CONNECT_BUDGET.as_secs(),
                "gamescope splash: the session's X server accepted no connection — nothing paints \
                 in this gamescope, so the capture starves; the reason is in the gamescope log"
            );
            anyhow::bail!("gamescope splash: connecting to the session DISPLAY did not return")
        }
    }
}
