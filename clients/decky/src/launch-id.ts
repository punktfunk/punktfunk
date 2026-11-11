// What may ride Steam's launch options as a `KEY=value` token. Steam hands the string to a shell,
// so a value outside these allowlists is refused, never escaped: the client must receive the
// opaque token verbatim. Kept free of Decky imports so the tests can run it under node.

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
