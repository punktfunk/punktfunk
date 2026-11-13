//! A profile on a person's own Linux account opens only on the root supervisor's word: the door
//! relays the device's proof and decides nothing (CONTRACT §5). The proof is the one
//! `ProfileUnlock` the hello announced, else none; the device is the certificate the QUIC
//! handshake authenticated. The client hears `Pending` while the supervisor checks the password
//! and starts the session, and the supervisor hears `Left` once the device is off the seat.
//!
//! No log line, trace field or error carries anything of an unlock but its proof kind and the
//! supervisor's answer code. A browser never opens such a profile.

use super::{link, Said};
use crate::mgmt::owner::Seats;
use anyhow::{anyhow, bail, Context, Result};
use pf_seats::ipc::{
    ApiError, Command, CommandResult, Device, ErrorCode, KeyAlg, Proof, UnlockKey,
};
use pf_seats::{Seat, SeatId, Secret};
use punktfunk_core::quic::v2::hello::ClientHello;
use punktfunk_core::quic::v2::msg::{self as v2msg, ProfileUnlock};
use punktfunk_core::quic::v2::registry::FEATURE_PROFILE_UNLOCK;
use punktfunk_core::reject::RejectReason;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the unlock a hello announced may take to follow it.
const PROOF_WAIT: Duration = Duration::from_secs(5);

/// A refusal's message id and its English.
type Words = (&'static str, &'static str);

const FROM_A_BROWSER: Words = (
    "close_unlock_from_browser",
    "Open this profile from the Punktfunk app.",
);
const UPDATE: Words = (
    "close_unlock_update_app",
    "This profile needs its account password. Update Punktfunk on this device.",
);
const UNREADABLE: Words = (
    "close_unlock_unreadable",
    "Couldn't read the password this device sent. Try again.",
);
const SILENT: Words = (
    "close_unlock_silent",
    "This computer's seat service didn't answer. Try again in a moment.",
);
const NOT_STARTED: Words = (
    "close_unlock_not_started",
    "This profile's desktop didn't start. Try again in a moment.",
);

fn said((id, text): Words) -> Said {
    Said::new(id, text)
}

/// Supervisor codes whose message is the sentence a person reads at the password prompt.
const PROMPT_CODES: &[&str] = &[
    "credential_wrong",
    "credential_must_change",
    "account_locked",
    "pam_unavailable",
    "unlock_locked_out",
    "proof_invalid",
    "account_at_desk",
];

/// The supervisor a connect from `fp` asks: the live one, or the fake a test armed for `fp`.
pub(crate) fn seats_for(fp: Option<&str>) -> Arc<dyn Seats> {
    #[cfg(test)]
    if let Some(armed) = fp.and_then(fake::armed) {
        return armed;
    }
    let _ = fp;
    Arc::new(crate::mgmt::owner::LiveSeats)
}

/// The device as the supervisor lists it: `fp`, the certificate the QUIC handshake
/// authenticated, and its pairing record's `label` cleaned to the supervisor's rule, else a
/// name that always passes it.
pub(crate) fn device(fp: &str, label: Option<&str>) -> Device {
    let name = crate::native_pairing::sanitize_device_name(label.unwrap_or(""), fp);
    let name = if pf_seats::unlock::valid_device_name(&name) {
        name
    } else {
        format!("device {}", fp.get(..8).unwrap_or("unknown"))
    };
    Device {
        fingerprint: fp.to_owned(),
        name,
    }
}

