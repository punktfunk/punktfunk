//! The owner's row: the seat on the box owner's own account, which the owner's own host serves
//! while the owner sits at the machine.
//!
//! While the owner has a session at the machine ([`owner_at_desk`]), the row's headless unit stays
//! down and the owner's own `punktfunk-host` serves the row ([`LinuxBackend::owner_status`]).
//! [`user_host`] starts or restarts that host: the owner's user units stand down while the row's
//! unit runs, so `start` is what brings them back.
//!
//! [`LinuxBackend::prepare_owners`] writes the owner's files again at supervisor start: `/run` is
//! empty after a boot, and the owner's own host would otherwise start on the box's ports, in the
//! door's way. The owner's home is also where the box library comes from:
//! [`refresh_box_library`] copies it for the door's group ([`door_reader`]).

use super::resources::{Order, UnitChange};
use super::{accounts, library_copy, logind, session, socket, trust_copy};
use super::{run, Inner, LinuxBackend};
use crate::backend::BackendError;
use crate::model::{Ledger, RuntimeState, RuntimeStatus, Seat};

/// Runs `systemctl --user <verb>` on the owner's own `punktfunk-host`. The owner's user units
/// stand down while the row's unit runs, so `start` is what brings them back: the host, and the
/// headless KDE session when the owner enabled it. A user with no manager has no login to host,
/// and nothing runs. `setpriv` becomes the user without a PAM session.
pub(super) fn user_host(account: &str, verb: &str) {
    const KDE_SESSION: &str = "punktfunk-kde-session.service";
    let Ok(Some(user)) = accounts::lookup(account) else {
        return;
    };
    let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
    let runtime = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
    let systemctl = |args: &[&str]| {
        let mut all = vec![
            "--reuid",
            uid.as_str(),
            "--regid",
            gid.as_str(),
            "--init-groups",
            "env",
            runtime.as_str(),
            "systemctl",
            "--user",
        ];
        all.extend_from_slice(args);
        run("systemctl_failed", "setpriv", &all)
    };
    let mut units = vec![verb, "punktfunk-host.service"];
    if verb == "start" && systemctl(&["is-enabled", "--quiet", KDE_SESSION]).is_ok() {
        units.push(KDE_SESSION);
    }
    if let Err(error) = systemctl(&units) {
        tracing::debug!(%account, verb, %error, "owner's own host not reached");
    }
}

/// The door's group, which reads the box's library. `None` while there is no such group.
pub(super) fn door_reader(inner: &Inner) -> Option<trust_copy::Reader> {
    let gid = match inner.door_gid.get() {
        Some(gid) => *gid,
        None => {
            let gid = accounts::group_gid(socket::DOOR_GROUP).ok().flatten()?;
            *inner.door_gid.get_or_init(|| gid)
        }
    };
    Some(trust_copy::Reader::group(gid))
}

/// Copies the owner's library into the box's library copy, once the owner's row is known.
/// Returns whether anything changed.
pub(super) fn refresh_box_library(inner: &Inner) -> Result<bool, BackendError> {
    let home = inner
        .owner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|owner| owner.home.clone());
    let Some(home) = home else { return Ok(false) };
    match door_reader(inner) {
        Some(reader) => library_copy::refresh_box_library(&home, &inner.state, reader),
        None => Ok(false),
    }
}

/// Whether the owner has a session at the machine. A `loginctl` that doesn't answer reads as no.
pub fn owner_at_desk(account: &str) -> bool {
    let own = logind::own_session(account);
    logind::sessions().is_ok_and(|all| logind::at_the_desk(&all, account, own.as_deref()))
}

impl LinuxBackend {
    pub(super) fn owner(&self) -> Option<accounts::Passwd> {
        self.inner
            .owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Writes the owner's files again at supervisor start: `/run` is empty after a boot, and the
    /// owner's own host would otherwise start on the box's ports, in the door's way. A host
    /// already up restarts onto files that changed; at boot none is up yet. The row is tracked, as
    /// [`crate::backend::PlatformBackend::adopt`] tracks it, for as long as it exists.
    pub fn prepare_owners(&self, ledger: &Ledger) {
        for seat in ledger.seats.iter().filter(|s| s.kind.is_owner()) {
            let prepared = accounts::require_owner(&seat.account).and_then(|passwd| {
                self.inner.apply_change(
                    seat,
                    Some(&passwd),
                    UnitChange::OwnerKnown,
                    Order::BeforeUnit,
                    None,
                )
            });
            match prepared {
                Ok(true) => user_host(&seat.account, "try-restart"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(account = %seat.account, %error, "owner's host files not written")
                }
            }
        }
    }

    /// The owner's row as it is: the headless unit's state while it runs, else running when the
    /// owner is at the machine and their own host answers on the row's ports, else stopped. A host
    /// the owner's manager keeps without a login has no desktop to stream, so it isn't the row.
    pub(super) fn owner_status(&self, seat: &Seat, state: &session::UnitState) -> RuntimeStatus {
        if state.live() {
            if state.active == "active" && !self.is_ready(seat) {
                return RuntimeStatus {
                    state: RuntimeState::Starting,
                    detail: Some("starting".into()),
                    ..RuntimeStatus::default()
                };
            }
            return state.runtime();
        }
        if self.serving(seat) && owner_at_desk(&seat.account) {
            return RuntimeStatus::running();
        }
        state.runtime()
    }
}
