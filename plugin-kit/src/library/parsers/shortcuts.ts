// Steam's BINARY `shortcuts.vdf` — the user's "Add a Non-Steam Game to My Library" entries.
//
// Ported from the host's in-tree scanner (crates/host/punktfunk-host/src/library/steam.rs), together
// with its unit tests, which are the real specification here: the format is undocumented, and the
// two id derivations below (`shortcutAppId`, `shortcutGameId`) are the difference between a
// shortcut that launches and one that silently does nothing.
//
// Format: a 1-byte type tag (`0x00` nested map, `0x01` string, `0x02` int32, `0x07` uint64), a
// NUL-terminated key, then a type-specific payload; `0x08` closes the current map. The whole file is
// one `shortcuts` map whose children (keyed "0", "1", …) are the individual shortcuts.
//
// Lenient and total by design: a truncated file or an unrecognized tag stops the walk and returns
// whatever parsed so far. A user's shortcuts file is not something to be strict about.

export interface Shortcut {
	/** The 32-bit shortcut appid — always high-bit set. Keys the entry id and its `grid/` art. */
	readonly appid: number;
	readonly name: string;
	/** The shortcut's target, as Steam stores it (quoted, possibly with trailing arguments). */
	readonly exe: string;
	readonly hidden: boolean;
}

/** A cursor over the buffer — the ported code's `pos` threaded explicitly. */
interface Cursor {
	pos: number;
}

/** Read a NUL-terminated UTF-8 string, advancing past the terminator. `undefined` if unterminated. */
const readCStr = (buf: Uint8Array, c: Cursor): string | undefined => {
	const start = c.pos;
	let end = start;
	while (end < buf.length && buf[end] !== 0) end++;
	if (end >= buf.length) return undefined;
	const s = new TextDecoder("utf-8").decode(buf.subarray(start, end));
	c.pos = end + 1;
	return s;
};

/** Read a little-endian int32, advancing 4 bytes. `undefined` if fewer than 4 remain. */
const readI32 = (buf: Uint8Array, c: Cursor): number | undefined => {
	if (c.pos + 4 > buf.length) return undefined;
	const v = new DataView(buf.buffer, buf.byteOffset + c.pos, 4).getInt32(
		0,
		true,
	);
	c.pos += 4;
	return v;
};

/** Skip a nested map's contents (positioned just after its key) up to and including its `0x08`. */
const skipMap = (buf: Uint8Array, c: Cursor): boolean => {
	for (;;) {
		if (c.pos >= buf.length) return false;
		const tag = buf[c.pos];
		c.pos += 1;
		if (tag === 0x08) return true;
		if (readCStr(buf, c) === undefined) return false;
		if (tag === 0x00) {
			if (!skipMap(buf, c)) return false;
		} else if (tag === 0x01) {
			if (readCStr(buf, c) === undefined) return false;
		} else if (tag === 0x02) {
			c.pos += 4;
		} else if (tag === 0x07) {
			c.pos += 8;
		} else {
			return false;
		}
	}
};

/** Parse one shortcut's fields (positioned just after its index key) up to the map-closing `0x08`. */
const parseOne = (buf: Uint8Array, c: Cursor): Shortcut | undefined => {
	let appid: number | undefined;
	let name = "";
	let exe = "";
	let hidden = false;
	for (;;) {
		if (c.pos >= buf.length) return undefined;
		const tag = buf[c.pos];
		c.pos += 1;
		if (tag === 0x08) break;
		const key = readCStr(buf, c)?.toLowerCase();
		if (key === undefined) return undefined;
		if (tag === 0x00) {
			if (!skipMap(buf, c)) return undefined; // nested map (e.g. `tags`) — not needed
		} else if (tag === 0x01) {
			const val = readCStr(buf, c);
			if (val === undefined) return undefined;
			if (key === "appname") name = val;
			else if (key === "exe") exe = val;
		} else if (tag === 0x02) {
			const val = readI32(buf, c);
			if (val === undefined) return undefined;
			if (key === "appid") appid = val >>> 0;
			else if (key === "ishidden") hidden = val !== 0;
		} else if (tag === 0x07) {
			c.pos += 8; // uint64 — skip
		} else {
			return undefined; // unknown tag: payload size unknown, can't continue safely
		}
	}
	if (name.trim() === "") return undefined; // nothing worth showing
	// Prefer the stored appid; fall back to Steam's derivation when it's absent (0 / missing).
	const id = appid && appid !== 0 ? appid : shortcutAppId(exe, name);
	return { appid: id, name, exe, hidden };
};

/** Parse a binary `shortcuts.vdf` into its shortcuts. Never throws. */
export const parseShortcuts = (buf: Uint8Array): Shortcut[] => {
	const out: Shortcut[] = [];
	const c: Cursor = { pos: 0 };
	// Enter the top-level map (`<0x00> "shortcuts" <NUL>`); tolerate any key name.
	if (buf[0] !== 0x00) return out;
	c.pos = 1;
	if (readCStr(buf, c) === undefined) return out;
	while (c.pos < buf.length) {
		const tag = buf[c.pos];
		c.pos += 1;
		if (tag !== 0x00) break; // `0x08` (end of shortcuts) or anything unexpected
		if (readCStr(buf, c) === undefined) break; // the index key ("0", "1", …)
		const sc = parseOne(buf, c);
		if (!sc) break;
		out.push(sc);
	}
	return out;
};

/** Standard reflected (IEEE) CRC-32 — what Steam hashes a shortcut's `exe + name` with. */
export const crc32 = (data: Uint8Array): number => {
	let crc = 0xffff_ffff;
	for (const byte of data) {
		crc ^= byte;
		for (let i = 0; i < 8; i++) {
			const mask = -(crc & 1);
			crc = (crc >>> 1) ^ (0xedb8_8320 & mask);
		}
	}
	return ~crc >>> 0;
};

/**
 * The 32-bit appid Steam derives for a shortcut from its target+name — `crc32(exe + name)` with the
 * high bit set. Only used when `shortcuts.vdf` omits the stored `appid` (very old Steam); modern
 * Steam writes it and the stored value is preferred.
 *
 * The high bit is load-bearing downstream: it is how a shortcut is told apart from a real store
 * appid, which is what makes the CDN art fetch skippable for shortcuts (they only ever have `grid/`
 * overrides).
 */
export const shortcutAppId = (exe: string, name: string): number =>
	(crc32(new TextEncoder().encode(exe + name)) | 0x8000_0000) >>> 0;

/**
 * The 64-bit game id `steam://rungameid/` needs in order to launch a non-Steam shortcut: high dword
 * = the 32-bit shortcut appid, low dword = the shortcut marker `0x02000000`.
 *
 * Handing `rungameid` the bare 32-bit appid does NOT launch a shortcut — it must be this composed
 * id. Returned as a decimal string because it exceeds 2^53 and would lose precision as a `number`.
 */
export const shortcutGameId = (appid: number): string =>
	((BigInt(appid >>> 0) << 32n) | 0x0200_0000n).toString();
