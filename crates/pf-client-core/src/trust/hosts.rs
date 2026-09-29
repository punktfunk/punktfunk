//! The known-hosts store: pinned records, the trust decisions that write them, and what
//! adverts and connects teach them.

use super::{config_dir, load_json_or_default, write_atomic};
use crate::presets::{PresetsFile, StreamPreset};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One trusted host: pinned cert fingerprint, how trust was granted, last-reached address.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct KnownHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// SHA-256 of the host certificate, lowercase hex — the pin for later connects.
    pub fp_hex: String,
    /// True if trust came from the SPAKE2 PIN ceremony (vs. trust-on-first-use).
    pub paired: bool,
    /// Unix seconds of the last successful connect. `default` so older stores load.
    #[serde(default)]
    pub last_used: Option<u64>,
    /// Wake-on-LAN MACs (`aa:bb:cc:dd:ee:ff`) learned from mDNS `mac` TXT while online,
    /// so we can wake a host that has stopped advertising. `default`; empty until learned.
    #[serde(default)]
    pub mac: Vec<String>,
    /// OS-identity chain (`windows` | `macos` | `linux[/<family>][/<id>]`) from mDNS `os`
    /// TXT, so the card icon survives sleep. `default`; elided when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    /// Management-API port (mDNS `mgmt` TXT), distinct from `port` (native QUIC). Persisted
    /// so a host that moved off 47990 stays reachable once the advert is gone. `None` =
    /// never learned; resolve via [`KnownHost::effective_mgmt_port`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mgmt_port: Option<u16>,
    /// Share this machine's clipboard with this host (design/clipboard-and-file-transfer.md).
    /// Per-host, not global. Default off; the host must also advertise `HOST_CAP_CLIPBOARD`.
    #[serde(default)]
    pub clipboard_sync: bool,
    /// Default settings preset for a plain click (design/client-settings-profiles.md).
    /// `None` or a deleted id → global defaults; a dangling binding never blocks a connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// Extra preset cards for this host; order = card order. Presentation only — not
    /// the default (`preset_id`). Duplicates and dangling ids are dropped at resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_presets: Vec<String>,
    /// Library title id → preset id: what a launch of that title streams with, beating
    /// `preset_id`. Keyed here rather than in the catalog for the §4.1 reason the host
    /// binding is — the catalog owns no host keys, and a title id is only unique per host.
    /// Dangling ids resolve to nothing, exactly like a dangling `preset_id`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub game_presets: BTreeMap<String, String>,
    /// Stable record id, minted lazily, never rewritten. Survives rename and DHCP.
    /// No lookup here is keyed by it — `fp_hex` / `addr:port` stay the keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Addresses this host left or was advertised at, newest first, at most
    /// [`PREV_ADDRS_MAX`]. A host lives at more than one — its LAN lease at home, a
    /// Tailscale address anywhere — so when `addr` goes silent `probe_known` asks these too,
    /// and moves there only when the pin answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prev_addrs: Vec<String>,
}

/// Pre-rename binding keys (`design/preset-rename.md`): read when the new key is absent,
/// and written beside it so a client older than the rename keeps its bindings.
const LEGACY_HOST_KEYS: [(&str, &str); 3] = [
    ("preset_id", "profile_id"),
    ("pinned_presets", "pinned_profiles"),
    ("game_presets", "game_profiles"),
];

impl<'de> Deserialize<'de> for KnownHost {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut v = serde_json::Value::deserialize(d)?;
        if let Some(host) = v.as_object_mut() {
            for (new, old) in LEGACY_HOST_KEYS {
                // The new key wins: only this client writes it, and always beside an equal
                // old one, so a record carrying it was last saved here.
                if let Some(legacy) = host.remove(old) {
                    host.entry(new).or_insert(legacy);
                }
            }
        }
        KnownHost::deserialize(v).map_err(serde::de::Error::custom)
    }
}

impl Serialize for KnownHost {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut v = KnownHost::serialize(self, serde_json::value::Serializer)
            .map_err(serde::ser::Error::custom)?;
        if let Some(host) = v.as_object_mut() {
            for (new, old) in LEGACY_HOST_KEYS {
                if let Some(value) = host.get(new).cloned() {
                    host.insert(old.into(), value);
                }
            }
        }
        v.serialize(s)
    }
}

/// How many left-behind addresses a host keeps.
pub const PREV_ADDRS_MAX: usize = 3;

impl Default for KnownHost {
    /// Blank record with a fresh stable id — construction sites use
    /// `KnownHost { name, addr, port, ..Default::default() }`, so a new field here cannot
    /// silently omit it.
    fn default() -> KnownHost {
        KnownHost {
            name: String::new(),
            addr: String::new(),
            port: 9777,
            fp_hex: String::new(),
            paired: false,
            last_used: None,
            mac: Vec::new(),
            os: String::new(),
            mgmt_port: None,
            clipboard_sync: false,
            preset_id: None,
            pinned_presets: Vec::new(),
            game_presets: BTreeMap::new(),
            id: Some(crate::presets::new_record_uuid()),
            prev_addrs: Vec::new(),
        }
    }
}

impl KnownHost {
    /// The key a host card and its probe result go by: the pin, else `addr:port`. A bare
    /// `fp_hex` is empty for every unpaired placeholder, so they would all share one key.
    pub fn card_key(&self) -> String {
        if self.fp_hex.is_empty() {
            format!("{}:{}", self.addr, self.port)
        } else {
            self.fp_hex.clone()
        }
    }

