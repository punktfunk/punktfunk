// Bounded filesystem reads and path confinement — the posture the in-host scanners established,
// ported so a library plugin inherits it instead of re-deriving it.
//
// The rules here exist because a plugin reads files it does not own: a launcher's manifests, a
// catalog cache, a `goggame-*.info` a user could have edited. None of that is hostile in the normal
// case, and all of it is untrusted in the case that matters.
import * as fs from "node:fs";
import * as path from "node:path";

/** A launcher manifest / `.acf` / `.info`: text, small. Matches `epic.rs`'s posture. */
export const MAX_MANIFEST_BYTES = 1024 * 1024;
/** A binary catalog cache (Epic's `catcache.bin`, a `shortcuts.vdf`): larger, still bounded. */
export const MAX_CACHE_BYTES = 32 * 1024 * 1024;

/**
 * Read a file as UTF-8, refusing anything over `max`. `undefined` on any error, a non-regular file,
 * or an over-cap file — a plugin scanning a directory must never die on one odd entry.
 *
 * The size is checked by `stat` BEFORE the read, so an enormous file costs a stat, not the memory.
 */
export const readTextCapped = (
	file: string,
	max = MAX_MANIFEST_BYTES,
): string | undefined => {
	try {
		const st = fs.statSync(file);
		if (!st.isFile() || st.size === 0 || st.size > max) return undefined;
		return fs.readFileSync(file, "utf8");
	} catch {
		return undefined;
	}
};

/** Read a file as bytes, refusing anything over `max`. Same posture as {@link readTextCapped}. */
export const readBytesCapped = (
	file: string,
	max = MAX_CACHE_BYTES,
): Uint8Array | undefined => {
	try {
		const st = fs.statSync(file);
		if (!st.isFile() || st.size === 0 || st.size > max) return undefined;
		return new Uint8Array(fs.readFileSync(file));
	} catch {
		return undefined;
	}
};

/** Read + `JSON.parse` a capped text file. `undefined` on any read or parse failure. */
export const readJsonCapped = <T = unknown>(
	file: string,
	max = MAX_MANIFEST_BYTES,
): T | undefined => {
	const text = readTextCapped(file, max);
	if (text === undefined) return undefined;
	try {
		return JSON.parse(text) as T;
	} catch {
		return undefined;
	}
};

/** List a directory's entry names, or `[]` if it isn't readable. */
export const listDir = (dir: string): string[] => {
	try {
		return fs.readdirSync(dir);
	} catch {
		return [];
	}
};

/**
 * Why a path is unusable. `denied` is a permission the operator can grant; `missing` is not, and
 * telling a scanner's user the difference is the whole reason this is not a boolean.
 *
 * Load-bearing on Windows: the plugin runner is `NT AUTHORITY\LocalService`, which holds no ACE
 * anywhere inside a user profile. A per-user install therefore stats exactly like an absent one,
 * and a scanner that cannot tell them apart reports "not installed" for a launcher sitting right
 * there. Folder access requests are the other half.
 */
export type Access = "ok" | "missing" | "denied";

/** `ok` when `want` holds, `denied` when the path is there and closed to us, else `missing`. */
const classify = (p: string, want: (st: fs.Stats) => boolean): Access => {
	try {
		return want(fs.statSync(p)) ? "ok" : "missing";
	} catch (e) {
		const code = (e as NodeJS.ErrnoException).code;
		return code === "EACCES" || code === "EPERM" ? "denied" : "missing";
	}
};

/** {@link Access} for a directory. */
export const dirAccess = (p: string): Access =>
	classify(p, (st) => st.isDirectory());

/** {@link Access} for a regular, non-empty file. */
export const fileAccess = (p: string): Access =>
	classify(p, (st) => st.isFile() && st.size > 0);

/**
 * The command granting the Windows plugin runner (`NT AUTHORITY\LocalService`, SID `S-1-5-19`)
 * read on `p`'s directory; `null` elsewhere. Kept for plugins built against 0.4.7 and earlier,
 * which import it — folder access requests replace it.
 */
export const grantCommand = (p: string): string | null =>
	process.platform === "win32"
		? `icacls "${path.win32.dirname(p)}" /grant "*S-1-5-19:(OI)(CI)(RX)"`
		: null;

/** Does this path exist as a directory? */
export const isDir = (p: string): boolean => dirAccess(p) === "ok";

/** Does this path exist as a regular, non-empty file? */
export const isFile = (p: string): boolean => fileAccess(p) === "ok";

/**
 * Join `rel` onto `base` **only if it cannot escape** — the port of the host's `confined_join`
 * (gog.rs), which exists because a crafted `goggame-<id>.info` could otherwise point a play task's
 * exe at an arbitrary program (security-review 2026-07-17).
 *
 * Refuses any relative path carrying a drive prefix (`C:`), a root (`/` or `\`), or a `..`
 * component — each of which `path.join` would let REPLACE or climb out of `base`. `undefined` ⇒
 * out of bounds, and the caller must refuse the launch rather than fall back to something plausible.
 */
export const confinedJoin = (base: string, rel: string): string | undefined => {
	if (rel === "") return undefined;
	// Normalize separators so a Windows-shaped relative path is checked on any platform (a plugin
	// may parse a Windows manifest while its tests run on Linux).
	const parts = rel.split(/[\\/]/);
	if (parts[0] === "") return undefined; // rooted
	if (/^[A-Za-z]:$/.test(parts[0])) return undefined; // drive prefix
	if (parts.some((p) => p === "..")) return undefined; // traversal
	const joined = path.join(base, ...parts.filter((p) => p !== "" && p !== "."));
	// Belt and braces: the component check above is the real guard, but a symlink-free string check
	// costs nothing and catches anything the split missed.
	const rootWithSep = base.endsWith(path.sep) ? base : base + path.sep;
	return joined === base || joined.startsWith(rootWithSep) ? joined : undefined;
};
