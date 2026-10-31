//! Display-topology seam: the calls the isolate ladder makes into `pf-win-display`.
//!
//! [`CcdOps`] is how [`super::isolate_txn`], the exclusive watch's survivor read, its
//! re-assert and a seat's HDR write reach the OS. [`WinCcd`] is the one production implementor
//! and passes each call straight on. The trait remains so tests can script what an isolate
//! observed.
//!
//! The manager's other `pf_win_display` calls are direct.

use std::sync::Arc;

use pf_win_display::snapshot::DisplaySnapshot;
use pf_win_display::win_display::{CcdTargetKey, IsolateOutcome, SavedConfig};

/// `Send + Sync` because the manager is a `&'static` singleton the watch thread reads.
pub(crate) trait CcdOps: Send + Sync {
    /// [`pf_win_display::win_display::isolate_displays_ccd_checked`].
    fn isolate_displays_ccd_checked(
        &self,
        keep: &[CcdTargetKey],
        store: bool,
    ) -> Option<(SavedConfig, IsolateOutcome)>;
    /// [`pf_win_display::display_events::snapshot_or_query`].
    fn snapshot_or_query(&self) -> Arc<DisplaySnapshot>;
    /// [`pf_win_display::monitor_devnode::disable_connected_inactive`].
    fn disable_connected_inactive(
        &self,
        keep: &[CcdTargetKey],
        baseline_active: &[CcdTargetKey],
        generation: u64,
    ) -> Vec<String>;
    /// [`pf_win_display::win_display::set_advanced_color`].
    fn set_advanced_color(&self, key: CcdTargetKey, enable: bool) -> bool;
}

/// The OS, through `pf-win-display`.
pub(crate) struct WinCcd;

impl CcdOps for WinCcd {
    fn isolate_displays_ccd_checked(
        &self,
        keep: &[CcdTargetKey],
        store: bool,
    ) -> Option<(SavedConfig, IsolateOutcome)> {
        pf_win_display::win_display::isolate_displays_ccd_checked(keep, store)
    }

    fn snapshot_or_query(&self) -> Arc<DisplaySnapshot> {
        pf_win_display::display_events::snapshot_or_query()
    }

    fn disable_connected_inactive(
        &self,
        keep: &[CcdTargetKey],
        baseline_active: &[CcdTargetKey],
        generation: u64,
    ) -> Vec<String> {
        pf_win_display::monitor_devnode::disable_connected_inactive(
            keep,
            baseline_active,
            generation,
        )
    }

    fn set_advanced_color(&self, key: CcdTargetKey, enable: bool) -> bool {
        pf_win_display::win_display::set_advanced_color(key, enable)
    }
}

/// A [`CcdOps`] that plays a [`Script`], logs each [`CcdCall`] and can hold an isolate open,
/// for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::VecDeque;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use pf_win_display::snapshot::DisplaySnapshot;
    use pf_win_display::win_display::{CcdTargetKey, IsolateOutcome, SavedConfig};

    use super::CcdOps;

    /// One call a [`FakeCcd`] took, with its arguments.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum CcdCall {
        Isolate {
            keep: Vec<CcdTargetKey>,
            store: bool,
        },
        Snapshot,
        DisableConnectedInactive {
            keep: Vec<CcdTargetKey>,
            baseline_active: Vec<CcdTargetKey>,
            generation: u64,
        },
        SetAdvancedColor {
            key: CcdTargetKey,
            enable: bool,
        },
    }

    /// What a [`FakeCcd`] answers. [`FakeCcd::script`] edits it between calls.
    pub(crate) struct Script {
        /// What each isolate observes, front first. `None`, or an empty queue: the first
        /// read failed and nothing was attempted.
        pub(crate) isolates: VecDeque<Option<IsolateOutcome>>,
        /// `snapshot_or_query`.
        pub(crate) snapshot: Arc<DisplaySnapshot>,
        /// Instance ids `disable_connected_inactive` reports disabled.
        pub(crate) disabled: Vec<String>,
    }

    struct State {
        script: Script,
        calls: Vec<CcdCall>,
        /// The next isolate's two channels: one says it started, one lets it return.
        hold: Option<(Sender<()>, Receiver<()>)>,
    }

    /// Clones share one script and one call log, so a test keeps a handle on the fake it
    /// boxed into a manager.
    #[derive(Clone)]
    pub(crate) struct FakeCcd(Arc<Mutex<State>>);

    /// The test's end of a held isolate.
    pub(crate) struct Hold {
        /// Fires once the isolate has started.
        pub(crate) entered: Receiver<()>,
        /// Lets it return.
        pub(crate) release: Sender<()>,
    }

    impl FakeCcd {
        /// No isolate scripted, an empty unpublished snapshot, nothing to disable.
        pub(crate) fn new() -> FakeCcd {
            let script = Script {
                isolates: VecDeque::new(),
                snapshot: Arc::new(DisplaySnapshot::empty(Instant::now())),
                disabled: Vec::new(),
            };
            let calls = Vec::new();
            FakeCcd(Arc::new(Mutex::new(State {
                script,
                calls,
                hold: None,
            })))
        }

        /// The next isolate blocks, outside the fake's lock, until the test releases it.
        pub(crate) fn hold_next_isolate(&self) -> Hold {
            let (started, entered) = mpsc::channel();
            let (release, released) = mpsc::channel();
            self.0.lock().unwrap().hold = Some((started, released));
            Hold { entered, release }
        }

        pub(crate) fn script(&self, edit: impl FnOnce(&mut Script)) {
            edit(&mut self.0.lock().unwrap().script);
        }

        /// The calls so far, emptied: each step of a test asserts only its own.
        pub(crate) fn take_calls(&self) -> Vec<CcdCall> {
            std::mem::take(&mut self.0.lock().unwrap().calls)
        }

        fn took<T>(&self, call: CcdCall, answer: impl FnOnce(&mut Script) -> T) -> T {
            let mut state = self.0.lock().unwrap();
            state.calls.push(call);
            answer(&mut state.script)
        }
    }

    impl CcdOps for FakeCcd {
        fn isolate_displays_ccd_checked(
            &self,
            keep: &[CcdTargetKey],
            store: bool,
        ) -> Option<(SavedConfig, IsolateOutcome)> {
            let keep = keep.to_vec();
            let observed = self.took(CcdCall::Isolate { keep, store }, |s| {
                s.isolates.pop_front().flatten()
            });
            let hold = self.0.lock().unwrap().hold.take();
            if let Some((started, released)) = hold {
                let _ = started.send(());
                let _ = released.recv();
            }
            let observed = observed?;
            // An empty pre-isolate config stands for the restore snapshot.
            Some(((Vec::new(), Vec::new()), observed))
        }

        fn snapshot_or_query(&self) -> Arc<DisplaySnapshot> {
            self.took(CcdCall::Snapshot, |s| s.snapshot.clone())
        }

        fn disable_connected_inactive(
            &self,
            keep: &[CcdTargetKey],
            baseline_active: &[CcdTargetKey],
            generation: u64,
        ) -> Vec<String> {
            let call = CcdCall::DisableConnectedInactive {
                keep: keep.to_vec(),
                baseline_active: baseline_active.to_vec(),
                generation,
            };
            self.took(call, |s| s.disabled.clone())
        }

        fn set_advanced_color(&self, key: CcdTargetKey, enable: bool) -> bool {
            self.took(CcdCall::SetAdvancedColor { key, enable }, |_| true)
        }
    }
}