    /// Learned mgmt port, else compiled-in 47990. Library/art calls must use this, not
    /// [`crate::library::DEFAULT_MGMT_PORT`] — that constant is the fallback, not the answer.
    pub fn effective_mgmt_port(&self) -> u16 {
        self.mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT)
    }

    /// Re-point at `addr:port`, remembering the address it leaves. `false`, and nothing
    /// changed, when already there.
    pub fn move_to(&mut self, addr: &str, port: u16) -> bool {
        if self.addr == addr && self.port == port {
            return false;
        }
        let old = std::mem::replace(&mut self.addr, addr.to_string());
        self.prev_addrs.retain(|a| a != addr && *a != old);
        if old != addr {
            self.prev_addrs.insert(0, old);
        }
        self.prev_addrs.truncate(PREV_ADDRS_MAX);
        self.port = port;
        true
    }

    /// Pins that still exist, in card order, no duplicates. Dangling ids disappear —
    /// a pin is presentation state, never an error.
    pub fn resolved_pins<'a>(&self, catalog: &'a PresetsFile) -> Vec<&'a StreamPreset> {
        let mut out: Vec<&StreamPreset> = Vec::new();
        for id in &self.pinned_presets {
            if out.iter().any(|p| p.id == *id) {
                continue;
            }
            if let Some(p) = catalog.find_by_id(id) {
                out.push(p);
            }
        }
        out
    }

    /// This title's binding, if it has one. Not resolved against the catalog here —
    /// [`resolve_preset`](crate::settings::resolve_preset) drops a dangling id, the same
    /// way it does for `preset_id`.
    pub fn preset_for_game(&self, game_id: &str) -> Option<&str> {
        self.game_presets.get(game_id).map(String::as_str)
    }

    /// Bind (or with `None`, clear) a title's preset. Idempotent, and a clear removes
    /// the key rather than storing an empty one — the map is skipped when serialising,
    /// so an unbound host keeps writing no `game_presets` at all.
    pub fn bind_game_preset(&mut self, game_id: &str, preset_id: Option<&str>) {
        match preset_id {
            Some(id) => drop(
                self.game_presets
                    .insert(game_id.to_string(), id.to_string()),
            ),
            None => drop(self.game_presets.remove(game_id)),
        }
    }

    /// Apply a person's edit. An address or port change goes through
    /// [`move_to`](Self::move_to), so the address it leaves stays a probe candidate. A blank
    /// name or address is no edit. `true` if anything changed.
    pub fn apply_edit(&mut self, edit: &HostEdit) -> bool {
        let mut changed = false;
        if let Some(name) = edit
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            changed |= self.name != name;
            self.name = name.to_string();
        }
        let addr = edit
            .addr
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or(&self.addr)
            .to_string();
        changed |= self.move_to(&addr, edit.port.unwrap_or(self.port));
        if let Some(macs) = &edit.macs {
            changed |= self.mac != *macs;
            self.mac = macs.clone();
        }
        changed
    }
}

/// A person's edit of a saved host: the Add and Edit forms' fields, already validated
/// ([`crate::wol::parse_mac_list`] for the MACs). `None` leaves a field as stored. An empty
/// `macs` clears them — a person may, where an advert may only teach one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostEdit {
    pub name: Option<String>,
    pub addr: Option<String>,
    pub port: Option<u16>,
    pub macs: Option<Vec<String>>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct KnownHosts {
    pub hosts: Vec<KnownHost>,
}

