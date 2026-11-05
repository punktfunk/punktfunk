//! Who may open a seat on a person's own account: the devices that proved the person is there,
//! and the proofs they give. Pure: the caller passes the time.
//!
//! A password or key proof makes its device present: while it is connected, until [`GRACE_SECS`]
//! after the door reports it left, and never past [`CAP_SECS`] from the unlock. Every presence
//! ends with the session. `stay` lets a device rejoin a running session with no proof; a device
//! with a `key` is admitted only while present, so a copy of its pairing key alone never reaches
//! the seat. A key proof signs [`message`], over a challenge [`Challenges`] hands out once. The
//! Linux backend keeps the list in `seats/<id>/devices.json`.

use crate::ipc::{Device, DeviceEntry, DeviceKind, KeyAlg, Needs, UnlockKey};
use crate::model::SeatId;
use aws_lc_rs::signature::{
    ParsedPublicKey, RsaParameters, VerificationAlgorithm, ECDSA_P256_SHA256_ASN1,
    RSA_PKCS1_2048_8192_SHA256, RSA_PSS_2048_8192_SHA256,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// How long a device stays present after it left: a reconnect needs no proof.
pub const GRACE_SECS: i64 = 600;
/// How long a presence lasts from its unlock, connected or not.
pub const CAP_SECS: i64 = 24 * 60 * 60;
/// Devices a row lists; the file stays far below its read cap.
pub const MAX_DEVICES: usize = 64;
/// How long a challenge may be answered.
pub const CHALLENGE_LIFE: Duration = Duration::from_secs(120);
const MAX_NAME_CHARS: usize = 64;
const MAX_RSA_KEY_BYTES: usize = 600;
const MAX_SIGNATURE_BYTES: usize = 1024;
/// What a key proof signs first (CONTRACT §3).
const CONTEXT: &[u8; 24] = b"punktfunk-seat-unlock-v1";

/// A person's row's devices, as `devices.json` holds them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    #[serde(default)]
    pub devices: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub fingerprint: String,
    pub name: String,
    #[serde(default)]
    pub stay: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<UnlockKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<Presence>,
    pub added_unix: i64,
    pub last_unix: i64,
}

/// A device's presence: from its last password or key proof, with no end while it is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub unlocked_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
}

impl Presence {
    fn live(&self, now: i64) -> bool {
        now.saturating_sub(self.unlocked_unix) < CAP_SECS
            && self.expires_unix.is_none_or(|end| now < end)
    }
}

impl Entry {
    fn admitted(&self) -> bool {
        self.present.is_some() || (self.stay && self.key.is_none())
    }

    fn kind(&self) -> DeviceKind {
        match (&self.key, self.stay) {
            (Some(_), _) => DeviceKind::Key,
            (None, true) => DeviceKind::Stay,
            (None, false) => DeviceKind::ForNow,
        }
    }
}

impl DeviceList {
    /// Whether every entry is one this supervisor would have written: a file that fails this is
    /// read as no device.
    pub fn valid(&self) -> bool {
        let mut seen = BTreeSet::new();
        self.devices.len() <= MAX_DEVICES
            && self.devices.iter().all(|e| {
                valid_fingerprint(&e.fingerprint)
                    && valid_device_name(&e.name)
                    && seen.insert(e.fingerprint.as_str())
                    && e.key.as_ref().is_none_or(|key| key_bytes(key).is_some())
            })
    }

    /// Ends each presence past its window and drops each entry left with nothing to keep it.
    /// `true` when anything changed. Every other method reads the list as of the last call.
    pub fn expire(&mut self, now: i64) -> bool {
        let before = self.clone();
        for entry in &mut self.devices {
            entry.present = entry.present.filter(|p| p.live(now));
        }
        self.prune();
        *self != before
    }

    fn prune(&mut self) {
        self.devices
            .retain(|e| e.stay || e.key.is_some() || e.present.is_some());
    }

