// What may ride Steam's launch options as a `KEY=value` token, and the options string itself.
// Steam hands the string to a shell, so a value outside these allowlists is refused, never
// escaped: the client must receive the opaque token verbatim. Kept free of Decky imports so the
// tests can run it under node.

/** UUIDs, `addr:port`, bracketed IPv6, `steam:570`, 12-hex preset ids; no shell metacharacter. */
const ID_TOKEN = /^[A-Za-z0-9._:@[\]-]+$/;

/** An absolute path of plain filename characters; no space, quote or shell metacharacter. */
const CLIENT_BIN = /^\/[A-Za-z0-9._/+@-]+$/;

const MAX_LEN = 128;

/** A host ref, preset, game or profile id. A leading `-` would read as a flag to the wrapper. */
export function isSafeLaunchId(id: string): boolean {
  return id.length <= MAX_LEN && !id.startsWith("-") && ID_TOKEN.test(id);
}

/** The resolved native client path the backend reports. */
export function isSafeClientBin(bin: string): boolean {
  return bin.length <= MAX_LEN && CLIENT_BIN.test(bin);
}

/** Per-launch extras beyond the host reference (all optional — {} is the plain stream). */
export interface LaunchOpts {
  /** A pinned card: stream with this preset, one-off (PF_PRESET → `--profile`). */
  presetId?: string;
  /**
   * A title in the host's library to launch into the stream (PF_GAME → `--game`), by its
   * store-qualified id (`steam:570`). The host resolves it; the Deck only names it.
   */
  gameId?: string;
  /** Play as this profile, for this stream only (PF_AS → `--as`), by its id. */
  profileId?: string;
  /**
   * Ask the host's operator to admit this Deck rather than typing a PIN (PF_REQUEST_ACCESS).
   * The connect PARKS until somebody approves it, and the launch runs SUPERVISED — see the
   * wrapper for why `--exec` is dropped on this path alone.
   */
  requestAccess?: boolean;
}

/**
 * Is a resolved native-client path safe to put in Steam's launch options? An unsafe id is a bug
 * in our own data, an unsafe path is just where the user installed the client — so the browse
 * shortcut degrades to its flatpak default rather than refusing to exist.
 */
export function safeClientBin(bin: string | undefined): bin is string {
  return !!bin && isSafeClientBin(bin);
}

/** Refuse a launch whose host ref or any named id could not ride the options verbatim. */
export function validateLaunch(ref: string, opts: LaunchOpts): void {
  if (!isSafeLaunchId(ref)) {
    throw new Error(`unsupported host reference: ${ref}`);
  }
  if (opts.presetId && !isSafeLaunchId(opts.presetId)) {
    throw new Error(`unsupported preset id: ${opts.presetId}`);
  }
  if (opts.gameId && !isSafeLaunchId(opts.gameId)) {
    throw new Error(`unsupported game id: ${opts.gameId}`);
  }
  if (opts.profileId && !isSafeLaunchId(opts.profileId)) {
    throw new Error(`unsupported profile id: ${opts.profileId}`);
  }
}

/** The Steam launch options for one stream: `KEY=value … %command% "<runner>"`. Ids are the
 *  caller's to validate first; the client path is checked here. */
export function launchOptions(
  ref: string,
  runner: string,
  clientBin: string,
  opts: LaunchOpts,
): string {
  const env = [`PF_REF=${ref}`];
  // Only for a native client install; without it the wrapper runs the flatpak.
  if (clientBin) {
    // The one launch-option value that is a path, not an id: a space would split Steam's tokenizer.
    if (!isSafeClientBin(clientBin)) {
      throw new Error(`client path can't ride Steam's launch options: ${clientBin}`);
    }
    env.push(`PF_CLIENT_BIN=${clientBin}`);
  }
  if (opts.presetId) {
    env.push(`PF_PRESET=${opts.presetId}`);
  }
  if (opts.gameId) {
    env.push(`PF_GAME=${opts.gameId}`);
  }
  if (opts.profileId) {
    env.push(`PF_AS=${opts.profileId}`);
  }
  if (opts.requestAccess) {
    env.push("PF_REQUEST_ACCESS=1");
  }
  // KEY=value ... %command% args — %command% expands to the shortcut exe (/bin/sh); the wrapper
  // script rides behind it as an argument and reads PF_* from the environment.
  return `${env.join(" ")} %command% "${runner}"`;
}