impl KnownHosts {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("client-known-hosts.json"))
    }

    /// The store, minting ids on records that lack one. Written back here, not "on the
    /// next save", so the id a caller sees is the one on disk. A read-only dir re-mints
    /// in memory; no lookup is keyed by id yet.
    pub fn load() -> KnownHosts {
        let mut k = Self::read();
        if k.mint_missing_ids() {
            let _ = k.save();
        }
        k
    }

    /// The store as on disk — no mint, so no write.
    ///
    /// [`KnownHosts::load`]'s mint is a write: two processes against a pre-mint store each
    /// mint a different id and race to save. A read-only consumer cannot take part in that.
    pub fn read() -> KnownHosts {
        Self::path()
            .map(|p| load_json_or_default(&p))
            .unwrap_or_default()
    }

    /// Mint a stable id on every record that lacks one. `true` = needs persisting.
    /// Idempotent: a store that has been through it once is byte-identical.
    pub fn mint_missing_ids(&mut self) -> bool {
        let mut minted = false;
        for h in &mut self.hosts {
            if h.id.as_deref().is_none_or(str::is_empty) {
                h.id = Some(crate::presets::new_record_uuid());
                minted = true;
            }
        }
        minted
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        // Temp+rename: losing this file to a torn write costs the user every pairing.
        write_atomic(&p, serde_json::to_string_pretty(self)?.as_bytes())?;
        // Omarchy menu mirrors this store; save() is the one door every mutation walks.
        // No-op unless `--omarchy-menu on` — a scoped test HOME never has that.
        // `desktop` too: `trust` is portable, and a TV build has no omarchy_menu to call.
        #[cfg(all(desktop, target_os = "linux"))]
        crate::omarchy_menu::sync_if_enabled();
        Ok(())
    }

    /// The record pinned to `fp_hex`. An empty fingerprint is not a key — it would match
    /// every not-yet-paired placeholder, and the first one is never the one meant.
    pub fn find_by_fp(&self, fp_hex: &str) -> Option<&KnownHost> {
        if fp_hex.is_empty() {
            return None;
        }
        self.hosts.iter().find(|h| h.fp_hex == fp_hex)
    }

    /// Load, hand the record pinned to `fp_hex` to `f`, and save when `f` says it changed.
    /// No-op, and no disk write, for an empty or unstored fingerprint.
    fn update_by_fp(fp_hex: &str, f: impl FnOnce(&mut KnownHost) -> bool) {
        if fp_hex.is_empty() {
            return;
        }
        let mut known = Self::load();
        if known
            .hosts
            .iter_mut()
            .find(|h| h.fp_hex == fp_hex)
            .is_some_and(f)
        {
            let _ = known.save();
        }
    }

    /// Index of the record an `addr:port` lookup resolves to (so mutators avoid a second
    /// borrow).
    ///
    /// A real fingerprint beats a placeholder; among real ones the last record wins —
    /// records are only appended by a trust decision. Lookup order, not authorisation:
    /// the pin still has to match the cert the host presents.
    pub fn index_by_addr(&self, addr: &str, port: u16) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, h) in self.hosts.iter().enumerate() {
            if h.addr != addr || h.port != port {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => !h.fp_hex.is_empty() || self.hosts[b].fp_hex.is_empty(),
            };
            if better {
                best = Some(i);
            }
        }
        best
    }

    pub fn find_by_addr(&self, addr: &str, port: u16) -> Option<&KnownHost> {
        self.index_by_addr(addr, port).map(|i| &self.hosts[i])
    }

    /// Index of the unpinned placeholder saved at `addr:port`. A record pinned there is an
    /// identity the address does not name on its own: both OS installs of a dual-boot box
    /// answer at one lease.
    pub fn placeholder_at(&self, addr: &str, port: u16) -> Option<usize> {
        self.hosts
            .iter()
            .position(|h| h.fp_hex.is_empty() && h.addr == addr && h.port == port)
    }

    /// Index of the record a dial is about. With a pin: the record pinned to it, else the
    /// placeholder at `addr:port` waiting for one — never a record pinned to another
    /// fingerprint, which at a shared address is the other OS of a dual-boot box, with its
    /// own name, binding and clipboard. `Some("")` is a card saved without a pin: its
    /// placeholder only. `None` is a bare typed address: whatever it answers with
    /// ([`KnownHosts::index_by_addr`]).
    pub fn resolve_index(&self, fp_hex: Option<&str>, addr: &str, port: u16) -> Option<usize> {
        let Some(fp_hex) = fp_hex else {
            return self.index_by_addr(addr, port);
        };
        (!fp_hex.is_empty())
            .then(|| self.hosts.iter().position(|h| h.fp_hex == fp_hex))
            .flatten()
            .or_else(|| self.placeholder_at(addr, port))
    }

    pub fn resolve(&self, fp_hex: Option<&str>, addr: &str, port: u16) -> Option<&KnownHost> {
        self.resolve_index(fp_hex, addr, port)
            .map(|i| &self.hosts[i])
    }

    /// Drop the record pinned to `fp_hex`. An empty fingerprint removes NOTHING: `retain`
    /// on `!= ""` would delete every not-yet-paired host at once, which is how one Forget
    /// used to take the whole set. Placeholders go through [`KnownHosts::remove_card`].
    pub fn remove_by_fp(&mut self, fp_hex: &str) -> bool {
        if fp_hex.is_empty() {
            return false;
        }
        let before = self.hosts.len();
        self.hosts.retain(|h| h.fp_hex != fp_hex);
        self.hosts.len() != before
    }

    /// Index of the record a UI card names: its stable [`KnownHost::id`], falling back to
    /// `addr:port` for a record minted before ids existed. Deliberately never keyed by a
    /// fingerprint — a card for an unpaired host carries an empty one.
    pub fn index_of_card(&self, id: Option<&str>, addr: &str, port: u16) -> Option<usize> {
        if let Some(id) = id.filter(|i| !i.is_empty()) {
            if let Some(i) = self.hosts.iter().position(|h| h.id.as_deref() == Some(id)) {
                return Some(i);
            }
        }
        self.index_by_addr(addr, port)
    }

    /// Drop exactly the record a card names — one record, never a class of them.
    pub fn remove_card(&mut self, id: Option<&str>, addr: &str, port: u16) -> bool {
        match self.index_of_card(id, addr, port) {
            Some(i) => {
                self.hosts.remove(i);
                true
            }
            None => false,
        }
    }

    /// Insert or refresh an entry, keyed by fingerprint. `paired` only ever upgrades
    /// (a later TOFU connect must not demote a PIN-paired host).
    pub fn upsert(&mut self, entry: KnownHost) {
        if let Some(h) = self.hosts.iter_mut().find(|h| h.fp_hex == entry.fp_hex) {
            // A label the user chose is theirs. A name that is empty, or is just the address,
            // is one the caller synthesised for want of anything better — re-pairing used to
            // replace "Desk" with "192.168.1.50".
            if !entry.name.is_empty() && (entry.name != entry.addr || h.name.is_empty()) {
                h.name = entry.name;
            }
            h.addr = entry.addr;
            h.port = entry.port;
            h.paired |= entry.paired;
            // A refresh without a timestamp must not erase the stored one.
            if entry.last_used.is_some() {
                h.last_used = entry.last_used;
            }
            // A trust-decision upsert carries no MAC — do not wipe learned ones.
            if !entry.mac.is_empty() {
                h.mac = entry.mac;
            }
            // Same for the OS chain: only a carrier moves it.
            if !entry.os.is_empty() {
                h.os = entry.os;
            }
            // Same for mgmt port: `None` on reconnect must not clear a learned 47991.
            if entry.mgmt_port.is_some() {
                h.mgmt_port = entry.mgmt_port;
            }
            // User-set fields a refresh never carries: clipboard, preset, pins,
            // per-game bindings, id. Only an upsert carrying a value moves one.
            if entry.clipboard_sync {
                h.clipboard_sync = true;
            }
            if entry.preset_id.is_some() {
                h.preset_id = entry.preset_id;
            }
            if !entry.pinned_presets.is_empty() {
                h.pinned_presets = entry.pinned_presets;
            }
            if !entry.game_presets.is_empty() {
                h.game_presets = entry.game_presets;
            }
            if h.id.as_deref().is_none_or(str::is_empty) {
                h.id = entry.id;
            }
        } else {
            self.hosts.push(entry);
        }
    }

    /// Save a host a person typed in, without dialing it. A record already at `addr:port`
    /// takes the edit; otherwise an unpinned placeholder, which the first trust decision
    /// pins ([`upsert_trusted`](Self::upsert_trusted) retires it then, keeping its MACs).
    /// Returns its index.
    pub fn add(&mut self, edit: &HostEdit) -> Result<usize> {
        let addr = edit.addr.as_deref().map(str::trim).unwrap_or_default();
        anyhow::ensure!(!addr.is_empty(), "empty host address");
        let port = edit.port.unwrap_or(9777);
        let i = match self.index_by_addr(addr, port) {
            Some(i) => i,
            None => {
                self.hosts.push(KnownHost {
                    name: addr.to_string(),
                    addr: addr.to_string(),
                    port,
                    ..Default::default()
                });
                self.hosts.len() - 1
            }
        };
        self.hosts[i].apply_edit(edit);
        Ok(i)
    }

    /// [`upsert`](Self::upsert) for an authorised trust decision (PIN, TOFU accept,
    /// delegated, headless pair). Also retires the fp-less placeholders claiming the
    /// same `addr:port`, whose pin this decision is.
    ///
    /// A record carrying a DIFFERENT fingerprint survives: an address is not an
    /// identity. Both OS installs of a dual-boot box answer at one lease with one MAC
    /// and a certificate each, so retiring by address takes the sibling the user
    /// paired. A re-keyed host leaves its old card behind for one Forget.
    /// Box fields (MAC, OS, preset, pins, last_used) ride onto the survivor. Not
    /// carried: `paired`, `clipboard_sync` (cert decisions), and the stable id (a deep
    /// link must not silently retarget).
    ///
    /// Discovery and wake re-key stay on plain `upsert` — an unauthenticated advert
    /// must not delete a saved host by claiming its address.
    pub fn upsert_trusted(&mut self, entry: KnownHost) {
        let (addr, port, fp_hex) = (entry.addr.clone(), entry.port, entry.fp_hex.clone());
        self.upsert(entry);
        // Nothing to supersede with: an fp-less record is a placeholder, not an identity.
        if fp_hex.is_empty() {
            return;
        }
        let (keep, retired): (Vec<KnownHost>, Vec<KnownHost>) = std::mem::take(&mut self.hosts)
            .into_iter()
            .partition(|h| !(h.addr == addr && h.port == port && h.fp_hex.is_empty()));
        self.hosts = keep;
        if retired.is_empty() {
            return;
        }
        let Some(h) = self.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) else {
            return;
        };
        for old in retired {
            tracing::info!(
                addr = %addr, port, kept_fp = %fp_hex,
                "retiring the unpinned placeholder this decision pins"
            );
            if h.mac.is_empty() {
                h.mac = old.mac;
            }
            if h.os.is_empty() {
                h.os = old.os;
            }
            if h.mgmt_port.is_none() {
                h.mgmt_port = old.mgmt_port;
            }
            if h.preset_id.is_none() {
                h.preset_id = old.preset_id;
            }
            if h.pinned_presets.is_empty() {
                h.pinned_presets = old.pinned_presets;
            }
            if h.game_presets.is_empty() {
                h.game_presets = old.game_presets;
            }
            if h.last_used.is_none() {
                h.last_used = old.last_used;
            }
        }
    }
}

