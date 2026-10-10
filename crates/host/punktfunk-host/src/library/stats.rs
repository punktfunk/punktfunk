//! Per-title play stats: last played, total play time, last run, launch count.
//!
//! A side table like `hidden.rs`: scanner and plugin titles are rebuilt on every
//! reconcile, so numbers written onto an entry would vanish. Keys are the stable
//! `<store>:<external_id>` ids. [`record_launch`] fires when this host spawns a
//! title; [`record_run_time`] adds what the lease watcher saw running
//! ([`crate::gamelease`]). A reconnect adopts the running game: neither a launch
//! nor a new run. Both credit only a recorded launch ([`crate::launchreg`]).
//! Each also credits the session's profile under `by_profile`; the totals count
//! every launch once, profile or not.
//!
//! Play time is time under a watcher: from the game seen running to its exit,
//! flushed once a minute so a crash loses under one. A game kept alive after its
//! session ends is not counted until a client comes back for it. Pin
//! `library-stats.json` in the tests below.

use super::*;
use std::sync::Mutex;
use std::time::Duration;

/// One title's numbers. On the wire as `GameEntry.stats`; absent until the first launch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GameStats {
    /// Unix ms of the most recent launch. Stamped at launch, so a game just started sorts first.
    pub last_played_unix_ms: u64,
    /// Every run added up.
    pub play_time_ms: u64,
    /// The run that started at `last_played_unix_ms`. Still growing while it runs.
    pub last_run_ms: u64,
    pub launch_count: u32,
}

impl GameStats {
    fn launched(&mut self, now_ms: u64) {
        self.last_played_unix_ms = now_ms;
        self.last_run_ms = 0;
        self.launch_count = self.launch_count.saturating_add(1);
    }

    #[cfg_attr(target_os = "macos", allow(dead_code, reason = "no watcher on macOS"))]
    fn ran(&mut self, delta_ms: u64) {
        self.play_time_ms = self.play_time_ms.saturating_add(delta_ms);
        self.last_run_ms = self.last_run_ms.saturating_add(delta_ms);
    }
}

/// One title on `GET /library`: the host-wide numbers, plus `mine` for the profile the
/// request named (`?as=`) once that profile has launched the title.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EntryStats {
    #[serde(flatten)]
    pub totals: GameStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mine: Option<GameStats>,
}

/// One title in the file: the totals inline, then each profile's own numbers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct TitleStats {
    #[serde(flatten)]
    pub totals: GameStats,
    /// Profile id → that profile's numbers. Absent until a profile launches the title.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_profile: BTreeMap<String, GameStats>,
}

impl TitleStats {
    /// The entry's `stats`, with `mine` filled for `profile`.
    pub(crate) fn entry(&self, profile: Option<&str>) -> EntryStats {
        EntryStats {
            totals: self.totals,
            mine: profile.and_then(|p| self.by_profile.get(p)).copied(),
        }
    }

    fn each(&mut self, profile: Option<&str>, f: impl Fn(&mut GameStats)) {
        f(&mut self.totals);
        if let Some(p) = profile {
            f(self.by_profile.entry(p.to_string()).or_default());
        }
    }
}

/// `library-stats.json`. A `BTreeMap` keeps the file diffable.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StatsFile {
    #[serde(default)]
    games: BTreeMap<String, TitleStats>,
}

fn stats_path() -> PathBuf {
    pf_paths::config_dir().join("library-stats.json")
}

/// Malformed or absent file → no stats. Numbers are never worth an empty library.
fn load_file() -> StatsFile {
    read_json_or_default(&stats_path())
}

fn save_file(file: &StatsFile) -> Result<()> {
    save_json(&stats_path(), &serde_json::to_string_pretty(file)?)
}

/// Load-modify-save under one lock: a launch and another game's minute flush
/// run on different threads and must not lose each other's write.
fn update(id: &str, f: impl FnOnce(&mut TitleStats)) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = load_file();
    f(file.games.entry(id.to_string()).or_default());
    if let Err(e) = save_file(&file) {
        tracing::warn!(error = %e, id, "library-stats.json not written");
    }
}

pub(crate) fn game_stats() -> BTreeMap<String, TitleStats> {
    load_file().games
}

/// Fill `stats.mine` on `games` with `profile`'s numbers. One file read for the lot.
pub(crate) fn fill_mine<'a>(games: impl IntoIterator<Item = &'a mut GameEntry>, profile: &str) {
    let file = load_file();
    for g in games {
        if let Some(t) = file.games.get(&g.id) {
            g.stats = Some(t.entry(Some(profile)));
        }
    }
}

