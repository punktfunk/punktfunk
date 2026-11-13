//! What a saved host's card says under its name: one sentence from the probe sweep, this
//! device's own session and what the host has up. Every desktop shell words a host alike.

use crate::trust::KnownHost;
use pf_l10n::client;

/// This device's session with a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Streaming,
}

/// What a saved card says under its name: one sentence, one register (design §2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    /// This device streams from it.
    Streaming,
    Playing(String),
    Online,
    /// Answers, but nothing is pinned yet: a click runs the pairing.
    NotPaired,
    /// Offline with a MAC and auto-wake on: a click wakes it first.
    OfflineWakes,
    Offline,
}

impl Status {
    pub fn of(
        host: &KnownHost,
        online: bool,
        phase: Option<Phase>,
        playing: &str,
        auto_wake: bool,
    ) -> Status {
        if let Some(phase) = phase {
            match phase {
                Phase::Connecting => Status::Connecting,
                Phase::Streaming => Status::Streaming,
            }
        } else if !online {
            if auto_wake && !host.mac.is_empty() {
                Status::OfflineWakes
            } else {
                Status::Offline
            }
        } else if host.fp_hex.is_empty() {
            Status::NotPaired
        } else if !playing.is_empty() {
            Status::Playing(playing.to_string())
        } else {
            Status::Online
        }
    }

    pub fn sentence(&self) -> String {
        match self {
            Status::Connecting => client::hostcard_status_connecting().into(),
            Status::Streaming => client::hostcard_status_streaming().into(),
            Status::Playing(title) => client::hostcard_status_playing(title),
            Status::Online => client::hostcard_status_online().into(),
            Status::NotPaired => client::hostcard_status_not_paired().into(),
            Status::OfflineWakes => client::hostcard_status_offline_wakes().into(),
            Status::Offline => client::hostcard_status_offline().into(),
        }
    }

    /// The host answers, so the dot beside the sentence is lit.
    pub fn live(&self) -> bool {
        matches!(
            self,
            Status::Online | Status::Streaming | Status::Playing(_) | Status::NotPaired
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, addr: &str, fp: &str) -> KnownHost {
        KnownHost {
            name: name.into(),
            addr: addr.into(),
            port: 9777,
            fp_hex: fp.into(),
            ..KnownHost::default()
        }
    }

    #[test]
    fn a_status_is_one_sentence() {
        let mut k = host("Desk", "10.0.0.2", "ab");
        assert_eq!(Status::of(&k, true, None, "", true), Status::Online);
        assert_eq!(
            Status::of(&k, true, None, "Hades", true).sentence(),
            "Playing Hades"
        );
        assert_eq!(
            Status::of(&k, true, Some(Phase::Connecting), "Hades", true),
            Status::Connecting
        );
        let streaming = Status::of(&k, false, Some(Phase::Streaming), "Hades", true);
        assert_eq!(
            streaming,
            Status::Streaming,
            "this device's stream beats the probe"
        );
        assert!(streaming.live());
        assert_eq!(Status::of(&k, false, None, "", true), Status::Offline);
        k.mac = vec!["aa:bb:cc:dd:ee:ff".into()];
        assert_eq!(Status::of(&k, false, None, "", true), Status::OfflineWakes);
        assert_eq!(Status::of(&k, false, None, "", false), Status::Offline);
        let placeholder = host("Den", "10.0.0.3", "");
        assert_eq!(
            Status::of(&placeholder, true, None, "", true),
            Status::NotPaired
        );
        assert!(Status::NotPaired.live());
        assert!(!Status::OfflineWakes.live());
    }
}
