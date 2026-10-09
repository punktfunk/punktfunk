// The host rows the QAM panel and the game page's "Play from" menu draw: the saved store and the
// live mDNS browse joined by pf-client-core's `same_host` rule. Pure, so scripts/test-merge.ts
// replays clients/shared/host-row-vectors.json through it under plain node.
import type { DiscoveredHost, Preset, SavedHost } from "./backend";

/**
 * How far this device has got with a host. The three states are what the row says under the
 * name, and which of them a host is in decides whether pressing it streams or opens the trust
 * sheet.
 *
 * - `paired`       — the host approved this device (a PIN ceremony, or request access).
 * - `trusted`      — its fingerprint is pinned but nobody has approved us yet. Streams work if
 *                    the host's policy is `optional`; under `required` the connect parks.
 * - `needs-access` — no pinned fingerprint. Not streamable until the trust sheet runs.
 */
export type TrustState = "paired" | "trusted" | "needs-access";

/**
 * One host as the panel shows it — the union of the saved store and the live mDNS browse.
 *
 * A saved host is ONLINE when it either advertises or answers the reachability probe, so a box
 * reached over Tailscale/VPN stops reading as offline. Discovered hosts that aren't saved are
 * appended as extra rows.
 */
export interface HostView {
  name: string;
  addr: string;
  port: number;
  /**
   * The fingerprint PINNED ON THE RECORD. "" means nothing is pinned, which is exactly what
   * makes a host unstreamable — the session binary refuses a pinless connect.
   *
   * Deliberately NOT filled in from a live advert. A host saved by address that happens to be
   * advertising right now still has an empty pin on disk, and borrowing the advert's here would
   * draw it as ready to stream while every launch refused for want of a fingerprint. What the
   * advert offers is [`advertisedFp`], and moving it onto the record is a trust decision the
   * user makes in the sheet.
   */
  fp: string;
  /** What the host is advertising right now, if anything — what request access would pin. */
  advertisedFp: string;
  /**
   * The host is answering at an address its record does not carry — it changed DHCP lease.
   *
   * This matters because a launch names the host by [`ref`], and the CLI dials whatever address
   * the RECORD holds. So the row would show the live address and dial the dead one. The record
   * has to be re-pointed before such a host can stream; `startStream` does it.
   */
  moved: boolean;
  paired: boolean;
  online: boolean;
  /**
   * The record carries a MAC, so a launch can wake it: the CLI runs its wake-and-wait loop
   * before dialling when the client's auto-wake setting is on. What lets an offline host still
   * be listed for a title.
   */
  wakeable: boolean;
  saved: boolean;
  /** The advert's policy ("required"|"optional"); "" when the host isn't advertising. */
  pairPolicy: string;
  /** OS-identity chain (live advert preferred, else the stored one); "" unknown. */
  os: string;
  /**
   * What a launch should NAME this host by: the record's stable id, which survives renames and
   * DHCP moves, falling back to `addr:port` for a row that has no record yet (a discovered host
   * the trust sheet is about to save, or a client too old to have minted ids).
   */
  ref: string;
  /** The host's default preset binding — applied silently by a plain connect, not a card. */
  preset: Preset | null;
  /** The cards to render nested under this host; already resolved against the catalog. */
  pinnedPresets: Preset[];
  lastUsed: number | null;
}

export function trustState(v: HostView): TrustState {
  if (v.paired) return "paired";
  return v.fp ? "trusted" : "needs-access";
}

/**
 * Must this host go through the trust sheet before it can stream?
 *
 * A pinned fingerprint is the ONLY rule. The session binary refuses a pinless connect, so a row
 * without one can offer nothing but a button that fails; with one, the connect is verified and
 * the host either admits it or parks it for an operator. The old rule also consulted the
 * advertised policy for unsaved hosts, which made the answer depend on which of two lists a row
 * came from — the same box could read differently before and after being saved.
 */
export function needsPair(v: HostView): boolean {
  return v.fp === "";
}

/**
 * Is this advert that saved record? The CLI's own match decides when it names the record; the
 * rule below, `same_host` in pf-client-core, covers a client too old to say and a record the CLI
 * read before it had an id. Two known fingerprints decide it alone: the other OS of a dual-boot
 * box answers at the same lease with the same MAC, so the address would read it as the OS
 * already saved.
 */