/// Load-upsert-save: the pin every trust decision (TOFU, PIN, delegated, headless) ends in.
/// `mac` is the wake MAC(s) the caller learned; empty keeps the saved ones.
pub fn persist_host(
    name: &str,
    addr: &str,
    port: u16,
    fp_hex: &str,
    paired: bool,
    mac: &[String],
) -> Result<()> {
    let mut known = KnownHosts::load();
    // `..Default::default()` so user-set fields arrive uncarried; a literal would
    // reset them on re-pair. `upsert_trusted`: this is the authorised decision.
    known.upsert_trusted(KnownHost {
        name: name.to_string(),
        addr: addr.to_string(),
        port,
        fp_hex: fp_hex.to_string(),
        paired,
        mac: mac.to_vec(),
        ..Default::default()
    });
    // Returned, not swallowed: this is the door every trust decision walks through, and the
    // callers say "Paired" the moment it comes back. A read-only config dir or a sandbox
    // denial used to be announced as success and was gone by the next launch.
    known.save()
}

/// Drop the fp-less placeholder for `addr:port`. `--add-host` with no `--fp` stores one;
/// [`persist_host`] then writes the real pin, so the placeholder would show twice.
/// No-op, and no disk write, when there is none.
pub fn forget_placeholder(addr: &str, port: u16) {
    let mut known = KnownHosts::load();
    let before = known.hosts.len();
    known
        .hosts
        .retain(|h| !(h.fp_hex.is_empty() && h.addr == addr && h.port == port));
    if known.hosts.len() != before {
        let _ = known.save();
    }
}

/// Load, [`KnownHosts::add`], save.
pub fn add_host(edit: &HostEdit) -> Result<()> {
    let mut known = KnownHosts::load();
    known.add(edit)?;
    known.save()
}

/// Record an advert should land on: the one its caller matched, by pin, or the placeholder
/// at its address. The address alone would teach the other OS of a dual-boot box this
/// one's OS mark and MAC.
fn learn_target<'a>(
    known: &'a mut KnownHosts,
    fp_hex: &str,
    addr: &str,
    port: u16,
) -> Option<&'a mut KnownHost> {
    let i = known.resolve_index(Some(fp_hex), addr, port)?;
    known.hosts.get_mut(i)
}

/// Copy MAC / OS / mgmt port from an advert onto a saved record and note the address it
/// advertises; `true` if anything moved. Pure (no disk). An omitted field is left alone, and
/// the MAC is learned once — see below.
fn apply_advert(
    h: &mut KnownHost,
    addr: &str,
    mac: &[String],
    os: &str,
    mgmt_port: Option<u16>,
) -> bool {
    let mut changed = false;
    // A candidate for `probe_known`, never a move: two machines sharing an mDNS hostname
    // hand each other's IPs to both adverts. A record with no pin cannot verify one.
    if !h.fp_hex.is_empty()
        && !addr.is_empty()
        && addr != h.addr
        && !h.prev_addrs.iter().any(|a| a == addr)
    {
        h.prev_addrs.insert(0, addr.to_string());
        h.prev_addrs.truncate(PREV_ADDRS_MAX);
        changed = true;
    }
    // An advert may TEACH a wake MAC, never replace one. The fingerprint it matched on is
    // broadcast in clear, so anything on the LAN can claim to be this host; every other
    // field here is corrected by the next real advert, but a MAC is not — a sleeping host
    // sends none, which is exactly when wake is the only way back.
    if !mac.is_empty() && h.mac.is_empty() {
        h.mac = mac.to_vec();
        changed = true;
    }
    if !os.is_empty() && h.os != os {
        h.os = os.to_string();
        changed = true;
    }
    // 0 is how "not advertised" reaches us from a caller whose own type has no `Option`.
    if mgmt_port.is_some_and(|p| p != 0 && h.mgmt_port != Some(p)) {
        h.mgmt_port = mgmt_port;
        changed = true;
    }
    changed
}

/// Persist MAC / OS / mgmt port from a live advert onto the record saved at `addr:port`,
/// and keep `advert_addr` as a place [`probe_known`](super::probe_known) asks. No-op, and
/// no disk write, when nothing changed — call it on every discovery tick.
///
/// [`KnownHosts::read`], not [`KnownHosts::load`]: `punktfunk discover` is not an
/// id-minter (see the race on [`KnownHosts::read`]). Takes the fields rather than a
/// `DiscoveredHost` because core and the WinUI shell each have their own type.
pub fn learn_from_advert(
    fp_hex: &str,
    addr: &str,
    port: u16,
    advert_addr: &str,
    mac: &[String],
    os: &str,
    mgmt_port: Option<u16>,
) {
    let mut known = KnownHosts::read();
    let Some(h) = learn_target(&mut known, fp_hex, addr, port) else {
        return;
    };
    if apply_advert(h, advert_addr, mac, os, mgmt_port) {
        let _ = known.save();
    }
}

/// Re-point a saved host at the address it now answers at, matched by fingerprint, keeping
/// the one it leaves in `prev_addrs`. No-op, and no disk write, when unchanged. Only for an
/// address where the pin answered: an mDNS advert's address is not proof of who is there.
pub fn rekey_addr(fp_hex: &str, addr: &str, port: u16) {
    KnownHosts::update_by_fp(fp_hex, |h| h.move_to(addr, port));
}

/// Stamp now as this host's last successful connect. No-op if the fingerprint is not stored:
/// an empty one would stamp a placeholder, and `last_used` drives the "most recent" accent.
pub fn touch_last_used(fp_hex: &str) {
    KnownHosts::update_by_fp(fp_hex, |h| {
        h.last_used = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        true
    });
}

