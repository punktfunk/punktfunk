//! Capture a real HID device's report descriptor, decode it, and print it in the shape the
//! `pf-gamepad` driver keeps its blobs in.
//!
//! WHY THIS EXISTS. `packaging/windows/drivers/pf-gamepad/src/lib.rs` serves a report descriptor
//! per emulated pad. Three of the four are verbatim captures off real hardware; `XBOX_RDESC` was
//! hand-constructed, and its own provenance warning came true three separate times (a missing
//! channel-proof Feature report meant the pad never delivered a single input report; there is no
//! OUTPUT collection at all, so rumble cannot arrive; `xinputhid` appears to validate the
//! descriptor and rejects ours). We claim a genuine Microsoft VID/PID, and SDL, Steam and Windows
//! all apply stock mappings keyed on it — so a layout that differs from the real pad lands every
//! control on the wrong action. Captures, not constructions.
//!
//! USAGE
//! ```text
//!   hid-descriptor-dump --list                     # every HID device, with vid/pid and usage
//!   hid-descriptor-dump --vid 045E --pid 0B22      # dump every collection of that device
//!   hid-descriptor-dump --vid 054C --pid 0CE6 --name DUALSENSE_RDESC
//!   hid-descriptor-dump --path '\\?\HID#...'       # one exact collection
//! ```
//!
//! WHAT THE DESCRIPTOR COMES FROM, PER PLATFORM. On Linux hidapi reads
//! `/sys/class/hidraw/hidrawN/device/report_descriptor` — the literal bytes the device sent. On
//! Windows there is no API that returns those bytes: the HID class driver keeps only the parsed
//! form, so hidapi RECONSTRUCTS a descriptor from `HidD_GetPreparsedData`. The reconstruction is
//! faithful in structure, item order and every field's bit offset — which is what a layout diff
//! needs — but the byte encoding may differ from the wire (an item the device sent as one byte can
//! come back as two, and hidapi emits collections it inferred). ⇒ **Diff the LAYOUT MAP and the
//! item listing, not the raw bytes, when the capture came off Windows.** A byte-exact capture
//! needs Linux hidraw.
//!
//! This tool is deliberately not a workspace member; see its `Cargo.toml`.

mod decode;
mod sc2;

use std::process::ExitCode;

struct Args {
    list: bool,
    vid: Option<u16>,
    pid: Option<u16>,
    path: Option<String>,
    name: Option<String>,
    read: Option<usize>,
    rust_source: Option<String>,
    symbol: Option<String>,
    sc2: bool,
    sc2_watch: Option<u64>,
}

/// Pull a `static NAME: [u8; N] = [ 0x.., ... ];` out of a Rust source file.
///
/// This is what makes the diff exact rather than eyeballed: our own shipped blobs get decoded by
/// the same decoder, into the same listing and the same layout table, as a capture off real
/// hardware. No hardware needed for this mode.
fn extract_rust_array(src: &str, symbol: &str) -> Result<Vec<u8>, String> {
    let at = src
        .find(&format!("static {symbol}:"))
        .ok_or_else(|| format!("no `static {symbol}:` in that file"))?;

    // 🛑 STRIP COMMENTS FIRST, then find the brackets — not the other way round. These arrays are
    // heavily annotated and the annotations contain both `,` and `]` (a comment documenting a wire
    // layout as `[0x03][enable][left]…` is real, and it appears inside `XBOX_RDESC`). Locating the
    // closing bracket on the raw text stops at the first `]` in a COMMENT and silently truncates
    // the array — which reads as a corrupt descriptor rather than a parse bug.
    let tail: String = src[at..]
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");

    // First `[` is the type's `[u8; N]`; the next one opens the literal.
    let open = tail
        .find('[')
        .and_then(|i| tail[i + 1..].find('[').map(|j| i + 1 + j + 1))
        .ok_or("no array literal found")?;
    let close = tail[open..]
        .find(']')
        .ok_or("array literal is never closed")?
        + open;
    let body = &tail[open..close];
    let mut out = Vec::new();
    for tok in body.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let hex = tok.trim_start_matches("0x").trim_start_matches("0X");
        out.push(u8::from_str_radix(hex, 16).map_err(|_| format!("`{tok}` is not a hex byte"))?);
    }
    Ok(out)
}

