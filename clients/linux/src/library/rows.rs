//! The Library's rows as plain values: each section the device keeps on, in its order
//! (design §2.5). The Games grid is cut into rows of `columns` posters, so one
//! virtualized list scrolls a library of thousands without building a widget per title.

use pf_client_core::collate::{self, GroupBy, GroupKey, SortKey};
use pf_client_core::library::{GameEntry, DESKTOP_ID};
use pf_client_core::library_layout::Section;

/// Recently played shows at most this many titles (design D6).
pub const RECENT_MAX: usize = 12;

/// One poster slot: a title from the catalog, or the host's desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tile {
    Game(usize),
    /// The host itself, streamed with no launch. Leads the Games grid when Desktops is off, so
    /// the Library is never a dead end.
    Desktop,
}

/// What a poster says under its title (design P6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Caption {
    None,
    LastPlayed,
    PlayTime,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Heading(String),
    /// One wide tile per paired host.
    Desktops,
    /// A horizontal band of posters.
    Band {
        tiles: Vec<Tile>,
        caption: Caption,
    },
    /// One row of the Games grid.
    Posters {
        tiles: Vec<Tile>,
        caption: Caption,
    },
}

pub struct Layout<'a> {
    pub games: &'a [GameEntry],
    pub sections: &'a [(Section, bool)],
    pub favorites: &'a [String],
    pub sort: SortKey,
    pub group: Option<GroupBy>,
    pub search: &'a str,
    pub columns: usize,
    pub paired_hosts: usize,
}

impl Layout<'_> {
    fn matches(&self, i: usize) -> bool {
        let g = &self.games[i];
        g.id != DESKTOP_ID
            && (self.search.is_empty()
                || g.title
                    .to_lowercase()
                    .contains(&self.search.trim().to_lowercase()))
    }

    fn sort_caption(&self) -> Caption {
        match self.sort {
            SortKey::Recent => Caption::LastPlayed,
            SortKey::PlayTime => Caption::PlayTime,
            _ => Caption::None,
        }
    }
}

/// The rows for one shelf. An empty section hides; Desktops ignores the search.
pub fn rows(l: &Layout) -> Vec<Row> {
    let mut out = Vec::new();
    let groups = collate::collate(l.games, l.sort, None);
    let in_order: Vec<usize> = groups.iter().flat_map(|g| g.games.clone()).collect();
    let desktops_on = l
        .sections
        .iter()
        .any(|(s, on)| *s == Section::Desktops && *on && l.paired_hosts > 0);
    for (section, on) in l.sections {
        if !on {
            continue;
        }
        let band = |tiles: Vec<Tile>, caption: Caption, title: &str, out: &mut Vec<Row>| {
            if !tiles.is_empty() {
                out.push(Row::Heading(title.into()));
                out.push(Row::Band { tiles, caption });
            }
        };
        match section {
            Section::Desktops if l.paired_hosts > 0 => {
                out.push(Row::Heading("Desktops".into()));
                out.push(Row::Desktops);
            }
            Section::Recent => {
                let mut played: Vec<usize> = (0..l.games.len())
                    .filter(|&i| l.matches(i) && last_played(&l.games[i]) > 0)
                    .collect();
                played.sort_by_key(|&i| std::cmp::Reverse(last_played(&l.games[i])));
                played.truncate(RECENT_MAX);
                let tiles = played.into_iter().map(Tile::Game).collect();
                band(tiles, Caption::LastPlayed, "Recently played", &mut out);
            }
            Section::Favorites => {
                let tiles = in_order
                    .iter()
                    .copied()
                    .filter(|&i| l.matches(i) && l.favorites.contains(&l.games[i].id))
                    .map(Tile::Game)
                    .collect();
                band(tiles, l.sort_caption(), "Favorites", &mut out);
            }
            Section::Launchers => {
                let tiles = groups
                    .iter()
                    .filter(|g| g.key == GroupKey::Launchers)
                    .flat_map(|g| g.games.iter().copied())
                    .filter(|&i| l.matches(i))
                    .map(Tile::Game)
                    .collect();
                band(tiles, Caption::None, "Launchers", &mut out);
            }
            Section::Games => games(l, !desktops_on, &mut out),
            // The console's; the desktop shell groups the grid instead (design D5).
            Section::Collections | Section::Desktops => {}
        }
    }
    out
}

/// The grid, grouped when asked, chunked into rows of posters.
fn games(l: &Layout, lead_desktop: bool, out: &mut Vec<Row>) {
    let caption = l.sort_caption();
    let columns = l.columns.max(1);
    let mut first = true;
    for group in collate::collate(l.games, l.sort, l.group) {
        if group.key == GroupKey::Launchers {
            continue;
        }
        let mut tiles: Vec<Tile> = group
            .games
            .into_iter()
            .filter(|&i| l.matches(i))
            .map(Tile::Game)
            .collect();
        if first && lead_desktop && l.search.is_empty() {
            tiles.insert(0, Tile::Desktop);
        }
        if tiles.is_empty() {
            continue;
        }
        let heading = match l.group {
            Some(_) => group.label,
            None => "Games".into(),
        };
        out.push(Row::Heading(heading));
        for chunk in tiles.chunks(columns) {
            out.push(Row::Posters {
                tiles: chunk.to_vec(),
                caption,
            });
        }
        first = false;
    }
    if first && lead_desktop && l.search.is_empty() {
        // No titles at all: the desktop still has a place to be pressed.
        out.push(Row::Heading("Games".into()));
        out.push(Row::Posters {
            tiles: vec![Tile::Desktop],
            caption,
        });
    }
}

