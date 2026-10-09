//! A short-lived PipeWire connection for one registry query on the calling thread: connect,
//! run sync [`rounds`](OneShot::round), drop. All rounds share one deadline, and a core error
//! ends the query, so a sick-but-connected daemon never wedges the caller.
//!
//! Twin of pf-client-core's `pw_oneshot.rs`, which copies this file because clients never link
//! host crates. A fix to one belongs in both.

use anyhow::{anyhow, bail, Context, Result};
use pipewire as pw;
use pw::spa::utils::result::AsyncSeq;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Budget for a whole query. A caller that must answer sooner passes its own, with the reason.
pub(super) const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct OneShot {
    // Declared first so it drops first: the hook unregisters while the core is alive.
    _core_listener: pw::core::Listener,
    pub(super) registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    mainloop: pw::main_loop::MainLoopRc,
    label: &'static str,
    deadline: Instant,
    awaited: Rc<Cell<Option<AsyncSeq>>>,
    failed: Rc<RefCell<Option<anyhow::Error>>>,
}

impl OneShot {
    /// `timeout` bounds every [`round`](Self::round) together, not each one.
    pub(super) fn connect(label: &'static str, timeout: Duration) -> Result<OneShot> {
        let (mainloop, core) = super::pw_setup::pw_connect(label)?;
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
    pub(super) fn round(&self) -> Result<()> {
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

    /// Bind the session manager's `default` metadata and read its subject-0 properties, the
    /// default-device keys. Runs two rounds, globals then the bind's replay, so register any
    /// other registry listener first: the first round is the only globals replay.
    pub(super) fn default_metadata(
        &self,
    ) -> Result<(pw::metadata::Metadata, HashMap<String, String>)> {
        type Bound = (pw::metadata::Metadata, pw::metadata::MetadataListener);
        let bound: Rc<RefCell<Option<Result<Bound>>>> = Rc::default();
        let props: Rc<RefCell<HashMap<String, String>>> = Rc::default();
        let globals = self
            .registry
            .add_listener_local()
            .global({
                let (registry, bound, props) =
                    (self.registry.clone(), bound.clone(), props.clone());
                move |global| {
                    if global.type_ != pw::types::ObjectType::Metadata
                        || bound.borrow().is_some()
                        || global.props.and_then(|p| p.get("metadata.name")) != Some("default")
                    {
                        return;
                    }
                    let md = registry
                        .bind::<pw::metadata::Metadata, _>(global)
                        .map_err(|e| anyhow!("bind default metadata: {e}"));
                    // Listen at bind: the replay can land in the same dispatch as the round's `done`.
                    let md = md.map(|md| {
                        let listener = md
                            .add_listener_local()
                            .property({
                                let props = props.clone();
                                move |subject, key, _type, value| {
                                    if let (0, Some(key)) = (subject, key) {
                                        let mut props = props.borrow_mut();
                                        match value {
                                            Some(v) => props.insert(key.to_owned(), v.to_owned()),
                                            None => props.remove(key),
                                        };
                                    }
                                    0
                                }
                            })
                            .register();
                        (md, listener)
                    });
                    *bound.borrow_mut() = Some(md);
                }
            })
            .register();
        self.round()?;
        drop(globals);
        let (md, _listener) = bound
            .take()
            .ok_or_else(|| anyhow!("no 'default' metadata object (is WirePlumber running?)"))??;
        self.round()?;
        Ok((md, props.take()))
    }
}
