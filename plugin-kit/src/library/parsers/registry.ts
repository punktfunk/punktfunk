// Windows registry reads by spawning `reg.exe query` — dependency-free, and (the part that
// matters) it works from the scripting runner's LocalService account.
//
// **HKLM always; HKCU only under a user's profile.** The box runner is `NT AUTHORITY\LocalService`,
// whose HKCU is its own empty hive, not the operator's — a plugin reading it there would see no
// launcher config and report nothing. A runner started as a signed-in user (a seat's) reads that
// user's hive, which is what a per-account source declares in its manifest's `registry`. The
// profile path tells the two apart, so an HKCU read is refused exactly where it would lie.
import { spawnSync } from "node:child_process";
import * as os from "node:os";

/** One `reg.exe query` value row. */
export interface RegValue {
	readonly name: string;
	/** `REG_SZ`, `REG_DWORD`, … */
	readonly type: string;
	readonly data: string;
}

const HKLM = "HKLM\\";
const HKCU = "HKCU\\";

/** A service account's home: `C:\Windows\ServiceProfiles\<name>` or `system32\config\systemprofile`. */
export const underServiceProfile = (home: string): boolean =>
	/\\(serviceprofiles|config\\systemprofile)(\\|$)/i.test(home);

/**
 * Is this a key path this module will touch? `HKLM\…` always; `HKCU\…` only when this process
 * has a user's profile, since a service account's HKCU is nobody's launcher config.
 */
export const validRegKey = (
	key: string,
	home: string = os.homedir(),
): boolean =>
	(key.startsWith(HKLM) ||
		(key.startsWith(HKCU) && !underServiceProfile(home))) &&
	key.length > HKLM.length &&
	key.length <= 260 &&
	!key.includes("..") &&
	// `reg.exe` takes the key as one argv element (no shell), but keep the charset tame anyway so a
	// malformed key can never turn into a switch.
	!key.startsWith("/") &&
	!/[\r\n\0"]/.test(key);

/**
 * Run `spawn`, and once more if the first run was killed rather than exiting.
 *
 * Bun on Windows fires a `spawnSync` timeout within milliseconds when the spawn is the first
 * after an idle event loop, which is every poll. A killed read looks like an absent key, and an
 * absent launcher reconciles its store empty. The second spawn runs normally.
 */
export const spawnAgainIfKilled = <T extends { status: number | null }>(
	spawn: () => T,
): T => {
	const first = spawn();
	return first.status === null ? spawn() : first;
};

const run = (args: string[]): string | undefined => {
	if (process.platform !== "win32") return undefined;
	const r = spawnAgainIfKilled(() =>
		spawnSync("reg.exe", args, {
			encoding: "utf8",
			windowsHide: true,
			// A registry read is instant; a hang means something is badly wrong and a scan must
			// not block on it forever.
			timeout: 10_000,
			maxBuffer: 4 * 1024 * 1024,
		}),
	);
	if (r.status !== 0 || typeof r.stdout !== "string") return undefined;
	return r.stdout;
};

/**
 * The values directly under one HKLM key. `[]` when the key is absent, unreadable, or this is not
 * Windows — a missing launcher is the normal case, never an error.
 */
export const regQueryValues = (key: string): RegValue[] => {
	if (!validRegKey(key)) return [];
	const out = run(["query", key]);
	if (out === undefined) return [];
	return parseRegQuery(out);
};

/** One named value under an HKLM key, or `undefined`. */
export const regQueryValue = (key: string, name: string): string | undefined =>
	regQueryValues(key).find((v) => v.name.toLowerCase() === name.toLowerCase())
		?.data;

/**
 * `reg.exe` always echoes the FULL hive name in its output rows, never the abbreviation it was
 * given: query `HKLM\SOFTWARE\…` and every line comes back `HKEY_LOCAL_MACHINE\SOFTWARE\…`.
 */
const HKLM_FULL = "HKEY_LOCAL_MACHINE\\";
const HKCU_FULL = "HKEY_CURRENT_USER\\";

/** The key as `reg.exe` prints it: the hive spelled out. */
const fullHive = (key: string): string => {
	const upper = key.toUpperCase();
	if (upper.startsWith(HKLM)) return HKLM_FULL + key.slice(HKLM.length);
	if (upper.startsWith(HKCU)) return HKCU_FULL + key.slice(HKCU.length);
	return key;
};

/**
 * Parse `reg.exe query <key>` output into the immediate subkey NAMES under `key`.
 *
 * Exported for tests, like {@link parseRegQuery}, and for the same reason — this is a text format
 * that quietly breaks, and it did: the previous version matched output lines against the
 * abbreviated `HKLM\…` prefix it was handed, while reg.exe prints `HKEY_LOCAL_MACHINE\…`. Nothing
 * ever matched, so it returned `[]` on every machine, forever, and the one plugin that uses it
 * (GOG) reported "no games installed" instead of failing. See the regSubKeys tests.
 *
 * Returns NAMES, not paths: the sole consumer composes `${key}\\${name}`, and a GOG subkey name IS
 * the product id that becomes the entry's `external_id`.
 */
export const parseRegSubKeys = (stdout: string, key: string): string[] => {
	const full = fullHive(key);
	const prefix = `${full.toLowerCase()}\\`;
	return (
		stdout
			.split(/\r?\n/)
			.map((l) => l.trim())
			.filter((l) => l.toLowerCase().startsWith(prefix))
			.map((l) => l.slice(full.length + 1))
			// Immediate children only — a deeper path still starts with the prefix.
			.filter((name) => name !== "" && !name.includes("\\"))
	);
};

/** The immediate SUBKEY NAMES under one HKLM key (GOG lists one subkey per installed game). */
export const regSubKeys = (key: string): string[] => {
	if (!validRegKey(key)) return [];
	const out = run(["query", key]);
	if (out === undefined) return [];
	return parseRegSubKeys(out, key);
};

/**
 * Parse `reg.exe query` output rows: `    <name>    <TYPE>    <data>`, separated by runs of
 * whitespace. Data may itself contain spaces (a path), so only the first two columns are split off.
 *
 * Exported for tests — the format is stable but this is exactly the kind of thing that quietly
 * breaks, and a plugin's tests can pin it without a Windows box.
 */
export const parseRegQuery = (stdout: string): RegValue[] => {
	const out: RegValue[] = [];
	for (const raw of stdout.split(/\r?\n/)) {
		// Value rows are indented; the key path header is not.
		if (!/^\s/.test(raw)) continue;
		const line = raw.trim();
		if (line === "") continue;
		const m = line.match(/^(.*?)\s{2,}(REG_[A-Z_]+)\s{2,}([\s\S]*)$/);
		if (!m) continue;
		out.push({ name: m[1], type: m[2], data: m[3] });
	}
	return out;
};
