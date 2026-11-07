//! Write-ahead journal of what a backend changes on the desk — the heads an `exclusive`
//! create disables, the headless output it adds, the box session a gamescope takeover takes —
//! so the change is taken back at teardown, and by the next host when this one dies mid-stream.
//!
//! One file per host process under `state_dir()/topology-journal/`, named by the owner's pid
//! and start time, rewritten whole with [`pf_paths::replace_file`] on every push and pop. A
//! backend [`push`]es an [`Undo`] **before** the step it reverses; the registry keeps the seqs
//! with the display group and [`apply`]s them, newest first, when the group empties;
//! [`reclaim`] runs every file whose owner is dead. The engine names no backend: each entry
//! goes to [`VirtualDisplay::undo`](crate::VirtualDisplay::undo) of the backend it records.
//!
//! An undo that fails, or whose heads do not read back lit, stays for the next pass. So does one
//! naming a head the compositor no longer lists: it may come back still disabled, and this host
//! runs the undo again once it does ([`apply`]). A file this version cannot read is left alone.
//! Pin the contract with the tests below; the design is
//! `design/component-health/16-topology-journal.md` (planning).

use serde::{Deserialize, Serialize};

/// One desk change a backend can take back: the inverse of a step its `create` takes, run by
/// the backend that pushed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Undo {
    /// Re-enable the heads an `exclusive` create disabled. A lit head is a no-op.
    EnableHeads { heads: Vec<Head> },
    /// Unplug the headless output a create added. `instance` is the compositor instance that
    /// named it: another instance may have given the name to the box's only screen, so a
    /// backend that cannot match it answers `Ok` and leaves the output alone.
    RemoveOutput { name: String, instance: String },
    /// Hand back one step a gamescope takeover took of the box's own session. `boot` is the
    /// boot it was taken in: the session lives in the user manager, so another boot has it back.
    Takeover { boot: String, step: TakeoverStep },
}

