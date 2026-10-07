//! The `punktfunk://` URL grammar: one parser/emitter for every native client
//! (`design/client-deep-links.md`). Swift (`PunktfunkShared/DeepLink.swift`) and
//! Kotlin keep their own ports; all three consume
//! `clients/shared/deeplink-vectors.json`.
//!
//! ```text
//! punktfunk://connect/<host-ref>[?fp=<64-hex>][&host=<addr[:port]>][&launch=<id>]
//!                               [&preset=<ref>][&as=<profile>][&name=<label>]
//! ```
//!
//! A URL may only do what a click on an existing card could do, minus trust
//! decisions: references (host record, settings preset, library id), never
//! values (resolution, bitrate, codec). `pair` is not a route; pairing stays
//! interactive. `pf://` parses as an alias so a typed or legacy link still
//! works, but nothing emits or registers it — claiming a two-letter scheme on
//! MSIX/Apple is squatting, and a link that resolves on one platform only is a trap.

use crate::trust::{KnownHost, KnownHosts};

/// Hostile-input caps. Generous for a real link; a pasted megabyte never reaches the decoder.
pub const MAX_URL_LEN: usize = 2048;
pub const MAX_HOST_REF_LEN: usize = 128;
pub const MAX_LAUNCH_LEN: usize = 128;
pub const MAX_PRESET_LEN: usize = 64;
pub const MAX_NAME_LEN: usize = 64;
/// A profile id or name; ids are at most 64 bytes on the wire.
pub const MAX_PROFILE_LEN: usize = 64;

/// Native control port; same default as every other client.
pub const DEFAULT_PORT: u16 = 9777;

/// Reserved routes parse so an unimplemented front-end can refuse instead of silently connecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Connect,
    Wake,
    Browse,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Connect => "connect",
            Route::Wake => "wake",
            Route::Browse => "browse",
        }
    }
}

/// Parsed and length/charset-checked. The consumer still has to resolve: references may not exist here.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DeepLink {
    pub route: Route,
    /// Host record id, display name, or `addr[:port]`.
    pub host_ref: String,
    /// Expected host cert pin; lowercase hex, 64 chars.
    pub fp: Option<String>,
    /// Dial this when the stable id no longer resolves (wiped store).
    pub host: Option<(String, u16)>,
    /// Store-qualified library id (`steam:570`).
    pub launch: Option<String>,
    /// Preset id or unique name; one-off, never rebinding.
    pub preset: Option<String>,
    /// Profile id or name to play as (`as=`): skips the picker for this connect and leaves
    /// the saved pick alone. Not `profile=`, which is the old spelling of `preset=`.
    pub as_profile: Option<String>,
    /// Label for the unknown-host confirmation sheet (external emitters).
    pub name: Option<String>,
}

/// Rejection codes shared with the Swift/Kotlin ports via `clients/shared/deeplink-vectors.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Not `punktfunk://` / `pf://` — ignore, do not warn.
    NotOurScheme,
    TooLong,
    UnknownRoute(String),
    /// Pairing is interactive; never a link.
    PairRefused,
    MissingHostRef,
    /// `%` not followed by two hex digits, or the decode is not UTF-8.
    BadEscape,
    /// A control character survived decoding; no legitimate field contains one.
    ControlChar,
    ParamTooLong(&'static str),
    BadFingerprint,
    BadHostParam,
    /// Outside the printable, shell-safe charset the host and Decky agree on.
    BadLaunchId,
}

