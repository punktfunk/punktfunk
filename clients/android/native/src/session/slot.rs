//! A media plane's place on the session: at most one plane runs, a start into a running slot
//! opens nothing, and a stop takes the plane and drops it. Every plane stops and joins its
//! thread in `Drop`.
//!
//! The drop runs under the slot's lock, so a racing stop returns only once the plane is gone:
//! `nativeClose` relies on that for pad audio. [`Slot`] is the rule without the lock. Every stop
//! steps its generation, so an open that ends after a stop finds itself late. Pinned by the
//! sequence rows below.

use std::sync::Mutex;

use super::lock_recover;

/// The slot rule, with no lock in it.
struct Slot<T> {
    plane: Option<T>,
    /// Steps on every stop.
    generation: u64,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            plane: None,
            generation: 0,
        }
    }
}

impl<T> Slot<T> {
    /// The plane that runs, or the generation an open starts under.
    fn begin(&self) -> Result<&T, u64> {
        self.plane.as_ref().ok_or(self.generation)
    }

    /// An open begun under `generation` ended with `opened`. Returns the plane that runs, `None`
    /// when a stop came since, and the plane to drop: a late one, or one a racing start beat.
    fn finish(&mut self, generation: u64, opened: Option<T>) -> (Option<&T>, Option<T>) {
        if generation != self.generation {
            return (None, opened);
        }
        let spare = if self.plane.is_some() {
            opened
        } else {
            self.plane = opened;
            None
        };
        (self.plane.as_ref(), spare)
    }

    fn stop(&mut self) -> Option<T> {
        self.generation += 1;
        self.plane.take()
    }
}

/// One plane's slot on the session handle.
pub(crate) struct PlaneSlot<T>(Mutex<Slot<T>>);

impl<T> Default for PlaneSlot<T> {
    fn default() -> Self {
        PlaneSlot(Mutex::default())
    }
}