    fn get(&self, fingerprint: &str) -> Option<&Entry> {
        self.devices.iter().find(|e| e.fingerprint == fingerprint)
    }

    pub fn key_of(&self, fingerprint: &str) -> Option<&UnlockKey> {
        self.get(fingerprint)?.key.as_ref()
    }

    /// Whether [`Self::grant`] has room for `fingerprint`: it is listed, or the list isn't full.
    pub fn fits(&self, fingerprint: &str) -> bool {
        self.get(fingerprint).is_some() || self.devices.len() < MAX_DEVICES
    }

    pub fn admitted(&self) -> BTreeSet<String> {
        let admitted = self.devices.iter().filter(|e| e.admitted());
        admitted.map(|e| e.fingerprint.clone()).collect()
    }

    pub fn needs(&self, fingerprint: &str, running: bool) -> Needs {
        match self.get(fingerprint) {
            Some(entry) if running && entry.admitted() => Needs::None,
            Some(entry) if entry.key.is_some() => Needs::Key,
            _ => Needs::Password,
        }
    }

    /// `device` present from `now`, after a password or key proof: `stay` set when asked, `key`
    /// stored when given. `false`, changing nothing, when the list is full without it.
    pub fn grant(&mut self, device: &Device, now: i64, stay: bool, key: Option<UnlockKey>) -> bool {
        if !self.fits(&device.fingerprint) {
            return false;
        }
        let at = self
            .devices
            .iter()
            .position(|e| e.fingerprint == device.fingerprint);
        let entry = match at {
            Some(at) => &mut self.devices[at],
            None => {
                self.devices.push(Entry {
                    fingerprint: device.fingerprint.clone(),
                    name: String::new(),
                    stay: false,
                    key: None,
                    present: None,
                    added_unix: now,
                    last_unix: now,
                });
                self.devices.last_mut().expect("just pushed")
            }
        };
        entry.name.clone_from(&device.name);
        entry.stay |= stay;
        if key.is_some() {
            entry.key = key;
        }
        entry.present = Some(Presence {
            unlocked_unix: now,
            expires_unix: None,
        });
        entry.last_unix = now;
        true
    }

    /// Whether `fingerprint` is admitted; a present one is connected again, so its presence has
    /// no end but its cap.
    pub fn rejoin(&mut self, fingerprint: &str, now: i64) -> bool {
        let Some(entry) = self
            .devices
            .iter_mut()
            .find(|e| e.fingerprint == fingerprint)
            .filter(|e| e.admitted())
        else {
            return false;
        };
        if let Some(presence) = &mut entry.present {
            presence.expires_unix = None;
        }
        entry.last_unix = now;
        true
    }

    /// `fingerprint` disconnected: its presence ends [`GRACE_SECS`] from `now`, or sooner if it
    /// already ends sooner.
    pub fn left(&mut self, fingerprint: &str, now: i64) {
        let found = self
            .devices
            .iter_mut()
            .find(|e| e.fingerprint == fingerprint);
        if let Some(presence) = found.and_then(|e| e.present.as_mut()) {
            let end = now.saturating_add(GRACE_SECS);
            presence.expires_unix = Some(presence.expires_unix.map_or(end, |e| e.min(end)));
        }
    }

    /// The session ended: no device is present any more.
    pub fn end_presence(&mut self) {
        for entry in &mut self.devices {
            entry.present = None;
        }
        self.prune();
    }

    /// Forgets `fingerprint`, its key and its stay included, or every device for `None`.
    pub fn sign_out(&mut self, fingerprint: Option<&str>) {
        self.devices
            .retain(|e| fingerprint.is_some_and(|fp| e.fingerprint != fp));
    }

    pub fn entries(&self) -> Vec<DeviceEntry> {
        let entry = |e: &Entry| DeviceEntry {
            fingerprint: e.fingerprint.clone(),
            name: e.name.clone(),
            kind: e.kind(),
            added_unix: e.added_unix,
            last_unix: e.last_unix,
        };
        self.devices.iter().map(entry).collect()
    }
}

