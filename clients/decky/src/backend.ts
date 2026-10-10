// Bridge to the Python backend (main.py) + shared types.
//
// Every call here is a thin shell over the headless `punktfunk` CLI, so these types are the
// CLI's JSON shapes rather than anything this plugin invents. That is deliberate: the plugin
// used to model the client's stores itself and drifted from them with every field the client
// added.

import { callable } from "@decky/api";

/** A preset as the CLI resolves it — ids are dangling-checked and names attached. */
export interface Preset {
  id: string;
  name: string;
}

/**
 * A host answering on mDNS right now (`punktfunk discover --json`).
 *
 * `saved`/`paired` are annotated BY THE CLI against the saved-hosts store — fingerprint first,
 * address second. The plugin does not join the two lists itself; that rule living in one place
 * is what stops this surface disagreeing with the desktop client about the same box.
 */
export interface DiscoveredHost {
  name: string;
  addr: string;
  port: number;
  fp: string; // advertised cert fingerprint (lowercase hex); "" when not advertised
  pair: string; // the HOST's policy: "required" | "optional"
  id: string; // the host's advertised stable id; "" when not advertised
  mgmt: number; // management-API port; 0 = not advertised
  os: string; // OS-identity chain, e.g. "linux/fedora/bazzite"; "" on older hosts
  saved: boolean;
  /** The saved record's id when the CLI matched one; null or absent from an older client. */
  saved_id?: string | null;
  paired: boolean;
}

/**
 * A host in the shared saved-hosts store (`punktfunk hosts list --probe --json`) — the same
 * `client-known-hosts.json` the desktop client owns.
 *
 * `online` comes from a mDNS-INDEPENDENT probe, so a host reached over Tailscale/VPN is not
 * shown offline merely because it never advertises; `null` means the probe was skipped.
 *
 * `preset` is the host's DEFAULT binding, which a plain connect applies silently. It is not
 * the same thing as `pinned_presets`, which are the cards a user chose to surface. Both come
 * back already resolved against the preset catalog, so this plugin never opens it. A client
 * older than the rename sends them only as `profile` / `pinned_profiles`.
 */
export interface SavedHost {
  id: string | null; // the record's stable id — the reference a launch should use
  name: string;
  addr: string;
  port: number;
  fp_hex: string; // "" for a placeholder saved by address with no pin yet
  paired: boolean;
  mac: string[];
  os: string;
  last_used: number | null;
  clipboard_sync: boolean;
  preset?: Preset | null;
  pinned_presets?: Preset[];
  profile?: Preset | null;
  pinned_profiles?: Preset[];
  online: boolean | null;
}

/**
 * Every backend call answers in this shape. `error` is a stable code, never prose:
 *
 * - `client-unavailable` — no client is installed, or the call never ran
 * - `client-outdated`    — the installed client predates the verb (exit 5 + `unknown command`)
 * - `unreachable`        — the host did not answer
 * - `refused`            — trust rejected: a wrong PIN, or a fingerprint that already differs
 * - `needs-pairing`      — the CLI refused because it needs a person
 * - `unresolved`         — nothing matched what was named
 * - `client-error`       — anything else; `detail` carries the CLI's own last line
 */
export interface CliResult {
  ok: boolean;
  error?: string;
  detail?: string;
}

export interface DiscoverResult extends CliResult {
  hosts?: DiscoveredHost[];
}

export interface HostsResult extends CliResult {
  hosts?: SavedHost[];
}

export interface PairResult extends CliResult {
  fp?: string;
}

/**
 * One title of a host's library (`punktfunk library <ref> --json`). `id` is store-qualified —
 * `steam:570`, `custom:…` — and is the handle a launch names; the Steam ones are what the game
 * page matches against Steam's own appids.
 */
export interface LibraryGame {
  id: string;
  store: string;
  title: string;
}

export interface LibraryResult extends CliResult {
  games?: LibraryGame[];
}

