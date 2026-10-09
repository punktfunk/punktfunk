// Valve Data Format (text) — the flat-field reader Steam's `libraryfolders.vdf` and
// `appmanifest_<appid>.acf` need, ported from the host's in-tree scanner
// (crates/host/punktfunk-host/src/library/steam.rs `vdf_value` / `vdf_paths` / `scan_manifests`).
//
// Deliberately NOT a full VDF parser. Every field these files expose that a library plugin cares
// about sits on one line as `"key"  "value"`, and a real parser would be a much larger surface to
// keep correct against a format Valve changes without notice. If you need nested values, read the
// file yourself — this is the 90% case, kept small enough to be obviously right.

/** `"<key>" "<value>"` on a single line → `<value>`. Whitespace between the two is arbitrary. */
export const vdfValue = (line: string, key: string): string | undefined => {
	const rest = line.trimStart();
	const prefix = `"${key}"`;
	if (!rest.startsWith(prefix)) return undefined;
	const after = rest.slice(prefix.length);
	const open = after.indexOf('"');
	if (open === -1) return undefined;
	const value = after.slice(open + 1);
	const close = value.indexOf('"');
	if (close === -1) return undefined;
	return value.slice(0, close);
};

/** The first `"<key>" "<value>"` anywhere in a multi-line document. */
export const vdfField = (text: string, key: string): string | undefined => {
	for (const line of text.split("\n")) {
		const v = vdfValue(line, key);
		if (v !== undefined) return v;
	}
	return undefined;
};

/**
 * Every `"path" "<dir>"` value in a `libraryfolders.vdf` — the extra drives Steam installs to.
 *
 * On Windows the values are backslash-escaped (`D:\\SteamLibrary`), so `\\` collapses to `\`. POSIX
 * paths need no unescaping, and the collapse is harmless there (a literal `\\` in a Linux path is
 * vanishingly rare and was already ambiguous).
 */
export const vdfPaths = (text: string): string[] =>
	text
		.split("\n")
		.map((l) => vdfValue(l, "path"))
		.filter((p): p is string => p !== undefined)
		.map((p) => p.replaceAll("\\\\", "\\"));

/** One installed title as described by its `appmanifest_<appid>.acf`. */
export interface AppManifest {
	readonly appid: number;
	readonly name: string;
	/** The bare folder name under this library's `common/` — resolve it yourself. */
	readonly installdir?: string;
}

/** Parse an `.acf` manifest's flat fields. `undefined` when it carries no usable appid+name. */
export const parseAppManifest = (text: string): AppManifest | undefined => {
	const appid = Number(vdfField(text, "appid"));
	const name = vdfField(text, "name");
	if (!Number.isInteger(appid) || appid <= 0 || !name) return undefined;
	const installdir = vdfField(text, "installdir");
	return installdir ? { appid, name, installdir } : { appid, name };
};

/**
 * Steam installs runtimes and redistributables as "apps" too. A *game* library must not list them.
 * Ported verbatim from the host scanner so an extracted steam plugin filters identically — the
 * parity harness compares entry sets, and a stray Proton row would fail it.
 */
export const isSteamTool = (appid: number, name: string): boolean => {
	// Steamworks Common Redistributables; Steam Linux Runtime 1.0/2.0/3.0 (Sniper/Soldier).
	const TOOL_IDS = [228980, 1070560, 1391110, 1628350, 1493710];
	if (TOOL_IDS.includes(appid)) return true;
	const n = name.toLowerCase();
	return (
		n.includes("proton") ||
		n.startsWith("steam linux runtime") ||
		n.includes("steamworks common") ||
		n.includes("steamvr")
	);
};