fn last_played(g: &GameEntry) -> u64 {
    g.stats.as_ref().map_or(0, |s| s.last_played_unix_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::library::GameStats;
    use pf_client_core::library_layout::sections;

    fn game(id: &str, title: &str, played: u64) -> GameEntry {
        GameEntry {
            id: id.into(),
            store: "steam".into(),
            title: title.into(),
            art: Default::default(),
            platform: None,
            developer: None,
            release_year: None,
            genres: Vec::new(),
            role: None,
            icon: None,
            stats: (played > 0).then_some(GameStats {
                last_played_unix_ms: played,
                play_time_ms: 0,
                last_run_ms: 0,
                launch_count: 1,
            }),
        }
    }

    fn launcher(id: &str) -> GameEntry {
        GameEntry {
            role: Some("launcher".into()),
            ..game(id, id, 0)
        }
    }

    fn layout<'a>(
        games: &'a [GameEntry],
        sections: &'a [(Section, bool)],
        favorites: &'a [String],
        search: &'a str,
    ) -> Layout<'a> {
        Layout {
            games,
            sections,
            favorites,
            sort: SortKey::HostOrder,
            group: None,
            search,
            columns: 2,
            paired_hosts: 1,
        }
    }

    fn headings(rows: &[Row]) -> Vec<&str> {
        rows.iter()
            .filter_map(|r| match r {
                Row::Heading(h) => Some(h.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sections_follow_the_stored_order_and_empty_ones_hide() {
        let games = [
            game("a", "Alpha", 5),
            game("b", "Beta", 0),
            game("c", "Gamma", 9),
        ];
        let order = sections("games,desktops,-favorites");
        let rows = rows(&layout(&games, &order, &[], ""));
        // Favorites is off, Launchers has nothing, Recent follows the stored ones.
        assert_eq!(headings(&rows), ["Games", "Desktops", "Recently played"]);
        let recent = rows
            .iter()
            .find_map(|r| match r {
                Row::Band { tiles, .. } => Some(tiles.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(recent, [Tile::Game(2), Tile::Game(0)], "newest first");
    }

    #[test]
    fn the_grid_is_rows_of_posters_without_launchers() {
        let games = [
            launcher("steam"),
            game("a", "A", 0),
            game("b", "B", 0),
            game("c", "C", 0),
        ];
        let order = sections("games");
        let rows = rows(&layout(&games, &order, &[], ""));
        let posters: Vec<Vec<Tile>> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Posters { tiles, .. } => Some(tiles.clone()),
                _ => None,
            })
            .collect();
        // Desktops is on, so the grid does not lead with the desktop tile.
        assert_eq!(
            posters,
            [vec![Tile::Game(1), Tile::Game(2)], vec![Tile::Game(3)]]
        );
    }

    #[test]
    fn with_desktops_off_the_grid_leads_with_the_desktop() {
        let games = [game("a", "A", 0)];
        let order = sections("-desktops");
        let rows = rows(&layout(&games, &order, &[], ""));
        assert!(rows.contains(&Row::Posters {
            tiles: vec![Tile::Desktop, Tile::Game(0)],
            caption: Caption::None,
        }));
    }

    #[test]
    fn search_filters_every_shelf_bound_section_but_not_desktops() {
        let games = [game("a", "Hades", 3), game("b", "Celeste", 4)];
        let favorites = vec!["a".to_string(), "b".to_string()];
        let order = sections("");
        let rows = rows(&layout(&games, &order, &favorites, "hAd"));
        assert!(rows.contains(&Row::Desktops));
        for r in &rows {
            if let Row::Band { tiles, .. } | Row::Posters { tiles, .. } = r {
                assert_eq!(tiles, &[Tile::Game(0)]);
            }
        }
    }

    #[test]
    fn recently_played_keeps_twelve() {
        let games: Vec<GameEntry> = (1..=20)
            .map(|i| game(&format!("g{i}"), &format!("G{i}"), i))
            .collect();
        let order = sections("recent");
        let rows = rows(&layout(&games, &order, &[], ""));
        let Some(Row::Band { tiles, caption }) = rows.get(1) else {
            panic!("a band follows its heading");
        };
        assert_eq!(tiles.len(), RECENT_MAX);
        assert_eq!(tiles[0], Tile::Game(19));
        assert_eq!(*caption, Caption::LastPlayed);
    }
}
