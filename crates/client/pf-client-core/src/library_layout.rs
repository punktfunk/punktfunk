//! How a device lays out a host's library: the sections of the Games tab and their order
//! (`Settings::library_sections`), the titles it marked favorite, and the captions a played
//! title carries. The console and the desktop shell share one settings file on a box, so
//! both read these through one definition.

use crate::trust::Settings;

/// One band of the Games tab, by the id `Settings::library_sections` stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Desktops,
    Recent,
    Favorites,
    Launchers,
    /// One tile per platform or store group.
    Collections,
    Games,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Section::Desktops,
        Section::Recent,
        Section::Favorites,
        Section::Launchers,
        Section::Collections,
        Section::Games,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Section::Desktops => "desktops",
            Section::Recent => "recent",
            Section::Favorites => "favorites",
            Section::Launchers => "launchers",
            Section::Collections => "collections",
            Section::Games => "games",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Desktops => "Desktops",
            Section::Recent => "Recently played",
            Section::Favorites => "Favorites",
            Section::Launchers => "Launchers",
            Section::Collections => "Collections",
            Section::Games => "Games",
        }
    }
}

/// `library_sections` as the Apple app parses it: ids in order, `-` before one switched
/// off, an unknown id dropped, a repeat keeping its first place, a known section the value
/// lacks appended switched on.
pub fn sections(stored: &str) -> Vec<(Section, bool)> {
    let mut out: Vec<(Section, bool)> = Vec::new();
    for token in stored.split(',').map(str::trim) {
        let (on, id) = match token.strip_prefix('-') {
            Some(id) => (false, id),
            None => (true, token),
        };
        let Some(s) = Section::ALL.into_iter().find(|s| s.id() == id) else {
            continue;
        };
        if !out.iter().any(|(seen, _)| *seen == s) {
            out.push((s, on));
        }
    }
    for s in Section::ALL {
        if !out.iter().any(|(seen, _)| *seen == s) {
            out.push((s, true));
        }
    }
    out
}

/// The stored form of `sections`.
pub fn stored_sections(sections: &[(Section, bool)]) -> String {
    sections
        .iter()
        .map(|(s, on)| format!("{}{}", if *on { "" } else { "-" }, s.id()))
        .collect::<Vec<_>>()
        .join(",")
}

/// `ms` ago, as a person says it: `just now`, `12 min ago`, `2 h ago`, `3 d ago`, `5 mo ago`.
pub fn ago(ms: u64) -> String {
    let min = ms / 60_000;
    match min {
        0 => "just now".into(),
        1..60 => format!("{min} min ago"),
        60..1_440 => format!("{} h ago", min / 60),
        1_440..43_200 => format!("{} d ago", min / 1_440),
        _ => format!("{} mo ago", min / 43_200),
    }
}

/// The Details card's play line: `Last played 2 h ago · 14 h total · 12 launches`. `None`
/// for a title never played here.
pub fn stats_line(s: &crate::library::GameStats, now_ms: u64) -> Option<String> {
    if s.last_played_unix_ms == 0 {
        return None;
    }
    let mut parts = vec![format!(
        "Last played {}",
        ago(now_ms.saturating_sub(s.last_played_unix_ms))
    )];
    let hours = s.play_time_ms / 3_600_000;
    if hours > 0 {
        parts.push(format!("{hours} h total"));
    }
    if s.launch_count > 0 {
        parts.push(format!(
            "{} launch{}",
            s.launch_count,
            if s.launch_count == 1 { "" } else { "es" }
        ));
    }
    Some(parts.join(" \u{b7} "))
}

/// Where a host's favorites live in the settings document's `extra` map.
fn favorites_key(fp_hex: &str) -> String {
    format!("favorites.{fp_hex}")
}

/// The titles marked favorite on host `fp_hex` from this device, in marking order.
pub fn favorites(settings: &Settings, fp_hex: &str) -> Vec<String> {
    settings
        .extra
        .get(&favorites_key(fp_hex))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Mark or unmark `id` on host `fp_hex`; `true` when it is now a favorite.
pub fn toggle_favorite(settings: &mut Settings, fp_hex: &str, id: &str) -> bool {
    let mut list = favorites(settings, fp_hex);
    let on = match list.iter().position(|f| f == id) {
        Some(i) => {
            list.remove(i);
            false
        }
        None => {
            list.push(id.to_string());
            true
        }
    };
    let key = favorites_key(fp_hex);
    if list.is_empty() {
        settings.extra.remove(&key);
    } else {
        settings.extra.insert(key, list.into());
    }
    on
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_parse_as_the_apple_app_does() {
        use Section::*;
        assert_eq!(
            sections(""),
            Section::ALL.map(|s| (s, true)).to_vec(),
            "empty is every section, on"
        );
        assert_eq!(
            sections("games,-recent,bogus,games,desktops"),
            vec![
                (Games, true),
                (Recent, false),
                (Desktops, true),
                (Favorites, true),
                (Launchers, true),
                (Collections, true),
            ]
        );
        let s = sections("desktops,recent,-favorites,launchers,-collections,games");
        assert_eq!(
            stored_sections(&s),
            "desktops,recent,-favorites,launchers,-collections,games"
        );
    }

    #[test]
    fn the_play_line_reads_like_a_person_says_it() {
        let s = crate::library::GameStats {
            last_played_unix_ms: 1_000,
            play_time_ms: 14 * 3_600_000,
            last_run_ms: 0,
            launch_count: 12,
        };
        assert_eq!(
            stats_line(&s, 1_000 + 2 * 3_600_000).as_deref(),
            Some("Last played 2 h ago \u{b7} 14 h total \u{b7} 12 launches")
        );
        assert_eq!(ago(30_000), "just now");
        assert_eq!(ago(3 * 86_400_000), "3 d ago");
        let never = crate::library::GameStats {
            last_played_unix_ms: 0,
            ..s
        };
        assert_eq!(stats_line(&never, 5), None);
    }

    #[test]
    fn favorites_toggle_per_host_and_drop_an_empty_list() {
        let mut settings = Settings::default();
        assert!(toggle_favorite(&mut settings, "aa", "steam:1"));
        assert!(toggle_favorite(&mut settings, "aa", "steam:2"));
        assert!(favorites(&settings, "bb").is_empty(), "per host");
        assert_eq!(favorites(&settings, "aa"), ["steam:1", "steam:2"]);
        assert!(!toggle_favorite(&mut settings, "aa", "steam:1"));
        assert!(!toggle_favorite(&mut settings, "aa", "steam:2"));
        assert!(settings.extra.is_empty(), "no empty list left behind");
    }
}
