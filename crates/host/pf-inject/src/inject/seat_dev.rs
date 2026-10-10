//! What a sandboxed seat may open: one symlink per node of the pads the host made for it.
//!
//! `pf-vdisplay` runs a seat's nested Steam with a directory of ours bound in as its
//! `/dev/input`, and `/dev/hidrawN` symlinked into the seat's `hidraw/`. Both resolve through
//! `hostdev/`, where that sandbox — and only that sandbox — has the real `/dev`. So exposing a
//! pad is writing one link, with no privilege and nothing to enter.
//!
//! Steam never looks at a node again once it failed to resolve, so the link has to exist BEFORE
//! the kernel announces the device: [`SeatDev::create`] pre-links a window of free numbers, makes
//! the pad, then drops the links it did not take. One [`create_lock`] serialises that host-wide,
//! because what the pad took is what appeared while it was being made.
//!
//! A visibility filter, not a trust boundary. Evidence:
//! `design/steam-seats-warm-launch-implementation-plan.md` WP-S3.

use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A node family and how many numbers are pre-linked for one pad. A Sony pad brings a touchpad
/// and a motion node beside its buttons, so `event` needs the widest window; too narrow and one
/// of a pad's nodes is born unreachable.
struct Kind {
    /// Directory under the host's `/dev`; empty for `/dev/hidrawN`.
    dir: &'static str,
    stem: &'static str,
    window: usize,
}

/// `/dev/hidraw*` and `/dev/input/{event,js}*` — everything a pad shows up as.
const KINDS: [Kind; 3] = [
    Kind {
        dir: "",
        stem: "hidraw",
        window: 4,
    },
    Kind {
        dir: "input",
        stem: "event",
        window: 8,
    },
    Kind {
        dir: "input",
        stem: "js",
        window: 4,
    },
];

/// The sandbox carries `/dev/hidraw0`..`hidraw63`, so a number past that cannot be reached from
/// inside it however the host links it.
const HIDRAW_LIMIT: u32 = 64;

/// How long a pad's nodes may take to appear. UHID registers inside the create; USB/IP has to
/// enumerate the device first.
const SETTLE: Duration = Duration::from_millis(750);

/// Nothing new for this long ends the wait. A pad's nodes are announced together, so the cost is
/// this once per controller plugged in, not per frame.
const SETTLE_QUIET: Duration = Duration::from_millis(60);

const SETTLE_POLL: Duration = Duration::from_millis(10);

/// Held across every pad create, filtered or not: a filtered seat learns which nodes its pad
/// took from what appeared while it was being made, and an unserialised create elsewhere would
/// land in that answer. Uncontended on a host with no seat — there is nothing to learn.
pub fn create_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// One pre-placed link and the node it stands for.
struct Placed {
    /// What the kernel creates, e.g. `/dev/input/event7`.
    node: PathBuf,
    /// The seat's link, e.g. `<S>/input/event7`.
    link: PathBuf,
    name: String,
}

/// The links one pad holds. Dropping it takes that pad out of its seat's view, which is what a
/// disconnect and the end of a session both are.
pub struct PadLinks {
    links: Vec<PathBuf>,
}

impl Drop for PadLinks {
    fn drop(&mut self) {
        for link in &self.links {
            let _ = std::fs::remove_file(link);
        }
    }
}

/// A seat's device directory (`pf_paths::gamescope_seat_dev_dir`).
pub struct SeatDev {
    root: PathBuf,
    /// The host's `/dev`. A fixture in tests.
    dev: PathBuf,
}

impl SeatDev {
    pub fn new(root: PathBuf) -> SeatDev {
        SeatDev {
            root,
            dev: PathBuf::from("/dev"),
        }
    }