fn parse_u16(s: &str) -> Option<u16> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(s, 16).ok()
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        list: false,
        vid: None,
        pid: None,
        path: None,
        name: None,
        read: None,
        rust_source: None,
        symbol: None,
        sc2: false,
        sc2_watch: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--list" | "-l" => a.list = true,
            "--vid" => {
                let v = it.next().ok_or("--vid wants a hex value")?;
                a.vid = Some(parse_u16(&v).ok_or_else(|| format!("--vid: {v} is not hex"))?);
            }
            "--pid" => {
                let v = it.next().ok_or("--pid wants a hex value")?;
                a.pid = Some(parse_u16(&v).ok_or_else(|| format!("--pid: {v} is not hex"))?);
            }
            "--path" => a.path = Some(it.next().ok_or("--path wants a device path")?),
            "--name" => a.name = Some(it.next().ok_or("--name wants an identifier")?),
            "--read" => {
                let v = it.next().ok_or("--read wants a count")?;
                a.read = Some(
                    v.parse()
                        .map_err(|_| format!("--read: {v} is not a count"))?,
                );
            }
            "--rust-source" => a.rust_source = Some(it.next().ok_or("--rust-source wants a path")?),
            "--symbol" => a.symbol = Some(it.next().ok_or("--symbol wants an identifier")?),
            "--sc2" => a.sc2 = true,
            "--sc2-watch" => {
                let v = it.next().ok_or("--sc2-watch wants seconds")?;
                a.sc2_watch = Some(
                    v.parse()
                        .map_err(|_| format!("--sc2-watch: {v} is not seconds"))?,
                );
            }
            "--help" | "-h" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if !a.list && a.vid.is_none() && a.path.is_none() && a.rust_source.is_none() {
        a.list = true;
    }
    if a.rust_source.is_some() != a.symbol.is_some() {
        return Err("--rust-source and --symbol go together".into());
    }
    Ok(a)
}

const HELP: &str = "\
hid-descriptor-dump — capture a real HID device's report descriptor

  --list                 list every HID device this box can open
  --vid <hex>            select by vendor id   (e.g. 045E)
  --pid <hex>            select by product id  (e.g. 0B22)
  --path <string>        select one exact collection by its device path
  --name <IDENT>         also emit a `static IDENT: [u8; N]` ready to paste into the driver
  --read <n>             after dumping, read n live input reports and show which bytes move
  --rust-source <file>   decode a blob we already ship instead of a device (no hardware needed)
  --symbol <IDENT>       which `static IDENT: [u8; N]` in that file to decode
  --sc2                  Steam Controller 2: print the pad's (and Puck's) feature replies
  --sc2-watch <secs>     Steam Controller 2: print each button edge by bit and name

With --vid/--pid every matching collection is dumped: a real Xbox pad presents two (a game
controller and a keyboard), and they are separate devices to hidapi.

--read is the ground truth a reconstructed descriptor cannot give you: it is the literal wire
bytes. Use it to settle report length, whether reports are numbered, and which byte a control
actually lives in — wiggle one control at a time and watch the changed-byte mask.";

/// Print a descriptor every way that is useful for a diff: raw, item listing, layout table,
/// a presence summary, and optionally a paste-ready Rust `static`.
fn report(desc: &[u8], emit_as: Option<&str>) {
    println!("\n-- RAW ({} bytes) --", desc.len());
    for (off, chunk) in desc.chunks(16).enumerate() {
        let hex = chunk
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!("  {:04X}  {hex}", off * 16);
    }

    let decoded = decode::decode(desc);
    println!("\n-- ITEMS --");
    print!("{}", decoded.listing);

    println!("\n-- LAYOUT --");
    print!("{}", decode::layout_map(&decoded.fields));

    println!("\n-- SUMMARY --");
    for (k, label) in [
        (decode::MainKind::Input, "INPUT"),
        (decode::MainKind::Output, "OUTPUT"),
        (decode::MainKind::Feature, "FEATURE"),
    ] {
        let count = decoded.fields.iter().filter(|f| f.kind == k).count();
        println!(
            "  {label:<8} items: {count}{}",
            if count == 0 { "   <-- NONE" } else { "" }
        );
    }
    if decoded.problems.is_empty() {
        println!("  structure: OK");
    } else {
        println!("  structure: {} PROBLEM(S)", decoded.problems.len());
        for p in &decoded.problems {
            println!("    - {p}");
        }
    }

    if let Some(name) = emit_as {
        println!("\n-- RUST --");
        print!("{}", decode::rust_array(name, desc));
    }
}