/// What one unlock's audit line says. No field holds, or is ever built from, the secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditLine {
    pub seat: SeatId,
    pub account: String,
    pub uid: Option<u32>,
    /// The device's fingerprint, or `malformed` for one that isn't one.
    pub device: String,
    /// The proof's wire name: `none`, `password` or `key`.
    pub proof: &'static str,
    /// `unlocked`, or the refusal's code.
    pub result: String,
    pub device_failures: u32,
    pub account_failures: usize,
}

impl std::fmt::Display for AuditLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let uid = self.uid.map_or("-".to_owned(), |uid| uid.to_string());
        write!(
            f,
            "seat={} account={:?} uid={uid} device={} proof={} result={} \
             device_failures={} account_failures={}",
            self.seat,
            self.account,
            self.device,
            self.proof,
            self.result,
            self.device_failures,
            self.account_failures
        )
    }
}

/// One live challenge per seat and device, each answerable once within [`CHALLENGE_LIFE`].
#[derive(Debug, Default)]
pub(crate) struct Challenges(HashMap<(SeatId, String), ([u8; 32], Instant)>);

impl Challenges {
    /// `bytes` as `fingerprint`'s challenge on `seat` from `now`, in place of any before it.
    pub(crate) fn mint(&mut self, seat: &SeatId, fingerprint: &str, bytes: [u8; 32], now: Instant) {
        self.0
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < CHALLENGE_LIFE);
        let key = (seat.clone(), fingerprint.to_owned());
        self.0.insert(key, (bytes, now));
    }

    /// `fingerprint`'s challenge on `seat`, used up whether or not it is still live.
    pub(crate) fn take(
        &mut self,
        seat: &SeatId,
        fingerprint: &str,
        now: Instant,
    ) -> Option<[u8; 32]> {
        let (bytes, at) = self.0.remove(&(seat.clone(), fingerprint.to_owned()))?;
        (now.saturating_duration_since(at) < CHALLENGE_LIFE).then_some(bytes)
    }

    /// Drops `seat`'s challenge for `fingerprint`, or all of `seat`'s for `None`.
    pub(crate) fn forget(&mut self, seat: &SeatId, fingerprint: Option<&str>) {
        self.0
            .retain(|(s, fp), _| s != seat || fingerprint.is_some_and(|f| f != fp));
    }
}

/// The 152 bytes a key proof signs: [`CONTEXT`], the challenge, the seat id's 32 hex characters,
/// the device's and the box's certificate fingerprints.
pub fn message(
    challenge: &[u8; 32],
    seat: &SeatId,
    device: &[u8; 32],
    box_fp: &[u8; 32],
) -> [u8; 152] {
    let mut out = [0_u8; 152];
    out[..24].copy_from_slice(CONTEXT);
    out[24..56].copy_from_slice(challenge);
    out[56..88].copy_from_slice(seat.as_str().as_bytes());
    out[88..120].copy_from_slice(device);
    out[120..].copy_from_slice(box_fp);
    out
}

/// 64 lowercase hex characters: a certificate's SHA-256.
pub fn valid_fingerprint(text: &str) -> bool {
    text.len() == 64 && lower_hex(text, 32).is_some()
}

/// `text` as the 32 bytes of a [`valid_fingerprint`].
pub fn fingerprint_bytes(text: &str) -> Option<[u8; 32]> {
    lower_hex(text, 32)?.try_into().ok()
}

/// 1 to 64 characters, none a control or a bidirectional mark that could reorder what is shown.
pub fn valid_device_name(name: &str) -> bool {
    let spoofy = |c: char| {
        matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
            || matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}')
    };
    (1..=MAX_NAME_CHARS).contains(&name.chars().count())
        && !name.chars().any(|c| c.is_control() || spoofy(c))
}

/// A signature's bytes from lowercase hex, 1 to 1024 of them.
pub fn signature_bytes(text: &str) -> Option<Vec<u8>> {
    lower_hex(text, MAX_SIGNATURE_BYTES)
}