/// The supervisor's yes for `device` onto `row`, a person's own Linux account, with the proof
/// `first` announced: the row as it runs, and the device's claim on it. Any other answer closes
/// the connect, typed, and errs.
pub(super) async fn unlock(
    conn: &link::SessionLink,
    send: &mut link::CtlSend,
    recv: &mut link::CtlReader,
    first: &ClientHello,
    row: &Seat,
    device: Option<Device>,
    seats: &Arc<dyn Seats>,
) -> Result<(Seat, Claim)> {
    if conn.is_web() {
        let reason = RejectReason::SeatUnavailable;
        conn.refuse_said(reason.close_code(), &said(FROM_A_BROWSER))
            .await;
        bail!("a browser asked for a person's own account");
    }
    let Some(device) = device else {
        super::close_rejected(conn, RejectReason::IdentityRequired).await;
        bail!("an anonymous client asked for a person's own account");
    };
    let offered = first.features.has(FEATURE_PROFILE_UNLOCK);
    let proof = if first.unlock_follows() {
        let deadline = tokio::time::Instant::now() + PROOF_WAIT;
        match recv
            .read_profile_unlock(deadline)
            .await
            .ok()
            .and_then(proof_of)
        {
            Some(proof) => proof,
            None => {
                let (reason, words) = locked(offered, said(UNREADABLE));
                conn.refuse_said(reason.close_code(), &words).await;
                bail!("the profile unlock the hello announced didn't arrive whole");
            }
        }
    } else {
        Proof::None
    };
    tracing::info!(seat = %row.id, proof = proof.kind(), "profile unlock relayed to the seat supervisor");
    let task = {
        let (seats, id) = (Arc::clone(seats), row.id.clone());
        tokio::task::spawn_blocking(move || {
            let claim = Claim::take(&id, &device.fingerprint);
            (claim, seats.ask(Command::Unlock { id, device, proof }))
        })
    };
    let (claim, answer) = answered(conn, send, task, seats).await?;
    let error = match answer {
        Ok(CommandResult::Unlocked { seat, started }) => {
            tracing::info!(seat = %seat.id, outcome = "unlocked", started, "profile unlock answered");
            return Ok((seat, claim));
        }
        Ok(_) => ApiError::new(ErrorCode::Transport, "unlock answered something else"),
        Err(error) => error,
    };
    let code = code_of(&error);
    tracing::info!(seat = %row.id, outcome = %code, "profile unlock answered");
    let (reason, words) = refusal(&error, offered);
    conn.refuse_said(reason.close_code(), &words).await;
    bail!("profile unlock refused: {code}")
}

type Unlocking = tokio::task::JoinHandle<(Claim, Result<CommandResult, ApiError>)>;

/// `task`'s answer, with `Pending` to the client every [`super::PENDING_EVERY`] meanwhile. A
/// client that goes first leaves an unlock it never hears to [`watch_left`].
async fn answered(
    conn: &link::SessionLink,
    send: &mut link::CtlSend,
    mut task: Unlocking,
    seats: &Arc<dyn Seats>,
) -> Result<(Claim, Result<CommandResult, ApiError>)> {
    let mut pending = tokio::time::interval(super::PENDING_EVERY);
    let gone = loop {
        tokio::select! {
            joined = &mut task => return joined.context("profile unlock task"),
            _ = conn.closed() => break "client left while its profile unlocked",
            _ = pending.tick() => {
                // Bounded: a peer that stops reading would hold the other arms with this one.
                let sent = punktfunk_core::quic::v2::io::send(&mut *send, &v2msg::Pending {});
                if !matches!(tokio::time::timeout(super::PENDING_EVERY, sent).await, Ok(Ok(_))) {
                    break "client stopped reading while its profile unlocked";
                }
            }
        }
    };
    let seats = Arc::clone(seats);
    tokio::spawn(async move {
        if let Ok((claim, Ok(CommandResult::Unlocked { .. }))) = task.await {
            watch_left(seats, claim, false);
        }
    });
    Err(anyhow!(gone))
}

/// `unlock` as the supervisor takes it: the password into a [`Secret`], keys and signatures as
/// lowercase hex. The wire's copy wipes as `unlock` drops. `None` for a kind or algorithm the
/// decoder already refuses.
fn proof_of(unlock: ProfileUnlock) -> Option<Proof> {
    match unlock.kind {
        v2msg::UNLOCK_PASSWORD => {
            let alg = match unlock.enroll_alg {
                v2msg::ENROLL_NONE => None,
                v2msg::ENROLL_P256 => Some(KeyAlg::P256),
                v2msg::ENROLL_RSA2048 => Some(KeyAlg::Rsa2048),
                _ => return None,
            };
            let password = std::str::from_utf8(&unlock.secret[..]).ok()?;
            Some(Proof::Password {
                password: Secret::new(password.to_owned()),
                stay_signed_in: unlock.stay_signed_in,
                enroll: alg.map(|alg| UnlockKey {
                    alg,
                    public: hex::encode(&unlock.enroll_key),
                }),
            })
        }
        v2msg::UNLOCK_KEY_PROOF => Some(Proof::Key {
            signature: hex::encode(&unlock.secret[..]),
        }),
        _ => None,
    }
}

/// A refusal at the prompt: `ProfileLocked` with `words` for a client that offered the
/// unlock, the update sentence for one that can't send it.
fn locked(offered: bool, words: Said) -> (RejectReason, Said) {
    if offered {
        (RejectReason::ProfileLocked, words)
    } else {
        (RejectReason::SeatUnavailable, said(UPDATE))
    }
}

