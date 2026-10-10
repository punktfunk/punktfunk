//! Where a bare launch opens: the host list, the default host's library, or its stream.
//!
//! One resolver for every surface (`design/default-host.md`). The default host is the
//! explicit `default_host` id when it names a paired record, else the only paired record.
//! Paired-only: a launch cannot pair, so an unpaired host is a dead landing. A dangling id
//! falls through to the derived rule, which is why Forget host needs no write hook —
//! [`clear_default`] is hygiene so a re-pair cannot resurrect an old choice.
//!
//! Held to the Swift and Kotlin ports by `clients/shared/start-screen-vectors.json`.

use crate::trust::{KnownHost, KnownHosts, Settings};

/// The `start_in` setting. Unknown reads as [`StartIn::Hosts`], the `library_view`
/// convention: a value a newer client wrote degrades, it never ends a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartIn {
    Hosts,
    Library,
    Stream,
}

impl StartIn {
    /// The three values in row order, for a left/right cycle.
    pub const ALL: [StartIn; 3] = [StartIn::Hosts, StartIn::Library, StartIn::Stream];

    pub fn parse(s: &str) -> StartIn {
        match s {
            "library" => StartIn::Library,
            "stream" => StartIn::Stream,
            _ => StartIn::Hosts,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StartIn::Hosts => "hosts",
            StartIn::Library => "library",
            StartIn::Stream => "stream",
        }
    }

    /// Settings-row label. The value line names the resolved host beside it.
    pub fn label(self) -> &'static str {
        match self {
            StartIn::Hosts => "Host list",
            StartIn::Library => "Library",
            StartIn::Stream => "Stream",
        }
    }

    /// One step of the settings row's cycle; wraps both ways.
    pub fn step(self, forward: bool) -> StartIn {
        let i = StartIn::ALL.iter().position(|v| *v == self).unwrap_or(0);
        let n = StartIn::ALL.len();
        StartIn::ALL[if forward { i + 1 } else { i + n - 1 } % n]
    }
}

/// The resolved landing, carrying an index into [`KnownHosts::hosts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    Hosts,
    Library(usize),
    /// The library plus one connect raised before the first frame. One attempt: a
    /// refusal lands on the library underneath, never a retry.
    Stream(usize),
}

impl Start {
    /// The host this landing opens on, as an index into [`KnownHosts::hosts`].
    pub fn host_index(self) -> Option<usize> {
        match self {
            Start::Hosts => None,
            Start::Library(i) | Start::Stream(i) => Some(i),
        }
    }
}

/// Where the default came from, for the one log line a launch prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Explicit,
    Derived,
    None,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Explicit => "explicit",
            Source::Derived => "derived",
            Source::None => "none",
        }
    }
}

/// A record a launch can land on: paired, with a pin to dial it by.
fn usable(h: &KnownHost) -> bool {
    h.paired && !h.fp_hex.is_empty()
}

/// The default host's index and where it came from. The explicit id wins when it names a
/// usable record; else the sole usable record; else nothing.
pub fn default_host_with_source(
    settings: &Settings,
    known: &KnownHosts,
) -> (Option<usize>, Source) {
    if let Some(want) = settings.default_host.as_deref().filter(|s| !s.is_empty()) {
        if let Some(i) = known
            .hosts
            .iter()
            .position(|h| usable(h) && h.id.as_deref() == Some(want))
        {
            return (Some(i), Source::Explicit);
        }
    }
    let mut usable_hosts = known.hosts.iter().enumerate().filter(|(_, h)| usable(h));
    match (usable_hosts.next(), usable_hosts.next()) {
        (Some((i, _)), None) => (Some(i), Source::Derived),
        _ => (None, Source::None),
    }
}

/// The default host's index, or `None`. See [`default_host_with_source`].
pub fn default_host(settings: &Settings, known: &KnownHosts) -> Option<usize> {
    default_host_with_source(settings, known).0
}

/// Where a bare launch opens. No default host degrades every value to the list.
pub fn start_screen(settings: &Settings, known: &KnownHosts) -> Start {
    let Some(i) = default_host(settings, known) else {
        return Start::Hosts;
    };
    match StartIn::parse(&settings.start_in) {
        StartIn::Hosts => Start::Hosts,
        StartIn::Library => Start::Library(i),
        StartIn::Stream => Start::Stream(i),
    }
}