/** One profile on a host, from `punktfunk profiles <ref> --json`. */
export interface HostProfile {
  id: string;
  display_name: string;
  accent?: string | null;
  owner: boolean;
  /** The one line under it: `Steam sign-in once`, `In use by Ben's Apple TV`. */
  note?: string | null;
}

export interface ProfilesResult extends CliResult {
  profiles?: HostProfile[];
  /** The profile this Deck plays as on that host, if it picked one. */
  picked?: string | null;
}

export interface RunnerInfo {
  runner: string; // absolute path to bin/punktfunkrun.sh
  app_id: string; // flatpak app id
  exists: boolean;
  // Which client the backend resolved: the flatpak, a native install (.deb/rpm/sysext/AUR/nix),
  // or none at all. Older backends send neither field — hence optional.
  client_kind?: "flatpak" | "native" | "none";
  // Absolute path of the native binary; "" for flatpak. Passed to the wrapper as PF_CLIENT_BIN.
  client_bin?: string;
}

export interface UpdateInfo {
  current: string; // installed PLUGIN version (package.json)
  latest: string; // newest plugin version in our registry for this channel
  artifact: string; // immutable zip URL Decky should install
  hash: string; // sha256 of that zip (Decky verifies it)
  channel: string; // "latest" (stable) | "canary"
  update_available: boolean; // a newer PLUGIN build is available
  // The CLIENT versions independently of this plugin, and how it updates depends on how it was
  // installed. A flatpak is a per-user install `sudo flatpak update` never touches, compared by
  // OSTree commit; every other install (.deb/.rpm/pacman/sysext/nix/source) is compared by the
  // client itself against the signed per-channel manifest (`punktfunk-client --check-update`).
  client_update_available: boolean;
  client_current: string; // installed client commit (flatpak) or version (native)
  client_latest: string; // newest client commit (flatpak) or version (native)
  client_install: string; // "flatpak" | "apt" | "dnf" | "pacman" | "sysext" | "nix" | "source" | ""
  // Who can perform the update: "flatpak" (this plugin runs it), "helper" (the client drives the
  // packaged root helper), "none" (nothing here can — show `client_command`).
  client_applier: string;
  client_command: string; // one copy-pastable line that updates this install by hand
  client_opt_in: string; // set when one-tap WOULD work after `usermod -aG punktfunk-update`
  // The client check couldn't complete — NEVER rendered as "up to date". "client-outdated" |
  // "client-unavailable" | "no-origin" | "fetch-failed" (flatpak: the remote was unreachable).
  client_error?: string;
  error?: string; // "update-channel-unknown" (dev build) | "fetch-failed"
}

// Steam-shortcut artwork (assets/ in the plugin dir): base64 PNGs keyed grid / gridwide /
// hero / logo, plus the icon's absolute path (SetShortcutIcon wants a file). Keys for
// missing files are absent.
export interface ShortcutArt {
  grid?: string;
  gridwide?: string;
  hero?: string;
  logo?: string;
  icon_path: string;
}

// ---- The four CLI shells --------------------------------------------------------------

/** Browse the LAN over mDNS. Bounded by the CLI (3 s) plus a cold-start allowance. */
export const discover = callable<[], DiscoverResult>("discover");
/** The saved hosts, probed for reachability, with profiles and pinned cards resolved. */
export const hosts = callable<[], HostsResult>("hosts");
/** The PIN ceremony. `refused` = wrong PIN or a host that isn't armed. */
export const pair = callable<
  [addr: string, port: number, pin: string, name: string],
  PairResult
>("pair");
/**
 * No-PIN pairing: wait (up to ~185 s) for the host's operator to approve this Deck, pinned to the
 * fingerprint `trustHost` saved. Never streams. On failure `detail` is a sentence for the user.
 */
export const requestAccess = callable<[addr: string, port: number, name: string], PairResult>(
  "request_access",
);
/**
 * Step 1 of request access: save the host with its ADVERTISED fingerprint, pinned but unpaired.
 * The launch that follows pins the same fingerprint, which is the only thing standing between a
 * 185 s wait for approval and an impostor answering for the host. Idempotent; a host already
 * saved under a DIFFERENT fingerprint comes back `refused` rather than being overwritten.
 */