/// What the client hears when the supervisor didn't let it on. A refusal at the prompt carries
/// the supervisor's sentence and its message `apierr_<code>`, none for `unlock_required` (show
/// the prompt). A supervisor that didn't answer, or a seat that didn't start, is
/// `SeatUnavailable` with a plain sentence: the supervisor's other messages are the operator's.
fn refusal(error: &ApiError, offered: bool) -> (RejectReason, Said) {
    let backend = |sentence: &str| Said {
        text: sentence.to_string(),
        id: error
            .said
            .as_deref()
            .map_or_else(String::new, |code| format!("apierr_{code}")),
        params: error.params.clone().into_iter().collect(),
    };
    let prompt = match (error.code, error.message.split_once(": ")) {
        (ErrorCode::Backend, Some(("unlock_required", _))) => Some(Said::default()),
        (ErrorCode::Backend, Some((code, sentence))) if PROMPT_CODES.contains(&code) => {
            Some(backend(sentence))
        }
        (ErrorCode::Capacity, _) => Some(backend(&error.message)),
        _ => None,
    };
    match prompt {
        Some(words) => locked(offered, words),
        None if error.code == ErrorCode::Transport => (RejectReason::SeatUnavailable, said(SILENT)),
        None => (RejectReason::SeatUnavailable, said(NOT_STARTED)),
    }
}

/// The code an answer leads with, for the log: a backend refusal's own, else the error's.
fn code_of(error: &ApiError) -> String {
    let own = error.message.split_once(": ").map(|(code, _)| code);
    match own {
        Some(code)
            if error.code == ErrorCode::Backend
                && !code.is_empty()
                && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            code.to_owned()
        }
        _ => serde_json::to_value(error.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
    }
}

/// Each (seat, device) a connect may still hold, and how many such connects. A connect claims
/// before it asks to unlock, and the last claim to go sends `Left` under this lock, so a `Left`
/// never reaches the supervisor after the rejoin that followed it. One lock for every seat: a
/// supervisor that hangs on a `Left` holds the next unlock's claim too.
static CLAIMS: Mutex<BTreeMap<(String, String), u32>> = Mutex::new(BTreeMap::new());

fn claims() -> std::sync::MutexGuard<'static, BTreeMap<(String, String), u32>> {
    CLAIMS.lock().unwrap_or_else(|e| e.into_inner())
}

/// One connect's claim on a person's seat for its device. Dropped, it lets go with no `Left`.
pub(crate) struct Claim {
    seat: SeatId,
    fp: String,
}

impl Claim {
    /// Blocking: waits out a `Left` in flight.
    fn take(seat: &SeatId, fp: &str) -> Claim {
        let claim = Claim {
            seat: seat.clone(),
            fp: fp.to_owned(),
        };
        *claims().entry(claim.key()).or_insert(0) += 1;
        claim
    }

    fn key(&self) -> (String, String) {
        (self.seat.as_str().to_owned(), self.fp.clone())
    }

