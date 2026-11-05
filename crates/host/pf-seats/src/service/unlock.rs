//! The commands of a seat on a person's own Linux account: the supervisor, never the door,
//! decides which device opens the person's desktop (CONTRACT §4, rules in [`crate::unlock`]).
//!
//! A password goes through the attempt limits and the PAM child outside `ops`, then everything it
//! rested on is read again under `ops`, where a stopped session is started through the same path
//! as a `start` and the device is let on once it runs. A key proof costs its device's allowance
//! alone and never reaches PAM. Every unlock writes one `punktfunk::audit` line; nothing in it,
//! or in any other line, carries the secret.

use super::{not_found, SeatService};
use crate::attempts::Target;
use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{ApiError, CommandResult, Device, ErrorCode, Proof, UnlockKey};
use crate::model::{RuntimeState, Seat, SeatId};
use crate::secret::Secret;
use crate::unlock::{self, DeviceList};
use rand::TryRng as _;
use std::time::Instant;

/// The answer to a device that must prove the person is there. The door shows its own prompt.
pub(super) fn unlock_required() -> ApiError {
    refusal(
        "unlock_required",
        "This profile needs its account password.",
    )
}

fn refusal(code: &str, sentence: &str) -> ApiError {
    ApiError::from(BackendError::new(code, sentence))
}

fn proof_invalid() -> BackendError {
    BackendError::new(
        "proof_invalid",
        "Couldn't confirm it's you on this device. Type the account password.",
    )
}

fn invalid(what: &str) -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest, what)
}

fn checked_fingerprint(fingerprint: &str) -> Result<[u8; 32], ApiError> {
    unlock::fingerprint_bytes(fingerprint)
        .ok_or_else(|| invalid("device fingerprint is not 64 lowercase hex"))
}

