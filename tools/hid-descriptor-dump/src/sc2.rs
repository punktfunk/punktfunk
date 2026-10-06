//! Steam Controller 2 ground truth: the feature replies a real pad gives, and which button bit
//! each physical button sets.
//!
//! The host answers Steam's feature GETs for a streamed pad. `--sc2` records what the real pad
//! answers to the same queries, so those answers can be replayed instead of made up. Every
//! request here is a read; only `--sc2-watch` writes, and only lizard mode (off while it reads,
//! back on at the end), as SDL does.

use hidapi::HidDevice;
use std::time::{Duration, Instant};

/// Controller feature report (`0x01`) and the Puck's dongle report (`0x02`).
const RID_PAD: u8 = 0x01;
const RID_DONGLE: u8 = 0x02;

/// Queries Steam and hid-steam send, as `(report id, command frame)`. The `0xED` keys and the
/// `0xF2` indexes come from Steam's writes to our virtual pad; the rest from hid-steam and SDL.
const PAD_QUERIES: &[(u8, &[u8])] = &[
    (RID_PAD, &[0x83, 0x00]),
    (RID_PAD, &[0xAE, 0x15, 0x00]),
    (RID_PAD, &[0xAE, 0x15, 0x01]),
    (RID_PAD, &[0xAE, 0x15, 0x02]),
    (RID_PAD, &[0xAE, 0x15, 0x03]),
    (RID_PAD, &[0xF2, 0x01, 0x00]),
    (RID_PAD, &[0xF2, 0x01, 0x01]),
    (RID_PAD, &[0xF2, 0x01, 0x02]),
];
const PUCK_QUERIES: &[(u8, &[u8])] = &[
    (RID_PAD, b"\xED\x17user/wireless_transport"),
    (RID_PAD, b"\xED\x08esb/bond"),
    (RID_DONGLE, &[0x83, 0x00]),
    (RID_DONGLE, &[0xA3, 0x00]),
    (RID_DONGLE, &[0xB4]),
];

/// `TritonButtons` bit → name. SDL's enum calls `0x40` VIEW and `0x4000` MENU, but SDL's own
/// mapping and hid-steam both make `0x40` Start (☰) and `0x4000` Select (⧉).
const BUTTONS: &[(u32, &str)] = &[
    (0x0000_0001, "A"),
    (0x0000_0002, "B"),
    (0x0000_0004, "X"),
    (0x0000_0008, "Y"),
    (0x0000_0010, "QAM (…)"),
    (0x0000_0020, "R3"),
    (0x0000_0040, "SDL-enum VIEW / kernel BTN_START (menu ☰?)"),
    (0x0000_0080, "R4"),
    (0x0000_0100, "R5"),
    (0x0000_0200, "RB"),
    (0x0000_0400, "DPAD_DOWN"),
    (0x0000_0800, "DPAD_RIGHT"),
    (0x0000_1000, "DPAD_LEFT"),
    (0x0000_2000, "DPAD_UP"),
    (0x0000_4000, "SDL-enum MENU / kernel BTN_SELECT (view ⧉?)"),
    (0x0000_8000, "L3"),
    (0x0001_0000, "STEAM"),
    (0x0002_0000, "L4"),
    (0x0004_0000, "L5"),
    (0x0008_0000, "LB"),
    (0x0010_0000, "right stick touch"),
    (0x0020_0000, "right pad touch"),
    (0x0040_0000, "right pad click"),
    (0x0080_0000, "RT click"),
    (0x0100_0000, "left stick touch"),
    (0x0200_0000, "left pad touch"),
    (0x0400_0000, "left pad click"),
    (0x0800_0000, "LT click"),
    (0x1000_0000, "right grip touch"),
    (0x2000_0000, "left grip touch"),
];

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Printable runs of 4+ ASCII characters, so a serial stands out in a reply.
fn ascii_runs(b: &[u8]) -> String {
    let mut out = Vec::new();
    let mut run = String::new();
    for &c in b {
        if c.is_ascii_graphic() {
            run.push(c as char);
        } else {
            if run.len() >= 4 {
                out.push(std::mem::take(&mut run));
            }
            run.clear();
        }
    }
    if run.len() >= 4 {
        out.push(run);
    }
    out.join(" | ")
}

/// A 64-byte feature frame: report id, then the command.
fn frame(rid: u8, cmd: &[u8]) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[0] = rid;
    f[1..1 + cmd.len()].copy_from_slice(cmd);
    f
}