/// Read live input reports and show which bytes ever move. The descriptor says where a control
/// SHOULD be; this says where it IS.
fn watch(dev: &hidapi::HidDevice, count: usize) {
    println!("\n-- LIVE REPORTS ({count} requested, 3 s each) --");
    println!("   (move ONE control at a time and read the changed-byte mask)");
    let mut buf = [0u8; 256];
    let mut first: Option<Vec<u8>> = None;
    let mut ever_changed = vec![false; 256];
    let mut got = 0usize;
    for _ in 0..count {
        match dev.read_timeout(&mut buf, 3000) {
            Ok(0) => {
                println!("  (timeout — no report; the pad may be idle)");
                continue;
            }
            Ok(n) => {
                got += 1;
                let sample = &buf[..n];
                let hex = sample
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                match &first {
                    None => {
                        println!("  len={n}  {hex}   <-- baseline");
                        first = Some(sample.to_vec());
                    }
                    Some(base) => {
                        let mut marks = String::new();
                        for i in 0..n {
                            let differs = base.get(i) != Some(&sample[i]);
                            if differs {
                                ever_changed[i] = true;
                            }
                            marks.push_str(if differs { "^^ " } else { ".. " });
                        }
                        println!("  len={n}  {hex}");
                        println!("           {marks}");
                    }
                }
            }
            Err(e) => {
                println!("  read error: {e}");
                break;
            }
        }
    }
    if let Some(base) = &first {
        let moved: Vec<String> = (0..base.len())
            .filter(|i| ever_changed[*i])
            .map(|i| i.to_string())
            .collect();
        println!(
            "  {got} report(s); report length {}; bytes that ever moved: {}",
            base.len(),
            if moved.is_empty() {
                "none".to_string()
            } else {
                moved.join(", ")
            }
        );
        println!(
            "  first byte of every report was 0x{:02X} — {}",
            base[0],
            if base[0] == 0x01 {
                "consistent with a numbered report id 1"
            } else {
                "note this when deciding whether reports are numbered"
            }
        );
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{HELP}");
            return ExitCode::FAILURE;
        }
    };

    // Decoding one of our own blobs needs no hardware, so it runs before hidapi is even opened —
    // this mode works on any box, including CI and a Mac.
    if let (Some(file), Some(symbol)) = (&args.rust_source, &args.symbol) {
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {file}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let desc = match extract_rust_array(&src, symbol) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("{}", "=".repeat(96));
        println!("SHIPPED BLOB  —  {symbol}  from {file}");
        println!("{}", "=".repeat(96));
        report(&desc, args.name.as_deref());
        return ExitCode::SUCCESS;
    }

    let api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: hidapi init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let devices: Vec<_> = api.device_list().collect();
    if args.list {
        println!(
            "{:<6} {:<6} {:<5} {:<5} {:<34} path",
            "vid", "pid", "page", "usage", "product"
        );
        for d in &devices {
            println!(
                "{:04X}   {:04X}   {:04X}  {:04X}  {:<34} {}",
                d.vendor_id(),
                d.product_id(),
                d.usage_page(),
                d.usage(),
                d.product_string().unwrap_or("—"),
                d.path().to_string_lossy()
            );
        }
        println!("\n{} device(s).", devices.len());
        if args.vid.is_none() && args.path.is_none() {
            return ExitCode::SUCCESS;
        }
    }

    let selected: Vec<_> = devices
        .iter()
        .filter(|d| {
            if let Some(p) = &args.path {
                return d.path().to_string_lossy() == p.as_str();
            }
            args.vid.is_none_or(|v| d.vendor_id() == v)
                && args.pid.is_none_or(|p| d.product_id() == p)
        })
        .collect();

    if selected.is_empty() {
        eprintln!(
            "error: nothing matched. If this is a Bluetooth pad, POWER IT ON — a disconnected BLE \
             device leaves its devnodes behind but has no HID interface to open."
        );
        return ExitCode::FAILURE;
    }

    let mut failures = 0usize;
    for (n, d) in selected.iter().enumerate() {
        println!("\n{}", "=".repeat(96));
        println!(
            "COLLECTION {}/{}  —  {:04X}:{:04X}  usage_page 0x{:04X} ({}) usage 0x{:04X}",
            n + 1,
            selected.len(),
            d.vendor_id(),
            d.product_id(),
            d.usage_page(),
            decode::usage_page_name(d.usage_page()),
            d.usage()
        );
        println!(
            "  manufacturer : {}",
            d.manufacturer_string().unwrap_or("—")
        );
        println!("  product      : {}", d.product_string().unwrap_or("—"));
        println!("  serial       : {}", d.serial_number().unwrap_or("—"));
        println!("  release      : 0x{:04X}", d.release_number());
        println!("  interface    : {}", d.interface_number());
        println!("  path         : {}", d.path().to_string_lossy());
        println!("{}", "=".repeat(96));

        let dev = match api.open_path(d.path()) {
            Ok(dev) => dev,
            Err(e) => {
                eprintln!("  !! open failed: {e}");
                failures += 1;
                continue;
            }
        };
        // 4 KiB is the HID class driver's own ceiling for a report descriptor.
        let mut buf = vec![0u8; 4096];
        let len = match dev.get_report_descriptor(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("  !! report-descriptor read failed: {e}");
                failures += 1;
                continue;
            }
        };
        buf.truncate(len);

        let emit_as = args.name.as_ref().map(|name| {
            if selected.len() > 1 {
                format!("{name}_COL{:02}", n + 1)
            } else {
                name.clone()
            }
        });
        report(&buf, emit_as.as_deref());

        if let Some(count) = args.read {
            watch(&dev, count);
        }
        // Only the vendor collections take Valve's feature reports: FF00:01 is a pad, FF00:02
        // the Puck's management interface.
        if d.usage_page() == 0xFF00 {
            if args.sc2 {
                sc2::query(&dev, d.product_id(), d.usage());
            }
            if let (Some(secs), 0x01) = (args.sc2_watch, d.usage()) {
                sc2::watch_buttons(&dev, secs);
            }
        }
    }

    if failures > 0 {
        eprintln!("\n{failures} collection(s) could not be read.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