function advertMatchesSaved(a: DiscoveredHost, s: SavedHost): boolean {
  if (a.saved_id && s.id) return a.saved_id === s.id;
  if (s.fp_hex && a.fp) return s.fp_hex.toLowerCase() === a.fp.toLowerCase();
  return s.addr === a.addr && s.port === a.port;
}

/**
 * The label a saved row shows.
 *
 * A saved record whose name IS its own address is a PLACEHOLDER, not a choice: `hosts add`
 * falls back to the address when the pairing path had nothing better, so the row ends up
 * captioned with the same string it already prints underneath. When the box is on the air it
 * is advertising its actual hostname — prefer that, and the row reads "home-worker-5" instead
 * of "192.168.1.21".
 *
 * A real saved name always wins over the advert, even a stale one: it may be a name the user
 * chose, and a live advert must never quietly overwrite that. Compared against the SAVED
 * address, so a host that moved DHCP lease still recognises its old address as a placeholder.
 */
function hostLabel(s: SavedHost, advert?: DiscoveredHost): string {
  const placeholder = !s.name || s.name === s.addr || s.name === `${s.addr}:${s.port}`;
  if (!placeholder) return s.name;
  return advert?.name || s.name || s.addr;
}

/**
 * Join the saved store and the live browse into the rows the panel draws.
 *
 * Fingerprint first, address second — a host that moved DHCP lease still matches its record,
 * and a different box that inherited the old address does not inherit its pairing.
 */
export function mergeHosts(saved: SavedHost[], discovered: DiscoveredHost[]): HostView[] {
  const views: HostView[] = saved.map((s) => {
    // Prefer a live advert's address: the host may have moved since it was last saved.
    const advert = discovered.find((a) => advertMatchesSaved(a, s));
    return {
      name: hostLabel(s, advert),
      addr: advert?.addr ?? s.addr,
      port: advert?.port ?? s.port,
      fp: s.fp_hex,
      advertisedFp: advert?.fp ?? "",
      moved: !!advert && (advert.addr !== s.addr || advert.port !== s.port),
      paired: s.paired,
      // The probe decides, not the advert: a suspending host sends no mDNS goodbye, so its
      // record lingers for up to 75 minutes — green pip, hidden Wake row, asleep machine.
      // The advert only stands in when the probe was skipped (`null`).
      online: s.online ?? !!advert,
      wakeable: (s.mac ?? []).length > 0,
      saved: true,
      pairPolicy: advert?.pair ?? "",
      os: advert?.os || s.os || "",
      ref: s.id || `${advert?.addr ?? s.addr}:${advert?.port ?? s.port}`,
      preset: s.preset ?? s.profile ?? null,
      pinnedPresets: s.pinned_presets ?? s.pinned_profiles ?? [],
      lastUsed: s.last_used,
    };
  });
  for (const a of discovered) {
    if (saved.some((s) => advertMatchesSaved(a, s))) {
      continue; // already rendered as its saved row, with a live pip
    }
    views.push({
      name: a.name,
      addr: a.addr,
      port: a.port,
      // No record, so nothing is pinned — whatever it advertises is an OFFER, not a pin.
      fp: "",
      advertisedFp: a.fp,
      moved: false, // no record, so nothing to be stale
      paired: false, // no record, so no pairing: an older CLI may still say otherwise
      online: true,
      wakeable: false, // no record, so no MAC
      saved: false,
      pairPolicy: a.pair,
      os: a.os,
      ref: `${a.addr}:${a.port}`,
      preset: null,
      pinnedPresets: [],
      lastUsed: null,
    });
  }
  return views.sort(sortRows);
}

/**
 * Online first, then most recently used, then by name. The host you streamed last night should
 * be the first thing under your thumb; a host that is off right now should never be.
 */
function sortRows(a: HostView, b: HostView): number {
  if (a.online !== b.online) return a.online ? -1 : 1;
  if ((a.lastUsed ?? 0) !== (b.lastUsed ?? 0)) return (b.lastUsed ?? 0) - (a.lastUsed ?? 0);
  return a.name.localeCompare(b.name);
}
