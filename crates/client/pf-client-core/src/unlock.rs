//! Opening a profile on a person's own account or a locked one: whether a dial prompts for the
//! account password or the profile's PIN, signs with this device's unlock key or goes as it is,
//! what the prompt sends, and what a refusal reopens. Both desktop shells drive these functions
//! and draw only the prompt; [`UnlockNeed::pin`] picks its form.
//!
//! The rules are pure, and `clients/shared/profile-unlock-vectors.json` pins them for Swift and
//! Kotlin. [`DeviceKeys`] is the platform's key store: Windows Hello on Windows, none on Linux.
//! An unlock built here reaches the session child through `orchestrate::spawn_child_with_unlock`
//! and nowhere else.

use crate::profiles::{ChallengeNames, UnlockChallenge, UnlockNeed};
use punktfunk_core::quic::v2::msg::{self, ProfileUnlock, Zeroizing};

/// What a shell does before it dials a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Dial with no unlock. The host may still close with `ProfileLocked`.
    Dial,
    /// Sign the host's challenge with this device's key (`key_unlock`). No proof means
    /// [`after_key_failure`].
    Key,
    /// The prompt, its form [`UnlockNeed::pin`]'s. `error` is the host's sentence for the line
    /// under the field.
    Prompt { error: Option<String> },
}

/// The gate for a profile `enumerate` lists with `need`. `holds_key`: this device has its
/// unlock key for the profile.
pub fn before_dial(need: UnlockNeed, holds_key: bool) -> Gate {
    match need {
        UnlockNeed::None => Gate::Dial,
        UnlockNeed::Key | UnlockNeed::PinKey if holds_key => Gate::Key,
        _ => Gate::Prompt { error: None },
    }
}

/// The host asks for the password or the PIN from a device that holds a key: it holds none for
/// the device, which it signed out.
pub fn stale_key(need: UnlockNeed, holds_key: bool) -> bool {
    holds_key && matches!(need, UnlockNeed::Password | UnlockNeed::Pin)
}

/// A key unlock gave no proof: no key, a cancelled sheet, a failed signature, no challenge.
pub fn after_key_failure() -> Gate {
    Gate::Prompt { error: None }
}

/// The prompt's error line for a `ProfileLocked` close that said `said`. Empty, or the
/// session's stand-in for an empty one, means "enter the password": no line.
pub fn refusal_line(said: &str) -> Option<String> {
    let stand_in = pf_l10n::host::reject_profile_locked();
    (!said.trim().is_empty() && said != stand_in).then(|| said.to_string())
}

/// Where a `ProfileLocked` close leads once the shell holds the profile's fresh row. A refused
/// unlock (`sent`) reopens the password prompt. A dial that carried none, such as the redial
/// after a lost stream, may unlock with this device's key first. Never a dial on its own.
pub fn after_refusal(
    need: UnlockNeed,
    key: Option<&DeviceKey>,
    sent: bool,
    error: Option<String>,
) -> Gate {
    match gate(need, key) {
        Gate::Key if !sent => Gate::Key,
        _ => Gate::Prompt { error },
    }
}

/// What a shell shows while the host holds a dial with `Pending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// The host's operator decides: the approval wait.
    Approval,
    /// The profile's desktop starts, which can take over a minute: a plain progress line.
    Starting,
}

/// A dial that carries an unlock waits for the desktop it starts, never for an approval.
pub fn pending(carries_unlock: bool) -> Pending {
    match carries_unlock {
        true => Pending::Starting,
        false => Pending::Approval,
    }
}

/// The progress line of [`Pending::Starting`].
pub fn starting_line(host: &str) -> String {
    pf_l10n::client::unlock_starting(host)
}

/// This device's unlock keys in the platform's secure hardware, one per name. Every call
/// blocks, so run them off the UI thread; `create` and `sign` show the system's sheet.
pub trait DeviceKeys {
    fn holds(&self, name: &str) -> bool;
    /// A new key under `name`, replacing one there, and its public half as a DER
    /// `RSAPublicKey`. `None`: none was made.
    fn create(&self, name: &str) -> Option<Vec<u8>>;
    /// RSA PKCS#1 v1.5 with SHA-256 over `message`. `None`: cancelled or failed.
    fn sign(&self, name: &str, message: &[u8]) -> Option<Vec<u8>>;
    fn delete(&self, name: &str);
}

