//! Every message id the host can send has its English in `l10n/messages/host/en.json`.
//!
//! The ids are read off the live source of the host, the seat supervisor, the virtual display
//! crate and the update helper: each literal with a message prefix, the code each coding call
//! takes, and the codes in `("code", "Sentence")` pairs under `mgmt/`. A message no site sends
//! is printed, not failed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SOURCES: &[&str] = &[
    "crates/host/punktfunk-host/src",
    "crates/host/pf-seats/src",
    "crates/host/pf-vdisplay/src",
    "crates/install/pf-update/src",
];

/// Where a `("code", "Sentence")` pair is an `apierr_` code.
const PAIRS_IN: &[&str] = &[
    "crates/host/punktfunk-host/src/mgmt/",
    "crates/host/punktfunk-host/src/hooks.rs",
    "crates/host/punktfunk-host/src/hostsys/",
];

/// Prefixes of a whole message id.
const PREFIXES: &[&str] = &[
    "apierr_",
    "close_",
    "diag_",
    "hostupdate_",
    "launch_",
    "reject_",
    "seatcheck_",
    "seatline_",
];

/// Literals with a message prefix that are not message ids.
const NOT_IDS: &[&str] = &["launch_workspace"];

/// A code the host sends with no message: the web console matches it itself.
const NO_MESSAGE: &[&str] = &["update_session_active"];

/// Codes the supervisor sends through a variable, so no call names them. Each must still be a
/// literal in the source.
const INDIRECT: &[&str] = &[
    "account_locked",
    "account_locked_windows",
    "credential_invalid_control_char",
    "credential_invalid_no_password",
    "credential_invalid_too_long",
    "credential_must_change",
    "credential_must_change_windows",
    "credential_must_change_windows_made",
    "credential_wrong",
    "credential_wrong_windows",
    "unlock_locked_out_device_minutes",
    "unlock_locked_out_device_seconds",
    "unlock_locked_out_password_minutes",
    "unlock_locked_out_password_seconds",
];

/// Calls whose first argument is an `apierr_` code.
const CODE_CALLS: &[&str] = &[
    "ApiError::coded(",
    ".said(",
    "Availability::refused(",
    "conflict(",
];

/// Calls whose second argument, after the status, is an `apierr_` code.
const STATUS_CALLS: &[&str] = &["api_error_coded(", "api_error_logged("];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// Each message by id: its English, or the variants of a plural.
fn messages() -> BTreeMap<String, serde_json::Value> {
    let path = repo().join("l10n/messages/host/en.json");
    let text = std::fs::read_to_string(&path).expect("read the host's English messages");
    serde_json::from_str(&text).expect("parse the host's English messages")
}