/// The bytes of 1 to `max` from lowercase hex; the length is checked before anything is decoded.
fn lower_hex(text: &str, max: usize) -> Option<Vec<u8>> {
    let digit = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let fits = !text.is_empty() && text.len() <= max * 2 && text.len().is_multiple_of(2);
    (fits && text.bytes().all(digit)).then(|| hex::decode(text).ok())?
}

/// `key`'s public bytes when its algorithm takes them: an uncompressed P-256 point on the curve,
/// or a DER `RSAPublicKey` of exactly 2048 bits. The size is checked before any parse.
pub fn key_bytes(key: &UnlockKey) -> Option<Vec<u8>> {
    let (bytes, alg): (Vec<u8>, &'static dyn VerificationAlgorithm) = match key.alg {
        KeyAlg::P256 => {
            let bytes = lower_hex(&key.public, 65).filter(|b| b.len() == 65 && b[0] == 4)?;
            (bytes, &ECDSA_P256_SHA256_ASN1)
        }
        KeyAlg::Rsa2048 => {
            let bytes = lower_hex(&key.public, MAX_RSA_KEY_BYTES)?;
            if RsaParameters::public_modulus_len(&bytes).ok()? != 2048 {
                return None;
            }
            (bytes, &RSA_PKCS1_2048_8192_SHA256)
        }
    };
    ParsedPublicKey::new(alg, &bytes).ok()?;
    Some(bytes)
}

/// Whether a signature of `len` bytes can be `alg`'s: an ECDSA P-256 DER signature is 8 to 72
/// bytes, an RSA 2048 one exactly 256.
fn signature_fits(alg: KeyAlg, len: usize) -> bool {
    match alg {
        KeyAlg::P256 => (8..=72).contains(&len),
        KeyAlg::Rsa2048 => len == 256,
    }
}

