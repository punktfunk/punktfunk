//! What the hosts page draws, as plain values: one [`CardModel`] per card, in the bands and
//! order the device's host sort asks for. Built from the store, the probe sweep and the live
//! adverts; the page compares a build with the last one, so an unchanged sweep redraws nothing.

use super::{saved_request, ConnectRequest};
use crate::discovery::{self, DiscoveredHost};
use crate::trust::{KnownHost, Settings};
use pf_client_core::host_order::{self, Arrangeable};
use std::collections::HashMap;

/// A preset as a card needs it: what to call it and the colour its chip carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub accent: Option<String>,
}

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
            Status::Connecting => "Connecting\u{2026}".into(),
            Status::Streaming => "Streaming".into(),
            Status::Playing(title) => format!("Playing {title}"),
            Status::Online => "Online".into(),
            Status::NotPaired => "Not paired \u{b7} click to pair".into(),
            Status::OfflineWakes => "Offline \u{b7} wakes on click".into(),
            Status::Offline => "Offline".into(),
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

/// Which host a card belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardKind {
    Saved {
        /// The stable record id the host page is keyed by; `None` only on a record older
        /// than ids, which the store mints on load.
        id: Option<String>,
        paired: bool,
        /// `Some((id, name))` on a pinned host+preset card: a one-click shortcut, never a
        /// second host record.
        pinned: Option<(String, String)>,
        /// The record `Settings::default_host` names.
        is_default: bool,
    },
    /// Found on the network and not saved. `pair_optional`: the host offers trust on first use.
    Discovered { pair_optional: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardModel {
    /// Unique among the cards: the host's key, then a pinned card's preset id.
    pub key: String,
    pub name: String,
    /// `addr:port`, for the tooltip and a discovered card's second line.
    pub address: String,
    /// The advertised OS chain, for the tile's mark.
    pub os: String,
    pub status: Status,
    /// What a plain click connects with: a pinned card's preset, else the host's binding.
    pub chip: Option<Preset>,
    pub kind: CardKind,
    pub request: ConnectRequest,
    pub last_used: Option<u64>,
}

impl CardModel {
    /// A click wakes first: offline with a MAC. A routed host that is mDNS-blind but awake
    /// still dials straight away (`WakeConnect` dials first).
    pub fn wake_first(&self) -> bool {
        matches!(self.status, Status::Offline | Status::OfflineWakes)
            && !self.request.mac.is_empty()
    }
}

impl Arrangeable for CardModel {
    fn name(&self) -> &str {
        &self.name
    }
    fn online(&self) -> bool {
        self.status.live()
    }
    fn last_used(&self) -> Option<u64> {
        self.last_used
    }
    fn preset_name(&self) -> Option<&str> {
        self.chip.as_ref().map(|p| p.name.as_str())
    }
}

/// A run of cards under one group caption; `title` is `None` when hosts are not grouped.
#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    pub title: Option<String>,
    pub cards: Vec<CardModel>,
}

/// The live state beside the store.
pub struct Live<'a> {
    /// The last probe sweep, by [`KnownHost::card_key`].
    pub probed: &'a HashMap<String, bool>,
    /// The card key this device's session is with, and how far it got.
    pub session: Option<(&'a str, Phase)>,
    /// What a host has up, by fingerprint; empty when nothing is.
    pub playing: &'a dyn Fn(&str) -> String,
}

impl Live<'_> {
    pub fn online(&self, k: &KnownHost) -> bool {
        self.probed.get(&k.card_key()).copied().unwrap_or(false)
    }
}

/// The saved cards in the device's order, split into its bands. A host's pinned cards follow
/// it and read its live state; a pin whose preset is gone does not render.
pub fn saved_bands(
    hosts: &[KnownHost],
    presets: &[Preset],
    settings: &Settings,
    live: &Live,
) -> Vec<Band> {
    let mut cards = Vec::new();
    for k in hosts {
        let online = live.online(k);
        let playing = if k.fp_hex.is_empty() {
            String::new()
        } else {
            (live.playing)(&k.fp_hex)
        };
        let is_default = k.id.is_some() && settings.default_host.as_deref() == k.id.as_deref();
        let card = |pinned: Option<&Preset>, phase: Option<Phase>| {
            let bound = k
                .preset_id
                .as_deref()
                .and_then(|id| presets.iter().find(|p| p.id == id));
            let mut request = saved_request(k);
            request.preset = pinned.map(|p| p.id.clone());
            CardModel {
                key: match pinned {
                    Some(p) => format!("{}\u{0}{}", k.card_key(), p.id),
                    None => k.card_key(),
                },
                name: k.name.clone(),
                address: format!("{}:{}", k.addr, k.port),
                os: k.os.clone(),
                status: Status::of(k, online, phase, &playing, settings.auto_wake),
                chip: pinned.or(bound).cloned(),
                kind: CardKind::Saved {
                    id: k.id.clone(),
                    paired: k.paired,
                    pinned: pinned.map(|p| (p.id.clone(), p.name.clone())),
                    is_default,
                },
                request,
                last_used: k.last_used,
            }
        };
        // The session shows on the host's own card, never on a pinned one.
        let key = k.card_key();
        let phase = live
            .session
            .and_then(|(at, phase)| (at == key).then_some(phase));
        cards.push(card(None, phase));
        for id in &k.pinned_presets {
            if let Some(p) = presets.iter().find(|p| &p.id == id) {
                cards.push(card(Some(p), None));
            }
        }
    }
    host_order::arrange(&mut cards, settings);
    let grouping = host_order::grouping(settings);
    let mut bands: Vec<Band> = Vec::new();
    for c in cards {
        let title = host_order::group_of(&c, grouping);
        match bands.last_mut() {
            Some(b) if b.title == title => b.cards.push(c),
            _ => bands.push(Band {
                title,
                cards: vec![c],
            }),
        }
    }
    bands
}