/// What a gamescope takeover takes of the box's own session, one variant per step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TakeoverStep {
    /// The box's autologin Game Mode units, killed and, where a display manager relogins,
    /// idled behind a drop-in. Undo restarts them.
    RestartAutologin { units: Vec<String> },
    /// Our managed `gamescope-session-plus` unit. Undo stops it.
    StopSession,
    /// SteamOS's own session, run headless under our drop-in. Undo restarts it without.
    RestoreSteamos,
    /// `SCREEN_*` set in the user manager. Undo unsets them.
    UnsetScreenEnv,
    /// Our bind drop-in on the box's Game Mode template. Undo removes it.
    RemoveSessionDropin,
    /// A display manager an older host stopped; no host stops one today. Undo restarts it.
    StartDisplayManager { unit: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub name: String,
    /// The exact mode to put back where the compositor forgets a disabled head's (KWin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::crash;
#[cfg(target_os = "linux")]
pub(crate) use linux::{apply, done, push, reclaim};

/// Journal re-enabling the heads `names` (the plan an `exclusive` create is about to darken)
/// for `backend`. Names only: sway and Hyprland keep a disabled head's mode themselves.
#[cfg(target_os = "linux")]
pub(crate) fn push_heads(backend: &'static str, names: &[String]) -> u64 {
    let heads = names
        .iter()
        .map(|name| Head {
            name: name.clone(),
            mode: None,
        })
        .collect();
    push(backend, Undo::EnableHeads { heads })
}

/// A dead host's output whose `instance` is not the `live` one (empty: none known) belongs to
/// another compositor instance, which may have given the name to another output. Logged and
/// left alone.
#[cfg(target_os = "linux")]
pub(crate) fn foreign_output(tag: &str, name: &str, instance: &str, live: &str) -> bool {
    let foreign = instance.is_empty() || instance != live;
    if foreign {
        tracing::info!(
            output = %name,
            "{tag}: a dead host's headless output belongs to another compositor instance — left alone"
        );
    }
    foreign
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Undo;
    use crate::VirtualDisplay;
    use anyhow::{bail, Context, Result};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Bumped when a reader of today's file could misread tomorrow's. A newer file is left alone.
    const VERSION: u32 = 1;

    /// How often a head that left mid-stream is looked for again: the delay between a monitor
    /// switched back on and its picture.
    const OWED_POLL: Duration = Duration::from_secs(2);

    /// Failed looks before an owed entry is left for the next host: a head that is back but
    /// refuses its re-enable must not have it re-run (a Hyprland `reload`) every poll.
    const OWED_TRIES: u32 = 3;

    #[derive(Serialize, Deserialize)]
    struct Doc {
        version: u32,
        owner: Owner,
        entries: Vec<Entry>,
    }

    /// The host process a file belongs to. `start` is the process start in ticks since boot,
    /// so a recycled pid is not the owner; `boot_id` so a pid from before a reboot is not either.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Owner {
        pid: u32,
        start: u64,
        boot_id: String,
    }

    impl Owner {
        fn me() -> Owner {
            let pid = std::process::id();
            Owner {
                pid,
                start: crate::proc::start_ticks(pid).unwrap_or(0),
                boot_id: crate::proc::boot_id(),
            }
        }

        fn alive(&self) -> bool {
            self.boot_id == crate::proc::boot_id()
                && crate::proc::pid_alive(self.pid)
                && crate::proc::start_ticks(self.pid) == Some(self.start)
        }

        fn file_name(&self) -> String {
            format!("{}-{}.json", self.pid, self.start)
        }
    }

    #[derive(Clone, Serialize, Deserialize)]
    struct Entry {
        seq: u64,
        backend: String,
        undo: Undo,
    }

    /// This process's journal: its file and the document on disk. `next` never repeats a seq
    /// the registry may still hold for a popped entry.
    struct Journal {
        path: PathBuf,
        doc: Doc,
        next: u64,
    }

    static JOURNAL: Mutex<Option<Journal>> = Mutex::new(None);

    fn dir() -> PathBuf {
        pf_paths::state_dir().join("topology-journal")
    }

    fn with<T>(f: impl FnOnce(&mut Journal) -> T) -> T {
        let mut slot = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        f(slot.get_or_insert_with(|| Journal::open(&dir())))
    }

    impl Journal {
        fn open(dir: &Path) -> Journal {
            let owner = Owner::me();
            Journal {
                path: dir.join(owner.file_name()),
                doc: Doc {
                    version: VERSION,
                    owner,
                    entries: Vec::new(),
                },
                next: 1,
            }
        }

        fn push(&mut self, backend: &str, undo: Undo) -> u64 {
            let seq = self.next;
            self.next += 1;
            self.doc.entries.push(Entry {
                seq,
                backend: backend.to_string(),
                undo,
            });
            self.persist();
            seq
        }

        fn settle(&mut self, seqs: &[u64]) {
            self.doc.entries.retain(|e| !seqs.contains(&e.seq));
            self.persist();
        }

        /// A write that fails is logged, not returned: the entry still lives in memory, so
        /// in-process teardown runs it; only a crash loses it.
        fn persist(&self) {
            if let Err(e) = write_doc(&self.path, &self.doc) {
                tracing::error!(
                    path = %self.path.display(), error = %e,
                    "topology journal not written — a crash now loses this undo"
                );
            }
        }
    }

    /// Record `undo` before the step it reverses. The seq keys [`apply`] and [`done`].
    pub(crate) fn push(backend: &'static str, undo: Undo) -> u64 {
        with(|j| j.push(backend, undo))
    }

    /// The teardown that owns `seq` did the undo itself.
    pub(crate) fn done(seq: u64) {
        with(|j| j.settle(&[seq]))
    }

    /// Run the entries of `backend` that `seqs` name, newest first, with the journal unlocked:
    /// an undo and its read-back take seconds, and another session's [`push`] must not wait.
    /// `Ok` removes an entry; `Err` keeps it for the next pass. A seq that is gone, or goes
    /// while an earlier one runs, is nothing to do. An entry naming a head that is not listed
    /// (unplugged, or a monitor switched off) stays, and [`watch_owed`] runs it again on return.
    pub(crate) fn apply(seqs: &[u64], backend: &mut dyn VirtualDisplay) {
        if seqs.is_empty() {
            return;
        }
        let name = backend.name();
        let picked: Vec<Entry> = with(|j| {
            let mine = |e: &&Entry| seqs.contains(&e.seq) && e.backend == name;
            j.doc.entries.iter().filter(mine).cloned().collect()
        });
        let mut owed = Vec::new();
        let undone = run(&picked, |e| {
            if !with(|j| j.doc.entries.iter().any(|now| now.seq == e.seq)) {
                return Ok(true);
            }
            backend.undo(&e.undo)?;
            let gone = settled(&*backend, &e.undo, crate::monitors::DISABLE_BUDGET)?;
            if !gone.is_empty() {
                tracing::info!(
                    backend = name, heads = ?gone,
                    "head left while the stream had it off — re-enabled when it is back"
                );
                owed.push(e.seq);
            }
            Ok(gone.is_empty())
        });
        with(|j| j.settle(&undone));
        if !owed.is_empty() {
            watch_owed(name, owed);
        }
    }

    /// Look for the heads of the `owed` entries every [`OWED_POLL`] until each is settled or
    /// gone. Umbriel and niri keep a runtime disable for the monitor and Hyprland a rule for its
    /// name, so a returning head comes back still disabled. After [`OWED_TRIES`] failed looks
    /// an entry is left in the journal for the next host.
    fn watch_owed(backend: &'static str, owed: Vec<u64>) {
        let mut owed: Vec<(u64, u32)> = owed.into_iter().map(|seq| (seq, 0)).collect();
        let spawned = std::thread::Builder::new()
            .name("topology-owed".into())
            .spawn(move || {
                while !owed.is_empty() {
                    std::thread::sleep(OWED_POLL);
                    let Some(mut b) = crate::backend_by_id(backend) else {
                        continue;
                    };
                    owed.retain_mut(|(seq, failed)| {
                        match retry_owed(*seq, &mut *b, crate::monitors::DISABLE_BUDGET) {
                            Ok(done) => !done,
                            Err(e) => {
                                *failed += 1;
                                tracing::warn!(
                                    backend, seq = *seq, tries = *failed,
                                    error = %format!("{e:#}"), "returning head not re-enabled"
                                );
                                *failed < OWED_TRIES
                            }
                        }
                    });
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "owed topology undo not watched — the next host runs it");
        }
    }

    /// One look at owed entry `seq`: `Ok(true)` once it is settled or gone. Waits while a
    /// managed output is listed: that session owns the desk, and its teardown comes first.
    /// Runs the undo only when a head it names is listed dark, so a lit one costs one listing.
    fn retry_owed(seq: u64, backend: &mut dyn VirtualDisplay, budget: Duration) -> Result<bool> {
        let Some(undo) = with(|j| {
            let entry = j.doc.entries.iter().find(|e| e.seq == seq);
            entry.map(|e| e.undo.clone())
        }) else {
            return Ok(true);
        };
        let Undo::EnableHeads { heads } = &undo else {
            return Ok(true);
        };
        let listed = backend.heads().context("list the heads")?;
        if listed.iter().any(|m| m.managed) {
            return Ok(false);
        }
        let dark = |h: &super::Head| listed.iter().any(|m| m.connector == h.name && !m.enabled);
        if heads.iter().any(dark) {
            backend.undo(&undo)?;
        }
        if !settled(&*backend, &undo, budget)?.is_empty() {
            return Ok(false);
        }
        done(seq);
        tracing::info!(
            backend = backend.name(),
            seq,
            "owed topology undo settled — the head is back lit"
        );
        Ok(true)
    }

    /// Test: this process's file becomes one a dead host left, and the next push opens a fresh
    /// one, as a host started after a crash does.
    #[cfg(test)]
    pub(crate) fn crash() {
        let mut slot = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut j) = slot.take() else {
            return;
        };
        let _ = std::fs::remove_file(&j.path);
        j.doc.owner.boot_id = "a boot that is over".to_string();
        j.doc.owner.start = 0;
        j.path = dir().join(j.doc.owner.file_name());
        j.persist();
    }

    /// Take back what dead hosts left: every file in the dir whose owner is gone, newest entry
    /// first, each through the backend `open` builds for its name. No backend (unknown name, or
    /// its compositor is not up), or a head it names that is not listed, keeps the entry, and a
    /// file with nothing undone is not rewritten. Cheap with nothing there: one listing.
    pub(crate) fn reclaim(open: &dyn Fn(&str) -> Option<Box<dyn VirtualDisplay>>) {
        reclaim_in(&dir(), open)
    }

    fn reclaim_in(dir: &Path, open: &dyn Fn(&str) -> Option<Box<dyn VirtualDisplay>>) {
        let Ok(files) = std::fs::read_dir(dir) else {
            return;
        };
        let me = Owner::me();
        for path in files.flatten().map(|f| f.path()) {
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let mut doc = match read_doc(&path) {
                Ok(doc) => doc,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %format!("{e:#}"),
                        "topology journal not read — left alone");
                    continue;
                }
            };
            if doc.owner == me || doc.owner.alive() {
                continue;
            }
            tracing::warn!(
                path = %path.display(), pid = doc.owner.pid, entries = doc.entries.len(),
                "a previous host died with desk changes pending — taking them back"
            );
            let undone = run(&doc.entries, |e| {
                let mut backend = open(&e.backend)
                    .with_context(|| format!("no {} backend to run the undo", e.backend))?;
                backend.undo(&e.undo)?;
                Ok(settled(&*backend, &e.undo, crate::monitors::DISABLE_BUDGET)?.is_empty())
            });
            if undone.is_empty() {
                continue; // nothing to rewrite, and the fsync can stall behind a busy disk
            }
            doc.entries.retain(|e| !undone.contains(&e.seq));
            if let Err(e) = write_doc(&path, &doc) {
                tracing::error!(path = %path.display(), error = %e, "topology journal not rewritten");
            }
        }
    }

    /// Read back what an undo claims: every head an `EnableHeads` names that is listed is lit
    /// within `budget`. The compositor applies asynchronously and may decide otherwise; a head
    /// still dark is the `Err` that keeps the entry. `Ok` names the heads not listed: an empty
    /// list settles the entry.
    fn settled(backend: &dyn VirtualDisplay, undo: &Undo, budget: Duration) -> Result<Vec<String>> {
        let Undo::EnableHeads { heads } = undo else {
            return Ok(Vec::new());
        };
        let deadline = Instant::now() + budget;
        loop {
            let listed = backend.heads().context("list the heads back")?;
            let dark: Vec<&str> = heads
                .iter()
                .filter(|h| listed.iter().any(|m| m.connector == h.name && !m.enabled))
                .map(|h| h.name.as_str())
                .collect();
            if dark.is_empty() {
                let gone = heads
                    .iter()
                    .filter(|h| !listed.iter().any(|m| m.connector == h.name));
                return Ok(gone.map(|h| h.name.clone()).collect());
            }
            if Instant::now() >= deadline {
                bail!("{} still disabled after the undo", dark.join(", "));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Undo `entries` newest first; the seqs whose undo answered `Ok(true)`. `Ok(false)` keeps
    /// its entry quietly; an `Err` keeps it and is logged with the backend's own words. No early
    /// stop: the entries are independent, and the order already puts a head's re-enable before
    /// its output's unplug.
    fn run(entries: &[Entry], mut undo: impl FnMut(&Entry) -> Result<bool>) -> Vec<u64> {
        let mut order: Vec<&Entry> = entries.iter().collect();
        order.sort_by_key(|e| std::cmp::Reverse(e.seq));
        order
            .into_iter()
            .filter(|e| match undo(e) {
                Ok(settled) => settled,
                Err(err) => {
                    tracing::error!(
                        backend = %e.backend, seq = e.seq, error = %format!("{err:#}"),
                        "topology undo did not run — kept for the next pass"
                    );
                    false
                }
            })
            .map(|e| e.seq)
            .collect()
    }

    fn read_doc(path: &Path) -> Result<Doc> {
        let bytes = std::fs::read(path).context("read")?;
        let doc: Doc = serde_json::from_slice(&bytes).context("parse")?;
        if doc.version > VERSION {
            anyhow::bail!("format version {}; this host reads {VERSION}", doc.version);
        }
        Ok(doc)
    }

    /// Empty is no file: a clean exit leaves nothing, and a dead file that is done goes away.
    fn write_doc(path: &Path, doc: &Doc) -> std::io::Result<()> {
        if doc.entries.is_empty() {
            return match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let bytes = serde_json::to_vec_pretty(doc).map_err(std::io::Error::other)?;
        pf_paths::replace_file(path, &bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::fake::{Call, FakeDisplay};
        use crate::topology_journal::Head;

        fn scratch(test: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("pf-journal-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        fn heads(names: &[&str]) -> Undo {
            Undo::EnableHeads {
                heads: names
                    .iter()
                    .map(|n| Head {
                        name: n.to_string(),
                        mode: None,
                    })
                    .collect(),
            }
        }

        fn output(name: &str) -> Undo {
            Undo::RemoveOutput {
                name: name.to_string(),
                instance: "sway-ipc.1000.42.sock".to_string(),
            }
        }

        /// The three steps of one exclusive create, in the order they are journaled.
        fn steps() -> [Undo; 3] {
            [output("HEADLESS-2"), heads(&["DP-1"]), heads(&["HDMI-A-1"])]
        }

        /// A file left by a host that died: what `push` wrote, with its owner gone.
        fn died(j: &mut Journal) {
            j.doc.owner.boot_id = "a boot that is over".to_string();
            j.persist();
        }

        fn files(dir: &Path) -> usize {
            std::fs::read_dir(dir).map_or(0, |d| d.count())
        }

        fn head(name: &str, enabled: bool) -> crate::monitors::PhysicalMonitor {
            crate::monitors::PhysicalMonitor {
                connector: name.into(),
                description: name.into(),
                width: 1920,
                height: 1080,
                refresh_mhz: 60_000,
                x: 0,
                y: 0,
                scale: 1.0,
                primary: false,
                enabled,
                managed: name.starts_with("PF-"),
            }
        }

        /// A fake desk whose heads [`steps`] names are plugged in and lit.
        fn lit_desk(name: &'static str) -> FakeDisplay {
            let fake = FakeDisplay::new(name);
            fake.script(|s| s.heads = vec![head("DP-1", true), head("HDMI-A-1", true)]);
            fake
        }

        /// Crash table: after k of the three steps, reclaim takes back exactly those k, newest
        /// first, and the file goes.
        #[test]
        fn a_crash_after_any_step_is_taken_back_in_reverse() {
            for k in 0..=3 {
                let dir = scratch(&format!("crash{k}"));
                let mut j = Journal::open(&dir);
                for undo in steps().into_iter().take(k) {
                    j.push("fake", undo);
                }
                died(&mut j);
                let fake = lit_desk("fake");
                reclaim_in(&dir, &|name| (name == "fake").then(|| fake.boxed()));
                let want: Vec<Call> = steps().into_iter().take(k).rev().map(Call::Undo).collect();
                assert_eq!(fake.take_calls(), want, "k = {k}");
                assert_eq!(files(&dir), 0, "k = {k}: a reclaimed file is gone");
                let _ = std::fs::remove_dir_all(&dir);
            }
        }

        /// The owner is alive (this process), so the file is nobody else's to run.
        #[test]
        fn a_live_hosts_journal_is_never_reclaimed() {
            let dir = scratch("live");
            let mut j = Journal::open(&dir);
            j.push("fake", output("HEADLESS-1"));
            let fake = FakeDisplay::new("fake");
            reclaim_in(&dir, &|_| Some(fake.boxed()));
            assert!(fake.take_calls().is_empty());
            assert_eq!(files(&dir), 1);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_newer_journal_is_left_alone_and_a_broken_one_too() {
            let dir = scratch("newer");
            std::fs::create_dir_all(&dir).unwrap();
            let newer = r#"{"version":2,"owner":{"pid":1,"start":0,"boot_id":"x","era":"next"},"entries":[]}"#;
            std::fs::write(dir.join("1-0.json"), newer).unwrap();
            std::fs::write(dir.join("2-0.json"), b"{ not json").unwrap();
            std::fs::write(dir.join("notes.txt"), b"not a journal").unwrap();
            let fake = FakeDisplay::new("fake");
            reclaim_in(&dir, &|_| Some(fake.boxed()));
            assert!(fake.take_calls().is_empty());
            assert_eq!(files(&dir), 3);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// One refused undo keeps its entry and stops nothing else; the next pass finishes.
        #[test]
        fn a_failed_undo_stays_for_the_next_pass_and_the_rest_still_run() {
            let dir = scratch("failed");
            let mut j = Journal::open(&dir);
            for undo in steps() {
                j.push("fake", undo);
            }
            died(&mut j);
            let fake = lit_desk("fake");
            fake.script(|s| s.fail_undo = Some(heads(&["DP-1"])));
            reclaim_in(&dir, &|_| Some(fake.boxed()));
            assert_eq!(
                fake.take_calls(),
                [
                    Call::Undo(heads(&["HDMI-A-1"])),
                    Call::Undo(heads(&["DP-1"])),
                    Call::Undo(output("HEADLESS-2")),
                ]
            );
            let left = read_doc(&dir.join(j.doc.owner.file_name())).expect("the file stays");
            assert_eq!(left.entries.len(), 1);
            assert_eq!(left.entries[0].undo, heads(&["DP-1"]));
            fake.script(|s| s.fail_undo = None);
            reclaim_in(&dir, &|_| Some(fake.boxed()));
            assert_eq!(fake.take_calls(), [Call::Undo(heads(&["DP-1"]))]);
            assert_eq!(files(&dir), 0);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An undo counts only once the heads read back lit: one still listed dark is the
        /// error that keeps the entry, one no longer listed (unplugged) is named as owed.
        #[test]
        fn an_undo_is_settled_when_its_heads_read_back_lit() {
            let fake = FakeDisplay::new("fake");
            let undo = heads(&["DP-1"]);
            let read = |undo: &Undo| settled(&fake, undo, Duration::ZERO);
            fake.script(|s| s.heads = vec![head("DP-1", false)]);
            let err = read(&undo).expect_err("still dark");
            assert_eq!(err.to_string(), "DP-1 still disabled after the undo");
            fake.script(|s| s.heads = vec![head("DP-1", true)]);
            assert!(read(&undo).unwrap().is_empty());
            fake.script(|s| s.heads = Vec::new());
            assert_eq!(read(&undo).unwrap(), ["DP-1"]);
            assert!(read(&output("HEADLESS-2")).unwrap().is_empty());
        }

        /// A dead host's undo of a head that is not listed keeps its entry for the next pass.
        #[test]
        fn a_dead_hosts_unplugged_head_stays_owed() {
            let dir = scratch("unplugged");
            let mut j = Journal::open(&dir);
            j.push("fake", heads(&["DP-1"]));
            died(&mut j);
            let fake = FakeDisplay::new("fake");
            reclaim_in(&dir, &|_| Some(fake.boxed()));
            assert_eq!(fake.take_calls(), [Call::Undo(heads(&["DP-1"]))]);
            assert_eq!(files(&dir), 1);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A backend that cannot be opened (its compositor is not up) keeps every entry, in a
        /// file left unwritten: a rewrite would reformat the trailing whitespace.
        #[test]
        fn no_backend_keeps_the_entries() {
            let dir = scratch("nobackend");
            let mut j = Journal::open(&dir);
            j.push("wlroots", heads(&["DP-1"]));
            died(&mut j);
            let path = dir.join(j.doc.owner.file_name());
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.extend_from_slice(b"\n\n");
            std::fs::write(&path, &bytes).unwrap();
            reclaim_in(&dir, &|_| None);
            assert_eq!(files(&dir), 1);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "nothing undone, nothing written"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// [`crate::fake::in_child`] for this module: [`apply`] runs on the process's journal.
        fn in_child(test: &str, body: fn()) {
            crate::fake::in_child(module_path!(), test, &[], body)
        }

        /// The seqs in this process's journal.
        fn seqs() -> Vec<u64> {
            with(|j| j.doc.entries.iter().map(|e| e.seq).collect())
        }

        /// In-process: a group's seqs run newest first through the live backend; a popped seq
        /// and another backend's entry are left alone; `done` settles an entry by itself.
        #[test]
        fn apply_runs_a_groups_seqs_newest_first_and_only_its_backends() {
            in_child(
                "apply_runs_a_groups_seqs_newest_first_and_only_its_backends",
                || {
                    let a = push("fake", output("HEADLESS-2"));
                    let b = push("fake", heads(&["DP-1"]));
                    let c = push("other", heads(&["DP-2"]));
                    let d = push("fake", heads(&["HDMI-A-1"]));
                    done(a);
                    let fake = lit_desk("fake");
                    apply(&[a, b, c, d], &mut *fake.boxed());
                    assert_eq!(
                        fake.take_calls(),
                        [
                            Call::Undo(heads(&["HDMI-A-1"])),
                            Call::Undo(heads(&["DP-1"]))
                        ]
                    );
                    assert_eq!(seqs(), [c]);
                    assert_eq!(with(|j| j.next), 5, "a popped seq is never minted again");
                },
            );
        }

        /// An undo runs with the journal unlocked: another session's push lands while it
        /// blocks, an entry settled meanwhile is not run, and only the undone entry goes.
        #[test]
        fn a_push_lands_while_an_undo_runs() {
            in_child("a_push_lands_while_an_undo_runs", || {
                let older = push("fake", output("HEADLESS-2"));
                let newer = push("fake", heads(&["DP-1"]));
                let fake = lit_desk("fake");
                let (release, gate) = std::sync::mpsc::channel();
                fake.script(|s| s.undo_gate = Some(gate));
                let mut vd = fake.boxed();
                let undoing = std::thread::spawn(move || apply(&[older, newer], &mut *vd));
                while fake.calls().is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let (pushed_tx, pushed) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let seq = push("other", heads(&["HDMI-A-1"]));
                    done(older);
                    pushed_tx.send(seq)
                });
                let other = pushed
                    .recv_timeout(Duration::from_secs(2))
                    .expect("the push waits on the undo");
                release.send(()).unwrap();
                undoing.join().unwrap();
                assert_eq!(fake.take_calls(), [Call::Undo(heads(&["DP-1"]))]);
                assert_eq!(seqs(), [other]);
            });
        }

        /// A head switched off mid-stream is not listed at the undo, so its entry stays. It is
        /// run again only once the head is back dark with no session on the desk, and settles
        /// once the head reads back lit.
        #[test]
        fn an_unplugged_head_is_owed_until_it_is_back() {
            in_child("an_unplugged_head_is_owed_until_it_is_back", || {
                let seq = push("fake", heads(&["DP-1"]));
                let fake = FakeDisplay::new("fake");
                fake.script(|s| s.heads = vec![head("PF-1", true)]);
                apply(&[seq], &mut *fake.boxed());
                assert_eq!(fake.take_calls(), [Call::Undo(heads(&["DP-1"]))]);
                assert_eq!(seqs(), [seq], "owed until DP-1 is back");

                let mut vd = fake.boxed();
                let mut retry = || retry_owed(seq, &mut *vd, Duration::ZERO);
                fake.script(|s| s.heads = vec![head("PF-1", true), head("DP-1", false)]);
                assert!(!retry().unwrap(), "a session holds the desk");
                fake.script(|s| s.heads = Vec::new());
                assert!(!retry().unwrap(), "still unplugged");
                assert!(fake.take_calls().is_empty());

                fake.script(|s| s.heads = vec![head("DP-1", false)]);
                assert!(
                    retry().is_err(),
                    "back dark, and the fake re-lights nothing"
                );
                assert_eq!(fake.take_calls(), [Call::Undo(heads(&["DP-1"]))]);
                fake.script(|s| s.heads = vec![head("DP-1", true)]);
                assert!(retry().unwrap());
                assert!(fake.take_calls().is_empty(), "a lit head needs no undo");
                assert!(seqs().is_empty());
                assert!(retry().unwrap(), "a settled entry is nothing to do");
            });
        }

        /// The watch [`apply`] starts settles the owed entry on its own once the head is back.
        #[test]
        fn the_owed_watch_settles_a_head_that_came_back() {
            in_child("the_owed_watch_settles_a_head_that_came_back", || {
                let seq = push("fake", heads(&["DP-1"]));
                let fake = FakeDisplay::new("fake");
                fake.serve_by_name();
                apply(&[seq], &mut *fake.boxed());
                assert_eq!(seqs(), [seq]);
                fake.script(|s| s.heads = vec![head("DP-1", true)]);
                let deadline = Instant::now() + OWED_POLL * 3;
                while !seqs().is_empty() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert!(seqs().is_empty(), "the watch settled the entry");
            });
        }
    }
}

/// The wire shape, pinned on every platform: a file a Linux host wrote is read by the next.
#[cfg(test)]
mod shape_tests {
    use super::{Head, Undo};

    #[test]
    fn an_undo_is_tagged_by_kind_and_a_head_without_a_mode_omits_it() {
        let heads = Undo::EnableHeads {
            heads: vec![
                Head {
                    name: "DP-1".into(),
                    mode: None,
                },
                Head {
                    name: "DP-2".into(),
                    mode: Some("2560x1440@144".into()),
                },
            ],
        };
        assert_eq!(
            serde_json::to_value(&heads).unwrap(),
            serde_json::json!({"kind": "enable_heads",
                "heads": [{"name": "DP-1"}, {"name": "DP-2", "mode": "2560x1440@144"}]})
        );
        let output = Undo::RemoveOutput {
            name: "HEADLESS-2".into(),
            instance: "sway-ipc.1000.42.sock".into(),
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains(r#""kind":"remove_output""#), "{json}");
        assert_eq!(serde_json::from_str::<Undo>(&json).unwrap(), output);
    }
}