export const trustHost = callable<
  [addr: string, port: number, fp: string, name: string],
  CliResult
>("trust_host");
/**
 * The host's game library. Paired hosts only: the library routes take the paired identity
 * over mTLS, so a host that merely has a pinned fingerprint answers `needs-pairing`.
 */
export const library = callable<[ref: string], LibraryResult>("library");

export const profiles = callable<[ref: string], ProfilesResult>("profiles");

// ---- Steam / plugin business (only a Decky plugin can do these) ------------------------

export const runnerInfo = callable<[], RunnerInfo>("runner_info");
export const shortcutArt = callable<[], ShortcutArt>("shortcut_art");

/**
 * A Steam game's own artwork, for the hidden shortcut that streams it: base64 images keyed like
 * `ShortcutArt`, each with a `<key>_type` of `jpg` | `png`, and the icon as bytes. Any piece
 * Steam's cache and the store CDN both lack is absent.
 */
export interface GameArt {
  ok: boolean;
  error?: string;
  grid?: string;
  grid_type?: string;
  gridwide?: string;
  gridwide_type?: string;
  hero?: string;
  hero_type?: string;
  logo?: string;
  logo_type?: string;
  /** The icon's bytes (`jpg` from Steam's cache or CDN, `png` for the Punktfunk fallback). */
  icon?: string;
  icon_type?: string;
  /** A ready file for SetShortcutIcon — only set for the PNG fallback. */
  icon_path?: string;
}
export const gameArt = callable<[appid: number, iconHash: string], GameArt>("game_art");
/** Write a converted PNG icon for a shortcut; returns the path SetShortcutIcon wants. */
export const saveIcon = callable<
  [appid: number, pngBase64: string],
  { ok: boolean; path?: string; error?: string }
>("save_icon");
// Install the Steam Input layout (native touchscreen `ts_n` + gamepad passthrough) and point our
// shortcut(s) at it, so the Deck touchscreen reaches the client as native touch with no manual
// controller setup. Best-effort + idempotent; keyed by the shared shortcut NAME (both shortcuts
// use the same name → the same lowercase configset key), so one call covers both.
export const applyControllerConfig = callable<
  [name: string],
  { ok: boolean; applied?: string[]; errors?: string[]; accounts?: number; error?: string; detail?: string }
>("apply_controller_config");
export const killStream = callable<[], { ok: boolean }>("kill_stream");
// Whether the streaming client's control socket exists (a stream/console client is up) —
// gates the QAM panel's host-button section.
export const streamRunning = callable<[], { running: boolean }>("stream_running");
// Press a HOST system button on the running stream: "guide" | "qam". The raw Steam/QAM
// presses stay on the Deck by default (the client's Controllers settings), so this — and
// holding Select — is how the host's own menus are reached.
export const hostAction = callable<[action: string], { ok: boolean; error?: string }>(
  "host_action",
);
// End a title this device launched on the host (`punktfunk end-game`). `ok` when it is gone;
// `notice` is the CLI's sentence either way.
export const endGame = callable<
  [ref: string, gameId: string],
  { ok: boolean; error?: string; notice?: string }
>("end_game");
export const checkUpdate = callable<[force: boolean], UpdateInfo>("check_update");
// Update the client by whichever route its install supports: `flatpak update --user` for the
// flatpak, `punktfunk-client --apply-update` (the packaged root helper) for a one-tap-capable
// native install. Everything else comes back `ok: false, error: "manual"` with `command` — the
// line to run by hand. A package-manager run can take minutes; the backend allows 15.
export const updateClient = callable<
  [],
  {
    ok: boolean;
    updated: boolean;
    staged?: boolean; // installed, but a reboot activates it (rpm-ostree)
    error?: string;
    detail?: string;
    command?: string; // set with error "manual"
  }
>("update_client");