/// Clear an explicit default that names `id`. `true` = the caller must save. Called from
/// Forget host and Forget pairing, where the record is about to stop being usable.
pub fn clear_default(settings: &mut Settings, id: Option<&str>) -> bool {
    match (settings.default_host.as_deref(), id) {
        (Some(cur), Some(gone)) if cur == gone => {
            settings.default_host = None;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, paired: bool) -> KnownHost {
        KnownHost {
            name: id.into(),
            addr: "10.0.0.1".into(),
            fp_hex: "ab".repeat(32),
            paired,
            id: Some(id.into()),
            ..Default::default()
        }
    }

    fn settings(start_in: &str, default_host: Option<&str>) -> Settings {
        Settings {
            start_in: start_in.into(),
            default_host: default_host.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn unknown_start_in_is_the_host_list() {
        assert_eq!(StartIn::parse(""), StartIn::Hosts);
        assert_eq!(StartIn::parse("shelf"), StartIn::Hosts);
        for v in StartIn::ALL {
            assert_eq!(StartIn::parse(v.as_str()), v);
        }
        assert_eq!(StartIn::Hosts.step(true), StartIn::Library);
        assert_eq!(StartIn::Hosts.step(false), StartIn::Stream);
        assert_eq!(StartIn::Stream.step(true), StartIn::Hosts);
    }

    #[test]
    fn a_pin_less_record_is_never_the_default() {
        let mut h = host("a", true);
        h.fp_hex.clear();
        let known = KnownHosts { hosts: vec![h] };
        assert_eq!(default_host(&settings("", None), &known), None);
    }

    #[test]
    fn a_dangling_id_falls_through_to_the_derived_rule() {
        let known = KnownHosts {
            hosts: vec![host("a", true)],
        };
        assert_eq!(
            default_host_with_source(&settings("", Some("gone")), &known),
            (Some(0), Source::Derived)
        );
    }

    #[test]
    fn an_id_naming_an_unpaired_record_is_no_default() {
        let known = KnownHosts {
            hosts: vec![host("a", false), host("b", true)],
        };
        // The id loses, and one usable record is left, so the derived rule answers.
        assert_eq!(
            default_host_with_source(&settings("", Some("a")), &known),
            (Some(1), Source::Derived)
        );
    }

    #[test]
    fn two_paired_hosts_need_a_choice() {
        let known = KnownHosts {
            hosts: vec![host("a", true), host("b", true)],
        };
        assert_eq!(
            start_screen(&settings("library", None), &known),
            Start::Hosts
        );
        assert_eq!(
            default_host_with_source(&settings("", Some("b")), &known),
            (Some(1), Source::Explicit)
        );
    }

    #[test]
    fn clear_default_only_fires_on_a_match() {
        let mut s = settings("", Some("a"));
        assert!(!clear_default(&mut s, Some("b")));
        assert!(!clear_default(&mut s, None));
        assert!(clear_default(&mut s, Some("a")));
        assert_eq!(s.default_host, None);
        assert!(!clear_default(&mut s, Some("a")));
    }

    /// Cross-language vector file; Swift and Kotlin consume it too. A case's host carries
    /// only what the policy reads — id, paired, and whether it holds a pin.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../../clients/shared/start-screen-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let cases = file["cases"].as_array().expect("cases array");
        assert!(cases.len() >= 10, "the vector file is the contract");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let s = Settings {
                start_in: case["start_in"].as_str().unwrap().into(),
                default_host: case
                    .get("default_host")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                ..Default::default()
            };
            let known = KnownHosts {
                hosts: case["hosts"]
                    .as_array()
                    .expect("hosts array")
                    .iter()
                    .map(|h| KnownHost {
                        name: h["name"].as_str().unwrap().into(),
                        addr: "10.0.0.1".into(),
                        fp_hex: if h["pinned"].as_bool().unwrap() {
                            "ab".repeat(32)
                        } else {
                            String::new()
                        },
                        paired: h["paired"].as_bool().unwrap(),
                        id: Some(h["id"].as_str().unwrap().into()),
                        ..Default::default()
                    })
                    .collect(),
            };
            let (idx, src) = default_host_with_source(&s, &known);
            let want = &case["expect"];
            let got = match start_screen(&s, &known) {
                Start::Hosts => "hosts",
                Start::Library(_) => "library",
                Start::Stream(_) => "stream",
            };
            assert_eq!(got, want["start"].as_str().unwrap(), "{name} start");
            assert_eq!(
                idx.map(|i| i as u64),
                want.get("host_index").and_then(serde_json::Value::as_u64),
                "{name} host_index"
            );
            assert_eq!(
                src.as_str(),
                want["source"].as_str().unwrap(),
                "{name} source"
            );
        }
    }
}