/// Adverts that match no saved host, by name. A saved host's advert shows as its card.
pub fn discovered_cards<'a>(
    adverts: impl Iterator<Item = &'a DiscoveredHost>,
    hosts: &[KnownHost],
    connecting: Option<&str>,
) -> Vec<CardModel> {
    let mut fresh: Vec<&DiscoveredHost> = adverts
        .filter(|a| !hosts.iter().any(|k| discovery::same_host(k, a)))
        .collect();
    fresh.sort_by(|a, b| a.name.cmp(&b.name).then(a.key.cmp(&b.key)));
    fresh
        .into_iter()
        .map(|a| {
            let request = ConnectRequest {
                name: a.name.clone(),
                addr: a.addr.clone(),
                port: a.port,
                fp_hex: (!a.fp_hex.is_empty()).then(|| a.fp_hex.clone()),
                // Trust on first use only when the host explicitly opts in.
                pair_optional: a.pair == "optional",
                launch: None,
                mac: a.mac.clone(),
                preset: None,
            };
            let key = request.card_key();
            CardModel {
                status: if connecting == Some(key.as_str()) {
                    Status::Connecting
                } else {
                    Status::Online
                },
                key,
                name: a.name.clone(),
                address: format!("{}:{}", a.addr, a.port),
                os: a.os.clone(),
                chip: None,
                kind: CardKind::Discovered {
                    pair_optional: request.pair_optional,
                },
                request,
                last_used: None,
            }
        })
        .collect()
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
            paired: !fp.is_empty(),
            ..Default::default()
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

    fn bands(hosts: &[KnownHost], presets: &[Preset], s: &Settings) -> Vec<Band> {
        let probed: HashMap<String, bool> = [("ab".to_string(), true)].into_iter().collect();
        let live = Live {
            probed: &probed,
            session: None,
            playing: &|fp| {
                if fp == "ab" {
                    "Hades".into()
                } else {
                    String::new()
                }
            },
        };
        saved_bands(hosts, presets, s, &live)
    }

    #[test]
    fn a_pinned_card_follows_its_host_with_its_own_key() {
        let game = Preset {
            id: "g".into(),
            name: "Game".into(),
            accent: None,
        };
        let mut desk = host("Desk", "10.0.0.2", "ab");
        desk.pinned_presets = vec!["g".into(), "gone".into()];
        let s = Settings {
            default_host: desk.id.clone(),
            ..Default::default()
        };
        let out = bands(&[desk], std::slice::from_ref(&game), &s);
        assert_eq!(out.len(), 1);
        let cards = &out[0].cards;
        assert_eq!(cards.len(), 2, "a pin whose preset is gone does not render");
        assert_eq!(cards[0].status, Status::Playing("Hades".into()));
        assert_eq!(cards[0].chip, None);
        assert_ne!(cards[0].key, cards[1].key);
        assert_eq!(cards[1].chip, Some(game));
        assert_eq!(cards[1].request.preset.as_deref(), Some("g"));
        for c in cards {
            assert!(matches!(
                c.kind,
                CardKind::Saved {
                    is_default: true,
                    ..
                }
            ));
        }
    }

    #[test]
    fn grouping_splits_the_cards_into_bands() {
        let mut s = Settings::default();
        s.extra
            .insert(host_order::HOST_GROUPING_KEY.into(), "status".into());
        let out = bands(
            &[
                host("Den", "10.0.0.3", "cd"),
                host("Desk", "10.0.0.2", "ab"),
            ],
            &[],
            &s,
        );
        let titles: Vec<_> = out.iter().map(|b| b.title.as_deref()).collect();
        assert_eq!(titles, [Some("Online"), Some("Offline")]);
        assert_eq!(out[0].cards[0].name, "Desk");
    }

    #[test]
    fn a_saved_host_is_not_also_discovered() {
        let advert = |key: &str, fp: &str| DiscoveredHost {
            key: key.into(),
            fullname: format!("{key}._punktfunk._udp.local."),
            name: key.into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: fp.into(),
            pair: "optional".into(),
            mgmt_port: None,
            mac: Vec::new(),
            os: String::new(),
        };
        let adverts = [advert("saved", "ab"), advert("new", "ef")];
        let cards = discovered_cards(adverts.iter(), &[host("Desk", "10.0.0.2", "ab")], None);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name, "new");
        assert!(cards[0].request.pair_optional);
    }
}