fn snake(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Each string literal outside comments: its byte span and its text.
fn literals(src: &str) -> Vec<(usize, usize, &str)> {
    let b = src.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        let ident_before = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if b[i..].starts_with(b"//") {
            i = b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |e| i + e);
        } else if b[i..].starts_with(b"/*") {
            i = src[i + 2..].find("*/").map_or(b.len(), |e| i + 4 + e);
        } else if b[i] == b'\'' {
            // A char literal ('x', '\'', '"'); anything else is a lifetime.
            let c = src[i + 1..].chars().next().unwrap_or(' ');
            i += if c == '\\' {
                src[i + 3..].find('\'').map_or(b.len() - i, |e| e + 4)
            } else if src[i + 1 + c.len_utf8()..].starts_with('\'') {
                2 + c.len_utf8()
            } else {
                1
            };
        } else if b[i] == b'r'
            && !ident_before
            && src[i + 1..].trim_start_matches('#').starts_with('"')
        {
            let hashes = src[i + 1..].len() - src[i + 1..].trim_start_matches('#').len();
            let start = i + 2 + hashes;
            let close = format!("\"{}", "#".repeat(hashes));
            let end = src[start..].find(&close).map_or(b.len(), |e| start + e);
            out.push((i, (end + close.len()).min(b.len()), &src[start..end]));
            i = end + close.len();
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(b.len());
            out.push((i, (end + 1).min(b.len()), &src[i + 1..end]));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// `src` without its `//` comments and the items under `#[cfg(test)]`, offsets kept.
fn live(src: &str) -> String {
    let mut code = src.as_bytes().to_vec();
    for (start, end, _) in literals(src) {
        code[start..end].fill(b' ');
    }
    let mut cuts = Vec::new();
    let mut at = 0;
    while let Some(found) = code[at..].windows(2).position(|w| w == b"//") {
        let start = at + found;
        let end = code[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(code.len(), |e| start + e);
        code[start..end].fill(b' ');
        cuts.push(start..end);
        at = end;
    }
    let code = String::from_utf8(code).expect("blanking keeps UTF-8");
    at = 0;
    while let Some(found) = code[at..].find("#[cfg(test)]") {
        let start = at + found;
        let (brace, semi) = (code[start..].find('{'), code[start..].find(';'));
        let end = match (brace, semi) {
            (_, Some(s)) if brace.is_none_or(|b| s < b) => start + s + 1,
            (Some(b), _) => {
                let mut depth = 0;
                let close = code[start + b..].bytes().position(|c| {
                    depth += i32::from(c == b'{') - i32::from(c == b'}');
                    depth == 0
                });
                close.map_or(code.len(), |c| start + b + c + 1)
            }
            _ => code.len(),
        };
        cuts.push(start..end);
        at = end;
    }
    let mut out = src.as_bytes().to_vec();
    for cut in cuts {
        for c in &mut out[cut] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8")
}

/// What a source file sends: whole ids, `apierr_` codes, worker ids and `hostupdate_` codes.
#[derive(Default)]
struct Sent {
    ids: BTreeMap<String, String>,
    codes: BTreeMap<String, String>,
    literals: BTreeSet<String>,
}

fn scan(rel: &str, src: &str, sent: &mut Sent) {
    let src = live(src);
    let lits = literals(&src);
    // The literal that starts at `from`, past whitespace only.
    let literal_at = |from: usize| {
        lits.iter()
            .find(|(start, ..)| *start >= from)
            .filter(|(start, ..)| src[from..*start].trim().is_empty())
            .map(|(.., text)| *text)
            .filter(|text| snake(text))
    };
    let mut code = |key: String| {
        sent.codes.entry(key).or_insert_with(|| rel.to_string());
    };
    let pairs = PAIRS_IN.iter().any(|dir| rel.starts_with(dir));
    for (k, (start, end, text)) in lits.iter().enumerate() {
        if !snake(text) {
            continue;
        }
        sent.literals.insert(text.to_string());
        if PREFIXES.iter().any(|p| text.starts_with(p)) {
            if !NOT_IDS.contains(text) {
                sent.ids
                    .entry(text.to_string())
                    .or_insert_with(|| rel.to_string());
            }
            continue;
        }
        let opens = src[..*start].trim_end().ends_with(['(', ',']);
        let Some((next, _, sentence)) = lits.get(k + 1) else {
            continue;
        };
        let gap: String = src[*end..*next].split_whitespace().collect();
        if pairs
            && opens
            && (gap == "," || gap == ",format!(")
            && sentence.starts_with(char::is_uppercase)
        {
            code(format!("apierr_{text}"));
        }
    }
    let calls = |name: &str| {
        src.match_indices(name)
            .map(|(at, _)| at + name.len())
            .collect::<Vec<_>>()
    };
    for name in CODE_CALLS {
        for at in calls(name) {
            if let Some(text) = literal_at(at) {
                code(format!("apierr_{text}"));
            }
        }
    }
    for name in STATUS_CALLS {
        for at in calls(name) {
            let comma = src[at..].find(',').map(|c| at + c + 1);
            if let Some(text) = comma.and_then(literal_at) {
                code(format!("apierr_{text}"));
            }
        }
    }
    for name in ["blocking(", "worker_stopped("] {
        for at in calls(name) {
            if let Some(text) = literal_at(at) {
                code(format!("apierr_worker_{text}_stopped"));
            }
        }
    }
    for at in calls("code: Some(") {
        if let Some(text) = literal_at(at) {
            code(format!("hostupdate_{text}"));
        }
    }
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir)
        .expect("list a source directory")
        .flatten()
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() && name != "tests" {
            walk(&path, files);
        } else if name.ends_with(".rs") && !name.ends_with("tests.rs") && name != "message_ids.rs" {
            files.push(path);
        }
    }
}

#[test]
fn every_id_the_host_sends_has_a_message() {
    let en = messages();
    let root = repo();
    let mut sent = Sent::default();
    for dir in SOURCES {
        let mut files = Vec::new();
        walk(&root.join(dir), &mut files);
        for path in files {
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            scan(&rel, &std::fs::read_to_string(&path).unwrap(), &mut sent);
        }
    }
    for code in INDIRECT {
        assert!(
            sent.literals.contains(*code),
            "{code} is in no source any more"
        );
        sent.codes
            .insert(format!("apierr_{code}"), "INDIRECT".into());
    }
    for reason in punktfunk_core::reject::RejectReason::ALL {
        sent.ids.insert(reason.message_id(), "reject.rs".into());
    }
    let missing: Vec<_> = sent
        .ids
        .iter()
        .chain(&sent.codes)
        .filter(|(key, _)| !en.contains_key(*key))
        .filter(|(key, _)| !NO_MESSAGE.iter().any(|c| key.ends_with(c)))
        .map(|(key, file)| format!("{key} ({file})"))
        .collect();
    assert!(missing.is_empty(), "sent with no message: {missing:#?}");
    assert!(
        sent.ids.len() > 200 && sent.codes.len() > 150,
        "the scan found the sites"
    );
    let unsent: Vec<_> = en
        .keys()
        .filter(|key| !sent.ids.contains_key(*key) && !sent.codes.contains_key(*key))
        .collect();
    eprintln!("messages no site sends: {unsent:?}");
}

/// Each `{name}` a message declares is among the params the host sends with it.
#[test]
fn a_worded_launch_fills_every_placeholder() {
    use crate::library::downloads::Waited;
    let en = messages();
    for waited in [
        Waited::Failed(Some("the server is down".into())),
        Waited::Failed(None),
        Waited::Cancelled,
        Waited::Paused,
        Waited::Stalled,
    ] {
        let outcome = waited.outcome("Quail").unwrap();
        let text = en[&outcome.message_id].as_str().expect("a plain message");
        for (name, _) in &outcome.message_params {
            assert!(text.contains(&format!("{{{name}}}")), "{name} in {text}");
        }
        let declared = text.matches('{').count();
        assert_eq!(declared, outcome.message_params.len(), "{text}");
    }
}

/// The scan reads past what would fool a naive one: a quote in a char literal, a raw string,
/// a comment, and a test module.
#[test]
fn the_scan_sees_only_live_literals() {
    let src = r##"
fn a() { let q = '"'; api_error_coded(StatusCode::OK, "seen_code", "Text"); }
// api_error_coded(StatusCode::OK, "commented", "Text");
fn b() -> &'static str { r#"raw "quoted""# }
#[cfg(test)]
mod tests { fn c() { ApiError::coded("test_only", "Text"); } }
fn d() { ApiError::coded("after_tests", "Text"); }
"##;
    let mut sent = Sent::default();
    scan("x.rs", src, &mut sent);
    let codes: Vec<_> = sent.codes.keys().map(String::as_str).collect();
    assert_eq!(codes, ["apierr_after_tests", "apierr_seen_code"]);
}