impl ParseError {
    pub fn code(&self) -> &'static str {
        match self {
            ParseError::NotOurScheme => "not-our-scheme",
            ParseError::TooLong => "too-long",
            ParseError::UnknownRoute(_) => "unknown-route",
            ParseError::PairRefused => "pair-refused",
            ParseError::MissingHostRef => "missing-host-ref",
            ParseError::BadEscape => "bad-escape",
            ParseError::ControlChar => "control-char",
            ParseError::ParamTooLong(_) => "param-too-long",
            ParseError::BadFingerprint => "bad-fingerprint",
            ParseError::BadHostParam => "bad-host-param",
            ParseError::BadLaunchId => "bad-launch-id",
        }
    }

    /// Names the failing reference so a shortcut never streams with the wrong settings.
    pub fn message(&self) -> String {
        match self {
            ParseError::NotOurScheme => "That isn't a Punktfunk link.".into(),
            ParseError::TooLong => "That link is too long to be genuine.".into(),
            ParseError::UnknownRoute(r) => format!("Punktfunk links can't do \"{r}\"."),
            ParseError::PairRefused => {
                "Pairing can't be done from a link — pair the host in Punktfunk first.".into()
            }
            ParseError::MissingHostRef => "That link doesn't say which host to use.".into(),
            ParseError::BadEscape | ParseError::ControlChar => {
                "That link is malformed and was ignored.".into()
            }
            ParseError::ParamTooLong(p) => format!("That link's \"{p}\" value is too long."),
            ParseError::BadFingerprint => "That link's host fingerprint isn't a valid one.".into(),
            ParseError::BadHostParam => "That link's host address isn't valid.".into(),
            ParseError::BadLaunchId => "That link's game id isn't a valid one.".into(),
        }
    }
}

/// `punktfunk`, or its `pf` input alias, in any case.
fn our_scheme(scheme: &str) -> bool {
    scheme.eq_ignore_ascii_case("punktfunk") || scheme.eq_ignore_ascii_case("pf")
}

/// Whether a command-line argument is a link addressed to us. [`parse`] validates it.
pub fn is_link_arg(arg: &str) -> bool {
    arg.split_once("://")
        .is_some_and(|(scheme, _)| our_scheme(scheme))
}

/// Hostile input is rejected here once for every front-end; `pf://` is an alias.
pub fn parse(url: &str) -> Result<DeepLink, ParseError> {
    if url.len() > MAX_URL_LEN {
        return Err(ParseError::TooLong);
    }
    let (scheme, rest) = url.split_once("://").ok_or(ParseError::NotOurScheme)?;
    if !our_scheme(scheme) {
        return Err(ParseError::NotOurScheme);
    }
    // Fragments are not in the grammar; drop `#…` so it cannot smuggle text past the caps.
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, q),
        None => (rest, ""),
    };

    let path = path.trim_end_matches('/');
    let (route_word, host_ref_raw) = match path.split_once('/') {
        Some((r, h)) => (r, h),
        // Bare path is a host-ref unless it is a route word: `punktfunk://pair` must refuse,
        // not hunt for a host named "pair".
        None if is_route_word(path) => (path, ""),
        None => ("connect", path),
    };
    let route = match route_word.to_ascii_lowercase().as_str() {
        "connect" => Route::Connect,
        "wake" => Route::Wake,
        "browse" => Route::Browse,
        "pair" => return Err(ParseError::PairRefused),
        other => return Err(ParseError::UnknownRoute(other.to_string())),
    };

    let host_ref = decode(host_ref_raw)?;
    if host_ref.is_empty() {
        return Err(ParseError::MissingHostRef);
    }
    if host_ref.chars().count() > MAX_HOST_REF_LEN {
        return Err(ParseError::ParamTooLong("host-ref"));
    }

    let mut link = DeepLink {
        route,
        host_ref,
        ..Default::default()
    };
    let mut legacy_preset: Option<String> = None;
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(key)?.to_ascii_lowercase();
        let value = decode(value)?;
        if value.is_empty() {
            continue; // Empty `?launch=` is absent, not an error.
        }
        // First occurrence wins; unknown keys are ignored. A second `fp=` must not override,
        // and a newer emitter's extra parameter must not refuse an otherwise valid link.
        match key.as_str() {
            "fp" if link.fp.is_none() => {
                let fp = value.to_ascii_lowercase();
                if fp.len() != 64 || !fp.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(ParseError::BadFingerprint);
                }
                link.fp = Some(fp);
            }
            "host" if link.host.is_none() => {
                link.host = Some(parse_addr_port(&value).ok_or(ParseError::BadHostParam)?);
            }
            "launch" if link.launch.is_none() => {
                if value.len() > MAX_LAUNCH_LEN {
                    return Err(ParseError::ParamTooLong("launch"));
                }
                if !is_safe_launch_id(&value) {
                    return Err(ParseError::BadLaunchId);
                }
                link.launch = Some(value);
            }
            "preset" if link.preset.is_none() => {
                if value.chars().count() > MAX_PRESET_LEN {
                    return Err(ParseError::ParamTooLong("preset"));
                }
                link.preset = Some(value);
            }
            // The pre-rename spelling: `preset` wins wherever it sits.
            "profile" if legacy_preset.is_none() => {
                if value.chars().count() > MAX_PRESET_LEN {
                    return Err(ParseError::ParamTooLong("profile"));
                }
                legacy_preset = Some(value);
            }
            "as" if link.as_profile.is_none() => {
                if value.chars().count() > MAX_PROFILE_LEN {
                    return Err(ParseError::ParamTooLong("as"));
                }
                link.as_profile = Some(value);
            }
            "name" if link.name.is_none() => {
                if value.chars().count() > MAX_NAME_LEN {
                    return Err(ParseError::ParamTooLong("name"));
                }
                link.name = Some(value);
            }
            _ => {}
        }
    }
    link.preset = link.preset.or(legacy_preset);
    Ok(link)
}

