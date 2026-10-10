//! Corpus replay: a captured host stream through the H.264 / H.265 planners.
//!
//! The capture hook (`PUNKTFUNK_DUMP_VIDEO=<dir>`) writes decoder input as
//! `au-<stamp>.<codec>` plus an `.idx` sidecar (`offset len flags complete`
//! per AU). This harness feeds those AUs through
//! [`pf_bitstream::h264::H264Planner`] / [`pf_bitstream::h265::H265Planner`]
//! and asserts every complete AU plans (bar spec skips) with no warnings a
//! clean capture must not produce. Vendored vectors prove spec streams;
//! this proves what our hosts emit.
//!
//! Ignored by default: captures are large and live outside the repo.
//!
//! ```text
//! PF_CORPUS=/path/to/au-1785970273.h265 \
//!   cargo test -p pf-bitstream --test corpus_replay -- --ignored --nocapture
//! ```
//!
//! Sidecar is `<data>.idx`; codec is the file extension.

use std::path::Path;
use std::path::PathBuf;

struct CapturedAu {
    offset: usize,
    len: usize,
    /// Wire `USER_FLAG_*`; the annex-B bytes do not carry it.
    _flags: u32,
    complete: bool,
}

/// `.idx` sidecar: one `offset len flags complete` line per AU; `#` and blanks skipped.
///
/// A malformed final line is dropped: killing the client routinely half-writes the
/// last buffered line. Anywhere else, a malformed line is a corrupt sidecar — do
/// not replay a subset.
fn read_index(path: &Path) -> Vec<CapturedAu> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read the index sidecar {}: {e}", path.display()));
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect();
    let last = lines.len().saturating_sub(1);
    let mut out = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        match parse_index_line(line) {
            Some(au) => out.push(au),
            None if n == last => {
                println!("note: dropping a truncated final index line ({line:?})");
            }
            None => panic!("index line {n} is malformed: {line:?}"),
        }
    }
    out
}

fn parse_index_line(line: &str) -> Option<CapturedAu> {
    let mut it = line.split_whitespace();
    let num = |raw: &str| -> Option<u64> {
        match raw.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => raw.parse().ok(),
        }
    };
    let offset = num(it.next()?)?;
    let len = num(it.next()?)?;
    let flags = num(it.next()?)?;
    let complete = num(it.next()?)?;
    Some(CapturedAu {
        offset: offset as usize,
        len: len as usize,
        _flags: flags as u32,
        complete: complete != 0,
    })
}

fn corpus_from_env() -> Option<(PathBuf, Vec<u8>, Vec<CapturedAu>)> {
    let path = PathBuf::from(std::env::var_os("PF_CORPUS")?);
    let data = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("cannot read the capture {}: {e}", path.display()));
    let mut idx = path.clone().into_os_string();
    idx.push(".idx");
    let mut index = read_index(Path::new(&idx));
    // Truncated capture: drop AUs past EOF, but only from the tail — a middle gap
    // must not disappear silently.
    let covered = index
        .iter()
        .take_while(|au| au.offset.saturating_add(au.len) <= data.len())
        .count();
    if covered < index.len() {
        println!(
            "note: dropping {} index entr{} past the end of the data file (truncated capture)",
            index.len() - covered,
            if index.len() - covered == 1 {
                "y"
            } else {
                "ies"
            },
        );
        index.truncate(covered);
    }
    assert!(!index.is_empty(), "the capture's index is empty");
    Some((path, data, index))
}

#[derive(Default)]
struct Tally {
    planned: usize,
    skipped: usize,
    errors: Vec<String>,
    warnings: Vec<String>,
    partial: usize,
}

impl Tally {
    /// Clean capture: every complete AU plans. A `MissingReference` warning means
    /// the planner invented a gap on a stream that never lost a packet.
    fn assert_clean(&self, total: usize) {
        println!(
            "planned {} / skipped {} / partial-AUs-ignored {} / errors {} / warnings {} \
             (of {total} captured AUs)",
            self.planned,
            self.skipped,
            self.partial,
            self.errors.len(),
            self.warnings.len(),
        );
        for w in self.warnings.iter().take(20) {
            println!("  warning: {w}");
        }
        for e in self.errors.iter().take(20) {
            println!("  ERROR: {e}");
        }
        assert!(
            self.errors.is_empty(),
            "{} AUs failed to plan — first: {}",
            self.errors.len(),
            self.errors[0],
        );
        assert!(
            self.warnings.is_empty(),
            "{} planner warnings on a clean capture — first: {}",
            self.warnings.len(),
            self.warnings[0],
        );
        assert!(self.planned > 0, "no AU planned at all");
    }
}

#[test]
#[ignore = "needs a capture: PF_CORPUS=<au-file> (see the module docs)"]
fn a_captured_host_stream_replays_through_the_planner() {
    let Some((path, data, index)) = corpus_from_env() else {
        panic!("PF_CORPUS is unset — see the module docs for the invocation");
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_owned();
    println!(
        "replaying {} ({} bytes, {} AUs, codec {ext})",
        path.display(),
        data.len(),
        index.len(),
    );

    let mut tally = Tally::default();
    // Planners take one complete AU. A partial (wire shard) is the pump's; skip it.
    let complete: Vec<&CapturedAu> = index.iter().filter(|au| au.complete).collect();
    tally.partial = index.len() - complete.len();

    match ext.as_str() {
        "h265" => {
            let mut planner = pf_bitstream::h265::H265Planner::new();
            for (i, au) in complete.iter().enumerate() {
                let bytes = &data[au.offset..au.offset + au.len];
                match planner.plan_au(bytes) {
                    Ok(plan) => {
                        tally.planned += 1;
                        for w in &plan.warnings {
                            tally.warnings.push(format!("AU {i}: {w:?}"));
                        }
                    }
                    // Spec 8.1.3 RASL skip: decode nothing, show nothing; the stream is healthy.
                    Err(pf_bitstream::h265::PlanError::RaslSkipped { .. }) => tally.skipped += 1,
                    Err(e) => tally.errors.push(format!("AU {i}: {e}")),
                }
            }
        }
        "h264" => {
            let mut planner = pf_bitstream::h264::H264Planner::new();
            for (i, au) in complete.iter().enumerate() {
                let bytes = &data[au.offset..au.offset + au.len];
                match planner.plan_au(bytes) {
                    Ok(plan) => {
                        tally.planned += 1;
                        for w in &plan.warnings {
                            tally.warnings.push(format!("AU {i}: {w:?}"));
                        }
                    }
                    Err(e) => tally.errors.push(format!("AU {i}: {e}")),
                }
            }
        }
        other => panic!("no planner for a .{other} capture (h264/h265 only today)"),
    }

    tally.assert_clean(index.len());
}
