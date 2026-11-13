//! How a backend that finishes access units off the session loop tells the loop: the shared
//! [`AuWake`].

use std::sync::{Arc, Mutex};

/// The handle [`crate::Encoder::set_au_wake`] takes.
type Wake = Arc<dyn Fn() + Send + Sync>;

/// The loop's wake for a finished access unit ([`crate::Encoder::set_au_wake`]), shared by
/// clone with the threads that see the completion. Rings nothing until the loop hands one over.
#[derive(Clone, Default)]
pub struct AuWake(Arc<Mutex<Option<Wake>>>);

impl AuWake {
    /// Every clone rings `wake` from now on; `None` silences them.
    pub fn set(&self, wake: Option<Wake>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = wake;
    }

    /// Call the loop's wake, outside the lock.
    pub fn ring(&self) {
        let wake = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A wake that counts its rings.
    fn counting() -> (AuWake, Arc<AtomicU32>) {
        let rings = Arc::new(AtomicU32::new(0));
        let wake = AuWake::default();
        let r = rings.clone();
        wake.set(Some(Arc::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        })));
        (wake, rings)
    }

    /// The wake a backend took before the loop handed one over rings the loop's once it has;
    /// clearing it silences every clone.
    #[test]
    fn a_wake_set_later_reaches_every_clone() {
        let held = AuWake::default();
        held.ring();
        let (wake, rings) = counting();
        let clone = held.clone();
        held.set(wake.0.lock().unwrap().clone());
        clone.ring();
        assert_eq!(rings.load(Ordering::SeqCst), 1);
        held.set(None);
        clone.ring();
        assert_eq!(rings.load(Ordering::SeqCst), 1);
    }
}