/// The code an answer leads with: a backend refusal's own, else the error's.
fn code_of(error: &ApiError) -> String {
    match (error.code, error.message.split_once(": ")) {
        (ErrorCode::Backend, Some((code, _))) => code.to_owned(),
        (code, _) => serde_json::to_value(code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
    }
}

impl<B: PlatformBackend> SeatService<B> {
    /// Lets `device` onto `id` with `proof`, then writes the audit line.
    pub(super) fn unlock(
        &self,
        id: &SeatId,
        device: &Device,
        proof: Proof,
    ) -> Result<CommandResult, ApiError> {
        let kind = proof.kind();
        let answer = self.unlock_with(id, device, proof);
        self.audit(id, device, kind, &answer);
        answer
    }

    fn unlock_with(
        &self,
        id: &SeatId,
        device: &Device,
        proof: Proof,
    ) -> Result<CommandResult, ApiError> {
        let seat = self.person(id)?;
        let fingerprint = checked_fingerprint(&device.fingerprint)?;
        if !unlock::valid_device_name(&device.name) {
            return Err(invalid("device name is not 1 to 64 printable characters"));
        }
        match proof {
            Proof::None => self.rejoin(&seat, device),
            Proof::Password {
                password,
                stay_signed_in,
                enroll,
            } => self.with_password(&seat, device, &password, stay_signed_in, enroll),
            Proof::Key { signature } => self.with_key(&seat, device, fingerprint, &signature),
        }
    }

    /// `id`'s row, when it is on a person's own Linux account.
    fn person(&self, id: &SeatId) -> Result<Seat, ApiError> {
        let seat = self.lock().seat(id).cloned().ok_or_else(|| not_found(id))?;
        if seat.adopted_uid.is_none() {
            return Err(invalid(
                "only a seat on a person's own Linux account unlocks",
            ));
        }
        Ok(seat)
    }

    /// `checked`'s row as the ledger holds it now, refused if it moved to another account or its
    /// account no longer resolves to the recorded uid. Called before anything starts or is granted.
    fn still(&self, checked: &Seat) -> Result<Seat, ApiError> {
        let seat = self.person(&checked.id)?;
        if seat.adopted_uid != checked.adopted_uid || seat.account != checked.account {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!("account {} changed while it was unlocked", checked.account),
            ));
        }
        self.backend.account_owned(&seat)?;
        Ok(seat)
    }

    fn runs(&self, seat: &Seat) -> bool {
        let status = self.backend.status(seat);
        status.is_ok_and(|s| s.state == RuntimeState::Running)
    }

    fn device_list(&self, seat: &Seat) -> Result<DeviceList, ApiError> {
        Ok(self.backend.devices(seat, &mut |_, _| {})?)
    }

    /// No proof: only an admitted device, only onto a running session, and only while the row's
    /// account is still the person it adopted.
    fn rejoin(&self, seat: &Seat, device: &Device) -> Result<CommandResult, ApiError> {
        self.backend.account_owned(seat)?;
        let running = self.runs(seat);
        let mut admitted = false;
        self.backend.devices(seat, &mut |list, now| {
            admitted = running && list.rejoin(&device.fingerprint, now);
        })?;
        if !admitted {
            return Err(unlock_required());
        }
        Ok(CommandResult::Unlocked {
            seat: self.person(&seat.id)?,
            started: false,
        })
    }

    /// The account's password through the limits and PAM, outside `ops`; then the row read again,
    /// the session started if it is down, and the device let on.
    fn with_password(
        &self,
        checked: &Seat,
        device: &Device,
        password: &Secret,
        stay: bool,
        enroll: Option<UnlockKey>,
    ) -> Result<CommandResult, ApiError> {
        if enroll
            .as_ref()
            .is_some_and(|key| unlock::key_bytes(key).is_none())
        {
            return Err(invalid(
                "enroll key is not a P-256 point or a 2048-bit RSA key",
            ));
        }
        let account = format!("uid:{}", checked.adopted_uid.unwrap_or_default());
        let target = Target {
            device: Some((checked.id.as_str(), &device.fingerprint)),
            account: Some(&account),
        };
        self.limited(target, || self.backend.check_password(checked, password))?;
        let _op = self.op();
        let seat = self.still(checked)?;
        if !self.device_list(&seat)?.fits(&device.fingerprint) {
            return Err(full());
        }
        let (seat, started) = self.running(seat)?;
        let mut granted = false;
        self.backend.devices(&seat, &mut |list, now| {
            granted = list.grant(device, now, stay, enroll.clone());
        })?;
        if !granted {
            return Err(full());
        }
        Ok(CommandResult::Unlocked { seat, started })
    }

    /// The enrolled key's signature over the live challenge, which this uses up whatever the
    /// outcome. Only the device's allowance pays for a failure.
    fn with_key(
        &self,
        checked: &Seat,
        device: &Device,
        fingerprint: [u8; 32],
        signature: &str,
    ) -> Result<CommandResult, ApiError> {
        let fp = device.fingerprint.as_str();
        let challenge = self.challenges().take(&checked.id, fp, Instant::now());
        let signature = unlock::signature_bytes(signature)
            .ok_or_else(|| invalid("signature is not 1 to 1024 bytes of lowercase hex"))?;
        let key = self
            .device_list(checked)?
            .key_of(fp)
            .cloned()
            .ok_or_else(unlock_required)?;
        let box_fp = self.box_fingerprint()?;
        let target = Target {
            device: Some((checked.id.as_str(), fp)),
            account: None,
        };
        self.limited(target, || {
            let challenge = challenge.ok_or_else(proof_invalid)?;
            let signed = unlock::message(&challenge, &checked.id, &fingerprint, &box_fp);
            unlock::verify(&key, &signed, &signature)
                .then_some(())
                .ok_or_else(proof_invalid)
        })?;
        let _op = self.op();
        let seat = self.still(checked)?;
        if self.device_list(&seat)?.key_of(fp) != Some(&key) {
            return Err(proof_invalid().into());
        }
        let (seat, started) = self.running(seat)?;
        let mut granted = false;
        self.backend.devices(&seat, &mut |list, now| {
            granted = list.key_of(fp) == Some(&key) && list.grant(device, now, false, None);
        })?;
        if !granted {
            return Err(proof_invalid().into());
        }
        Ok(CommandResult::Unlocked { seat, started })
    }

    /// `seat` as it runs, started through the same path as a `start` when it is down. The caller
    /// holds `ops`. `true` when this started it.
    fn running(&self, seat: Seat) -> Result<(Seat, bool), ApiError> {
        if self.runs(&seat) {
            return Ok((seat, false));
        }
        Ok((self.change_locked(&seat.id, true)?, true))
    }

    /// The pinned box fingerprint, or the refusal of every key proof while it doesn't match.
    fn box_fingerprint(&self) -> Result<[u8; 32], ApiError> {
        self.backend.box_fingerprint().map_err(|error| {
            tracing::warn!(%error, "key proofs refused: the box certificate");
            refusal(
                "proof_invalid",
                "This machine's identity changed, so this device can't unlock with its key. Type the account password.",
            )
        })
    }

    pub(super) fn unlock_state(
        &self,
        id: &SeatId,
        fingerprint: &str,
    ) -> Result<CommandResult, ApiError> {
        let seat = self.person(id)?;
        checked_fingerprint(fingerprint)?;
        let running = self.runs(&seat);
        let needs = self.device_list(&seat)?.needs(fingerprint, running);
        Ok(CommandResult::UnlockState { needs })
    }

    /// 32 bytes from the OS for `fingerprint`'s key to sign, in place of its last challenge.
    pub(super) fn challenge(
        &self,
        id: &SeatId,
        fingerprint: &str,
    ) -> Result<CommandResult, ApiError> {
        let seat = self.person(id)?;
        checked_fingerprint(fingerprint)?;
        if self.device_list(&seat)?.key_of(fingerprint).is_none() {
            return Err(unlock_required());
        }
        self.box_fingerprint()?;
        let mut bytes = [0_u8; 32];
        rand::rngs::SysRng.try_fill_bytes(&mut bytes).map_err(|e| {
            ApiError::from(BackendError::new(
                "challenge_random",
                format!("draw a challenge: {e}"),
            ))
        })?;
        self.challenges()
            .mint(id, fingerprint, bytes, Instant::now());
        Ok(CommandResult::Challenge {
            challenge: hex::encode(bytes),
            seat: id.clone(),
        })
    }

    pub(super) fn left(&self, id: &SeatId, fingerprint: &str) -> Result<CommandResult, ApiError> {
        let seat = self.person(id)?;
        checked_fingerprint(fingerprint)?;
        self.backend
            .devices(&seat, &mut |list, now| list.left(fingerprint, now))?;
        Ok(CommandResult::Left)
    }

    pub(super) fn sign_out(
        &self,
        id: &SeatId,
        fingerprint: Option<&str>,
    ) -> Result<CommandResult, ApiError> {
        let seat = self.person(id)?;
        if let Some(fingerprint) = fingerprint {
            checked_fingerprint(fingerprint)?;
        }
        self.backend
            .devices(&seat, &mut |list, _| list.sign_out(fingerprint))?;
        self.challenges().forget(id, fingerprint);
        Ok(CommandResult::SignedOut)
    }

    pub(super) fn devices(&self, id: &SeatId) -> Result<CommandResult, ApiError> {
        let seat = self.person(id)?;
        let devices = self.device_list(&seat)?.entries();
        Ok(CommandResult::Devices { devices })
    }

    /// The journal line of one unlock: seat, account, device, proof, result and the failures
    /// counted so far. A fingerprint that isn't one is logged as `malformed`.
    fn audit(
        &self,
        id: &SeatId,
        device: &Device,
        proof: &str,
        answer: &Result<CommandResult, ApiError>,
    ) {
        let seat = self.lock().seat(id).cloned();
        let fp = device.fingerprint.as_str();
        let fp = if unlock::valid_fingerprint(fp) {
            fp
        } else {
            "malformed"
        };
        let account = seat.as_ref().map_or("", |s| s.account.as_str());
        let uid = seat.as_ref().and_then(|s| s.adopted_uid);
        let key = format!("uid:{}", uid.unwrap_or_default());
        let target = Target {
            device: Some((id.as_str(), fp)),
            account: Some(&key),
        };
        let (device_failures, account_failures) = self.attempts().failures(target, Instant::now());
        let result = match answer {
            Ok(_) => "unlocked".to_owned(),
            Err(error) => code_of(error),
        };
        tracing::info!(target: "punktfunk::audit", seat = %id, account, ?uid, device = fp,
            proof, result = %result, device_failures, account_failures, "seat unlock");
    }

    fn challenges(&self) -> std::sync::MutexGuard<'_, unlock::Challenges> {
        self.challenges
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

fn full() -> ApiError {
    ApiError::new(
        ErrorCode::Capacity,
        "This profile has too many signed-in devices. Sign one out in the console, then try again.",
    )
}
