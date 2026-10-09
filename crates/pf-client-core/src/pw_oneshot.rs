//! A short-lived PipeWire connection for one registry query on the calling thread: connect,
//! run sync [`rounds`](OneShot::round), drop. All rounds share one deadline, and a core error
//! ends the query, so a sick-but-connected daemon never wedges the caller.
//!
//! Twin of pf-audio's `linux/pw_oneshot.rs` minus its metadata read: clients never link host
//! crates. A fix to one belongs in both.

use anyhow::{anyhow, bail, Context, Result};
use pipewire as pw;
use pw::spa::utils::result::AsyncSeq;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Budget for a whole query. The pad-audio thread runs these, up to two at session end (one in
/// flight, then `restore_profile`), and the session's terminal event waits on its join.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct OneShot {
    // Declared first so it drops first: the hook unregisters while the core is alive.
    _core_listener: pw::core::Listener,
    pub(crate) registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    mainloop: pw::main_loop::MainLoopRc,
    label: &'static str,
    deadline: Instant,
    awaited: Rc<Cell<Option<AsyncSeq>>>,
    failed: Rc<RefCell<Option<anyhow::Error>>>,
}

impl OneShot {
    /// `timeout` bounds every [`round`](Self::round) together, not each one.
    pub(crate) fn connect(label: &'static str, timeout: Duration) -> Result<OneShot> {
        pw::init();
        let mainloop =
            pw::main_loop::MainLoopRc::new(None).with_context(|| format!("{label} MainLoop"))?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .with_context(|| format!("{label} Context"))?;
        let core = context
            .connect_rc(None)
            .with_context(|| format!("{label} connect (is PipeWire running in this session?)"))?;
        let registry = core
            .get_registry_rc()
            .with_context(|| format!("{label} registry"))?;
        let awaited: Rc<Cell<Option<AsyncSeq>>> = Rc::default();
        let failed: Rc<RefCell<Option<anyhow::Error>>> = Rc::default();
        let core_listener = core
            .add_listener_local()
            .done({
                let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE && awaited.get() == Some(seq) {
                        mainloop.quit();
                    }
                }
            })
            .error({
                let (mainloop, failed) = (mainloop.clone(), failed.clone());
                move |id, _seq, res, message| {
                    failed.borrow_mut().get_or_insert_with(|| {
                        anyhow!("pipewire core error id={id} res={res}: {message}")
                    });
                    mainloop.quit();
                }
            })
            .register();
        Ok(OneShot {
            _core_listener: core_listener,
            registry,
            core,
            mainloop,
            label,
            deadline: Instant::now() + timeout,
            awaited,
            failed,
        })
    }

    /// Run until the server has answered everything issued so far: the globals replay,
    /// binds and their replays, pending writes.
    pub(crate) fn round(&self) -> Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("{} round-trip timed out", self.label);
        }
        let timed_out = Rc::new(Cell::new(false));
        let timer = self.mainloop.loop_().add_timer({
            let (mainloop, timed_out) = (self.mainloop.clone(), timed_out.clone());
            move |_| {
                timed_out.set(true);
                mainloop.quit();
            }
        });
        let _ = timer.update_timer(Some(left), None);
        let seq = self
            .core
            .sync(0)
            .with_context(|| format!("{} sync", self.label))?;
        self.awaited.set(Some(seq));
        self.mainloop.run();
        if let Some(e) = self.failed.borrow_mut().take() {
            return Err(e);
        }
        if timed_out.get() {
            bail!("{} round-trip timed out", self.label);
        }
        Ok(())
    }
}