/// This host spawned `id` just now, for `profile`'s session. Not for an adopted
/// launch (a reconnect).
pub fn record_launch(id: &str, profile: Option<&str>) {
    let now_ms = crate::clock::unix_ms();
    update(id, |s| s.each(profile, |g| g.launched(now_ms)));
}

/// `id` was seen running for another `delta` since the last call.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no watcher on macOS"))]
pub fn record_run_time(id: &str, profile: Option<&str>, delta: Duration) {
    if delta.is_zero() {
        return;
    }
    let ms = delta.as_millis() as u64;
    update(id, |s| s.each(profile, |g| g.ran(ms)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A launch stamps and counts; runs add to both totals; the next launch
    /// resets only the last run.
    #[test]
    fn runs_add_up_and_a_new_launch_resets_the_last_run() {
        let mut s = GameStats::default();
        s.launched(100);
        s.ran(5);
        s.ran(7);
        assert_eq!(
            s,
            GameStats {
                last_played_unix_ms: 100,
                play_time_ms: 12,
                last_run_ms: 12,
                launch_count: 1,
            }
        );
        s.launched(200);
        assert_eq!(s.last_played_unix_ms, 200);
        assert_eq!(s.play_time_ms, 12, "the total survives a launch");
        assert_eq!(s.last_run_ms, 0, "the new run starts empty");
        assert_eq!(s.launch_count, 2);
    }

    #[test]
    fn malformed_file_keeps_no_stats() {
        let f: StatsFile = serde_json::from_str("{ not json").unwrap_or_default();
        assert!(f.games.is_empty());
        let f: StatsFile = serde_json::from_str("{}").expect("an empty object is valid");
        assert!(f.games.is_empty(), "absent key means no stats");
    }

    /// The persisted shape is the contract an operator may hand-edit — pin it.
    #[test]
    fn file_roundtrips_the_documented_shape() {
        let raw = r#"{"games":{"steam:70":{"last_played_unix_ms":1757160000000,"play_time_ms":5400000,"last_run_ms":2700000,"launch_count":12}}}"#;
        let f: StatsFile = serde_json::from_str(raw).expect("parses");
        assert_eq!(f.games["steam:70"].totals.launch_count, 12);
        assert_eq!(serde_json::to_string(&f).expect("serializes"), raw);

        let raw = r#"{"games":{"steam:70":{"last_played_unix_ms":9,"play_time_ms":5,"last_run_ms":5,"launch_count":2,"by_profile":{"4f1c3a9b0e27":{"last_played_unix_ms":9,"play_time_ms":5,"last_run_ms":5,"launch_count":1}}}}}"#;
        let f: StatsFile = serde_json::from_str(raw).expect("parses");
        assert_eq!(
            f.games["steam:70"].by_profile["4f1c3a9b0e27"].launch_count,
            1
        );
        assert_eq!(serde_json::to_string(&f).expect("serializes"), raw);
    }

    /// Totals count every launch; a profile counts only its own.
    #[test]
    fn a_profile_counts_its_own_and_the_totals_count_all() {
        let mut t = TitleStats::default();
        t.each(Some("kid"), |g| g.launched(100));
        t.each(Some("kid"), |g| g.ran(30));
        t.each(None, |g| g.launched(200));
        t.each(Some("enrico"), |g| g.launched(300));
        t.each(Some("enrico"), |g| g.ran(10));
        assert_eq!(t.totals.launch_count, 3);
        assert_eq!(t.totals.play_time_ms, 40);
        assert_eq!(t.totals.last_played_unix_ms, 300);
        assert_eq!(t.by_profile["kid"].play_time_ms, 30);
        assert_eq!(t.by_profile["enrico"].launch_count, 1);
        assert_eq!(t.entry(Some("kid")).mine.unwrap().last_played_unix_ms, 100);
        assert_eq!(t.entry(Some("gone")).mine, None);
        assert_eq!(t.entry(None).totals, t.totals);
    }

    /// `mine` sits inside `stats`, beside the totals a client already reads.
    #[test]
    fn mine_rides_beside_the_totals_on_the_wire() {
        let mut t = TitleStats::default();
        t.each(Some("kid"), |g| g.launched(100));
        let wire = serde_json::to_value(t.entry(Some("kid"))).unwrap();
        assert_eq!(wire["launch_count"], 1);
        assert_eq!(wire["mine"]["launch_count"], 1);
        assert!(serde_json::to_value(t.entry(None))
            .unwrap()
            .get("mine")
            .is_none());
    }
}
