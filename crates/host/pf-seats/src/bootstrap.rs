//! Length-capped bootstrap passed only through the RDP keeper's inherited stdin.
//!
//! The frame carries the local account, plaintext password, and expected leaf
//! pin. It has a fixed magic/version, explicit lengths, and a 4 KiB outer cap.
//! Password storage uses `Zeroizing`; the type's `Debug` output is redacted.
//! The process launcher writes the frame after job assignment, then closes its
//! pipe. No bootstrap field is accepted on argv or through the environment.
//! Pure tests keep this boundary covered on non-Windows builders.

use std::fmt;
use std::io::{self, Read};
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"PFSEATRD";
const VERSION: u16 = 1;
pub(crate) const MAX_BOOTSTRAP_BYTES: usize = 4096;
const PIN_BYTES: usize = 32;
const HEADER_BYTES: usize = MAGIC.len() + 2 + 2 + 2 + PIN_BYTES;

pub struct RdpBootstrap {
    pub account: String,
    pub password: Zeroizing<String>,
    pub pin: [u8; PIN_BYTES],
}

impl fmt::Debug for RdpBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RdpBootstrap")
            .field("account", &self.account)
            .field("password", &"<redacted>")
            .field("pin", &hex::encode(self.pin))
            .finish()
    }
}

impl RdpBootstrap {
    pub(crate) fn new(
        account: String,
        password: Zeroizing<String>,
        pin: [u8; PIN_BYTES],
    ) -> io::Result<Self> {
        if account.is_empty()
            || account.len() > crate::model::MAX_ACCOUNT_LEN
            || !account
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap account is invalid",
            ));
        }
        if !(32..=256).contains(&password.len())
            || !password.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RDP bootstrap password is invalid",
            ));
        }
        Ok(Self {
            account,
            password,
            pin,
        })
    }

    pub(crate) fn encode(&self) -> io::Result<Zeroizing<Vec<u8>>> {
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
        let payload_len = HEADER_BYTES + usize::from(account_len) + usize::from(password_len);
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
        framed.extend_from_slice(&self.pin);
        framed.extend_from_slice(self.account.as_bytes());
        framed.extend_from_slice(self.password.as_bytes());
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
        if HEADER_BYTES + account_len + password_len != length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap fields do not match its length",
            ));
        }
        let mut pin = [0_u8; PIN_BYTES];
        pin.copy_from_slice(&payload[14..14 + PIN_BYTES]);
        let account_start = HEADER_BYTES;
        let password_start = account_start + account_len;
        let account =
            String::from_utf8(payload[account_start..password_start].to_vec()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RDP bootstrap account is not UTF-8",
                )
            })?;
        let password = String::from_utf8(payload[password_start..].to_vec()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "RDP bootstrap password is not UTF-8",
            )
        })?;
        Self::new(account, Zeroizing::new(password), pin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bootstrap_round_trips_and_redacts_password() {
        let password = "A1!".repeat(16);
        let bootstrap = RdpBootstrap::new(
            "pf-seat-1".into(),
            Zeroizing::new(password.clone()),
            [7; 32],
        )
        .unwrap();
        let encoded = bootstrap.encode().unwrap();
        let decoded = RdpBootstrap::read_from(&mut Cursor::new(encoded.as_slice())).unwrap();
        assert_eq!(decoded.account, "pf-seat-1");
        assert_eq!(decoded.password.as_str(), password);
        assert_eq!(decoded.pin, [7; 32]);
        assert!(!format!("{decoded:?}").contains(&password));
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
