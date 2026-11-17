//! A HID 1.11 report-descriptor decoder, written for ONE job: making a captured descriptor
//! diffable, by eye, against the hand-annotated blobs in `crates/host/pf-driver-proto/src/`.
//!
//! Two outputs matter, and they answer different questions:
//!
//! * the **item listing** — one line per HID item, formatted like the annotated `static
//!   ..._RDESC` arrays there, so a capture can be pasted straight in and read side by side;
//! * the **layout map** — the running bit offset of every field, per report id and per report
//!   kind. This is the one that catches the bugs that actually bite: `xbox_proto`'s layout tests
//!   pin byte offsets, and a descriptor that declares the same usages in a different ORDER lands
//!   every control on the wrong byte while looking correct item for item.

use std::fmt::Write as _;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MainKind {
    Input,
    Output,
    Feature,
}

impl MainKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MainKind::Input => "Input",
            MainKind::Output => "Output",
            MainKind::Feature => "Feature",
        }
    }
}

/// One `Input`/`Output`/`Feature` main item, resolved against the global/local state in force.
pub struct Field {
    pub kind: MainKind,
    pub report_id: u8,
    /// Bit offset within the report, report id byte NOT included (it is offset 0 of the wire
    /// bytes, so add 8 when comparing against a wire layout that carries the id).
    pub bit_offset: u32,
    pub bit_size: u32,
    pub count: u32,
    pub usage_page: u16,
    pub usages: Vec<u32>,
    pub usage_range: Option<(u32, u32)>,
    pub logical_min: i64,
    pub logical_max: i64,
    pub flags: u32,
}

impl Field {
    fn is_constant(&self) -> bool {
        self.flags & 1 != 0
    }

    /// How the field would be written in an `Input (...)` annotation.
    fn flags_str(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        parts.push(if self.flags & 0x01 != 0 {
            "Cnst"
        } else {
            "Data"
        });
        parts.push(if self.flags & 0x02 != 0 { "Var" } else { "Arr" });
        parts.push(if self.flags & 0x04 != 0 { "Rel" } else { "Abs" });
        if self.flags & 0x08 != 0 {
            parts.push("Wrap");
        }
        if self.flags & 0x10 != 0 {
            parts.push("NonLin");
        }
        if self.flags & 0x20 != 0 {
            parts.push("NoPref");
        }
        if self.flags & 0x40 != 0 {
            parts.push("Null State");
        }
        if self.flags & 0x80 != 0 {
            parts.push("Volatile");
        }
        if self.flags & 0x100 != 0 {
            parts.push("Buff");
        }
        parts.join(",")
    }
}

pub struct Decoded {
    /// The annotated item listing.
    pub listing: String,
    pub fields: Vec<Field>,
    /// Anything structurally wrong — trailing bytes, unbalanced collections, a truncated item.
    pub problems: Vec<String>,
}

#[derive(Clone, Default)]
struct GlobalState {
    usage_page: u16,
    logical_min: i64,
    logical_max: i64,
    physical_min: i64,
    physical_max: i64,
    unit: u32,
    unit_exp: u32,
    report_size: u32,
    report_id: u8,
    report_count: u32,
}

/// Running bit cursor, keyed by (report id, kind) — each report kind numbers its bits from zero.
#[derive(Default)]
struct Cursors {
    input: Vec<(u8, u32)>,
    output: Vec<(u8, u32)>,
    feature: Vec<(u8, u32)>,
}

impl Cursors {
    fn take(&mut self, kind: MainKind, id: u8, bits: u32) -> u32 {
        let v = match kind {
            MainKind::Input => &mut self.input,
            MainKind::Output => &mut self.output,
            MainKind::Feature => &mut self.feature,
        };
        match v.iter_mut().find(|(rid, _)| *rid == id) {
            Some((_, at)) => {
                let start = *at;
                *at += bits;
                start
            }
            None => {
                v.push((id, bits));
                0
            }
        }
    }
}

/// Sign-extend `value`, which came off the wire in `size` bytes.
fn sign_extend(value: u32, size: usize) -> i64 {
    match size {
        1 => value as u8 as i8 as i64,
        2 => value as u16 as i16 as i64,
        4 => value as i32 as i64,
        _ => value as i64,
    }
}