impl DeepLink {
    /// Always `punktfunk://`, never the `pf://` alias. Self-emitted links carry the stable
    /// id and `host`+`fp` so a wiped store still resolves.
    pub fn to_url(&self) -> String {
        let mut s = format!(
            "punktfunk://{}/{}",
            self.route.as_str(),
            encode(&self.host_ref)
        );
        let mut sep = '?';
        let mut push = |s: &mut String, key: &str, value: &str| {
            s.push(sep);
            sep = '&';
            s.push_str(key);
            s.push('=');
            s.push_str(&encode(value));
        };
        if let Some(fp) = &self.fp {
            push(&mut s, "fp", fp);
        }
        if let Some((addr, port)) = &self.host {
            let host = if *port == DEFAULT_PORT {
                addr.clone()
            } else if addr.contains(':') {
                format!("[{addr}]:{port}") // IPv6 literals need brackets in `host=`
            } else {
                format!("{addr}:{port}")
            };
            push(&mut s, "host", &host);
        }
        if let Some(launch) = &self.launch {
            push(&mut s, "launch", launch);
        }
        // `profile=` as well, for clients that predate `preset=`.
        if let Some(preset) = &self.preset {
            push(&mut s, "preset", preset);
            push(&mut s, "profile", preset);
        }
        if let Some(profile) = &self.as_profile {
            push(&mut s, "as", profile);
        }
        if let Some(name) = &self.name {
            push(&mut s, "name", name);
        }
        s
    }

    /// Id first (address-independent). Address and pin so a missing record degrades
    /// to a confirmation sheet, not an unresolvable click.
    pub fn for_host(host: &KnownHost, launch: Option<&str>, preset: Option<&str>) -> DeepLink {
        DeepLink {
            route: Route::Connect,
            host_ref: host.id.clone().unwrap_or_else(|| {
                punktfunk_core::discovery::join_host_port(&host.addr, host.port)
            }),
            fp: (!host.fp_hex.is_empty()).then(|| host.fp_hex.clone()),
            host: Some((host.addr.clone(), host.port)),
            launch: launch.map(str::to_string),
            preset: preset.map(str::to_string),
            as_profile: None,
            name: None,
        }
    }

    /// `fp` contradicts the pinned host cert; the only safe answer is a hard refusal.
    pub fn pin_conflict(&self, host: &KnownHost) -> bool {
        match (&self.fp, host.fp_hex.is_empty()) {
            (Some(fp), false) => !fp.eq_ignore_ascii_case(&host.fp_hex),
            _ => false,
        }
    }
}