/// SET the command, then GET until the reply echoes it, for up to a second. A Puck fetches a
/// pad's reply over the radio and stalls the GET until it has one.
fn exchange(dev: &HidDevice, rid: u8, cmd: &[u8]) -> Result<Vec<u8>, String> {
    dev.send_feature_report(&frame(rid, cmd))
        .map_err(|e| format!("set: {e}"))?;
    let mut last = String::from("nothing read");
    for tries in 1..=50 {
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = [0u8; 65];
        buf[0] = rid;
        match dev.get_feature_report(&mut buf) {
            Ok(n) if buf[..n].get(1) == cmd.first() => {
                if tries > 1 {
                    println!("  (reply after {} ms)", tries * 20);
                }
                return Ok(buf[..n].to_vec());
            }
            Ok(n) => last = hex(&buf[..n]),
            Err(e) => last = format!("get: {e}"),
        }
    }
    Err(format!("no echo of {:02X} in 1 s; last {last}", cmd[0]))
}

/// Run every query this collection answers and print request, reply and any serial in it.
pub fn query(dev: &HidDevice, pid: u16, usage: u16) {
    let puck = matches!(pid, 0x1304 | 0x1305);
    println!("\n-- SC2 FEATURE REPLIES (pid {pid:04X}, usage FF00:{usage:02X}) --");
    for rid in [RID_PAD, RID_DONGLE] {
        let mut buf = [0u8; 65];
        buf[0] = rid;
        match dev.get_feature_report(&mut buf) {
            Ok(n) => println!("  bare GET {rid:02X}         -> {}", hex(&buf[..n])),
            Err(e) => println!("  bare GET {rid:02X}         -> error: {e}"),
        }
    }
    let extra: &[(u8, &[u8])] = if puck { PUCK_QUERIES } else { &[] };
    for &(rid, cmd) in PAD_QUERIES.iter().chain(extra) {
        let shown = hex(&cmd[..cmd.len().min(3)]);
        match exchange(dev, rid, cmd) {
            Ok(r) => {
                let text = ascii_runs(&r[2.min(r.len())..]);
                println!("  {rid:02X} {shown:<12} -> {}", hex(&r));
                if !text.is_empty() {
                    println!("  {:<15}    text: {text}", "");
                }
            }
            Err(e) => println!("  {rid:02X} {shown:<12} -> {e}"),
        }
    }
}

/// `0x87 SET_SETTINGS`, one entry: `LIZARD_MODE` (9) = `on`.
fn lizard(dev: &HidDevice, on: bool) {
    let _ = dev.send_feature_report(&frame(RID_PAD, &[0x87, 0x03, 0x09, u8::from(on), 0x00]));
}

/// Print each button edge by bit and name for `secs` seconds. Press ☰ and ⧉ and read which bit
/// moves: that settles the Start/Select question on real hardware.
pub fn watch_buttons(dev: &HidDevice, secs: u64) {
    println!("\n-- SC2 BUTTONS ({secs} s) — press each button once, ☰ and ⧉ first --");
    let end = Instant::now() + Duration::from_secs(secs);
    let mut refresh = Instant::now();
    let mut was = 0u32;
    let mut buf = [0u8; 64];
    while Instant::now() < end {
        // The pad falls back to lizard mode within seconds unless this repeats (SDL: 3 s).
        if refresh <= Instant::now() {
            lizard(dev, false);
            refresh = Instant::now() + Duration::from_secs(2);
        }
        let n = match dev.read_timeout(&mut buf, 100) {
            Ok(n) => n,
            Err(e) => {
                println!("  read error: {e}");
                break;
            }
        };
        if n < 6 || !matches!(buf[0], 0x42 | 0x45 | 0x47) {
            continue;
        }
        let now = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
        for &(bit, name) in BUTTONS {
            if (now ^ was) & bit != 0 {
                let edge = if now & bit != 0 { "down" } else { "up  " };
                println!("  {edge} 0x{bit:08X}  {name}   (report {:02X})", buf[0]);
            }
        }
        let unknown = (now ^ was) & !BUTTONS.iter().fold(0, |m, &(b, _)| m | b);
        if unknown != 0 {
            println!("  ??   0x{unknown:08X}  not in the table");
        }
        was = now;
    }
    lizard(dev, true);
}