pub fn usage_page_name(page: u16) -> &'static str {
    match page {
        0x01 => "Generic Desktop",
        0x02 => "Simulation Controls",
        0x03 => "VR Controls",
        0x04 => "Sport Controls",
        0x05 => "Game Controls",
        0x06 => "Generic Device Controls",
        0x07 => "Keyboard/Keypad",
        0x08 => "LED",
        0x09 => "Button",
        0x0A => "Ordinal",
        0x0C => "Consumer",
        0x0D => "Digitizer",
        0x0F => "Physical Input Device (PID)",
        0xFF00..=0xFFFF => "Vendor Defined",
        _ => "",
    }
}

fn usage_name(page: u16, usage: u32) -> &'static str {
    match (page, usage) {
        (0x01, 0x01) => "Pointer",
        (0x01, 0x02) => "Mouse",
        (0x01, 0x04) => "Joystick",
        (0x01, 0x05) => "Game Pad",
        (0x01, 0x06) => "Keyboard",
        (0x01, 0x30) => "X",
        (0x01, 0x31) => "Y",
        (0x01, 0x32) => "Z",
        (0x01, 0x33) => "Rx",
        (0x01, 0x34) => "Ry",
        (0x01, 0x35) => "Rz",
        (0x01, 0x36) => "Slider",
        (0x01, 0x37) => "Dial",
        (0x01, 0x38) => "Wheel",
        (0x01, 0x39) => "Hat switch",
        (0x01, 0x3A) => "Counted Buffer",
        (0x01, 0x80) => "System Control",
        (0x01, 0x85) => "System Main Menu",
        (0x02, 0xC4) => "Accelerator",
        (0x02, 0xC5) => "Brake",
        (0x02, 0xBB) => "Throttle",
        (0x02, 0xBA) => "Rudder",
        (0x06, 0x20) => "Battery Strength",
        (0x0C, 0x01) => "Consumer Control",
        (0x0C, 0x223) => "AC Home",
        (0x0C, 0x224) => "AC Back",
        _ => "",
    }
}

fn collection_name(v: u32) -> &'static str {
    match v {
        0x00 => "Physical",
        0x01 => "Application",
        0x02 => "Logical",
        0x03 => "Report",
        0x04 => "Named Array",
        0x05 => "Usage Switch",
        0x06 => "Usage Modifier",
        _ => "Vendor",
    }
}