    /// The device is off the seat: the last claim tells the supervisor. Blocking.
    fn leave(self, seats: &dyn Seats) {
        let held = claims();
        if held.get(&self.key()) == Some(&1) {
            let left = seats.ask(Command::Left {
                id: self.seat.clone(),
                fingerprint: self.fp.clone(),
            });
            if let Err(error) = left {
                tracing::debug!(code = ?error.code, "seat supervisor did not take a device that left");
            }
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut held = claims();
        let key = self.key();
        match held.get_mut(&key) {
            Some(n) if *n > 1 => *n -= 1,
            _ => {
                held.remove(&key);
            }
        }
    }
}

/// Watches the claim's seat, then lets the claim go: with a `Left` once its seat host, answering,
/// misses the device twice in a row after it landed, after [`super::SEAT_LANDING`] when it never
/// did, or at once when no `landing` is due; with none once the seat has said nothing for
/// [`super::SEAT_SILENCE`]. Spawns, so it runs inside the runtime.
pub(super) fn watch_left(seats: Arc<dyn Seats>, claim: Claim, landing: bool) {
    tokio::spawn(async move {
        let started = Instant::now();
        let (mut heard, mut landed, mut missed) = (started, !landing, 0);
        loop {
            tokio::time::sleep(super::SEAT_POLL).await;
            let (asking, id, fp) = (Arc::clone(&seats), claim.seat.clone(), claim.fp.clone());
            let asked = tokio::task::spawn_blocking(move || on_seat(&*asking, &id, &fp));
            let here = tokio::time::timeout(super::SEAT_SILENCE, asked)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
            if let Some(here) = here {
                heard = Instant::now();
                landed |= here;
                missed = if here { 0 } else { missed + 1 };
            }
            match settle(here, landed, missed, started.elapsed(), heard.elapsed()) {
                Some(true) => {
                    let _ = tokio::task::spawn_blocking(move || claim.leave(&*seats)).await;
                    return;
                }
                Some(false) => return,
                None => {}
            }
        }
    });
}

/// One look's verdict: `Some(true)` the device is gone, `Some(false)` stop with no `Left`,
/// `None` look again. `missed` counts the answering looks in a row without the device.
fn settle(
    here: Option<bool>,
    landed: bool,
    missed: u32,
    waited: Duration,
    silent: Duration,
) -> Option<bool> {
    match here {
        Some(false) if missed >= 2 && (landed || waited > super::SEAT_LANDING) => Some(true),
        None if silent > super::SEAT_SILENCE => Some(false),
        _ => None,
    }
}

/// Whether `fp` streams on seat `id`; `None` when the seat isn't running or its host doesn't say.
fn on_seat(seats: &dyn Seats, id: &SeatId, fp: &str) -> Option<bool> {
    let ledger = seats.ledger();
    let (seat, _) = ledger.seat(id.as_str())?;
    if seat.runtime.state != pf_seats::RuntimeState::Running {
        return None;
    }
    let who = crate::seats::occupants(seat)?;
    Some(
        who.iter()
            .any(|o| crate::session_status::is_client(&o.client, fp)),
    )
}

/// A supervisor a test scripts, armed per device fingerprint so no other test's connect meets it.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use crate::mgmt::owner::{Answer, Forwarded, Target};
    use crate::seats::Snapshot;

    type Answers = Box<dyn Fn(&Command) -> Result<CommandResult, ApiError> + Send + Sync>;

    /// The ledger it lists, the answer to every other command, and what it was asked. No seat
    /// host answers a call.
    pub(crate) struct Fake {
        pub(crate) seats: Vec<Seat>,
        pub(crate) answer: Answers,
        /// How long an `Unlock` takes before it answers.
        pub(crate) unlock_takes: Duration,
        pub(crate) asked: Mutex<Vec<Command>>,
        pub(crate) called: Mutex<Vec<Target>>,
    }