    /// Make a pad whose nodes this seat can open, and keep only the links it took.
    ///
    /// The caller holds [`create_lock`]. A create that fails leaves nothing behind: the window
    /// drops with the guard.
    pub fn create<P>(&self, open: impl FnOnce() -> Result<P>) -> Result<(P, PadLinks)> {
        let placed = self.place();
        // Kept so a failed `open` still removes what it pre-linked.
        let mut links = PadLinks {
            links: placed.iter().map(|p| p.link.clone()).collect(),
        };
        let pad = open()?;
        let took = self.settle(&placed);
        if took.is_empty() {
            tracing::warn!(seat_dev = %self.root.display(),
                "controller nodes did not appear in time, so this seat keeps links for every \
                 number the pad could have taken until it goes");
            return Ok((pad, links));
        }
        links.links.retain(|l| took.iter().any(|p| &p.link == l));
        for p in placed
            .iter()
            .filter(|p| !took.iter().any(|t| t.link == p.link))
        {
            let _ = std::fs::remove_file(&p.link);
        }
        tracing::info!(
            seat_dev = %self.root.display(),
            nodes = %took.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(","),
            "controller exposed to its seat"
        );
        Ok((pad, links))
    }

    /// Pre-link the next free numbers of every family, and take those numbers off any other seat
    /// that pre-linked them and never pruned — one directory at a time may hold a number, or the
    /// pad about to take it would be born inside somebody else's seat.
    fn place(&self) -> Vec<Placed> {
        let mut out = Vec::new();
        let siblings = self.siblings();
        for kind in &KINDS {
            let dir = self.root.join(link_dir(kind));
            if let Err(e) = pf_paths::create_private_dir(&dir) {
                tracing::warn!(dir = %dir.display(), error = %e,
                    "seat device directory not created — this seat sees no controller");
                continue;
            }
            for n in free_numbers(&present(&self.dev, kind), kind) {
                let name = format!("{}{n}", kind.stem);
                let link = dir.join(&name);
                for sib in &siblings {
                    let _ = std::fs::remove_file(sib.join(link_dir(kind)).join(&name));
                }
                let target = self.root.join("hostdev").join(kind.dir).join(&name);
                let _ = std::fs::remove_file(&link);
                if let Err(e) = symlink(&target, &link) {
                    tracing::warn!(link = %link.display(), error = %e,
                        "controller link not written — this seat may not see the pad");
                    continue;
                }
                out.push(Placed {
                    node: self.dev.join(kind.dir).join(&name),
                    link,
                    name,
                });
            }
        }
        out
    }

    /// The other seats on this box, by the name we gave them
    /// ([`pf_paths::is_gamescope_seat_dev_dir`]). [`place`](Self::place) deletes inside these,
    /// so a directory another program left in the runtime dir is never one of them, however it
    /// is furnished.
    fn siblings(&self) -> Vec<PathBuf> {
        let Some(parent) = self.root.parent() else {
            return Vec::new();
        };
        std::fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|d| *d != self.root && pf_paths::is_gamescope_seat_dev_dir(d))
            .collect()
    }

    /// Which of the pre-linked numbers the pad actually took. Empty means it said nothing in
    /// time, and the caller keeps the whole window rather than hide the pad.
    fn settle<'a>(&self, placed: &'a [Placed]) -> Vec<&'a Placed> {
        let deadline = Instant::now() + SETTLE;
        let mut took: Vec<&Placed> = Vec::new();
        let mut quiet_since: Option<Instant> = None;
        loop {
            let now: Vec<&Placed> = placed.iter().filter(|p| p.node.exists()).collect();
            if now.len() != took.len() {
                took = now;
                quiet_since = Some(Instant::now());
            }
            let settled = quiet_since.is_some_and(|q| q.elapsed() >= SETTLE_QUIET);
            if settled || Instant::now() >= deadline {
                return took;
            }
            std::thread::sleep(SETTLE_POLL);
        }
    }
}

/// Where a family's links live under the seat: `hidraw/`, or `input/` for what the sandbox binds
/// in as `/dev/input`.
fn link_dir(kind: &Kind) -> &'static str {
    if kind.dir.is_empty() {
        "hidraw"
    } else {
        kind.dir
    }
}