pub fn decode(desc: &[u8]) -> Decoded {
    let mut listing = String::new();
    let mut problems = Vec::new();
    let mut fields = Vec::new();

    let mut g = GlobalState::default();
    let mut stack: Vec<GlobalState> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let mut usage_min: Option<u32> = None;
    let mut usage_max: Option<u32> = None;
    let mut cursors = Cursors::default();
    let mut depth: usize = 0;

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        let start = i;

        // Long items (prefix 0xFE) exist in the spec and in no gamepad we have ever seen; carry
        // them through so an unexpected one is reported rather than silently desynchronising the
        // rest of the parse.
        if prefix == 0xFE {
            if i + 2 >= desc.len() {
                problems.push(format!("truncated long item at byte {start}"));
                break;
            }
            let data_size = desc[i + 1] as usize;
            let tag = desc[i + 2];
            let end = i + 3 + data_size;
            if end > desc.len() {
                problems.push(format!("long item at byte {start} runs past the end"));
                break;
            }
            let _ = writeln!(
                listing,
                "{:pad$}0xFE, /* long item, tag 0x{tag:02X}, {data_size} bytes */",
                "",
                pad = depth * 2
            );
            i = end;
            continue;
        }

        let size_code = (prefix & 0x03) as usize;
        let data_size = if size_code == 3 { 4 } else { size_code };
        let ty = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;
        if i + 1 + data_size > desc.len() {
            problems.push(format!(
                "truncated item at byte {start}: prefix 0x{prefix:02X} wants {data_size} data bytes, \
                 {} remain",
                desc.len() - i - 1
            ));
            break;
        }
        let mut raw: u32 = 0;
        for b in 0..data_size {
            raw |= (desc[i + 1 + b] as u32) << (8 * b);
        }
        let signed = sign_extend(raw, data_size);
        i += 1 + data_size;

        let bytes_hex = desc[start..i]
            .iter()
            .map(|b| format!("0x{b:02X},"))
            .collect::<Vec<_>>()
            .join(" ");

        // Indentation mirrors the annotated arrays in the driver: collections indent their body.
        let mut emit = |depth: usize, text: String| {
            let _ = writeln!(
                listing,
                "{:<38} // {:pad$}{text}",
                bytes_hex,
                "",
                pad = depth * 2
            );
        };

        match ty {
            // ---- Main ----
            0 => match tag {
                0x08 | 0x09 | 0x0B => {
                    let kind = match tag {
                        0x08 => MainKind::Input,
                        0x09 => MainKind::Output,
                        _ => MainKind::Feature,
                    };
                    let bits = g.report_size * g.report_count;
                    let bit_offset = cursors.take(kind, g.report_id, bits);
                    let f = Field {
                        kind,
                        report_id: g.report_id,
                        bit_offset,
                        bit_size: g.report_size,
                        count: g.report_count,
                        usage_page: g.usage_page,
                        usages: usages.clone(),
                        usage_range: match (usage_min, usage_max) {
                            (Some(a), Some(b)) => Some((a, b)),
                            _ => None,
                        },
                        logical_min: g.logical_min,
                        logical_max: g.logical_max,
                        flags: raw,
                    };
                    emit(depth, format!("{} ({})", kind.as_str(), f.flags_str()));
                    fields.push(f);
                    usages.clear();
                    usage_min = None;
                    usage_max = None;
                }
                0x0A => {
                    emit(depth, format!("Collection ({})", collection_name(raw)));
                    depth += 1;
                    usages.clear();
                    usage_min = None;
                    usage_max = None;
                }
                0x0C => {
                    depth = depth.saturating_sub(1);
                    emit(depth, "End Collection".to_string());
                    usages.clear();
                    usage_min = None;
                    usage_max = None;
                }
                _ => {
                    problems.push(format!("unknown Main tag 0x{tag:X} at byte {start}"));
                    emit(depth, format!("<unknown Main tag 0x{tag:X}>"));
                }
            },
            // ---- Global ----
            1 => match tag {
                0x0 => {
                    g.usage_page = raw as u16;
                    let n = usage_page_name(g.usage_page);
                    emit(
                        depth,
                        if n.is_empty() {
                            format!("Usage Page (0x{:04X})", g.usage_page)
                        } else {
                            format!("Usage Page ({n})")
                        },
                    );
                }
                0x1 => {
                    g.logical_min = signed;
                    emit(depth, format!("Logical Minimum ({signed})"));
                }
                0x2 => {
                    g.logical_max = signed;
                    emit(
                        depth,
                        // A maximum is only signed when the minimum was; showing both readings
                        // keeps a `0x25 0xFF` (255 or -1) from being silently misread.
                        if g.logical_min < 0 || signed >= 0 {
                            format!("Logical Maximum ({signed})")
                        } else {
                            format!("Logical Maximum ({signed} — unsigned reading: {raw})")
                        },
                    );
                }
                0x3 => {
                    g.physical_min = signed;
                    emit(depth, format!("Physical Minimum ({signed})"));
                }
                0x4 => {
                    g.physical_max = signed;
                    emit(depth, format!("Physical Maximum ({signed})"));
                }
                0x5 => {
                    g.unit_exp = raw;
                    emit(depth, format!("Unit Exponent (0x{raw:X})"));
                }
                0x6 => {
                    g.unit = raw;
                    emit(
                        depth,
                        match raw {
                            0x14 => "Unit (Eng Rot: Degrees)".to_string(),
                            0x00 => "Unit (None)".to_string(),
                            _ => format!("Unit (0x{raw:X})"),
                        },
                    );
                }
                0x7 => {
                    g.report_size = raw;
                    emit(depth, format!("Report Size ({raw})"));
                }
                0x8 => {
                    g.report_id = raw as u8;
                    emit(depth, format!("Report ID ({raw})"));
                }
                0x9 => {
                    g.report_count = raw;
                    emit(depth, format!("Report Count ({raw})"));
                }
                0xA => {
                    stack.push(g.clone());
                    emit(depth, "Push".to_string());
                }
                0xB => {
                    match stack.pop() {
                        Some(prev) => g = prev,
                        None => problems.push(format!("Pop with an empty stack at byte {start}")),
                    }
                    emit(depth, "Pop".to_string());
                }
                _ => {
                    problems.push(format!("unknown Global tag 0x{tag:X} at byte {start}"));
                    emit(depth, format!("<unknown Global tag 0x{tag:X}>"));
                }
            },
            // ---- Local ----
            2 => match tag {
                0x0 => {
                    // A 4-byte Usage carries its page in the high half.
                    let (page, u) = if data_size == 4 {
                        ((raw >> 16) as u16, raw & 0xFFFF)
                    } else {
                        (g.usage_page, raw)
                    };
                    usages.push(u);
                    let n = usage_name(page, u);
                    emit(
                        depth,
                        if n.is_empty() {
                            format!("Usage (0x{u:02X})")
                        } else {
                            format!("Usage ({n})")
                        },
                    );
                }
                0x1 => {
                    usage_min = Some(raw);
                    emit(depth, format!("Usage Minimum ({raw})"));
                }
                0x2 => {
                    usage_max = Some(raw);
                    emit(depth, format!("Usage Maximum ({raw})"));
                }
                0x3 => emit(depth, format!("Designator Index ({raw})")),
                0x4 => emit(depth, format!("Designator Minimum ({raw})")),
                0x5 => emit(depth, format!("Designator Maximum ({raw})")),
                0x7 => emit(depth, format!("String Index ({raw})")),
                0x8 => emit(depth, format!("String Minimum ({raw})")),
                0x9 => emit(depth, format!("String Maximum ({raw})")),
                0xA => emit(depth, format!("Delimiter ({raw})")),
                _ => {
                    problems.push(format!("unknown Local tag 0x{tag:X} at byte {start}"));
                    emit(depth, format!("<unknown Local tag 0x{tag:X}>"));
                }
            },
            _ => {
                problems.push(format!("reserved item type at byte {start}"));
                emit(depth, "<reserved item type>".to_string());
            }
        }
    }

    if depth != 0 {
        problems.push(format!("{depth} collection(s) never closed"));
    }
    if !stack.is_empty() {
        problems.push(format!("{} Push(es) never popped", stack.len()));
    }

    Decoded {
        listing,
        fields,
        problems,
    }
}