    impl Fake {
        pub(crate) fn new(
            seats: Vec<Seat>,
            answer: impl Fn(&Command) -> Result<CommandResult, ApiError> + Send + Sync + 'static,
        ) -> Fake {
            Fake {
                seats,
                answer: Box::new(answer),
                unlock_takes: Duration::ZERO,
                asked: Mutex::new(Vec::new()),
                called: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn asked(&self) -> Vec<Command> {
            self.asked.lock().unwrap().clone()
        }

        pub(crate) fn called(&self) -> Vec<Target> {
            self.called.lock().unwrap().clone()
        }
    }

    impl Seats for Fake {
        fn snapshot(&self) -> Arc<Snapshot> {
            Arc::new(self.ledger())
        }

        fn call(&self, target: &Target, _: Forwarded) -> Answer {
            self.called.lock().unwrap().push(target.clone());
            Answer::Down
        }

        fn ledger(&self) -> Snapshot {
            Snapshot {
                on: true,
                seats: self.seats.clone(),
                ..Snapshot::default()
            }
        }

        fn ask(&self, command: Command) -> Result<CommandResult, ApiError> {
            if matches!(command, Command::Unlock { .. }) {
                std::thread::sleep(self.unlock_takes);
            }
            let answer = (self.answer)(&command);
            self.asked.lock().unwrap().push(command);
            answer
        }
    }

    /// A person's own Linux account, running, its host pinned and on `port`.
    pub(crate) fn person(id: &str, port: u16) -> Seat {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "Jane", "account": "jdoe", "display_slot": 12,
            "native_port": port, "mgmt_port": 47995, "runtime": { "state": "running" },
            "fingerprint": "5eed".repeat(16), "adopted_uid": 1500,
        }))
        .unwrap()
    }

    /// A supervisor refusal as the supervisor words it.
    pub(crate) fn refused(code: &str, sentence: &str) -> ApiError {
        ApiError::new(ErrorCode::Backend, format!("{code}: {sentence}"))
    }

    static ARMED: Mutex<Vec<(String, Arc<dyn Seats>)>> = Mutex::new(Vec::new());

    /// While the guard lives, a connect from `fp` asks `seats`.
    pub(crate) fn arm(fp: &str, seats: Arc<dyn Seats>) -> Armed {
        ARMED.lock().unwrap().push((fp.to_owned(), seats));
        Armed(fp.to_owned())
    }

    pub(crate) struct Armed(String);

    impl Drop for Armed {
        fn drop(&mut self) {
            let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            armed.retain(|(fp, _)| fp != &self.0);
        }
    }

    pub(super) fn armed(fp: &str) -> Option<Arc<dyn Seats>> {
        let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        armed
            .iter()
            .find(|(armed, _)| armed.eq_ignore_ascii_case(fp))
            .map(|(_, seats)| Arc::clone(seats))
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{refused, Fake};
    use super::*;
    use punktfunk_core::quic::v2::msg::Zeroizing;

    fn unlock(kind: u8, secret: &[u8], stay: bool, alg: u8, key: Vec<u8>) -> ProfileUnlock {
        ProfileUnlock::new(kind, Zeroizing::new(secret.to_vec()), stay, alg, key).unwrap()
    }

    /// The password reaches the supervisor as typed, never trimmed; a key to enrol and a
    /// signature as lowercase hex.
    #[test]
    fn each_proof_kind_maps_to_the_supervisor_s() {
        let p256 = [vec![4u8], vec![0xab; 64]].concat();
        let typed = " hunter2 Bärbel ";
        let Some(Proof::Password {
            password,
            stay_signed_in,
            enroll,
        }) = proof_of(unlock(
            v2msg::UNLOCK_PASSWORD,
            typed.as_bytes(),
            true,
            v2msg::ENROLL_P256,
            p256.clone(),
        ))
        else {
            panic!("a password")
        };
        assert_eq!(password.expose(), typed);
        assert!(stay_signed_in);
        assert_eq!(
            enroll,
            Some(UnlockKey {
                alg: KeyAlg::P256,
                public: hex::encode(&p256)
            })
        );
        let rsa = [vec![0x30u8], vec![0xCD; 269]].concat();
        let Some(Proof::Password { enroll, .. }) = proof_of(unlock(
            v2msg::UNLOCK_PASSWORD,
            b"pw",
            false,
            v2msg::ENROLL_RSA2048,
            rsa.clone(),
        )) else {
            panic!("a password")
        };
        assert_eq!(enroll.unwrap().public, hex::encode(&rsa));
        let bare = proof_of(unlock(v2msg::UNLOCK_PASSWORD, b"pw", false, 0, Vec::new()));
        assert!(matches!(
            bare,
            Some(Proof::Password {
                enroll: None,
                stay_signed_in: false,
                ..
            })
        ));
        let signed = proof_of(unlock(
            v2msg::UNLOCK_KEY_PROOF,
            &[0x30, 0xAB],
            false,
            0,
            Vec::new(),
        ));
        assert_eq!(
            signed,
            Some(Proof::Key {
                signature: "30ab".into()
            })
        );
    }

    /// A client that offered the unlock reads the supervisor's sentence at its prompt, none for
    /// `unlock_required`; an older one is told to update. Nothing else the supervisor says
    /// reaches a client.
    #[test]
    fn each_refusal_reaches_each_client_generation() {
        use RejectReason::{ProfileLocked, SeatUnavailable};
        let wrong = refused("credential_wrong", "That isn't jdoe's password.");
        let cases = [
            (refused("unlock_required", "x"), (ProfileLocked, "")),
            (
                wrong.clone(),
                (ProfileLocked, "That isn't jdoe's password."),
            ),
            (
                refused("credential_must_change", "Expired."),
                (ProfileLocked, "Expired."),
            ),
            (
                refused("account_locked", "Locked."),
                (ProfileLocked, "Locked."),
            ),
            (
                refused("pam_unavailable", "No login."),
                (ProfileLocked, "No login."),
            ),
            (
                refused("unlock_locked_out", "Wait 12 min."),
                (ProfileLocked, "Wait 12 min."),
            ),
            (
                refused("proof_invalid", "Type it."),
                (ProfileLocked, "Type it."),
            ),
            (
                refused("account_at_desk", "At the desk."),
                (ProfileLocked, "At the desk."),
            ),
            (
                ApiError::new(ErrorCode::Capacity, "Sign one out."),
                (ProfileLocked, "Sign one out."),
            ),
            (
                refused("seat_dirs", "create /var/lib/x: denied"),
                (SeatUnavailable, NOT_STARTED.1),
            ),
            (
                ApiError::new(ErrorCode::InvalidRequest, "device name is bad"),
                (SeatUnavailable, NOT_STARTED.1),
            ),
            (
                ApiError::new(ErrorCode::Transport, "connect: No such file"),
                (SeatUnavailable, SILENT.1),
            ),
        ];
        let heard = |error: &ApiError, offered: bool| {
            let (reason, words) = refusal(error, offered);
            (reason, words.text)
        };
        for (error, (reason, sentence)) in &cases {
            let want = (*reason, sentence.to_string());
            assert_eq!(heard(error, true), want, "{}", error.message);
        }
        for (error, (reason, sentence)) in &cases {
            let old = heard(error, false);
            if *reason == ProfileLocked {
                let update = (SeatUnavailable, UPDATE.1.to_string());
                assert_eq!(old, update, "{}", error.message);
            } else {
                assert_eq!(old, (*reason, sentence.to_string()), "{}", error.message);
            }
        }
        let worded = refusal(&wrong.clone().said("credential_wrong_windows"), true).1;
        assert_eq!(worded.id, "apierr_credential_wrong_windows");
        assert_eq!(refusal(&wrong, true).1.id, "");
        assert_eq!(refusal(&wrong, false).1.id, UPDATE.0);
        assert_eq!(code_of(&wrong), "credential_wrong");
        assert_eq!(
            code_of(&ApiError::new(ErrorCode::Transport, "read: timed out")),
            "transport"
        );
    }

    /// The pairing record's name as the supervisor takes it, or one that always passes.
    #[test]
    fn a_device_name_always_passes_the_supervisor_s_rule() {
        let fp = "ab".repeat(32);
        for label in [Some("Jane's iPhone"), Some("\u{202E}x\n"), Some(""), None] {
            let named = device(&fp, label);
            assert!(
                pf_seats::unlock::valid_device_name(&named.name),
                "{label:?}"
            );
            assert_eq!(named.fingerprint, fp);
        }
        assert_eq!(device(&fp, Some("Jane's iPhone")).name, "Jane's iPhone");
        assert_eq!(device(&fp, None).name, "device abababab");
        let long = device(&fp, Some(&"ä".repeat(80))).name;
        assert!(pf_seats::unlock::valid_device_name(&long));
    }

    /// A device counts as gone only on two answering looks in a row without it, once it landed
    /// or the landing is long overdue. A seat that can't say for too long ends the watch with
    /// no `Left`.
    #[test]
    fn a_device_leaves_only_once_its_seat_twice_misses_it() {
        let s = Duration::from_secs;
        let at = |here, landed, missed, waited, silent| {
            settle(here, landed, missed, s(waited), s(silent))
        };
        assert_eq!(at(Some(true), true, 0, 600, 0), None, "playing");
        assert_eq!(
            at(Some(false), true, 1, 600, 0),
            None,
            "one miss: a reconnect"
        );
        assert_eq!(at(Some(false), true, 2, 600, 0), Some(true), "gone");
        assert_eq!(at(Some(false), false, 5, 30, 0), None, "still dialling in");
        assert_eq!(at(Some(false), false, 5, 121, 0), Some(true), "never came");
        assert_eq!(at(None, true, 0, 600, 30), None, "a seat that blinked");
        assert_eq!(at(None, true, 0, 600, 61), Some(false), "a seat that hangs");
    }

    /// Only the last of a device's claims on a seat tells the supervisor it left; another
    /// device's or another seat's claim changes nothing.
    #[test]
    fn only_the_last_claim_sends_left() {
        let sup = Fake::new(Vec::new(), |_| Ok(CommandResult::Left));
        let seat = SeatId::parse("c1a1".repeat(8)).unwrap();
        let other = SeatId::parse("c1a2".repeat(8)).unwrap();
        let (me, them) = ("aa".repeat(32), "bb".repeat(32));
        let first = Claim::take(&seat, &me);
        let second = Claim::take(&seat, &me);
        let elsewhere = Claim::take(&other, &me);
        let theirs = Claim::take(&seat, &them);
        first.leave(&sup);
        assert!(sup.asked().is_empty(), "the second connect may still play");
        drop(theirs);
        drop(elsewhere);
        second.leave(&sup);
        assert_eq!(
            sup.asked(),
            [Command::Left {
                id: seat.clone(),
                fingerprint: me.clone()
            }]
        );
        let again = Claim::take(&seat, &me);
        drop(again);
        assert!(!claims().contains_key(&(seat.as_str().to_owned(), me)));
    }
}
