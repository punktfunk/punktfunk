//! How a device orders its saved hosts: the `host_sort` and `host_grouping` keys every shell
//! on the device reads from [`Settings::extra`] under the Apple app's names. The console and
//! the desktop shell share one settings file, so one reading of the keys keeps them agreeing.

use crate::trust::Settings;

pub const HOST_SORT_KEY: &str = "host_sort";
pub const HOST_GROUPING_KEY: &str = "host_grouping";
pub const HOST_SORTS: [(&str, &str); 3] = [
    ("added", "Date added"),
    ("name", "Name"),
    ("lastConnected", "Last connected"),
];
pub const HOST_GROUPINGS: [(&str, &str); 3] =
    [("none", "None"), ("preset", "Preset"), ("status", "Status")];

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

/// The band a card sits in under `grouping`, or `None` ungrouped.
pub fn group_of<T: Arrangeable>(h: &T, grouping: &str) -> Option<String> {
    match grouping {
        "status" => Some(if h.online() { "Online" } else { "Offline" }.into()),
        "preset" => Some(h.preset_name().unwrap_or("No preset").to_string()),
        _ => None,
    }
}

/// Order the cards as Settings asks: bands first (Online before Offline, presets by name with
/// "No preset" last), then the sort inside each. Stable, so equal cards keep store order,
/// which is the order they were added.
pub fn arrange<T: Arrangeable>(hosts: &mut [T], s: &Settings) {
    let grouping = grouping(s);
    let sort = sort(s);
    let band = |h: &T| match (grouping, group_of(h, grouping)) {
        ("status", _) => (u8::from(!h.online()), String::new()),
        (_, Some(name)) if name == "No preset" => (1, String::new()),
        (_, Some(name)) => (0, name.to_lowercase()),
        (_, None) => (0, String::new()),
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