impl<T> PlaneSlot<T> {
    /// Starts a plane if none runs, `open` running under the lock, and reads the plane that runs.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn start<R>(
        &self,
        open: impl FnOnce() -> Option<T>,
        read: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        let mut s = lock_recover(&self.0);
        let generation = match s.begin() {
            Ok(plane) => return Some(read(plane)),
            Err(generation) => generation,
        };
        // Nothing is late or spare: no stop or start can land while the lock is held.
        let (running, _) = s.finish(generation, open());
        running.map(read)
    }

    /// [`PlaneSlot::start`] with `open` outside the lock, for an open slow enough to stall a stop.
    /// A stop during the open wins: the late plane is dropped here, on the starting thread. Of
    /// two racing opens the first to finish runs; both read it.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn start_unlocked<R>(
        &self,
        open: impl FnOnce() -> Option<T>,
        read: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        let generation = match lock_recover(&self.0).begin() {
            Ok(plane) => return Some(read(plane)),
            Err(generation) => generation,
        };
        let opened = open();
        let mut s = lock_recover(&self.0);
        let (running, spare) = s.finish(generation, opened);
        let read = running.map(read);
        drop(s);
        drop(spare);
        read
    }

    /// Stops any plane, then opens the next outside the lock. A plane a racing start put in
    /// meanwhile is stopped. Whether `open` gave a plane.
    pub(crate) fn replace(&self, open: impl FnOnce() -> Option<T>) -> bool {
        self.stop();
        let Some(plane) = open() else {
            return false;
        };
        drop(lock_recover(&self.0).plane.replace(plane));
        true
    }

    /// Takes the plane and drops it under the lock.
    pub(crate) fn stop(&self) {
        let mut s = lock_recover(&self.0);
        drop(s.stop());
    }

    pub(crate) fn running(&self) -> bool {
        lock_recover(&self.0).plane.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaneSlot, Slot};
    use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

    type Log = Arc<Mutex<Vec<String>>>;

    fn note(log: &Log, line: String) {
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    /// A plane that logs its stop.
    struct Fake(u8, Log);

    impl Drop for Fake {
        fn drop(&mut self) {
            note(&self.1, format!("stop {}", self.0));
        }
    }

    #[derive(Clone, Copy)]
    enum Op {
        /// A start whose open gives plane `.0`, or fails when `.1` is false.
        Start(u8, bool),
        Replace(u8, bool),
        Stop,
    }

    /// Runs `ops` on one slot: each open, each stop, and what each call returned.
    fn run(ops: &[Op]) -> Vec<String> {
        let log = Log::default();
        let slot = PlaneSlot::default();
        let open = |id: u8, ok: bool| {
            let log = log.clone();
            move || {
                note(&log, format!("open {id}"));
                ok.then(|| Fake(id, log.clone()))
            }
        };
        for &op in ops {
            match op {
                Op::Start(id, ok) => {
                    let read = slot.start(open(id, ok), |p: &Fake| p.0);
                    note(&log, format!("start {read:?}"));
                }
                Op::Replace(id, ok) => {
                    let opened = slot.replace(open(id, ok));
                    note(&log, format!("replace {opened}"));
                }
                Op::Stop => slot.stop(),
            }
        }
        note(&log, format!("running {}", slot.running()));
        log.lock().unwrap().clone()
    }

    #[test]
    fn a_slot_runs_one_plane_and_a_stop_drops_it_once() {
        use Op::{Replace, Start, Stop};
        let rows: [(&str, &[Op], &[&str]); 5] = [
            (
                "a start into a running slot opens nothing",
                &[Start(1, true), Start(2, true)],
                &["open 1", "start Some(1)", "start Some(1)", "running true"],
            ),
            (
                "a failed open leaves the slot empty for the next start",
                &[Start(1, false), Start(2, true)],
                &[
                    "open 1",
                    "start None",
                    "open 2",
                    "start Some(2)",
                    "running true",
                ],
            ),
            (
                "a second stop finds nothing",
                &[Start(1, true), Stop, Stop],
                &["open 1", "start Some(1)", "stop 1", "running false"],
            ),
            (
                "a replace stops the running plane before it opens the next",
                &[Replace(1, true), Replace(2, true)],
                &[
                    "open 1",
                    "replace true",
                    "stop 1",
                    "open 2",
                    "replace true",
                    "running true",
                ],
            ),
            (
                "a replace whose open fails leaves the slot empty",
                &[Replace(1, true), Replace(2, false)],
                &[
                    "open 1",
                    "replace true",
                    "stop 1",
                    "open 2",
                    "replace false",
                    "running false",
                ],
            ),
        ];
        for (name, ops, want) in rows {
            assert_eq!(run(ops), want, "{name}");
        }
    }

    /// The generation the slot's starts hold to: an open that ends after a stop is late, and of
    /// two opens under one generation the first to finish runs.
    #[test]
    fn an_open_that_ends_after_a_stop_is_late() {
        let mut s = Slot::default();
        let Err(g) = s.begin() else { panic!() };
        s.stop();
        assert_eq!(s.finish(g, Some(1)), (None, Some(1)));
        let Err(g) = s.begin() else { panic!() };
        assert_eq!(s.finish(g, Some(2)), (Some(&2), None));
        assert_eq!(s.begin(), Ok(&2));
        s.stop();
        let Err(g) = s.begin() else { panic!() };
        assert_eq!(s.finish(g, Some(3)), (Some(&3), None));
        assert_eq!(s.finish(g, Some(4)), (Some(&3), Some(4)));
    }

    /// A plane whose drop signals `entered`, then waits for `release`.
    struct Joining(SyncSender<()>, Receiver<()>);

    impl Drop for Joining {
        fn drop(&mut self) {
            let _ = self.0.send(());
            let _ = self.1.recv();
        }
    }

    /// A stop racing another returns only once the first one's join is done (pad audio's fd).
    #[test]
    fn a_racing_stop_waits_for_the_join() {
        let (entered, joining) = sync_channel(1);
        let (release, held) = channel();
        let slot = Arc::new(PlaneSlot::default());
        slot.start(|| Some(Joining(entered, held)), |_| ());
        let first = thread::spawn({
            let slot = slot.clone();
            move || slot.stop()
        });
        joining.recv().unwrap();
        let (done, second_done) = channel();
        let second = thread::spawn({
            let slot = slot.clone();
            move || {
                slot.stop();
                done.send(()).unwrap();
            }
        });
        assert!(
            second_done.recv_timeout(Duration::from_millis(50)).is_err(),
            "the second stop returned while the first was still joining"
        );
        release.send(()).unwrap();
        second_done.recv().unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert!(!slot.running());
    }

    /// An open that signals it began, then waits for its release before it gives plane `id`.
    fn held_open(
        id: u8,
        log: &Log,
    ) -> (
        impl FnOnce() -> Option<Fake> + Send + 'static,
        Receiver<()>,
        Sender<()>,
    ) {
        let (began, begun) = channel();
        let (release, held) = channel();
        let log = log.clone();
        let open = move || {
            began.send(()).unwrap();
            held.recv().unwrap();
            Some(Fake(id, log))
        };
        (open, begun, release)
    }

    /// `f` on another thread; fails when it has not returned within a second.
    fn within<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        let (done, result) = channel();
        thread::spawn(move || done.send(f()).unwrap());
        result
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked behind an open in flight")
    }

    /// Starts an unlocked open of plane `id` on its own thread and returns once it is opening.
    fn opening(
        slot: &Arc<PlaneSlot<Fake>>,
        id: u8,
        log: &Log,
    ) -> (thread::JoinHandle<Option<u8>>, Sender<()>) {
        let (open, begun, release) = held_open(id, log);
        let slot = slot.clone();
        let start = thread::spawn(move || slot.start_unlocked(open, |p| p.0));
        begun
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked behind an open in flight");
        (start, release)
    }

    /// R6-1: a stop during a slow open returns at once, and so does the running query. The
    /// late plane is dropped on the starting thread and the slot stays empty.
    #[test]
    fn a_stop_during_an_open_wins_without_waiting_for_it() {
        let log = Log::default();
        let slot = Arc::new(PlaneSlot::default());
        let (start, release) = opening(&slot, 1, &log);
        within({
            let slot = slot.clone();
            move || slot.stop()
        });
        assert!(!within({
            let slot = slot.clone();
            move || slot.running()
        }));
        release.send(()).unwrap();
        assert_eq!(start.join().unwrap(), None);
        assert!(!slot.running());
        assert_eq!(*log.lock().unwrap(), ["stop 1"]);
    }

    /// Two starts racing leave one plane: the first open to finish runs, and both read it.
    #[test]
    fn racing_starts_leave_one_plane() {
        let log = Log::default();
        let slot = Arc::new(PlaneSlot::default());
        let (first, release_first) = opening(&slot, 1, &log);
        let (second, release_second) = opening(&slot, 2, &log);
        release_second.send(()).unwrap();
        assert_eq!(second.join().unwrap(), Some(2));
        release_first.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Some(2));
        assert_eq!(*log.lock().unwrap(), ["stop 1"]);
        assert!(slot.running());
    }

    /// A start after the stop runs; the open the stop cut off ends late and is dropped.
    #[test]
    fn a_start_after_a_stop_beats_the_open_it_cut_off() {
        let log = Log::default();
        let slot = Arc::new(PlaneSlot::default());
        let (first, release) = opening(&slot, 1, &log);
        within({
            let slot = slot.clone();
            move || slot.stop()
        });
        let second = within({
            let (slot, log) = (slot.clone(), log.clone());
            move || slot.start_unlocked(|| Some(Fake(2, log)), |p| p.0)
        });
        assert_eq!(second, Some(2));
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), None);
        assert_eq!(*log.lock().unwrap(), ["stop 1"]);
        assert!(slot.running());
    }
}