/// This device's unlock key for one profile on one box: the store and the key's name.
pub struct DeviceKey<'a> {
    keys: &'a dyn DeviceKeys,
    name: String,
}

impl<'a> DeviceKey<'a> {
    /// The key for `profile_id` on the box pinned at `box_fp`. `None` for an id no host mints,
    /// which then gets no key at all.
    pub fn new(keys: &'a dyn DeviceKeys, box_fp: &[u8; 32], profile_id: &str) -> Option<Self> {
        is_profile_id(profile_id).then(|| DeviceKey {
            keys,
            name: format!(
                "punktfunk-unlock-{}-{profile_id}",
                crate::trust::hex(box_fp)
            ),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// An id `enumerate` can list: 1 to 64 of `[A-Za-z0-9_-]`. It names a key and a URL path.
pub fn is_profile_id(id: &str) -> bool {
    (1..=msg::PROFILE_ID_MAX).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// [`before_dial`], after deleting a key the host signed out ([`stale_key`]). Asks the store
/// only for a profile that needs an unlock.
pub fn gate(need: UnlockNeed, key: Option<&DeviceKey>) -> Gate {
    let holds = need != UnlockNeed::None && key.is_some_and(|k| k.keys.holds(&k.name));
    match key {
        Some(k) if stale_key(need, holds) => {
            k.keys.delete(&k.name);
            before_dial(need, false)
        }
        _ => before_dial(need, holds),
    }
}

/// Why the wire refuses `password`, as the prompt's error line. Never trimmed.
pub fn password_problem(password: &[u8]) -> Option<&'static str> {
    if password.is_empty() {
        Some(pf_l10n::client::unlock_error_empty())
    } else if password.len() > msg::UNLOCK_PASSWORD_MAX {
        Some(pf_l10n::client::unlock_error_too_long())
    } else if password.contains(&0) || std::str::from_utf8(password).is_err() {
        Some(pf_l10n::client::unlock_error_bad_char())
    } else {
        None
    }
}

/// The prompt's kind-1 unlock: `password` as typed. The switch `on` enrols a new key on a
/// device with `key`, else asks to stay signed in. A key that can't be made, or isn't RSA
/// 2048, is left out without a word. `Err` is the prompt's error line; no key is made then.
pub fn prompt_unlock(
    password: Zeroizing<Vec<u8>>,
    on: bool,
    key: Option<&DeviceKey>,
) -> Result<ProfileUnlock, &'static str> {
    if let Some(line) = password_problem(&password) {
        return Err(line);
    }
    secret_unlock(password, on, key).map_err(|_| pf_l10n::client::unlock_error_bad_char())
}

/// How many digits a profile's PIN has.
pub const PIN_LEN: std::ops::RangeInclusive<usize> = 4..=8;

/// `typed` as the PIN the wire carries: [`PIN_LEN`] ASCII digits, a full-width digit read as
/// its ASCII one. `Err` is the prompt's error line for anything else.
pub fn pin_digits(typed: &[u8]) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let shape = pf_l10n::client::unlock_pin_error_shape();
    if typed.is_empty() {
        return Err(pf_l10n::client::unlock_pin_error_empty());
    }
    let text = std::str::from_utf8(typed).map_err(|_| shape)?;
    // Sized for the longest PIN, so a push never moves it and leaves a copy behind.
    let mut pin = Zeroizing::new(Vec::with_capacity(*PIN_LEN.end()));
    for c in text.chars() {
        let digit = match c {
            '0'..='9' => c as u8,
            '\u{FF10}'..='\u{FF19}' => b'0' + (u32::from(c) - 0xFF10) as u8,
            _ => return Err(shape),
        };
        if pin.len() == *PIN_LEN.end() {
            return Err(shape);
        }
        pin.push(digit);
    }
    PIN_LEN.contains(&pin.len()).then_some(pin).ok_or(shape)
}

/// The PIN prompt's kind-1 unlock: [`pin_digits`] of `typed`, then [`prompt_unlock`]'s switch.
pub fn pin_unlock(
    typed: &[u8],
    on: bool,
    key: Option<&DeviceKey>,
) -> Result<ProfileUnlock, &'static str> {
    let pin = pin_digits(typed)?;
    secret_unlock(pin, on, key).map_err(|_| pf_l10n::client::unlock_pin_error_shape())
}

/// `secret` as a kind-1 unlock, with [`prompt_unlock`]'s switch. `Err` when the wire refuses it.
fn secret_unlock(
    secret: Zeroizing<Vec<u8>>,
    on: bool,
    key: Option<&DeviceKey>,
) -> Result<ProfileUnlock, ()> {
    let enrol = match key {
        Some(k) if on => match k.keys.create(&k.name) {
            Some(public) if is_rsa2048(&public) => Some(public),
            Some(_) => {
                k.keys.delete(&k.name);
                None
            }
            None => None,
        },
        _ => None,
    };
    let stay = on && key.is_none();
    let (alg, public) = match enrol {
        Some(public) => (msg::ENROLL_RSA2048, public),
        None => (msg::ENROLL_NONE, Vec::new()),
    };
    ProfileUnlock::new(msg::UNLOCK_PASSWORD, secret, stay, alg, public).map_err(|_| ())
}

/// The kind-2 unlock for `challenge` on profile `profile_id`: the message the challenge names,
/// a seat's or this profile's, signed by `key`. A challenge named for another profile signs
/// nothing. `None` sends the shell to [`after_key_failure`].
pub fn key_proof(
    (challenge, profile_id): (&UnlockChallenge, &str),
    device_fp: &[u8; 32],
    box_fp: &[u8; 32],
    key: &DeviceKey,
) -> Option<ProfileUnlock> {
    let c = &challenge.challenge;
    let message = match &challenge.names {
        ChallengeNames::Seat(seat) => msg::unlock_signed_message(c, seat, device_fp, box_fp)
            .ok()?
            .to_vec(),
        ChallengeNames::Profile(id) if id == profile_id => {
            msg::profile_unlock_signed_message(c, id, device_fp, box_fp)
                .ok()?
                .to_vec()
        }
        ChallengeNames::Profile(_) => return None,
    };
    let signature = key.keys.sign(&key.name, &message)?;
    let proof = Zeroizing::new(signature);
    ProfileUnlock::new(
        msg::UNLOCK_KEY_PROOF,
        proof,
        false,
        msg::ENROLL_NONE,
        Vec::new(),
    )
    .ok()
}

/// [`Gate::Key`]: a challenge from `api`'s host, then [`key_proof`]. `api` must pin the box.
/// Blocks; signing shows the system's sheet.
#[cfg(desktop)]
pub fn key_unlock(
    api: &crate::library::HostApi,
    profile_id: &str,
    key: &DeviceKey,
) -> Option<ProfileUnlock> {
    let box_fp = api.pin?;
    let device_fp = punktfunk_core::quic::endpoint::fingerprint_of_pem(&api.identity.0).ok()?;
    let challenge = crate::profiles::unlock_challenge(api, profile_id)
        .map_err(|e| tracing::info!(error = %e, "unlock challenge unavailable"))
        .ok()?;
    key_proof((&challenge, profile_id), &device_fp, &box_fp, key)
}

/// `der` is one DER `RSAPublicKey` with a 2048-bit modulus and nothing after it. The host
/// checks the key again; this keeps a key it would drop from being enrolled.
fn is_rsa2048(der: &[u8]) -> bool {
    fn tlv(tag: u8, b: &[u8]) -> Option<(&[u8], &[u8])> {
        let (&t, b) = b.split_first()?;
        let (&l, b) = b.split_first()?;
        let (len, b) = match l {
            0..=0x7f => (usize::from(l), b),
            0x81 => b.split_first().map(|(&n, b)| (usize::from(n), b))?,
            0x82 => b
                .split_at_checked(2)
                .map(|(n, b)| (usize::from(u16::from_be_bytes([n[0], n[1]])), b))?,
            _ => return None,
        };
        (t == tag).then_some(())?;
        b.split_at_checked(len)
    }
    let parsed = (|| {
        let (seq, rest) = tlv(0x30, der)?;
        let (n, seq) = tlv(0x02, seq)?;
        let (e, seq) = tlv(0x02, seq)?;
        Some((rest.is_empty() && seq.is_empty(), n, e))
    })();
    let Some((true, n, e)) = parsed else {
        return false;
    };
    let n = match n {
        [0, rest @ ..] => rest,
        _ => n,
    };
    n.len() == 256 && n[0] & 0x80 != 0 && e.first().is_some_and(|b| b & 0x80 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A key store that answers from its fields and logs every call.
    #[derive(Default)]
    struct Fake {
        held: bool,
        public: Option<Vec<u8>>,
        signature: Option<Vec<u8>>,
        calls: RefCell<Vec<String>>,
        signed: RefCell<Vec<u8>>,
    }

    impl DeviceKeys for Fake {
        fn holds(&self, name: &str) -> bool {
            self.calls.borrow_mut().push(format!("holds {name}"));
            self.held
        }
        fn create(&self, name: &str) -> Option<Vec<u8>> {
            self.calls.borrow_mut().push(format!("create {name}"));
            self.public.clone()
        }
        fn sign(&self, name: &str, message: &[u8]) -> Option<Vec<u8>> {
            self.calls.borrow_mut().push(format!("sign {name}"));
            *self.signed.borrow_mut() = message.to_vec();
            self.signature.clone()
        }
        fn delete(&self, name: &str) {
            self.calls.borrow_mut().push(format!("delete {name}"));
        }
    }

    const BOX: [u8; 32] = [0xbb; 32];

    fn key(keys: &Fake) -> DeviceKey<'_> {
        DeviceKey::new(keys, &BOX, "9a3f1c2b7e40").unwrap()
    }

    fn calls(keys: &Fake) -> Vec<String> {
        let name = format!("punktfunk-unlock-{}-9a3f1c2b7e40", "bb".repeat(32));
        keys.calls
            .borrow()
            .iter()
            .map(|c| c.replace(&name, "K"))
            .collect()
    }

    /// A DER `RSAPublicKey` whose modulus has `bits` bits.
    fn rsa(bits: usize) -> Vec<u8> {
        let len = |n: usize| match n {
            0..=0x7f => vec![n as u8],
            0x80..=0xff => vec![0x81, n as u8],
            _ => vec![0x82, (n >> 8) as u8, n as u8],
        };
        let int = |v: Vec<u8>| {
            let v = if v[0] & 0x80 != 0 {
                [vec![0], v].concat()
            } else {
                v
            };
            [vec![0x02], len(v.len()), v].concat()
        };
        let mut modulus = vec![0x5a; bits.div_ceil(8)];
        modulus[0] = 0x80 >> ((8 - bits % 8) % 8);
        let body = [int(modulus), int(vec![1, 0, 1])].concat();
        [vec![0x30], len(body.len()), body].concat()
    }

    fn pw(s: &str) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(s.as_bytes().to_vec())
    }