/// Numbers of this family the host has now.
fn present(dev: &Path, kind: &Kind) -> BTreeSet<u32> {
    std::fs::read_dir(dev.join(kind.dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix(kind.stem)?
                .parse::<u32>()
                .ok()
        })
        .collect()
}

/// The window: the lowest free numbers, which is the order the kernel hands them out in.
/// `hidraw` stops at what the sandbox carries links for.
fn free_numbers(present: &BTreeSet<u32>, kind: &Kind) -> Vec<u32> {
    let limit = if kind.dir.is_empty() {
        HIDRAW_LIMIT
    } else {
        u32::MAX
    };
    (0..limit)
        .filter(|n| !present.contains(n))
        .take(kind.window)
        .collect()
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows hosts have no seat directory, so nothing ever asks for a link here.
#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("seat device links are POSIX only"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Fixture {
        dev: PathBuf,
        seats: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let base =
                std::env::temp_dir().join(format!("pf-seat-dev-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let b = Fixture {
                dev: base.join("dev"),
                seats: base.join("run"),
            };
            std::fs::create_dir_all(b.dev.join("input")).unwrap();
            std::fs::create_dir_all(&b.seats).unwrap();
            b
        }

        /// A seat root named as the host names one, so the sibling rule sees it.
        fn root(&self, id: &str) -> PathBuf {
            self.seats.join(format!("punktfunk-gamescope-{id}-dev"))
        }

        fn seat(&self, id: &str) -> SeatDev {
            SeatDev {
                root: self.root(id),
                dev: self.dev.clone(),
            }
        }

        /// What a pad's create does to the host: the kernel's nodes appear.
        fn plug(&self, names: &[&str]) {
            for name in names {
                let dir = if name.starts_with("hidraw") {
                    ""
                } else {
                    "input"
                };
                std::fs::write(self.dev.join(dir).join(name), b"").unwrap();
            }
        }

        fn links(&self, id: &str) -> Vec<String> {
            let root = self.root(id);
            let mut out: Vec<String> = ["hidraw", "input"]
                .iter()
                .flat_map(|sub| {
                    std::fs::read_dir(root.join(sub))
                        .into_iter()
                        .flatten()
                        .flatten()
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            out.sort();
            out
        }
    }

    /// The pad's own nodes stay linked; everything the window covered on spec goes.
    #[test]
    fn a_pad_keeps_the_links_it_took_and_loses_the_rest() {
        let b = Fixture::new("keep");
        let seat = b.seat("cafe0123");
        let (_pad, links) = seat
            .create(|| {
                b.plug(&["hidraw0", "event0", "event1", "js0"]);
                Ok(())
            })
            .unwrap();
        assert_eq!(b.links("cafe0123"), ["event0", "event1", "hidraw0", "js0"]);
        // Every link resolves through the directory only the sandbox has a real `/dev` on.
        let target = std::fs::read_link(seat.root.join("hidraw/hidraw0")).unwrap();
        assert!(target.ends_with("hostdev/hidraw0"), "{}", target.display());
        let target = std::fs::read_link(seat.root.join("input/event1")).unwrap();
        assert!(
            target.ends_with("hostdev/input/event1"),
            "{}",
            target.display()
        );
        drop(links);
        assert!(
            b.links("cafe0123").is_empty(),
            "the pad went, its links went"
        );
    }

    /// The links are in place before the create returns — Steam never retries a node that was
    /// missing when it looked.
    #[test]
    fn the_window_is_linked_before_the_pad_exists() {
        let b = Fixture::new("order");
        let seat = b.seat("cafe0123");
        let (seen, _links) = seat
            .create(|| {
                let seen = b.links("cafe0123");
                b.plug(&["hidraw0", "event0"]);
                Ok(seen)
            })
            .unwrap();
        assert_eq!(seen.len(), 16, "4 hidraw + 8 event + 4 js: {seen:?}");
        assert!(seen.contains(&"hidraw0".to_string()));
        assert!(seen.contains(&"event7".to_string()));
    }

    /// A create that fails leaves no link behind, so the next pad's numbers are free.
    #[test]
    fn a_failed_create_takes_its_window_with_it() {
        let b = Fixture::new("fail");
        let seat = b.seat("cafe0123");
        assert!(seat
            .create(|| anyhow::bail!("no /dev/uhid") as Result<()>)
            .is_err());
        assert!(b.links("cafe0123").is_empty());
    }

    /// The failure this work package exists to stop: one seat's Steam opening another's pad.
    #[test]
    fn a_seat_never_holds_a_link_for_a_pad_another_seat_created() {
        let b = Fixture::new("cross");
        let (a, c) = (b.seat("aaaa1111"), b.seat("bbbb2222"));
        let (_pad_a, _links_a) = a
            .create(|| {
                b.plug(&["hidraw0", "event0"]);
                Ok(())
            })
            .unwrap();
        let (_pad_b, _links_b) = c
            .create(|| {
                b.plug(&["hidraw1", "event1"]);
                Ok(())
            })
            .unwrap();
        assert_eq!(b.links("aaaa1111"), ["event0", "hidraw0"]);
        assert_eq!(b.links("bbbb2222"), ["event1", "hidraw1"]);
    }

    /// A number is taken back only from a seat. Another program's runtime directory is not one
    /// however it is furnished, and a create must not delete inside it.
    #[test]
    fn a_foreign_directory_that_holds_hidraw_is_left_alone() {
        let b = Fixture::new("foreign");
        let foreign = b.seats.join("some-app-runtime");
        std::fs::create_dir_all(foreign.join("hidraw")).unwrap();
        std::fs::write(foreign.join("hidraw/hidraw0"), b"keep me").unwrap();
        let (_pad, _links) = b
            .seat("cafe0123")
            .create(|| {
                b.plug(&["hidraw0", "event0"]);
                Ok(())
            })
            .unwrap();
        assert!(
            foreign.join("hidraw/hidraw0").exists(),
            "a directory we did not name is not another seat"
        );
    }

    /// A seat that could not learn its nodes keeps the whole window — and the next pad's number
    /// is still taken off it, so that window never becomes another seat's pad.
    #[test]
    fn a_kept_window_is_taken_back_by_the_seat_that_gets_the_number() {
        let b = Fixture::new("stale");
        let (a, c) = (b.seat("aaaa1111"), b.seat("bbbb2222"));
        // Nothing appears: `a` holds links for hidraw0..3 and event0..7.
        let (_pad_a, _links_a) = a.create(|| Ok(())).unwrap();
        assert_eq!(b.links("aaaa1111").len(), 16);
        let (_pad_b, _links_b) = c
            .create(|| {
                b.plug(&["hidraw0", "event0"]);
                Ok(())
            })
            .unwrap();
        assert_eq!(b.links("bbbb2222"), ["event0", "hidraw0"]);
        assert!(
            !b.links("aaaa1111").contains(&"hidraw0".to_string()),
            "{:?}",
            b.links("aaaa1111")
        );
    }

    /// The window starts at the lowest number the kernel would hand out, and never reaches past
    /// what the sandbox carries a link for.
    #[test]
    fn the_window_is_the_lowest_free_numbers() {
        let taken: BTreeSet<u32> = [0, 1, 3].into_iter().collect();
        assert_eq!(free_numbers(&taken, &KINDS[0]), [2, 4, 5, 6]);
        assert_eq!(free_numbers(&taken, &KINDS[1]), [2, 4, 5, 6, 7, 8, 9, 10]);
        let full: BTreeSet<u32> = (0..64).collect();
        assert!(
            free_numbers(&full, &KINDS[0]).is_empty(),
            "the sandbox carries 64 hidraw links and no more"
        );
        assert_eq!(free_numbers(&full, &KINDS[2]), [64, 65, 66, 67]);
    }
}