/// Persist mgmt port from the session `Welcome`, keyed by fingerprint.
///
/// mDNS-free: [`learn_from_advert`] needs a visible advert; this fires on any successful
/// connect, including a host added by IP. No-op, and no disk write, when unchanged.
pub fn learn_mgmt_port_by_fp(fp_hex: &str, mgmt_port: u16) {
    if mgmt_port == 0 {
        return;
    }
    KnownHosts::update_by_fp(fp_hex, |h| {
        let changed = h.mgmt_port != Some(mgmt_port);
        h.mgmt_port = Some(mgmt_port);
        changed
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::parse_hex32;

    /// Unpaired placeholders must not share the empty pin as a key: each would show the
    /// last-probed one's pip.
    #[test]
    fn card_key_is_the_pin_else_the_address() {
        let placeholder = |addr: &str| KnownHost {
            addr: addr.into(),
            port: 9777,
            ..Default::default()
        };
        assert_eq!(placeholder("10.0.0.2").card_key(), "10.0.0.2:9777");
        assert_ne!(
            placeholder("10.0.0.2").card_key(),
            placeholder("10.0.0.3").card_key()
        );
        let pinned = KnownHost {
            fp_hex: "ab".repeat(32),
            ..placeholder("10.0.0.2")
        };
        assert_eq!(pinned.card_key(), "ab".repeat(32));
    }

    /// 64-hex fingerprint of one repeated digit — readable and distinct per letter.
    fn fp(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    /// WinUI known-hosts shape (no `last_used`) loads; same path so the two clients share it.
    #[test]
    fn known_hosts_reads_winui_shell_shape() {
        let shell = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true, "mac": ["aa:bb:cc:dd:ee:ff"]
        }]}"#;
        let k: KnownHosts = serde_json::from_str(shell).unwrap();
        let h = k.find_by_addr("192.168.1.50", 9777).unwrap();
        assert!(h.paired);
        assert_eq!(h.last_used, None);
        assert_eq!(h.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert!(parse_hex32(&h.fp_hex).is_some());
        // Pre-`os` store loads empty and serializes without the key.
        assert_eq!(h.os, "");
        assert!(!serde_json::to_string(&k).unwrap().contains("\"os\""));
    }

    /// Learned OS chain round-trips; an absent key stays absent.
    #[test]
    fn known_hosts_os_chain_round_trips() {
        let k = KnownHosts {
            hosts: vec![KnownHost {
                name: "HTPC".into(),
                addr: "192.168.1.181".into(),
                port: 9777,
                os: "linux/fedora/bazzite".into(),
                ..Default::default()
            }],
        };
        let text = serde_json::to_string(&k).unwrap();
        let back: KnownHosts = serde_json::from_str(&text).unwrap();
        assert_eq!(back.hosts[0].os, "linux/fedora/bazzite");
    }

    /// A pre-presets store loads with no binding/pins and serializes without the new
    /// keys. Id is minted by `load()`, not by deserialization.
    #[test]
    fn known_hosts_migration_is_a_no_op_on_a_pre_presets_store() {
        let old = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true, "clipboard_sync": true
        }]}"#;
        let mut k: KnownHosts = serde_json::from_str(old).unwrap();
        let h = &k.hosts[0];
        assert_eq!(h.preset_id, None);
        assert!(h.pinned_presets.is_empty());
        assert!(h.game_presets.is_empty());
        assert_eq!(h.id, None);
        assert!(h.clipboard_sync);
        let text = serde_json::to_string(&k).unwrap();
        assert!(!text.contains("preset_id"));
        assert!(!text.contains("pinned_presets"));
        assert!(!text.contains("game_presets"));
        assert!(!text.contains("\"id\""));

        // Second pass reports nothing to persist and leaves the minted id alone.
        assert!(k.mint_missing_ids());
        let minted = k.hosts[0].id.clone().unwrap();
        assert_eq!(minted.len(), 36);
        assert!(!k.mint_missing_ids());
        assert_eq!(k.hosts[0].id.as_deref(), Some(minted.as_str()));
        // Empty-string id counts as missing, not as an identity.
        k.hosts[0].id = Some(String::new());
        assert!(k.mint_missing_ids());
        assert_ne!(k.hosts[0].id.as_deref(), Some(""));
    }

    /// A record saved before the rename keeps its bindings, and serializing writes both
    /// spellings, so a client older than the rename still reads them.
    #[test]
    fn a_pre_rename_host_record_keeps_its_bindings() {
        let old = r#"{"hosts":[{
            "name": "Desk", "addr": "192.168.1.50", "port": 9777, "fp_hex": "", "paired": true,
            "profile_id": "aaaaaaaaaaaa", "pinned_profiles": ["bbbbbbbbbbbb"],
            "game_profiles": {"halo": "cccccccccccc"}
        }]}"#;
        let k: KnownHosts = serde_json::from_str(old).unwrap();
        let h = &k.hosts[0];
        assert_eq!(h.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(h.pinned_presets, vec!["bbbbbbbbbbbb".to_string()]);
        assert_eq!(h.preset_for_game("halo"), Some("cccccccccccc"));

        let saved = serde_json::to_value(&k).unwrap();
        let rec = &saved["hosts"][0];
        for (new, old) in LEGACY_HOST_KEYS {
            assert!(!rec[new].is_null(), "{new} is written");
            assert_eq!(rec[new], rec[old], "{old} mirrors {new}");
        }
        let again: KnownHosts = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(serde_json::to_value(&again).unwrap(), saved);
    }

    /// Both spellings present: the new one wins, since only this client writes it.
    #[test]
    fn a_new_binding_key_wins_over_its_old_spelling() {
        let both = r#"{"hosts":[{"name": "Desk", "addr": "10.0.0.2", "port": 9777,
            "fp_hex": "", "paired": true, "preset_id": "111111111111", "profile_id": "222222222222"}]}"#;
        let k: KnownHosts = serde_json::from_str(both).unwrap();
        assert_eq!(k.hosts[0].preset_id.as_deref(), Some("111111111111"));
    }

    /// `upsert` preserves user-set fields a trust-decision payload does not carry.
    #[test]
    fn upsert_preserves_user_set_host_state() {
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk".into(),
                addr: "192.168.1.50".into(),
                port: 9777,
                fp_hex: fp.into(),
                paired: true,
                last_used: Some(1000),
                mac: vec!["aa:bb:cc:dd:ee:ff".into()],
                os: "linux/fedora/bazzite".into(),
                // Not 47990: the default would make the keep-on-upsert assertions pass vacuously.
                mgmt_port: Some(47991),
                clipboard_sync: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                pinned_presets: vec!["bbbbbbbbbbbb".into()],
                game_presets: [("halo".to_string(), "cccccccccccc".to_string())].into(),
                id: Some("11111111-2222-4333-8444-555555555555".into()),
                prev_addrs: vec![],
            }],
        };
        // What `persist_host` builds: a trust decision, nothing else.
        k.upsert(KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.51".into(),
            port: 9777,
            fp_hex: fp.into(),
            paired: false,
            ..Default::default()
        });
        let h = &k.hosts[0];
        assert_eq!(k.hosts.len(), 1);
        assert_eq!(h.addr, "192.168.1.51");
        assert!(h.paired);
        assert_eq!(h.last_used, Some(1000));
        assert_eq!(h.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert_eq!(h.os, "linux/fedora/bazzite");
        // Reconnect must not reset mgmt port to None — the library would 404 on 47990.
        assert_eq!(h.mgmt_port, Some(47991));
        assert!(h.clipboard_sync);
        assert_eq!(h.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(h.pinned_presets, vec!["bbbbbbbbbbbb".to_string()]);
        assert_eq!(h.preset_for_game("halo"), Some("cccccccccccc"));
        assert_eq!(
            h.id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );

        // A carried value does move the binding (UI rebind path).
        k.upsert(KnownHost {
            fp_hex: fp.into(),
            preset_id: Some("cccccccccccc".into()),
            pinned_presets: vec!["dddddddddddd".into()],
            ..Default::default()
        });
        assert_eq!(k.hosts[0].preset_id.as_deref(), Some("cccccccccccc"));
        assert_eq!(k.hosts[0].pinned_presets, vec!["dddddddddddd".to_string()]);

        // Same for the per-game map: only a payload that carries one replaces it.
        k.hosts[0].bind_game_preset("halo", Some("dddddddddddd"));
        k.upsert(KnownHost {
            fp_hex: fp.into(),
            ..Default::default()
        });
        assert_eq!(k.hosts[0].preset_for_game("halo"), Some("dddddddddddd"));
        k.hosts[0].bind_game_preset("halo", None);
        assert!(k.hosts[0].game_presets.is_empty());
    }

    /// A store written before `mgmt_port` loads, resolves to 47990, then takes and
    /// keeps a learned value — the port must outlive the advert.
    #[test]
    fn mgmt_port_survives_a_store_that_predates_it_and_then_persists() {
        // Store written before the field existed: no `mgmt_port` key.
        let old = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true
        }]}"#;
        let mut k: KnownHosts = serde_json::from_str(old).unwrap();
        assert_eq!(k.hosts[0].mgmt_port, None, "absent key decodes to None");
        assert_eq!(
            k.hosts[0].effective_mgmt_port(),
            crate::library::DEFAULT_MGMT_PORT,
            "unknown resolves to the compiled-in default, i.e. today's behaviour"
        );
        // Unset stays out of the serialized form so an untouched store is byte-stable.
        assert!(!serde_json::to_string(&k).unwrap().contains("mgmt_port"));

        // A learned port takes effect and round-trips.
        k.hosts[0].mgmt_port = Some(47991);
        assert_eq!(k.hosts[0].effective_mgmt_port(), 47991);
        let round: KnownHosts = serde_json::from_str(&serde_json::to_string(&k).unwrap()).unwrap();
        assert_eq!(round.hosts[0].mgmt_port, Some(47991));

        // A placeholder's learned port rides onto the pin that retires it, else it
        // drops back to 47990.
        let fresh = fp('a');
        let mut k2 = KnownHosts {
            hosts: vec![KnownHost {
                addr: "192.168.1.50".into(),
                port: 9777,
                mgmt_port: Some(47991),
                ..Default::default()
            }],
        };
        k2.upsert_trusted(KnownHost {
            name: "Gaming PC".into(),
            addr: "192.168.1.50".into(),
            port: 9777,
            fp_hex: fresh.clone(),
            paired: true,
            ..Default::default()
        });
        let kept = k2.hosts.iter().find(|h| h.fp_hex == fresh).unwrap();
        assert_eq!(
            kept.mgmt_port,
            Some(47991),
            "the pin must not lose the port"
        );
    }

    /// Two identities at one address are two records. A dual-boot box answers on one
    /// lease with one MAC and a certificate per OS, so pairing the second OS must not
    /// retire the first: it took the user's name, presets and pins with it.
    #[test]
    fn upsert_trusted_keeps_a_second_identity_at_one_address() {
        let (first, second) = (fp('c'), fp('a'));
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "ENRICOS-DESKTOP (local)".into(),
                addr: "127.0.0.1".into(),
                port: 9777,
                fp_hex: first.clone(),
                paired: true,
                last_used: Some(1000),
                mac: vec!["aa:bb:cc:dd:ee:ff".into()],
                os: "windows".into(),
                mgmt_port: Some(47991),
                clipboard_sync: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                pinned_presets: vec!["bbbbbbbbbbbb".into()],
                game_presets: [("halo".to_string(), "cccccccccccc".to_string())].into(),
                id: Some("11111111-2222-4333-8444-555555555555".into()),
                prev_addrs: vec![],
            }],
        };
        // The other OS on that box: same address, a certificate the client has never seen.
        k.upsert_trusted(KnownHost {
            name: "127.0.0.1".into(),
            addr: "127.0.0.1".into(),
            port: 9777,
            fp_hex: second.clone(),
            paired: true,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 2);
        let kept = k.find_by_fp(&first).expect("the first OS keeps its record");
        assert_eq!(kept.name, "ENRICOS-DESKTOP (local)");
        assert_eq!(kept.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert_eq!(kept.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(kept.pinned_presets, vec!["bbbbbbbbbbbb".to_string()]);
        assert_eq!(kept.preset_for_game("halo"), Some("cccccccccccc"));
        assert!(kept.clipboard_sync);
        assert_eq!(
            kept.id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555"),
            "a deep link to the first OS still resolves"
        );
        assert!(k.find_by_fp(&second).is_some());
        // The address alone can only answer with one of them: the newest decision.
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, second);
    }

    /// A host that only moved address keeps its one record, `paired`, clipboard, and id.
    #[test]
    fn upsert_trusted_keeps_a_host_that_only_moved_address() {
        let same = fp('a');
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk".into(),
                addr: "192.168.1.50".into(),
                port: 9777,
                fp_hex: same.clone(),
                paired: true,
                clipboard_sync: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                id: Some("11111111-2222-4333-8444-555555555555".into()),
                ..Default::default()
            }],
        };
        k.upsert_trusted(KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.51".into(),
            port: 9777,
            fp_hex: same.clone(),
            paired: false,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 1);
        let h = &k.hosts[0];
        assert_eq!(h.addr, "192.168.1.51");
        assert!(h.paired);
        assert!(h.clipboard_sync);
        assert_eq!(h.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(
            h.id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
    }

    /// The addresses a host left are kept, newest first, without repeats, three at most.
    #[test]
    fn move_to_remembers_the_addresses_a_host_left() {
        let mut h = KnownHost {
            addr: "100.64.0.7".into(),
            fp_hex: fp('a'),
            ..Default::default()
        };
        assert!(h.move_to("192.168.1.9", 9777));
        assert!(!h.move_to("192.168.1.9", 9777));
        assert!(h.move_to("100.64.0.7", 9777));
        assert_eq!(h.prev_addrs, ["192.168.1.9"]);
        for a in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
            h.move_to(a, 9777);
        }
        assert_eq!(h.prev_addrs, ["10.0.0.2", "10.0.0.1", "100.64.0.7"]);
        assert_eq!(h.addr, "10.0.0.3");
    }

    /// Superseding is scoped to the decision's `addr:port`. An fp-less save retires nothing.
    #[test]
    fn upsert_trusted_leaves_other_addresses_and_placeholders_alone() {
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    name: "Other box".into(),
                    addr: "192.168.1.50".into(),
                    port: 9777,
                    fp_hex: fp('c'),
                    paired: true,
                    ..Default::default()
                },
                // Same address, different port: a distinct endpoint, not a duplicate.
                KnownHost {
                    name: "Second host".into(),
                    addr: "192.168.1.51".into(),
                    port: 9778,
                    fp_hex: fp('d'),
                    paired: true,
                    ..Default::default()
                },
            ],
        };
        k.upsert_trusted(KnownHost {
            name: "New box".into(),
            addr: "192.168.1.51".into(),
            port: 9777,
            fp_hex: fp('a'),
            paired: true,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 3);
        assert_eq!(
            k.find_by_addr("192.168.1.50", 9777).unwrap().fp_hex,
            fp('c')
        );
        assert_eq!(
            k.find_by_addr("192.168.1.51", 9778).unwrap().fp_hex,
            fp('d')
        );

        // Fp-less save alongside a real record: nothing retired; address still hits the pin.
        k.upsert_trusted(KnownHost {
            name: "Typed by hand".into(),
            addr: "192.168.1.50".into(),
            port: 9777,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 4);
        assert_eq!(
            k.find_by_addr("192.168.1.50", 9777).unwrap().fp_hex,
            fp('c')
        );
    }

    /// A duplicated store resolves to the newest trust decision, not the first record.
    /// Load does not delete; retirement waits for the next trust decision.
    #[test]
    fn a_duplicated_store_resolves_to_the_newest_record() {
        let (dead, live) = (fp('c'), fp('a'));
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    name: "ENRICOS-DESKTOP (local)".into(),
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: dead.clone(),
                    paired: true,
                    last_used: Some(9999),
                    ..Default::default()
                },
                KnownHost {
                    name: "127.0.0.1".into(),
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: live.clone(),
                    paired: true,
                    ..Default::default()
                },
            ],
        };
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, live);
        assert!(k.find_by_fp(&dead).is_some());
        // A placeholder appended later never displaces a real pin.
        k.hosts.push(KnownHost {
            addr: "127.0.0.1".into(),
            port: 9777,
            ..Default::default()
        });
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, live);
        k.upsert_trusted(KnownHost {
            name: "127.0.0.1".into(),
            addr: "127.0.0.1".into(),
            port: 9777,
            fp_hex: live.clone(),
            paired: true,
            ..Default::default()
        });
        assert_eq!(
            k.hosts.len(),
            2,
            "the placeholder goes, the other pin stays"
        );
        assert!(k.find_by_fp(&dead).is_some());
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, live);
    }

    /// An advert lands on the fingerprint match, not a stale namesake earlier in the file.
    /// Forgetting one host that has never been paired takes that host and no other.
    ///
    /// `remove_by_fp` is `retain(|h| h.fp_hex != key)`, so an empty key kept only the hosts
    /// WITH a fingerprint — one Forget on an address-added card wiped every address-added
    /// card in the file. Records saved by address really do carry an empty fingerprint
    /// (`KnownHost { addr, port, ..Default::default() }`), which is what made it reachable.
    #[test]
    fn forgetting_one_unpaired_host_keeps_the_others() {
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    name: "Desk".into(),
                    addr: "192.168.1.50".into(),
                    port: 9777,
                    ..Default::default()
                },
                KnownHost {
                    name: "Shed".into(),
                    addr: "192.168.1.51".into(),
                    port: 9777,
                    ..Default::default()
                },
                KnownHost {
                    name: "Paired".into(),
                    addr: "192.168.1.52".into(),
                    port: 9777,
                    fp_hex: fp('a'),
                    ..Default::default()
                },
            ],
        };
        assert!(k.hosts.iter().all(|h| h.id.is_some()), "ids are minted");

        // An empty fingerprint is not a key, in either direction.
        assert!(!k.remove_by_fp(""), "an empty fingerprint removes nothing");
        assert_eq!(k.hosts.len(), 3);
        assert!(k.find_by_fp("").is_none(), "and finds nothing");

        // Forget "Shed" by the card's own identity.
        let shed = k.hosts[1].id.clone();
        assert!(k.remove_card(shed.as_deref(), "192.168.1.51", 9777));
        let left: Vec<&str> = k.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(left, ["Desk", "Paired"], "only the named card went");

        // A record old enough to predate the minted ids still resolves by address.
        k.hosts[0].id = None;
        assert!(k.remove_card(None, "192.168.1.50", 9777));
        assert_eq!(k.hosts.len(), 1);
        // …and a card naming nothing in the file removes nothing.
        assert!(!k.remove_card(None, "192.168.1.99", 9777));
        assert_eq!(k.hosts.len(), 1);
    }

    #[test]
    fn learn_target_prefers_the_fingerprint_match() {
        let (dead, live) = (fp('c'), fp('a'));
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: dead.clone(),
                    ..Default::default()
                },
                KnownHost {
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: live.clone(),
                    ..Default::default()
                },
            ],
        };
        learn_target(&mut k, &live, "127.0.0.1", 9777).unwrap().os = "windows".into();
        assert_eq!(k.find_by_fp(&live).unwrap().os, "windows");
        assert_eq!(k.find_by_fp(&dead).unwrap().os, "");
        // A pin nobody holds is not its neighbour's; no pin names only a placeholder.
        assert!(learn_target(&mut k, &fp('e'), "127.0.0.1", 9777).is_none());
        assert!(learn_target(&mut k, "", "127.0.0.1", 9777).is_none());
        // Unknown host: write nothing.
        assert!(learn_target(&mut k, &fp('e'), "10.0.0.9", 9777).is_none());
    }

    /// Both OS installs of a dual-boot box at one address: a pin resolves its own record or
    /// the placeholder waiting for it, never the sibling. Only a bare address falls back.
    #[test]
    fn a_pin_resolves_its_own_record_never_the_sibling() {
        let (windows, linux) = (fp('a'), fp('b'));
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk (Windows)".into(),
                addr: "192.168.1.9".into(),
                port: 9777,
                fp_hex: windows.clone(),
                paired: true,
                ..Default::default()
            }],
        };
        assert!(k.resolve(Some(&linux), "192.168.1.9", 9777).is_none());
        assert!(k.resolve(Some(""), "192.168.1.9", 9777).is_none());
        assert!(k.placeholder_at("192.168.1.9", 9777).is_none());
        assert_eq!(
            k.resolve(None, "192.168.1.9", 9777).unwrap().fp_hex,
            windows
        );
        // A moved lease is still the same pin.
        let moved = k.resolve(Some(&windows), "192.168.1.20", 9777);
        assert_eq!(moved.unwrap().name, "Desk (Windows)");

        k.hosts.push(KnownHost {
            name: "Desk (Linux)".into(),
            addr: "192.168.1.9".into(),
            port: 9777,
            ..Default::default()
        });
        assert_eq!(k.placeholder_at("192.168.1.9", 9777), Some(1));
        let waiting = k.resolve(Some(&linux), "192.168.1.9", 9777);
        assert_eq!(waiting.unwrap().name, "Desk (Linux)");
        // An advert from the second OS teaches its placeholder, not the Windows record.
        learn_target(&mut k, "", "192.168.1.9", 9777).unwrap().os = "linux/arch/cachyos".into();
        assert_eq!(k.hosts[0].os, "");
        assert_eq!(k.hosts[1].os, "linux/arch/cachyos");
    }

    /// An advert writes what it carries, leaves omitted fields, and reports no change
    /// on a repeat — so every discovery tick can call it.
    #[test]
    fn apply_advert_learns_what_it_carries_and_keeps_what_it_omits() {
        let mut h = KnownHost::default();
        let mac = vec!["aa:bb:cc:dd:ee:ff".to_string()];
        assert!(apply_advert(&mut h, "", &mac, "linux/arch", Some(47991)));
        assert_eq!(h.mac, mac);
        assert_eq!(h.os, "linux/arch");
        assert_eq!(h.mgmt_port, Some(47991));
        assert!(!apply_advert(&mut h, "", &mac, "linux/arch", Some(47991)));
        // Absent fields must not overwrite a known MAC — that would cost wake.
        assert!(!apply_advert(&mut h, "", &[], "", None));
        assert_eq!(h.mac, mac);
        assert_eq!(h.os, "linux/arch");
        assert_eq!(h.mgmt_port, Some(47991));
        // 0 is "not advertised" from a caller with no Option — not a port.
        assert!(!apply_advert(&mut h, "", &[], "", Some(0)));
        assert_eq!(h.mgmt_port, Some(47991));
        assert!(apply_advert(&mut h, "", &[], "", Some(47992)));
        assert_eq!(h.mgmt_port, Some(47992));
    }

    /// An advertised address never moves a pinned card: two machines sharing a hostname
    /// hand each other's IPs to both adverts. It waits in `prev_addrs` for `probe_known`.
    #[test]
    fn apply_advert_notes_its_address_without_moving() {
        let mut h = KnownHost {
            addr: "192.168.1.9".into(),
            fp_hex: "ab".repeat(32),
            ..Default::default()
        };
        assert!(apply_advert(&mut h, "192.168.1.20", &[], "", None));
        assert_eq!(h.addr, "192.168.1.9");
        assert_eq!(h.prev_addrs, ["192.168.1.20"]);
        assert!(!apply_advert(&mut h, "192.168.1.20", &[], "", None));
        assert!(!apply_advert(&mut h, "192.168.1.9", &[], "", None));
        // Nothing to verify a candidate against without a pin.
        let mut bare = KnownHost {
            addr: "192.168.1.9".into(),
            ..Default::default()
        };
        assert!(!apply_advert(&mut bare, "192.168.1.20", &[], "", None));
        assert!(bare.prev_addrs.is_empty());
    }

    /// Pins render in card order, deduplicated; dangling ids disappear, never error.
    #[test]
    fn resolved_pins_drop_duplicates_and_dangling_ids() {
        use crate::presets::{PresetsFile, StreamPreset};
        let catalog = PresetsFile {
            version: 1,
            presets: vec![
                StreamPreset {
                    id: "aaaaaaaaaaaa".into(),
                    name: "Work".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "bbbbbbbbbbbb".into(),
                    name: "Game".into(),
                    ..StreamPreset::new("")
                },
            ],
        };
        let h = KnownHost {
            pinned_presets: vec![
                "bbbbbbbbbbbb".into(),
                "deleted00000".into(),
                "bbbbbbbbbbbb".into(),
                "aaaaaaaaaaaa".into(),
            ],
            ..Default::default()
        };
        let names: Vec<&str> = h
            .resolved_pins(&catalog)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["Game", "Work"]);
        assert!(KnownHost::default().resolved_pins(&catalog).is_empty());
    }

    /// A typed edit moves the address the way a re-key does and replaces the MACs; blank
    /// fields are no edit; a typed MAC outranks any advert.
    #[test]
    fn a_typed_edit_moves_the_address_and_owns_the_macs() {
        let typed = vec!["01:02:03:04:05:06".to_string()];
        let learned = vec!["aa:bb:cc:dd:ee:ff".to_string()];
        let mut h = KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.9".into(),
            fp_hex: fp('a'),
            mac: learned.clone(),
            ..Default::default()
        };
        assert!(h.apply_edit(&HostEdit {
            name: Some(" Den ".into()),
            addr: Some("192.168.1.20".into()),
            port: Some(9800),
            macs: Some(typed.clone()),
        }));
        assert_eq!(h.name, "Den");
        assert_eq!((h.addr.as_str(), h.port), ("192.168.1.20", 9800));
        assert_eq!(h.prev_addrs, ["192.168.1.9"]);
        assert_eq!(h.mac, typed);

        let blank = HostEdit {
            name: Some("  ".into()),
            addr: Some(String::new()),
            ..Default::default()
        };
        assert!(!h.apply_edit(&blank));
        assert_eq!(h.name, "Den");
        assert!(!apply_advert(&mut h, "", &learned, "", None));
        assert_eq!(h.mac, typed);

        // Cleared, the next advert may teach one again.
        assert!(h.apply_edit(&HostEdit {
            macs: Some(Vec::new()),
            ..Default::default()
        }));
        assert!(apply_advert(&mut h, "", &learned, "", None));
        assert_eq!(h.mac, learned);
    }

    /// Add saves an unpinned placeholder with its MACs; the same address again edits it; the
    /// first pairing pins it and keeps what was typed.
    #[test]
    fn an_added_host_waits_as_a_placeholder_for_its_first_pairing() {
        let macs = vec!["aa:bb:cc:dd:ee:ff".to_string()];
        let mut k = KnownHosts::default();
        let edit = HostEdit {
            addr: Some("192.168.1.9".into()),
            port: Some(9777),
            macs: Some(macs.clone()),
            ..Default::default()
        };
        let i = k.add(&edit).unwrap();
        assert_eq!(k.hosts[i].name, "192.168.1.9");
        assert!(k.hosts[i].fp_hex.is_empty());
        assert_eq!(k.hosts[i].mac, macs);

        let named = HostEdit {
            name: Some("Desk".into()),
            ..edit.clone()
        };
        assert_eq!(k.add(&named).unwrap(), i);
        assert_eq!(k.hosts.len(), 1);
        assert_eq!(k.hosts[0].name, "Desk");

        k.upsert_trusted(KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.9".into(),
            port: 9777,
            fp_hex: fp('a'),
            paired: true,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 1);
        assert_eq!(k.hosts[0].mac, macs);
        assert!(k.add(&HostEdit::default()).is_err());
    }
}
