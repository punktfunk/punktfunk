//! The encoder knobs, as one value every backend reads: the host resolves them from its
//! `host.env` into [`EncodeKnobs`] and the driver installs them with [`set`] before it opens a
//! backend. A process that never calls [`set`] (the host's own in-process encoders, the spike,
//! the Linux tests) gets the defaults, and [`dev_override`] lays the process environment over
//! either — the same variables, so a `setx /M` on the box still steers WUDFHost for an A/B.

pub use pf_driver_proto::encode::{truthy, EncodeKnobs};
use std::sync::RwLock;

static REQUESTED: RwLock<Option<EncodeKnobs>> = RwLock::new(None);

/// Install the knobs the request carried. Per process, not per session: the host fills them
/// from one `host.env`, and the last SET_ENCODE wins for the next open.
pub fn set(knobs: EncodeKnobs) {
    *REQUESTED.write().unwrap_or_else(|e| e.into_inner()) = Some(knobs);
}

/// The knobs in force: the requested ones (or the defaults) with the environment laid over.
#[must_use]
pub fn get() -> EncodeKnobs {
    let base = REQUESTED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_default();
    dev_override::apply(base)
}

/// The environment as a knob source. In the host this IS the source (its `host.env` is in
/// the process environment); in WUDFHost it is the machine environment, a documented
/// override for one-box experiments that the request's values otherwise make unnecessary.
pub mod dev_override {
    use super::EncodeKnobs;

    /// `base` with every `PUNKTFUNK_*` knob variable that is set in the environment applied
    /// over it. An unset variable leaves the field; a set one parses as
    /// [`EncodeKnobs::apply_env`] does.
    #[must_use]
    pub fn apply(mut base: EncodeKnobs) -> EncodeKnobs {
        for name in EncodeKnobs::ENV_NAMES {
            if let Ok(v) = std::env::var(name) {
                base.apply_env(name, &v);
            }
        }
        base
    }

    /// The knobs the environment alone describes: what the host puts in the request.
    #[must_use]
    pub fn from_env() -> EncodeKnobs {
        apply(EncodeKnobs::default())
    }
}