/// A saved host's link built from the store, which holds the stable id and pin a screen
/// does not; `launch` makes a game's link. The record is the one
/// [`KnownHosts::resolve`] names, and `None` means it has left the store.
pub fn saved_host_link(
    known: &KnownHosts,
    fp_hex: Option<&str>,
    addr: &str,
    port: u16,
    preset: Option<&str>,
    launch: Option<&str>,
) -> Option<String> {
    let host = known.resolve(fp_hex, addr, port)?;
    Some(DeepLink::for_host(host, launch, preset).to_url())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostResolution {
    /// Named by the unguessable record id (one-click, subject to [`DeepLink::pin_conflict`]).
    Known(usize),
    /// Guessable display name, address, or `host=`. Confirm first; then [`HostResolution::Known`].
    Confirm(usize),
    /// No record, but an address to dial. Confirmation sheet, then pairing/TOFU. Never auto-connect.
    Unknown {
        addr: String,
        port: u16,
        name: Option<String>,
        fp: Option<String>,
    },
    /// Name matched more than one saved host; refuse, never guess.
    Ambiguous,
    Unresolvable,
}

/// Resolve in order: stable record id → unique case-insensitive name → `addr[:port]`
/// literal → `host=` recovery.
///
/// Only the record id is unguessable, so only it is [`HostResolution::Known`].
/// `.desktop` files register `x-scheme-handler/punktfunk`, so any web page can
/// hand this a guessable name or LAN address; those are [`HostResolution::Confirm`].
///
/// Returns an index, not a borrow, so callers can rekey / touch-last-used.
pub fn resolve_host(link: &DeepLink, known: &KnownHosts) -> HostResolution {
    if let Some(i) = known
        .hosts
        .iter()
        .position(|h| h.id.as_deref().is_some_and(|id| id == link.host_ref))
    {
        return HostResolution::Known(i);
    }
    let by_name: Vec<usize> = known
        .hosts
        .iter()
        .enumerate()
        .filter(|(_, h)| h.name.eq_ignore_ascii_case(&link.host_ref))
        .map(|(i, _)| i)
        .collect();
    match by_name.len() {
        1 => return HostResolution::Confirm(by_name[0]),
        0 => {}
        _ => return HostResolution::Ambiguous,
    }
    // Literal `addr[:port]`, then `host=`. At that address the link's `fp` picks its record —
    // a dual-boot box answers there with two pins — else only a placeholder stands in. A
    // stale record id must fall through to `host=` (or refusal), never be offered as a box.
    let literal = looks_like_address(&link.host_ref)
        .then(|| parse_addr_port(&link.host_ref))
        .flatten();
    for candidate in [literal.clone(), link.host.clone()].into_iter().flatten() {
        let (addr, port) = (candidate.0.as_str(), candidate.1);
        let found = match link.fp.as_deref() {
            Some(fp) => known
                .hosts
                .iter()
                .position(|h| h.addr == addr && h.port == port && h.fp_hex == fp)
                .or_else(|| known.placeholder_at(addr, port)),
            None => known.index_by_addr(addr, port),
        };
        if let Some(i) = found {
            return HostResolution::Confirm(i);
        }
    }
    match literal.or_else(|| link.host.clone()) {
        Some((addr, port)) => HostResolution::Unknown {
            addr,
            port,
            name: link.name.clone(),
            fp: link.fp.clone(),
        },
        None => HostResolution::Unresolvable,
    }
}

/// A stale record id is not an address: offering to dial a UUID would skip `host=` recovery.
fn looks_like_address(s: &str) -> bool {
    let uuid_shaped = s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    !uuid_shaped
        && !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

/// Grammar routes plus `pair`, reserved so it is refused rather than taken as a host name.
fn is_route_word(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "connect" | "wake" | "browse" | "pair"
    )
}

/// Split `host[:port]` where a MISSING port is meaningful — a dialog falls back to its own
/// port field, a CLI to the saved record — so it is reported rather than defaulted.
/// `Some((addr, None))` = a host and no port. IPv6-aware: see [`parse_addr_port`].
pub fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    let (addr, port) = parse_addr_port(s)?;
    // `parse_addr_port` supplies DEFAULT_PORT when none was written, and a bare `::1` is all
    // address despite its colons — so "was one spelled?" is a suffix test, not a colon test.
    let spelled = s.len() > addr.len() && s.ends_with(&format!(":{port}"));
    Some((addr, spelled.then_some(port)))
}

