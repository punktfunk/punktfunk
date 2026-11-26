//! Desk pads: a game controller plugged into the box, lent to one seat.
//!
//! The ledger says which row holds a pad, and the backend applies that to the hardware. A lend,
//! a move and a take-back each save the row first, then ask the backend to hand the hardware
//! over ([`PlatformBackend::settle_pads`]). If the backend can't, the rows are put back as they
//! were. Which pad may be lent is what the backend reads from the hardware, never what the
//! caller says.

use super::{not_found, SeatService};
use crate::backend::{BackendError, PlatformBackend, PresentPad};
use crate::ipc::{ApiError, CommandResult, DeskPad, ErrorCode, PadState};
use crate::model::{valid_pad_key, Ledger, LentPad, RuntimeState, Seat, SeatId, ValidationError};

impl<B: PlatformBackend> SeatService<B> {
    /// Every pad plugged in that may be lent, and every lent one, plugged in or not.
    pub(super) fn pads(&self) -> Result<CommandResult, ApiError> {
        let ledger = self.ledger();
        let present = self.backend().desk_pads(&ledger).map_err(ApiError::from)?;
        Ok(CommandResult::Pads {
            pads: listing(&ledger, present),
        })
    }

    /// Lends `key` to seat `id`, from the desk or from the seat that holds it. A pad the seat
    /// holds already changes nothing.
    pub(super) fn lend(&self, id: &SeatId, key: &str) -> Result<CommandResult, ApiError> {
        check_key(key)?;
        let _op = self.op();
        let before = self.ledger();
        if before.seat(id).is_none() {
            return Err(not_found(id));
        }
        if before.holder_of(key).is_some_and(|seat| &seat.id == id) {
            return self.pads();
        }
        let pad = self
            .backend()
            .lendable_pad(key, &before)
            .map_err(ApiError::from)?;
        let lent = LentPad {
            key: pad.key,
            name: pad.name,
            vendor_id: pad.vendor_id,
            product_id: pad.product_id,
            port: pad.port,
        };
        self.amend(|next| match next.lend(id, lent) {
            Ok(true) => Ok(()),
            Ok(false) => Err(not_found(id)),
            Err(ValidationError::TooManyPads(_)) => Err(full()),
            Err(error) => Err(ApiError::from(error)),
        })?;
        self.settle(&before, key)?;
        self.pads()
    }

    /// Returns `key` to the desk. A pad no seat holds changes nothing.
    pub(super) fn take_back(&self, key: &str) -> Result<CommandResult, ApiError> {
        check_key(key)?;
        let _op = self.op();
        let before = self.ledger();
        if before.holder_of(key).is_none() {
            return self.pads();
        }
        self.amend(|next| {
            next.take_back(key);
            Ok(())
        })?;
        self.settle(&before, key)?;
        self.pads()
    }

    /// Has the backend hand `key` to its holder in the ledger just saved. When it can't, every
    /// row gets the pads it had in `before`, and the backend is asked to put the hardware back.
    fn settle(&self, before: &Ledger, key: &str) -> Result<(), ApiError> {
        let keys = [key.to_owned()];
        let Err(error) = self.backend().settle_pads(&self.ledger(), &keys) else {
            return Ok(());
        };
        let restored = self.amend(|next| {
            for seat in &mut next.seats {
                seat.lent = before
                    .seat(&seat.id)
                    .map(|was| was.lent.clone())
                    .unwrap_or_default();
            }
            Ok(())
        });
        match restored {
            Ok(()) => {
                if let Err(again) = self.backend().settle_pads(&self.ledger(), &keys) {
                    tracing::warn!(error = %again, key, "desk pad not put back");
                }
            }
            Err(unsaved) => tracing::warn!(error = %unsaved.message, key, "lent pads not restored"),
        }
        Err(ApiError::from(error))
    }
}

fn check_key(key: &str) -> Result<(), ApiError> {
    if valid_pad_key(key) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "not a desk pad's key",
        ))
    }
}

fn full() -> ApiError {
    ApiError::from(
        BackendError::new(
            "pads_full",
            "This profile already has four controllers. Give one back first.",
        )
        .said("pads_full"),
    )
}

/// Whether a pad lent to `seat` is live: its seat runs or is starting.
fn live(seat: &Seat) -> bool {
    matches!(
        seat.runtime.state,
        RuntimeState::Running | RuntimeState::Starting
    )
}

/// The list the console shows: each plugged-in pad with its holder, then each lent pad that is
/// unplugged, by name and key.
fn listing(ledger: &Ledger, present: Vec<PresentPad>) -> Vec<DeskPad> {
    let state = |holder: Option<&Seat>| match holder {
        None => PadState::Desk,
        Some(seat) if live(seat) => PadState::Live,
        Some(_) => PadState::Parked,
    };
    let mut pads: Vec<DeskPad> = present
        .into_iter()
        .map(|pad| {
            let holder = ledger.holder_of(&pad.key);
            DeskPad {
                state: state(holder),
                seat: holder.map(|seat| seat.id.clone()),
                key: pad.key,
                name: pad.name,
                vendor_id: pad.vendor_id,
                product_id: pad.product_id,
                present: true,
            }
        })
        .collect();
    for seat in &ledger.seats {
        for lent in &seat.lent {
            if pads.iter().any(|pad| pad.key == lent.key) {
                continue;
            }
            pads.push(DeskPad {
                key: lent.key.clone(),
                name: lent.name.clone(),
                vendor_id: lent.vendor_id,
                product_id: lent.product_id,
                present: false,
                state: state(Some(seat)),
                seat: Some(seat.id.clone()),
            });
        }
    }
    pads.sort_by(|a, b| (&a.name, &a.key).cmp(&(&b.name, &b.key)));
    pads
}