/// Whether `signature` is `key`'s over `message`: ECDSA P-256, or RSA with SHA-256 as PKCS#1
/// v1.5 or PSS (salt as long as the digest). A key or a signature of the wrong size is refused
/// before any verification runs.
pub fn verify(key: &UnlockKey, message: &[u8], signature: &[u8]) -> bool {
    if !signature_fits(key.alg, signature.len()) {
        return false;
    }
    let Some(public) = key_bytes(key) else {
        return false;
    };
    let verifies = |alg: &'static dyn VerificationAlgorithm| {
        ParsedPublicKey::new(alg, &public).is_ok_and(|k| k.verify_sig(message, signature).is_ok())
    };
    match key.alg {
        KeyAlg::P256 => verifies(&ECDSA_P256_SHA256_ASN1),
        // Windows Hello's documentation names both paddings; either is the same key's proof.
        KeyAlg::Rsa2048 => {
            verifies(&RSA_PKCS1_2048_8192_SHA256) || verifies(&RSA_PSS_2048_8192_SHA256)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair as _, ECDSA_P256_SHA256_ASN1_SIGNING};

    const NOW: i64 = 1_800_000_000;

    fn device(n: u8) -> Device {
        Device {
            fingerprint: hex::encode([n; 32]),
            name: format!("Device {n}"),
        }
    }

    /// The vector of CONTRACT §3: every implementation of the message carries it.
    #[test]
    fn the_signed_message_matches_the_contract_vector() {
        let challenge: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
        let seat = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let signed = message(&challenge, &seat, &[0xaa; 32], &[0xbb; 32]);
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &signed);
        assert_eq!(
            hex::encode(digest.as_ref()),
            "31de46821f3058729b14c3585ad13159085ecb0d18c87194a34b1bddc8ddc31a"
        );
    }

    /// A password or key proof makes a device present; `left` ends it 600 s later, a rejoin in
    /// that window clears the end again, and nothing outlives 24 h from the unlock.
    #[test]
    fn presence_lasts_while_connected_plus_ten_minutes_and_at_most_a_day() {
        let mut list = DeviceList::default();
        let phone = device(1);
        assert!(list.grant(&phone, NOW, false, None));
        assert_eq!(list.admitted(), BTreeSet::from([phone.fingerprint.clone()]));
        list.left(&phone.fingerprint, NOW + 10);
        assert!(!list.expire(NOW + 609));
        assert!(list.expire(NOW + 610), "gone 600 s after it left");
        assert!(list.devices.is_empty(), "nothing keeps a for-now device");

        list.grant(&phone, NOW, false, None);
        list.left(&phone.fingerprint, NOW + 10);
        list.left(&phone.fingerprint, NOW + 300);
        assert_eq!(
            list.devices[0].present.unwrap().expires_unix,
            Some(NOW + 610),
            "a second left never extends"
        );
        assert!(list.rejoin(&phone.fingerprint, NOW + 500));
        assert!(
            !list.expire(NOW + 5000),
            "connected again: no end but the cap"
        );
        assert!(!list.expire(NOW + CAP_SECS - 1));
        assert!(list.expire(NOW + CAP_SECS));
        assert!(list.admitted().is_empty());
    }

    /// Stay keeps a device admitted with no presence, until it is signed out. A key keeps it
    /// listed but admitted only while present, whatever `stay` says.
    #[test]
    fn stay_admits_without_presence_and_a_key_never_does() {
        let mut list = DeviceList::default();
        let (tv, phone, laptop) = (device(1), device(2), device(3));
        let key = UnlockKey {
            alg: KeyAlg::P256,
            public: "04".repeat(65),
        };
        list.grant(&tv, NOW, true, None);
        list.grant(&phone, NOW, false, Some(key.clone()));
        list.grant(&laptop, NOW, true, Some(key));
        list.end_presence();
        assert_eq!(list.admitted(), BTreeSet::from([tv.fingerprint.clone()]));
        assert_eq!(list.devices.len(), 3, "stay and key outlive the session");
        assert_eq!(list.needs(&tv.fingerprint, true), Needs::None);
        assert_eq!(list.needs(&tv.fingerprint, false), Needs::Password);
        assert_eq!(list.needs(&phone.fingerprint, true), Needs::Key);
        assert_eq!(list.needs(&laptop.fingerprint, true), Needs::Key);
        assert_eq!(list.needs(&device(9).fingerprint, true), Needs::Password);
        assert!(!list.rejoin(&phone.fingerprint, NOW));
        let kinds: Vec<_> = list.entries().into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [DeviceKind::Stay, DeviceKind::Key, DeviceKind::Key]);

        list.sign_out(Some(&tv.fingerprint));
        assert!(list.admitted().is_empty());
        assert_eq!(list.devices.len(), 2);
        list.sign_out(None);
        assert!(list.devices.is_empty());
    }

    /// A later unlock keeps what the first one set, renames the device and moves its last time;
    /// the list holds no more than [`MAX_DEVICES`].
    #[test]
    fn a_grant_updates_its_entry_and_the_list_is_capped() {
        let mut list = DeviceList::default();
        let mut tv = device(1);
        list.grant(&tv, NOW, true, None);
        tv.name = "Living room".into();
        list.grant(&tv, NOW + 5, false, None);
        let entry = &list.devices[0];
        assert!(entry.stay, "stay holds until a sign-out");
        assert_eq!(
            (entry.name.as_str(), entry.added_unix, entry.last_unix),
            ("Living room", NOW, NOW + 5)
        );
        for n in 2..=MAX_DEVICES as u8 {
            assert!(list.grant(&device(n), NOW, false, None));
        }
        assert!(!list.grant(&device(200), NOW, false, None));
        assert_eq!(list.devices.len(), MAX_DEVICES);
        assert!(
            list.grant(&tv, NOW, false, None),
            "a listed device still unlocks"
        );
    }

    #[test]
    fn a_challenge_is_answered_once_and_within_two_minutes() {
        let mut live = Challenges::default();
        let (seat, t0) = (SeatId::random(), Instant::now());
        let fp = device(1).fingerprint;
        live.mint(&seat, &fp, [7; 32], t0);
        assert_eq!(
            live.take(&seat, &fp, t0 + Duration::from_secs(119)),
            Some([7; 32])
        );
        assert_eq!(live.take(&seat, &fp, t0), None, "used up");

        live.mint(&seat, &fp, [1; 32], t0);
        live.mint(&seat, &fp, [2; 32], t0);
        assert_eq!(
            live.take(&seat, &fp, t0),
            Some([2; 32]),
            "one live per device"
        );
        live.mint(&seat, &fp, [3; 32], t0);
        assert_eq!(live.take(&seat, &fp, t0 + CHALLENGE_LIFE), None, "expired");
        assert_eq!(live.take(&seat, &fp, t0), None, "and used up by the try");

        live.mint(&seat, &fp, [4; 32], t0);
        assert_eq!(
            live.take(&SeatId::random(), &fp, t0),
            None,
            "another seat's"
        );
        live.forget(&seat, None);
        assert_eq!(live.take(&seat, &fp, t0), None);
    }

    /// Only lowercase hex of the right length crosses; sizes are refused before anything parses.
    #[test]
    fn fingerprints_names_keys_and_signatures_are_checked_before_use() {
        let fp = "ab".repeat(32);
        assert!(valid_fingerprint(&fp));
        for bad in [
            "",
            &"ab".repeat(31),
            &"AB".repeat(32),
            &format!("{fp}0"),
            &"zz".repeat(32),
        ] {
            assert!(!valid_fingerprint(bad), "{bad}");
        }
        assert!(valid_device_name("Jane's iPhone"));
        assert!(valid_device_name(&"ä".repeat(64)));
        for bad in [
            String::new(),
            "ä".repeat(65),
            "TV\n".into(),
            "a\u{202E}b".into(),
        ] {
            assert!(!valid_device_name(&bad), "{bad:?}");
        }
        assert_eq!(
            signature_bytes(&"00".repeat(1024)).map(|s| s.len()),
            Some(1024)
        );
        assert!(signature_bytes(&"00".repeat(1025)).is_none());
        assert!(signature_bytes("").is_none());
        assert!(!signature_fits(KeyAlg::Rsa2048, 255) && !signature_fits(KeyAlg::Rsa2048, 257));
        assert!(!signature_fits(KeyAlg::P256, 73) && !signature_fits(KeyAlg::P256, 7));

        let point = |len: usize| UnlockKey {
            alg: KeyAlg::P256,
            public: format!("04{}", "11".repeat(len - 1)),
        };
        for len in [64, 65, 66] {
            assert!(
                key_bytes(&point(len)).is_none(),
                "{len} bytes, off the curve"
            );
        }
    }

    /// A key proof verifies only over the exact message it signed, with the key that signed it.
    #[test]
    fn a_p256_proof_verifies_over_its_own_message_only() {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
        let key = UnlockKey {
            alg: KeyAlg::P256,
            public: hex::encode(pair.public_key().as_ref()),
        };
        assert_eq!(key_bytes(&key).map(|k| k.len()), Some(65));
        let seat = SeatId::random();
        let signed = message(&[1; 32], &seat, &[2; 32], &[3; 32]);
        let signature = pair.sign(&rng, &signed).unwrap();
        assert!(verify(&key, &signed, signature.as_ref()));
        let other = message(&[1; 32], &SeatId::random(), &[2; 32], &[3; 32]);
        assert!(!verify(&key, &other, signature.as_ref()));
        let stranger = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
        let theirs = stranger.sign(&rng, &signed).unwrap();
        assert!(!verify(&key, &signed, theirs.as_ref()));
        let mut truncated = signature.as_ref().to_vec();
        truncated.pop();
        assert!(!verify(&key, &signed, &truncated));
    }

    /// RSA takes a PKCS#1 key of exactly 2048 bits, never another size or a wrapped one.
    #[test]
    fn an_rsa_key_is_exactly_2048_bits() {
        use aws_lc_rs::rsa::{KeyPair, KeySize};
        use aws_lc_rs::signature::RSA_PKCS1_SHA256;
        let rsa = |size| {
            let pair = KeyPair::generate(size).unwrap();
            let public = hex::encode(pair.public_key().as_ref());
            (
                pair,
                UnlockKey {
                    alg: KeyAlg::Rsa2048,
                    public,
                },
            )
        };
        let (pair, key) = rsa(KeySize::Rsa2048);
        assert!(key_bytes(&key).is_some());
        let signed = message(&[1; 32], &SeatId::random(), &[2; 32], &[3; 32]);
        let mut signature = vec![0; pair.public_modulus_len()];
        pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            &signed,
            &mut signature,
        )
        .unwrap();
        assert!(verify(&key, &signed, &signature));
        signature[0] ^= 1;
        assert!(!verify(&key, &signed, &signature));

        let (_, bigger) = rsa(KeySize::Rsa3072);
        assert!(key_bytes(&bigger).is_none(), "3072 bits");
        let as_p256 = UnlockKey {
            alg: KeyAlg::P256,
            ..key.clone()
        };
        assert!(key_bytes(&as_p256).is_none());
        let oversized = UnlockKey {
            public: format!("{}00", key.public).repeat(2),
            ..key
        };
        assert!(key_bytes(&oversized).is_none(), "over 600 bytes");
    }

    /// An RSA 2048 proof passes as PKCS#1 v1.5 or as PSS with SHA-256, over its own message
    /// only, and a P-256 key never takes either.
    #[test]
    fn an_rsa_proof_passes_with_either_padding() {
        use aws_lc_rs::rsa::{KeyPair, KeySize};
        use aws_lc_rs::signature::{RsaEncoding, RSA_PKCS1_SHA256, RSA_PSS_SHA256};
        let pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let key = UnlockKey {
            alg: KeyAlg::Rsa2048,
            public: hex::encode(pair.public_key().as_ref()),
        };
        let signed = message(&[1; 32], &SeatId::random(), &[2; 32], &[3; 32]);
        let other = message(&[4; 32], &SeatId::random(), &[2; 32], &[3; 32]);
        let sign = |padding: &'static dyn RsaEncoding| {
            let mut signature = vec![0; pair.public_modulus_len()];
            pair.sign(padding, &SystemRandom::new(), &signed, &mut signature)
                .unwrap();
            signature
        };
        let p256 = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
        let point = hex::encode(p256.public_key().as_ref());
        for padding in [&RSA_PKCS1_SHA256 as &dyn RsaEncoding, &RSA_PSS_SHA256] {
            let signature = sign(padding);
            assert_eq!(signature.len(), 256);
            assert!(verify(&key, &signed, &signature));
            assert!(!verify(&key, &other, &signature), "another message");
            for public in [key.public.clone(), point.clone()] {
                let as_p256 = UnlockKey {
                    alg: KeyAlg::P256,
                    public,
                };
                assert!(!verify(&as_p256, &signed, &signature), "never the RSA path");
            }
        }
    }

    /// A file is read only as the list it was written as: a bad entry spoils the whole of it.
    #[test]
    fn a_list_with_a_bad_entry_is_not_valid() {
        let mut list = DeviceList::default();
        list.grant(&device(0xab), NOW, true, None);
        assert!(list.valid());
        let mut twice = list.clone();
        twice.devices.push(twice.devices[0].clone());
        let mut upper = list.clone();
        upper.devices[0].fingerprint.make_ascii_uppercase();
        let mut keyed = list.clone();
        keyed.devices[0].key = Some(UnlockKey {
            alg: KeyAlg::Rsa2048,
            public: "30".into(),
        });
        for bad in [twice, upper, keyed] {
            assert!(!bad.valid());
        }
    }
}