/// Split `host[:port]`, defaulting to [`DEFAULT_PORT`]. A bare IPv6 (`::1`) keeps its colons;
/// a bracketed one (`[::1]:9777`) gives up its brackets. `None` = not a host reference at all.
///
/// Shared because a hand-rolled `rsplit_once(':')` reads `::1` as host `:` port `1`.
pub fn parse_addr_port(s: &str) -> Option<(String, u16)> {
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        if addr.is_empty() {
            return None;
        }
        return match tail {
            "" => Some((addr.to_string(), DEFAULT_PORT)),
            t => Some((addr.to_string(), t.strip_prefix(':')?.parse().ok()?)),
        };
    }
    match s.rsplit_once(':') {
        // Head still has a colon (`::1`): not a port separator.
        Some((head, _)) if head.contains(':') => Some((s.to_string(), DEFAULT_PORT)),
        Some((addr, port)) if !addr.is_empty() => Some((addr.to_string(), port.parse().ok()?)),
        Some(_) => None,
        None => Some((s.to_string(), DEFAULT_PORT)),
    }
}

/// Printable non-space ASCII without shell metacharacters. Decky puts the id in a
/// Steam launch-option env token; a quote or backtick breaks downstream. Opaque to us.
/// No leading `-`: the id rides session argv, where `--browse` would read as a flag.
fn is_safe_launch_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !br#""'\$`"#.contains(&b))
}

/// `%` plus two hex digits, UTF-8, no surviving control. A lenient decoder lets
/// `%00` or a stray `\n` into a filename or log.
fn decode(s: &str) -> Result<String, ParseError> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).ok_or(ParseError::BadEscape)?;
                let hi = (hex[0] as char).to_digit(16).ok_or(ParseError::BadEscape)?;
                let lo = (hex[1] as char).to_digit(16).ok_or(ParseError::BadEscape)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    let text = String::from_utf8(out).map_err(|_| ParseError::BadEscape)?;
    if text.chars().any(|c| c.is_control()) {
        return Err(ParseError::ControlChar);
    }
    Ok(text)
}