/// The bit-offset table. This is what a layout diff should be read off — item order, not item
/// presence, is what silently lands a control on the wrong byte.
pub fn layout_map(fields: &[Field]) -> String {
    let mut out = String::new();
    for kind in [MainKind::Input, MainKind::Output, MainKind::Feature] {
        let mut ids: Vec<u8> = fields
            .iter()
            .filter(|f| f.kind == kind)
            .map(|f| f.report_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let of_report: Vec<&Field> = fields
                .iter()
                .filter(|f| f.kind == kind && f.report_id == id)
                .collect();
            let bits: u32 = of_report.iter().map(|f| f.bit_size * f.count).sum();
            // The id byte is on the wire whenever the descriptor numbers its reports at all.
            let wire = if id == 0 {
                bits.div_ceil(8) as usize
            } else {
                bits.div_ceil(8) as usize + 1
            };
            let _ = writeln!(
                out,
                "\n  {} report 0x{id:02X} — {bits} bits, {wire} bytes on the wire{}",
                kind.as_str(),
                if id == 0 {
                    " (unnumbered)"
                } else {
                    " (id included)"
                }
            );
            let _ = writeln!(
                out,
                "    {:<12} {:<9} {:<26} {:<20} flags",
                "byte.bit", "size×cnt", "usage", "logical range"
            );
            for f in of_report {
                let id_shift = if id == 0 { 0 } else { 8 };
                let abs = f.bit_offset + id_shift;
                let usage = if let Some((a, b)) = f.usage_range {
                    format!("{} {a}..{b}", usage_page_name(f.usage_page))
                } else if f.usages.is_empty() {
                    if f.is_constant() {
                        "— (padding)".to_string()
                    } else {
                        "— (none declared)".to_string()
                    }
                } else {
                    f.usages
                        .iter()
                        .map(|u| {
                            let n = usage_name(f.usage_page, *u);
                            if n.is_empty() {
                                format!("0x{u:02X}")
                            } else {
                                n.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(
                    out,
                    "    {:<12} {:<9} {:<26} {:<20} {}",
                    format!("{}.{}", abs / 8, abs % 8),
                    format!("{}×{}", f.bit_size, f.count),
                    usage,
                    format!("{}..{}", f.logical_min, f.logical_max),
                    f.flags_str()
                );
            }
        }
    }
    out
}

/// Emit the blob as a `static` ready to paste into the driver.
pub fn rust_array(name: &str, desc: &[u8]) -> String {
    let mut out = format!(
        "#[rustfmt::skip]\nstatic {name}: [u8; {}] = [\n",
        desc.len()
    );
    for chunk in desc.chunks(16) {
        out.push_str("    ");
        for b in chunk {
            let _ = write!(out, "0x{b:02X}, ");
        }
        out.push('\n');
    }
    out.push_str("];\n");
    out
}
