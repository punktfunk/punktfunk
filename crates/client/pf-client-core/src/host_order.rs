//! How a device orders its saved hosts: the `host_sort` and `host_grouping` keys every shell
//! on the device reads from [`Settings::extra`] under the Apple app's names. The console and
//! the desktop shell share one settings file, so one reading of the keys keeps them agreeing.

use crate::settings::Label;
use crate::trust::Settings;

pub const HOST_SORT_KEY: &str = "host_sort";
pub const HOST_GROUPING_KEY: &str = "host_grouping";
/// `(stored value, label)`.
pub const HOST_SORTS: [(&str, Label); 3] = [
    ("added", pf_l10n::client::player_host_sort_added),
    ("name", pf_l10n::client::player_host_sort_name),
    (
        "lastConnected",
        pf_l10n::client::player_host_sort_last_connected,
    ),
];
pub const HOST_GROUPINGS: [(&str, Label); 3] = [
    ("none", pf_l10n::client::player_host_grouping_none),
    ("preset", pf_l10n::client::player_host_grouping_preset),
    ("status", pf_l10n::client::player_host_grouping_status),
];

/// One host card as the order needs it.
pub trait Arrangeable {
    fn name(&self) -> &str;
    fn online(&self) -> bool;
    fn last_used(&self) -> Option<u64>;
    /// The preset this card connects with: a pinned card's own, else the host's binding.
    fn preset_name(&self) -> Option<&str>;
}

impl<T: Arrangeable + ?Sized> Arrangeable for &T {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn online(&self) -> bool {
        (**self).online()
    }
    fn last_used(&self) -> Option<u64> {
        (**self).last_used()
    }
    fn preset_name(&self) -> Option<&str> {
        (**self).preset_name()
    }
}

fn extra<'s>(s: &'s Settings, key: &str, default: &'s str) -> &'s str {
    s.extra.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

/// The sort in force: `added` (store order), `name` or `lastConnected`.
pub fn sort(s: &Settings) -> &str {
    extra(s, HOST_SORT_KEY, "added")
}

/// The grouping in force; Apple's store may still say `profile`, the old name for presets.
pub fn grouping(s: &Settings) -> &str {
    match extra(s, HOST_GROUPING_KEY, "none") {
        "profile" => "preset",
        g => g,
    }
}

/// A band of the host row, named only where it is drawn ([`Band::label`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band<'a> {
    Online,
    Offline,
    /// The preset's name.
    Preset(&'a str),
    NoPreset,
}

impl Band<'_> {
    /// The name over the band's first card.
    pub fn label(self) -> String {
        match self {
            Band::Online => pf_l10n::client::hosts_group_online().into(),
            Band::Offline => pf_l10n::client::hosts_group_offline().into(),
            Band::Preset(name) => name.into(),
            Band::NoPreset => pf_l10n::client::hosts_group_no_preset().into(),
        }
    }
}

/// The band a card sits in under `grouping`, or `None` ungrouped.
pub fn band_of<'a, T: Arrangeable>(h: &'a T, grouping: &str) -> Option<Band<'a>> {
    match grouping {
        "status" => Some(if h.online() {
            Band::Online
        } else {
            Band::Offline
        }),
        "preset" => Some(h.preset_name().map_or(Band::NoPreset, Band::Preset)),
        _ => None,
    }
}

/// [`band_of`]'s name.
pub fn group_of<T: Arrangeable>(h: &T, grouping: &str) -> Option<String> {
    band_of(h, grouping).map(Band::label)
}

/// Order the cards as Settings asks: bands first (Online before Offline, presets by name with
/// no preset last), then the sort inside each. Stable, so equal cards keep store order,
/// which is the order they were added.
pub fn arrange<T: Arrangeable>(hosts: &mut [T], s: &Settings) {
    let grouping = grouping(s);
    let sort = sort(s);
    let band = |h: &T| match band_of(h, grouping) {
        Some(Band::Offline | Band::NoPreset) => (1, String::new()),
        Some(Band::Preset(name)) => (0, name.to_lowercase()),
        Some(Band::Online) | None => (0, String::new()),
    };
    hosts.sort_by(|a, b| {
        band(a).cmp(&band(b)).then_with(|| match sort {
            "name" => a.name().to_lowercase().cmp(&b.name().to_lowercase()),
            // Most recent first; a host never connected to goes last.
            "lastConnected" => b.last_used().cmp(&a.last_used()),
            _ => std::cmp::Ordering::Equal,
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Card(&'static str, bool, Option<u64>, Option<&'static str>);

    impl Arrangeable for Card {
        fn name(&self) -> &str {
            self.0
        }
        fn online(&self) -> bool {
            self.1
        }
        fn last_used(&self) -> Option<u64> {
            self.2
        }
        fn preset_name(&self) -> Option<&str> {
            self.3
        }
    }

    fn order(sort: &str, grouping: &str) -> Vec<&'static str> {
        let mut s = Settings::default();
        s.extra.insert(HOST_SORT_KEY.into(), sort.into());
        s.extra.insert(HOST_GROUPING_KEY.into(), grouping.into());
        let mut cards = vec![
            Card("desk", false, Some(5), Some("Work")),
            Card("Attic", true, None, None),
            Card("couch", true, Some(9), Some("Game")),
        ];
        arrange(&mut cards, &s);
        cards.iter().map(|c| c.0).collect()
    }

    #[test]
    fn bands_come_first_then_the_sort() {
        assert_eq!(order("added", "none"), ["desk", "Attic", "couch"]);
        assert_eq!(order("name", "none"), ["Attic", "couch", "desk"]);
        assert_eq!(order("lastConnected", "none"), ["couch", "desk", "Attic"]);
        assert_eq!(order("name", "status"), ["Attic", "couch", "desk"]);
        // Presets by name, "No preset" last; the old `profile` spelling still groups.
        assert_eq!(order("added", "profile"), ["couch", "desk", "Attic"]);
    }
}
