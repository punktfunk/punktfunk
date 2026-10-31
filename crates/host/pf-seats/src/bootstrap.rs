//! Length-capped bootstrap passed only through the RDP keeper's inherited stdin.
//!
//! The frame carries the local account, plaintext password, expected leaf pin,
//! and the seat's host root, where the keeper reads the host's disconnect mark.
//! It has a fixed magic/version, explicit lengths, and a 4 KiB outer cap.
//! Password storage uses `Zeroizing`; the type's `Debug` output is redacted.
//! The process launcher writes the frame after job assignment, then closes its
//! pipe. No bootstrap field is accepted on argv or through the environment.
//! Pure tests keep this boundary covered on non-Windows builders.

use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"PFSEATRD";
const VERSION: u16 = 2;
pub(crate) const MAX_BOOTSTRAP_BYTES: usize = 4096;
const PIN_BYTES: usize = 32;
const HEADER_BYTES: usize = MAGIC.len() + 2 + 2 + 2 + 2 + PIN_BYTES;

pub struct RdpBootstrap {
    pub account: String,
    pub password: Zeroizing<String>,
    pub pin: [u8; PIN_BYTES],
    /// The seat's `hosts\<seat id>` directory: its host's config dir.
    pub host_root: PathBuf,
}

impl fmt::Debug for RdpBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RdpBootstrap")
            .field("account", &self.account)
            .field("password", &"<redacted>")
            .field("pin", &hex::encode(self.pin))
            .field("host_root", &self.host_root)
            .finish()
    }
}

/// A seat account's password, a person's own as well as a minted one: 1..=256 bytes of UTF-8
/// without control characters.
pub(crate) fn password_rule(value: &str) -> bool {
    (1..=256).contains(&value.len()) && !value.chars().any(char::is_control)
}