    #[test]
    fn a_row_reads_every_unlock_word() {
        use crate::profiles::ListedProfile;
        let row = |extra: serde_json::Value| -> ListedProfile {
            let mut v = serde_json::json!({"id": "kid"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(v).unwrap()
        };
        assert_eq!(row(serde_json::json!({})).unlock, UnlockNeed::None);
        for (word, need) in [
            ("none", UnlockNeed::None),
            ("password", UnlockNeed::Password),
            ("key", UnlockNeed::Key),
            ("passkey", UnlockNeed::Password),
        ] {
            assert_eq!(
                row(serde_json::json!({ "unlock": word })).unlock,
                need,
                "{word}"
            );
        }
    }

    #[test]
    fn a_dial_prompts_signs_or_goes() {
        use UnlockNeed::{Key, Password};
        let prompt = Gate::Prompt { error: None };
        for (need, holds, want) in [
            (UnlockNeed::None, false, Gate::Dial),
            (UnlockNeed::None, true, Gate::Dial),
            (Password, false, prompt.clone()),
            (Password, true, prompt.clone()),
            (Key, true, Gate::Key),
            (Key, false, prompt.clone()),
        ] {
            assert_eq!(before_dial(need, holds), want, "{need:?} holds {holds}");
            assert_eq!(stale_key(need, holds), need == Password && holds);
        }
        assert_eq!(after_key_failure(), prompt);
    }

    /// The store is asked only for a profile that needs an unlock, and a key the host
    /// signed out is deleted before the prompt.
    #[test]
    fn the_gate_deletes_a_key_the_host_signed_out() {
        let held = Fake {
            held: true,
            ..Fake::default()
        };
        assert_eq!(gate(UnlockNeed::None, Some(&key(&held))), Gate::Dial);
        assert!(calls(&held).is_empty());
        assert_eq!(gate(UnlockNeed::Key, Some(&key(&held))), Gate::Key);
        assert_eq!(calls(&held), ["holds K"]);
        held.calls.borrow_mut().clear();
        let prompt = Gate::Prompt { error: None };
        assert_eq!(gate(UnlockNeed::Password, Some(&key(&held))), prompt);
        assert_eq!(calls(&held), ["holds K", "delete K"]);

        let none = Fake::default();
        assert_eq!(gate(UnlockNeed::Key, Some(&key(&none))), prompt);
        assert_eq!(gate(UnlockNeed::Password, Some(&key(&none))), prompt);
        assert_eq!(calls(&none), ["holds K", "holds K"]);
        assert_eq!(gate(UnlockNeed::Key, None), prompt);
    }

    #[test]
    fn a_refusal_shows_the_hosts_sentence() {
        assert_eq!(
            refusal_line("Wrong password.").as_deref(),
            Some("Wrong password.")
        );
        let stand_in = "This profile is locked. Enter its password.";
        for empty in ["", "  ", stand_in] {
            assert_eq!(refusal_line(empty), None, "{empty:?}");
        }
    }

    /// A refused unlock always prompts. A dial that carried none, the redial after a lost
    /// stream among them, tries the device key first; nothing ever dials again by itself.
    #[test]
    fn a_refusal_prompts_or_tries_the_key_never_redials() {
        let held = Fake {
            held: true,
            ..Fake::default()
        };
        let line = || Some("Signed in at the machine.".to_string());
        let prompt = Gate::Prompt { error: line() };
        let k = key(&held);
        assert_eq!(
            after_refusal(UnlockNeed::Key, Some(&k), true, line()),
            prompt
        );
        assert_eq!(
            after_refusal(UnlockNeed::Key, Some(&k), false, line()),
            Gate::Key
        );
        assert_eq!(after_refusal(UnlockNeed::Key, None, false, line()), prompt);
        assert_eq!(
            after_refusal(UnlockNeed::None, Some(&k), false, line()),
            prompt
        );
        assert_eq!(
            after_refusal(UnlockNeed::Password, Some(&k), false, line()),
            prompt
        );
        assert_eq!(calls(&held).last().map(String::as_str), Some("delete K"));
    }

    /// `Pending` on a dial that carries an unlock is the desktop starting, not an approval.
    #[test]
    fn pending_on_an_unlock_dial_is_progress_not_approval() {
        assert_eq!(pending(true), Pending::Starting);
        assert_eq!(pending(false), Pending::Approval);
        assert_eq!(
            starting_line("Desk"),
            "Starting your desktop on Desk\u{2026}"
        );
    }

    #[test]
    fn the_prompt_stays_signed_in_where_a_device_has_no_keys() {
        for on in [false, true] {
            let u = prompt_unlock(pw(" tr0ub4dor &3 "), on, None).unwrap();
            assert_eq!(u.kind, msg::UNLOCK_PASSWORD);
            assert_eq!(u.secret.as_slice(), b" tr0ub4dor &3 ", "never trimmed");
            assert_eq!(u.stay_signed_in, on);
            assert_eq!((u.enroll_alg, u.enroll_key.len()), (msg::ENROLL_NONE, 0));
        }
    }

    /// With keys the switch enrols instead: a fresh RSA 2048 key rides the password, and a
    /// key that can't be made or has the wrong size is left out without a word.
    #[test]
    fn the_prompt_enrols_a_key_where_a_device_has_them() {
        let good = Fake {
            public: Some(rsa(2048)),
            ..Fake::default()
        };
        let u = prompt_unlock(pw("pw"), true, Some(&key(&good))).unwrap();
        assert_eq!(u.enroll_alg, msg::ENROLL_RSA2048);
        assert_eq!(u.enroll_key, rsa(2048));
        assert!(!u.stay_signed_in);
        assert_eq!(calls(&good), ["create K"]);

        let off = prompt_unlock(pw("pw"), false, Some(&key(&good))).unwrap();
        assert_eq!(off.enroll_alg, msg::ENROLL_NONE);
        assert!(!off.stay_signed_in);
        assert_eq!(calls(&good), ["create K"], "the switch off makes no key");

        let failed = Fake::default();
        let u = prompt_unlock(pw("pw"), true, Some(&key(&failed))).unwrap();
        assert_eq!((u.enroll_alg, u.stay_signed_in), (msg::ENROLL_NONE, false));

        let wrong = Fake {
            public: Some(rsa(4096)),
            ..Fake::default()
        };
        let u = prompt_unlock(pw("pw"), true, Some(&key(&wrong))).unwrap();
        assert_eq!(u.enroll_alg, msg::ENROLL_NONE);
        assert_eq!(calls(&wrong), ["create K", "delete K"]);
    }

    #[test]
    fn a_password_the_wire_refuses_makes_no_key() {
        let keys = Fake {
            public: Some(rsa(2048)),
            ..Fake::default()
        };
        let long = "x".repeat(msg::UNLOCK_PASSWORD_MAX + 1);
        for (bad, line) in [
            ("", "Enter the password."),
            (long.as_str(), "That password is too long."),
            (
                "a\0b",
                "That password has a character Punktfunk can't send.",
            ),
        ] {
            let got = prompt_unlock(pw(bad), true, Some(&key(&keys)));
            assert_eq!(got.err(), Some(line));
        }
        assert!(calls(&keys).is_empty());
        let longest = "x".repeat(msg::UNLOCK_PASSWORD_MAX);
        assert!(prompt_unlock(pw(&longest), false, None).is_ok());
    }

    /// The key signs the contract's message: its test vector hashes to the published value.
    #[test]
    fn a_key_proof_signs_the_contract_message() {
        let keys = Fake {
            signature: Some(vec![7; 256]),
            ..Fake::default()
        };
        let challenge = UnlockChallenge {
            challenge: std::array::from_fn(|i| i as u8 + 1),
            names: ChallengeNames::Seat("0123456789abcdef0123456789abcdef".into()),
        };
        let challenge = (&challenge, "9a3f1c2b7e40");
        let proof = key_proof(challenge, &[0xaa; 32], &BOX, &key(&keys)).unwrap();
        assert_eq!(proof.kind, msg::UNLOCK_KEY_PROOF);
        assert_eq!(proof.secret.as_slice(), &[7; 256][..]);
        let signed = keys.signed.borrow();
        assert_eq!(
            crate::trust::hex(&punktfunk_core::tls::cert_fingerprint(&signed)),
            "31de46821f3058729b14c3585ad13159085ecb0d18c87194a34b1bddc8ddc31a"
        );
        assert_eq!(calls(&keys), ["sign K"]);

        let cancelled = Fake::default();
        assert!(key_proof(challenge, &[0xaa; 32], &BOX, &key(&cancelled)).is_none());
        let other_seat = UnlockChallenge {
            names: ChallengeNames::Seat("not a seat".into()),
            ..challenge.0.clone()
        };
        let other_seat = (&other_seat, "9a3f1c2b7e40");
        assert!(key_proof(other_seat, &[0xaa; 32], &BOX, &key(&keys)).is_none());
    }

    fn vectors() -> serde_json::Value {
        let raw = include_str!("../../../../clients/shared/profile-unlock-vectors.json");
        serde_json::from_str(raw).unwrap()
    }

    fn need(word: &serde_json::Value) -> UnlockNeed {
        serde_json::from_value(word.clone()).unwrap()
    }

    fn gate_word(gate: &Gate) -> &'static str {
        match gate {
            Gate::Dial => "dial",
            Gate::Key => "key",
            Gate::Prompt { .. } => "prompt",
        }
    }

    /// `profile-unlock-vectors.json`: each word as its need and the prompt's form, and the gate
    /// before a dial and after a refusal, through the key store as the shells call it.
    #[test]
    fn the_gate_follows_the_shared_vectors() {
        use crate::profiles::ListedProfile;
        let v = vectors();
        for case in v["unlock_words"].as_array().unwrap() {
            let mut row = serde_json::json!({"id": "kid"});
            if !case["word"].is_null() {
                row["unlock"] = case["word"].clone();
            }
            let read = serde_json::from_value::<ListedProfile>(row).unwrap().unlock;
            assert_eq!(read, need(&case["need"]), "{case}");
            let form = match read {
                UnlockNeed::None => serde_json::Value::Null,
                n => (if n.pin() { "pin" } else { "password" }).into(),
            };
            assert_eq!(form, case["form"], "{case}");
        }
        for case in v["gate"].as_array().unwrap() {
            let (n, holds) = (need(&case["need"]), case["holds_key"].as_bool().unwrap());
            assert_eq!(gate_word(&before_dial(n, holds)), case["gate"], "{case}");
            assert_eq!(stale_key(n, holds), case["delete_key"], "{case}");
            let keys = Fake {
                held: holds,
                ..Fake::default()
            };
            assert_eq!(
                gate_word(&gate(n, Some(&key(&keys)))),
                case["gate"],
                "{case}"
            );
            let deleted = calls(&keys).iter().any(|c| c == "delete K");
            assert_eq!(deleted, case["delete_key"], "{case}");
        }
        for case in v["after_refusal"].as_array().unwrap() {
            let (n, holds) = (need(&case["need"]), case["holds_key"].as_bool().unwrap());
            let keys = Fake {
                held: holds,
                ..Fake::default()
            };
            let sent = case["sent"].as_bool().unwrap();
            let got = after_refusal(n, Some(&key(&keys)), sent, None);
            assert_eq!(gate_word(&got), case["gate"], "{case}");
        }
    }

    /// The key signs each message of the vectors, a seat's and a locked profile's, and nothing
    /// for a challenge named for another profile.
    #[test]
    fn a_key_proof_signs_the_shared_vectors() {
        let b32 = |s: &serde_json::Value| crate::trust::parse_hex32(s.as_str().unwrap()).unwrap();
        for case in vectors()["messages"].as_array().unwrap() {
            let keys = Fake {
                signature: Some(vec![7; 72]),
                ..Fake::default()
            };
            let form = case["form"].as_str().unwrap();
            let id = case[form].as_str().unwrap().to_string();
            let (names, profile) = match form {
                "seat" => (ChallengeNames::Seat(id), "9a3f1c2b7e40".to_string()),
                _ => (ChallengeNames::Profile(id.clone()), id),
            };
            let challenge = UnlockChallenge {
                challenge: b32(&case["challenge"]),
                names,
            };
            let (device, host) = (b32(&case["device"]), b32(&case["box"]));
            let proof = key_proof((&challenge, &profile), &device, &host, &key(&keys));
            assert_eq!(proof.unwrap().kind, msg::UNLOCK_KEY_PROOF);
            let signed = punktfunk_core::tls::cert_fingerprint(&keys.signed.borrow());
            assert_eq!(
                crate::trust::hex(&signed),
                case["sha256"],
                "{}",
                case["name"]
            );
        }
        let keys = Fake {
            signature: Some(vec![7; 72]),
            ..Fake::default()
        };
        let other = UnlockChallenge {
            challenge: [1; 32],
            names: ChallengeNames::Profile("0123456789ab".into()),
        };
        let theirs = (&other, "9a3f1c2b7e40");
        assert!(key_proof(theirs, &[2; 32], &BOX, &key(&keys)).is_none());
        assert!(
            calls(&keys).is_empty(),
            "no sheet for another profile's challenge"
        );
    }

    /// The PIN field's text as the wire carries it, or the line the prompt shows; the switch
    /// stays signed in or enrols as the password prompt's does.
    #[test]
    fn a_pin_follows_the_shared_vectors() {
        for case in vectors()["pins"].as_array().unwrap() {
            let got = pin_digits(case["typed"].as_str().unwrap().as_bytes());
            match case["error"].as_str() {
                None => assert_eq!(
                    got.unwrap().as_slice(),
                    case["sent"].as_str().unwrap().as_bytes()
                ),
                Some("empty") => assert_eq!(got.err(), Some("Enter the PIN."), "{case}"),
                Some(_) => assert_eq!(got.err(), Some("A PIN is 4 to 8 digits."), "{case}"),
            }
        }
        let u = pin_unlock("２４６８".as_bytes(), true, None).unwrap();
        assert_eq!(u.kind, msg::UNLOCK_PASSWORD);
        assert_eq!(
            (u.secret.as_slice(), u.stay_signed_in),
            (&b"2468"[..], true)
        );
        let keys = Fake {
            public: Some(rsa(2048)),
            ..Fake::default()
        };
        assert!(pin_unlock(b"24a8", true, Some(&key(&keys))).is_err());
        assert!(
            calls(&keys).is_empty(),
            "a PIN the prompt refuses makes no key"
        );
        let u = pin_unlock(b"2468", true, Some(&key(&keys))).unwrap();
        assert_eq!(
            (u.enroll_alg, u.stay_signed_in),
            (msg::ENROLL_RSA2048, false)
        );
        assert_eq!(calls(&keys), ["create K"]);
    }

    /// Each challenge answer of the vectors names a seat, a profile, or is refused.
    #[test]
    fn a_challenge_answer_follows_the_shared_vectors() {
        for case in vectors()["challenge_answers"].as_array().unwrap() {
            let parsed = UnlockChallenge::parse(case["body"].to_string().as_bytes());
            let names = match (case["seat"].as_str(), case["profile"].as_str()) {
                (Some(seat), _) => Some(ChallengeNames::Seat(seat.into())),
                (_, Some(id)) => Some(ChallengeNames::Profile(id.into())),
                _ => None,
            };
            assert_eq!(parsed.map(|c| c.names), names, "{case}");
        }
    }

    #[test]
    fn a_key_is_named_for_its_box_and_profile() {
        let keys = Fake::default();
        assert_eq!(
            key(&keys).name(),
            format!("punktfunk-unlock-{}-9a3f1c2b7e40", "bb".repeat(32))
        );
        let long = "a".repeat(65);
        for bad in ["", "../x", "kid desk", "k\u{e4}", long.as_str()] {
            assert!(DeviceKey::new(&keys, &BOX, bad).is_none(), "{bad:?}");
        }
        assert!(is_profile_id("operator") && is_profile_id(&long[..64]));
    }

    #[test]
    fn only_a_whole_rsa_2048_key_is_enrolled() {
        assert!(is_rsa2048(&rsa(2048)));
        for bits in [2047, 2049, 1024, 4096] {
            assert!(!is_rsa2048(&rsa(bits)), "{bits}");
        }
        let mut trailing = rsa(2048);
        trailing.push(0);
        let mut tag = rsa(2048);
        tag[0] = 0x31;
        for bad in [trailing, tag, Vec::new(), vec![0x30, 0x82]] {
            assert!(!is_rsa2048(&bad));
        }
    }

    #[test]
    fn a_challenge_answer_is_checked_before_use() {
        let ok = format!(
            r#"{{"challenge":"{}","seat":"{}"}}"#,
            "0a".repeat(32),
            "c".repeat(32)
        );
        let c = UnlockChallenge::parse(ok.as_bytes()).unwrap();
        assert_eq!(c.challenge, [0x0a; 32]);
        assert_eq!(c.names, ChallengeNames::Seat("c".repeat(32)));
        for bad in [
            format!(
                r#"{{"challenge":"{}","seat":"{}"}}"#,
                "0A".repeat(32),
                "c".repeat(32)
            ),
            format!(
                r#"{{"challenge":"{}","seat":"{}"}}"#,
                "0a".repeat(31),
                "c".repeat(32)
            ),
            format!(
                r#"{{"challenge":"{}","seat":"{}"}}"#,
                "0a".repeat(32),
                "C".repeat(32)
            ),
            format!(
                r#"{{"challenge":"{}","seat":"{}"}}"#,
                "0a".repeat(32),
                "c".repeat(31)
            ),
            r#"{"challenge":"x"}"#.to_string(),
        ] {
            assert!(UnlockChallenge::parse(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    /// A stopped seat that needs an unlock dials; the unlock starts it.
    #[test]
    fn a_locked_stopped_seat_dials_instead_of_waking() {
        use crate::profiles::{seat_gate, ListedProfile, SeatGate};
        let row = |unlock: &str| -> ListedProfile {
            serde_json::from_value(serde_json::json!({
                "id": "kid", "seat": { "state": "stopped" }, "unlock": unlock,
            }))
            .unwrap()
        };
        assert_eq!(seat_gate(&row("none")), SeatGate::Wake);
        for word in ["password", "key", "pin", "pin_key"] {
            assert_eq!(seat_gate(&row(word)), SeatGate::Dial, "{word}");
        }
    }
}