/// Unreserved plus `:`, which is legal in a query and left alone by Apple's `URLComponents`
/// so the three emitters agree on `steam:570`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::KnownHost;

    fn host(name: &str, addr: &str, id: &str, fp: &str) -> KnownHost {
        KnownHost {
            name: name.into(),
            addr: addr.into(),
            port: DEFAULT_PORT,
            fp_hex: fp.into(),
            paired: true,
            id: Some(id.into()),
            ..Default::default()
        }
    }

    /// Cross-language vector file; Swift and Kotlin consume it too.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../clients/shared/deeplink-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let cases = file["cases"].as_array().expect("cases array");
        assert!(
            cases.len() > 20,
            "the vector file is the contract; keep it rich"
        );
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let url = case["url"].as_str().unwrap();
            let got = parse(url);
            match case.get("error").and_then(|e| e.as_str()) {
                Some(code) => {
                    let err = got.expect_err(&format!("{name}: expected {code}, parsed ok"));
                    assert_eq!(err.code(), code, "{name}");
                }
                None => {
                    let link = got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
                    let want = &case["expect"];
                    assert_eq!(
                        link.route.as_str(),
                        want["route"].as_str().unwrap(),
                        "{name}"
                    );
                    assert_eq!(link.host_ref, want["host_ref"].as_str().unwrap(), "{name}");
                    let opt = |k: &str| want.get(k).and_then(|v| v.as_str()).map(str::to_string);
                    assert_eq!(link.fp, opt("fp"), "{name} fp");
                    assert_eq!(link.launch, opt("launch"), "{name} launch");
                    assert_eq!(link.preset, opt("preset"), "{name} preset");
                    assert_eq!(link.name, opt("name"), "{name} name");
                    assert_eq!(link.as_profile, opt("as"), "{name} as");
                    let (addr, port) = match &link.host {
                        Some((a, p)) => (Some(a.clone()), Some(u64::from(*p))),
                        None => (None, None),
                    };
                    assert_eq!(addr, opt("host_addr"), "{name} host_addr");
                    assert_eq!(
                        port,
                        want.get("host_port").and_then(|v| v.as_u64()),
                        "{name} host_port"
                    );
                    if let Some(emit) = case.get("emit").and_then(|v| v.as_str()) {
                        assert_eq!(link.to_url(), emit, "{name} emit");
                    }
                }
            }
        }
    }

    /// Codes, not just `Err`, are the shared vocabulary with the vector file and the ports.
    #[test]
    fn refusals_are_specific() {
        assert_eq!(parse("https://example.com/"), Err(ParseError::NotOurScheme));
        assert_eq!(parse("punktfunk:/connect/x"), Err(ParseError::NotOurScheme));
        assert_eq!(
            parse(&format!("punktfunk://connect/{}", "a".repeat(MAX_URL_LEN))),
            Err(ParseError::TooLong)
        );
        assert_eq!(parse("punktfunk://pair/1234"), Err(ParseError::PairRefused));
        assert_eq!(
            parse("punktfunk://teardown/host"),
            Err(ParseError::UnknownRoute("teardown".into()))
        );
        assert_eq!(
            parse("punktfunk://connect/"),
            Err(ParseError::MissingHostRef)
        );
        assert_eq!(
            parse(&format!(
                "punktfunk://connect/{}",
                "n".repeat(MAX_HOST_REF_LEN + 1)
            )),
            Err(ParseError::ParamTooLong("host-ref"))
        );
    }

    #[test]
    fn host_resolution_order_and_recovery() {
        let fp = "a".repeat(64);
        let known = KnownHosts {
            hosts: vec![
                host(
                    "Desk",
                    "192.168.1.50",
                    "11111111-2222-4333-8444-555555555555",
                    &fp,
                ),
                host(
                    "Couch",
                    "192.168.1.60",
                    "66666666-7777-4888-8999-aaaaaaaaaaaa",
                    "",
                ),
                host(
                    "Couch",
                    "192.168.1.61",
                    "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff",
                    "",
                ),
            ],
        };
        let r = |url: &str| resolve_host(&parse(url).unwrap(), &known);

        assert_eq!(
            r("punktfunk://connect/11111111-2222-4333-8444-555555555555"),
            HostResolution::Known(0)
        );
        assert_eq!(r("punktfunk://connect/desk"), HostResolution::Confirm(0));
        assert_eq!(r("punktfunk://connect/couch"), HostResolution::Ambiguous);
        assert_eq!(
            r("punktfunk://connect/192.168.1.50"),
            HostResolution::Confirm(0)
        );
        assert_eq!(
            r("punktfunk://connect/192.168.1.50:9777"),
            HostResolution::Confirm(0)
        );
        assert_eq!(
            r("punktfunk://connect/00000000-0000-4000-8000-000000000000?host=192.168.1.50"),
            HostResolution::Confirm(0)
        );
        assert_eq!(
            r(&format!(
                "punktfunk://connect/10.0.0.9:7000?name=Studio&fp={fp}"
            )),
            HostResolution::Unknown {
                addr: "10.0.0.9".into(),
                port: 7000,
                name: Some("Studio".into()),
                fp: Some(fp.clone()),
            }
        );
        assert_eq!(
            r("punktfunk://connect/nas.local"),
            HostResolution::Unknown {
                addr: "nas.local".into(),
                port: DEFAULT_PORT,
                name: None,
                fp: None,
            }
        );
        // Stale record id is not a hostname; without `host=` there is nothing to dial.
        assert_eq!(
            r("punktfunk://connect/00000000-0000-4000-8000-000000000000"),
            HostResolution::Unresolvable
        );
        assert_eq!(
            r("punktfunk://connect/Basement%20PC"),
            HostResolution::Unresolvable
        );

        let link = parse(&format!("punktfunk://connect/desk?fp={}", "b".repeat(64))).unwrap();
        assert!(link.pin_conflict(&known.hosts[0]));
        assert!(!parse(&format!("punktfunk://connect/desk?fp={fp}"))
            .unwrap()
            .pin_conflict(&known.hosts[0]));
        // Empty stored pin: nothing to contradict.
        assert!(!link.pin_conflict(&known.hosts[1]));
    }

    #[test]
    fn only_the_record_id_dials_without_asking() {
        let fp = "a".repeat(64);
        let known = KnownHosts {
            hosts: vec![host(
                "Desk",
                "192.168.1.50",
                "11111111-2222-4333-8444-555555555555",
                &fp,
            )],
        };
        let r = |url: &str| resolve_host(&parse(url).unwrap(), &known);

        assert_eq!(
            r("punktfunk://connect/11111111-2222-4333-8444-555555555555"),
            HostResolution::Known(0)
        );
        for guess in [
            "punktfunk://connect/desk",
            "punktfunk://connect/DESK",
            "punktfunk://connect/192.168.1.50",
            "punktfunk://connect/192.168.1.50:9777",
            // Extra params do not upgrade a guess to one-click.
            "punktfunk://connect/desk?launch=steam:570",
            "punktfunk://connect/00000000-0000-4000-8000-000000000000?host=192.168.1.50",
        ] {
            assert_eq!(r(guess), HostResolution::Confirm(0), "{guess}");
        }
    }

    /// Both OS installs of a dual-boot box at one address: a link's `fp` picks its own
    /// record, and a pin nobody holds is a new host, never the neighbour.
    #[test]
    fn a_links_pin_picks_its_own_os_at_a_shared_address() {
        let (windows, linux, third) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let known = KnownHosts {
            hosts: vec![
                host(
                    "Desk (Windows)",
                    "192.168.1.50",
                    "11111111-2222-4333-8444-555555555555",
                    &windows,
                ),
                host(
                    "Desk (Linux)",
                    "192.168.1.50",
                    "66666666-7777-4888-8999-aaaaaaaaaaaa",
                    &linux,
                ),
            ],
        };
        let r = |url: &str| resolve_host(&parse(url).unwrap(), &known);

        let at = |fp: &str| format!("punktfunk://connect/192.168.1.50?fp={fp}");
        assert_eq!(r(&at(&windows)), HostResolution::Confirm(0));
        assert_eq!(r(&at(&linux)), HostResolution::Confirm(1));
        assert_eq!(
            r(&at(&third)),
            HostResolution::Unknown {
                addr: "192.168.1.50".into(),
                port: DEFAULT_PORT,
                name: None,
                fp: Some(third.clone()),
            }
        );
        // A stale id recovering through `host=` lands on the pin's own record too.
        assert_eq!(
            r(&format!(
                "punktfunk://connect/00000000-0000-4000-8000-000000000000\
                 ?host=192.168.1.50&fp={linux}"
            )),
            HostResolution::Confirm(1)
        );
    }

    #[test]
    fn self_emitted_links_round_trip() {
        let fp = "c".repeat(64);
        let mut h = host(
            "Desk",
            "192.168.1.50",
            "11111111-2222-4333-8444-555555555555",
            &fp,
        );
        h.port = 7777;
        let link = DeepLink::for_host(&h, Some("steam:570"), Some("aaaaaaaaaaaa"));
        let url = link.to_url();
        assert_eq!(
            url,
            "punktfunk://connect/11111111-2222-4333-8444-555555555555\
             ?fp=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\
             &host=192.168.1.50:7777&launch=steam:570&preset=aaaaaaaaaaaa&profile=aaaaaaaaaaaa"
        );
        assert_eq!(parse(&url).unwrap(), link);
        let known = KnownHosts {
            hosts: vec![h.clone()],
        };
        assert_eq!(
            saved_host_link(
                &known,
                Some(&fp),
                "10.0.0.9",
                1,
                Some("aaaaaaaaaaaa"),
                Some("steam:570")
            ),
            Some(url),
            "the store's record, wherever the screen last saw it"
        );
        assert!(is_link_arg("PF://connect/Desk") && !is_link_arg("--pf://"));

        // Pre-migration record with no id still emits a resolvable `addr:port`.
        let mut plain = h.clone();
        plain.id = None;
        assert_eq!(
            DeepLink::for_host(&plain, None, None).host_ref,
            "192.168.1.50:7777"
        );
        plain.addr = "fd00::5".into();
        let v6 = DeepLink::for_host(&plain, None, None);
        assert_eq!(v6.host_ref, "[fd00::5]:7777");
        assert_eq!(
            parse_addr_port(&v6.host_ref),
            Some(("fd00::5".into(), 7777))
        );

        let link = DeepLink {
            host_ref: "Wohnzimmer PC".into(),
            name: Some("Büro · Mac".into()),
            as_profile: Some("Kid".into()),
            ..Default::default()
        };
        assert!(link
            .to_url()
            .starts_with("punktfunk://connect/Wohnzimmer%20PC?"));
        assert_eq!(parse(&link.to_url()).unwrap(), link);
    }

    #[test]
    fn addr_port_forms() {
        assert_eq!(
            parse_addr_port("192.168.1.5"),
            Some(("192.168.1.5".into(), 9777))
        );
        assert_eq!(
            parse_addr_port("192.168.1.5:1234"),
            Some(("192.168.1.5".into(), 1234))
        );
        assert_eq!(parse_addr_port("::1"), Some(("::1".into(), 9777)));
        assert_eq!(parse_addr_port("[::1]"), Some(("::1".into(), 9777)));
        assert_eq!(parse_addr_port("[::1]:1234"), Some(("::1".into(), 1234)));
        assert_eq!(parse_addr_port("host:notaport"), None);
        assert_eq!(parse_addr_port("[::1]junk"), None);
        assert_eq!(parse_addr_port(""), None);
        // Emitted IPv6 `host=` is bracketed so parse accepts it again.
        let link = DeepLink {
            host_ref: "x".into(),
            host: Some(("::1".into(), 1234)),
            ..Default::default()
        };
        assert_eq!(parse(&link.to_url()).unwrap().host, link.host);
    }
    /// A port is reported only when it was WRITTEN, and an IPv6 literal keeps its colons.
    ///
    /// Seven hand-rolled `rsplit_once(':')` splitters read `::1` as host `:` port `1`; the
    /// three callers that must tell "no port" from "port 9777" apart used to hand-roll that
    /// distinction too.
    #[test]
    fn split_host_port_reports_whether_a_port_was_written() {
        use super::split_host_port as split;
        assert_eq!(
            split("192.168.1.50"),
            Some(("192.168.1.50".into(), None)),
            "a bare host names no port"
        );
        assert_eq!(
            split("192.168.1.50:47990"),
            Some(("192.168.1.50".into(), Some(47990)))
        );
        // A bare IPv6 is all address, colons and all — the case every rsplit got wrong.
        assert_eq!(split("::1"), Some(("::1".into(), None)));
        assert_eq!(split("[::1]"), Some(("::1".into(), None)));
        assert_eq!(split("[::1]:9777"), Some(("::1".into(), Some(9777))));
        assert_eq!(
            split("fe80::1%eth0"),
            Some(("fe80::1%eth0".into(), None)),
            "a zone id is part of the address"
        );
        // The default port spelled out is still spelled out.
        assert_eq!(
            split("desk:9777"),
            Some(("desk".into(), Some(super::DEFAULT_PORT)))
        );
        assert_eq!(split("desk:nope"), None, "an unusable port is not a split");
        assert_eq!(split(""), None);
    }
}