impl RdpBootstrap {
    /// A frame for `account`, a SAM account name, `password` by [`password_rule`], and
    /// `host_root`, an absolute UTF-8 path.
    pub(crate) fn new(
        account: String,
        password: Zeroizing<String>,
        pin: [u8; PIN_BYTES],
        host_root: PathBuf,
    ) -> io::Result<Self> {
        if crate::model::validate_sam_account(&account).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap account is invalid",
            ));
        }
        if !password_rule(&password) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap password is invalid",
            ));
        }
        if !host_root.is_absolute() || host_root.to_str().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap host root is not an absolute UTF-8 path",
            ));
        }
        Ok(Self {
            account,
            password,
            pin,
            host_root,
        })
    }

    pub(crate) fn encode(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let host_root = self.host_root.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap host root is not UTF-8",
            )
        })?;
        let host_root_len = u16::try_from(host_root.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap host root is too long",
            )
        })?;
        let account_len = u16::try_from(self.account.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap account is too long",
            )
        })?;
        let password_len = u16::try_from(self.password.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap password is too long",
            )
        })?;
        let payload_len = HEADER_BYTES
            + usize::from(account_len)
            + usize::from(password_len)
            + usize::from(host_root_len);
        if payload_len > MAX_BOOTSTRAP_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap exceeds its size limit",
            ));
        }
        let mut framed = Zeroizing::new(Vec::with_capacity(4 + payload_len));
        framed.extend_from_slice(&(payload_len as u32).to_be_bytes());
        framed.extend_from_slice(MAGIC);
        framed.extend_from_slice(&VERSION.to_be_bytes());
        framed.extend_from_slice(&account_len.to_be_bytes());
        framed.extend_from_slice(&password_len.to_be_bytes());
        framed.extend_from_slice(&host_root_len.to_be_bytes());
        framed.extend_from_slice(&self.pin);
        framed.extend_from_slice(self.account.as_bytes());
        framed.extend_from_slice(self.password.as_bytes());
        framed.extend_from_slice(host_root.as_bytes());
        Ok(framed)
    }

    /// Read the frame the supervisor wrote. The keeper exe calls this on its stdin.
    pub fn read_from(reader: &mut impl Read) -> io::Result<Self> {
        let mut prefix = [0_u8; 4];
        reader.read_exact(&mut prefix)?;
        let length = u32::from_be_bytes(prefix) as usize;
        if !(HEADER_BYTES..=MAX_BOOTSTRAP_BYTES).contains(&length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap length is invalid",
            ));
        }
        let mut payload = Zeroizing::new(vec![0_u8; length]);
        reader.read_exact(&mut payload)?;
        if &payload[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap magic is invalid",
            ));
        }
        let version = u16::from_be_bytes([payload[8], payload[9]]);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap version is unsupported",
            ));
        }
        let account_len = usize::from(u16::from_be_bytes([payload[10], payload[11]]));
        let password_len = usize::from(u16::from_be_bytes([payload[12], payload[13]]));
        let host_root_len = usize::from(u16::from_be_bytes([payload[14], payload[15]]));
        if HEADER_BYTES + account_len + password_len + host_root_len != length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap fields do not match its length",
            ));
        }
        let mut pin = [0_u8; PIN_BYTES];
        pin.copy_from_slice(&payload[16..16 + PIN_BYTES]);
        let account_start = HEADER_BYTES;
        let password_start = account_start + account_len;
        let host_root_start = password_start + password_len;
        let account =
            String::from_utf8(payload[account_start..password_start].to_vec()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RDP bootstrap account is not UTF-8",
                )
            })?;
        let password = String::from_utf8(payload[password_start..host_root_start].to_vec())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RDP bootstrap password is not UTF-8",
                )
            })?;
        let host_root = String::from_utf8(payload[host_root_start..].to_vec()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap host root is not UTF-8",
            )
        })?;
        Self::new(account, Zeroizing::new(password), pin, host_root.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An absolute path on either OS; nothing is read from it.
    fn host_root() -> PathBuf {
        let root = if cfg!(windows) {
            r"C:\pf\hosts\seat"
        } else {
            "/pf/hosts/seat"
        };
        PathBuf::from(root)
    }

    #[test]
    fn bootstrap_round_trips_and_redacts_password() {
        let password = "A1!".repeat(16);
        let bootstrap = RdpBootstrap::new(
            "pf-seat-1".into(),
            Zeroizing::new(password.clone()),
            [7; 32],
            host_root(),
        )
        .unwrap();
        let encoded = bootstrap.encode().unwrap();
        let decoded = RdpBootstrap::read_from(&mut Cursor::new(encoded.as_slice())).unwrap();
        assert_eq!(decoded.account, "pf-seat-1");
        assert_eq!(decoded.password.as_str(), password);
        assert_eq!(decoded.pin, [7; 32]);
        assert_eq!(decoded.host_root, host_root());
        assert!(!format!("{decoded:?}").contains(&password));
    }

    /// The host root must be absolute, and the frame that carries it still fits in 4 KiB.
    #[test]
    fn bootstrap_refuses_a_relative_or_oversized_host_root() {
        let new = |root: PathBuf| {
            RdpBootstrap::new("Ben".into(), Zeroizing::new("pw".into()), [7; 32], root)
        };
        assert!(new(PathBuf::from("hosts").join("seat")).is_err());
        assert!(new(PathBuf::new()).is_err());
        let long = new(host_root().join("a".repeat(MAX_BOOTSTRAP_BYTES))).unwrap();
        assert!(long.encode().is_err());
    }

    /// A version 1 frame names no host root, so it is refused rather than read short.
    #[test]
    fn bootstrap_refuses_a_version_1_frame() {
        let (account, password) = (b"pf-seat-1", b"A1!A1!A1!");
        let mut payload = Vec::from(*MAGIC);
        payload.extend_from_slice(&1_u16.to_be_bytes());
        payload.extend_from_slice(&(account.len() as u16).to_be_bytes());
        payload.extend_from_slice(&(password.len() as u16).to_be_bytes());
        payload.extend_from_slice(&[7; PIN_BYTES]);
        payload.extend_from_slice(account);
        payload.extend_from_slice(password);
        let mut frame = Vec::from((payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        let refused = RdpBootstrap::read_from(&mut Cursor::new(frame)).unwrap_err();
        assert_eq!(refused.to_string(), "RDP bootstrap version is unsupported");
    }

    /// A person's password, short and past ASCII, and an account name with a space cross the
    /// frame; an empty, an over-long and a control-bearing password do not.
    #[test]
    fn bootstrap_carries_a_human_password() {
        let password = "Bärbel!23";
        assert_eq!(password.len(), 10);
        let bootstrap = RdpBootstrap::new(
            "Bärbel Ott".into(),
            Zeroizing::new(password.into()),
            [7; 32],
            host_root(),
        )
        .unwrap();
        let encoded = bootstrap.encode().unwrap();
        let decoded = RdpBootstrap::read_from(&mut Cursor::new(encoded.as_slice())).unwrap();
        assert_eq!(decoded.account, "Bärbel Ott");
        assert_eq!(decoded.password.as_str(), password);

        for bad in [String::new(), "a".repeat(257), "pass\nword".into()] {
            let refused = RdpBootstrap::new(
                "Ben".into(),
                Zeroizing::new(bad.clone()),
                [7; 32],
                host_root(),
            );
            assert!(refused.is_err(), "{} bytes", bad.len());
        }
        let slash = RdpBootstrap::new(
            "a/b".into(),
            Zeroizing::new("pw".into()),
            [7; 32],
            host_root(),
        );
        assert!(slash.is_err());
    }

    #[test]
    fn bootstrap_rejects_oversized_and_truncated_frames() {
        let oversized = ((MAX_BOOTSTRAP_BYTES + 1) as u32).to_be_bytes();
        assert!(RdpBootstrap::read_from(&mut Cursor::new(oversized)).is_err());

        let mut truncated = Vec::from((HEADER_BYTES as u32).to_be_bytes());
        truncated.extend_from_slice(MAGIC);
        assert!(RdpBootstrap::read_from(&mut Cursor::new(truncated)).is_err());
    }
}
