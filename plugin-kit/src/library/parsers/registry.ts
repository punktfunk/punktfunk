// Windows registry reads through advapi32, in-process: UTF-16 paths stay intact, nothing is
// spawned, and "no such key" stays distinct from "refused". `reg.exe` is the fallback for a Bun
// with no FFI. HKLM and `HKEY_USERS\<SID>` always; HKCU only under a user's profile, because a
// service account's HKCU is nobody's launcher config.
import { dlopen, FFIType } from "bun:ffi";
import { spawnSync } from "node:child_process";
import * as os from "node:os";

/** One value under a key. */
export interface RegValue {
	readonly name: string;
	/** `REG_SZ`, `REG_DWORD`, … */
	readonly type: string;
	readonly data: string;
}

const HKLM = "HKLM\\";
const HKCU = "HKCU\\";
const HKU = "HKU\\";

/** Each hive by the prefix a key carries, the name `reg.exe` prints, and advapi32's handle. */
const HIVES = [
	{
		prefix: HKLM,
		full: "HKEY_LOCAL_MACHINE\\",
		handle: 0xffff_ffff_8000_0002n,
	},
	{ prefix: HKCU, full: "HKEY_CURRENT_USER\\", handle: 0xffff_ffff_8000_0001n },
	{ prefix: HKU, full: "HKEY_USERS\\", handle: 0xffff_ffff_8000_0003n },
] as const;

/** A service account's home: `C:\Windows\ServiceProfiles\<name>` or `system32\config\systemprofile`. */
export const underServiceProfile = (home: string): boolean =>
	/\\(serviceprofiles|config\\systemprofile)(\\|$)/i.test(home);

/** Is this a key path this module will touch? See the module docs on the hives. */
export const validRegKey = (
	key: string,
	home: string = os.homedir(),
): boolean =>
	(key.startsWith(HKLM) ||
		key.startsWith(HKU) ||
		(key.startsWith(HKCU) && !underServiceProfile(home))) &&
	key.length > HKLM.length &&
	key.length <= 260 &&
	!key.includes("..") &&
	!key.startsWith("/") &&
	!/[\r\n\0"]/.test(key);

/** `KEY_READ | KEY_WOW64_64KEY`: the 64-bit view, `WOW6432Node` named where it is wanted. */
const KEY_READ_64 = 0x2_0019 | 0x0100;
const ERROR_FILE_NOT_FOUND = 2;
const ERROR_ACCESS_DENIED = 5;
const ERROR_MORE_DATA = 234;
/** Longest value name a key can hold, in UTF-16 units, with its NUL. */
const MAX_VALUE_NAME = 16_384;
/** No launcher fact runs this long; a larger value is skipped, not truncated. */
const MAX_VALUE_BYTES = 64 * 1024;
const REG_TYPES: Readonly<Record<number, string>> = {
	0: "REG_NONE",
	1: "REG_SZ",
	2: "REG_EXPAND_SZ",
	3: "REG_BINARY",
	4: "REG_DWORD",
	7: "REG_MULTI_SZ",
	11: "REG_QWORD",
};

const load = () =>
	dlopen("advapi32.dll", {
		RegOpenKeyExW: {
			args: [FFIType.u64, FFIType.ptr, FFIType.u32, FFIType.u32, FFIType.ptr],
			returns: FFIType.i32,
		},
		RegEnumKeyExW: {
			args: [
				FFIType.u64,
				FFIType.u32,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
			],
			returns: FFIType.i32,
		},
		RegEnumValueW: {
			args: [
				FFIType.u64,
				FFIType.u32,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
				FFIType.ptr,
			],
			returns: FFIType.i32,
		},
		RegCloseKey: { args: [FFIType.u64], returns: FFIType.i32 },
	}).symbols;
type Advapi = ReturnType<typeof load>;
let loaded: Advapi | null | undefined;

/** advapi32, loaded once; `null` off Windows or where this Bun has no FFI. */
const advapi = (): Advapi | null => {
	if (loaded === undefined) {
		try {
			loaded = process.platform === "win32" ? load() : null;
		} catch {
			loaded = null;
		}
	}
	return loaded;
};

const wide = (s: string): Buffer => Buffer.from(`${s}\0`, "utf16le");

const noted = new Set<string>();

/** An absent key is the normal case. Any other failure is said once per key. */
const noteFailure = (key: string, code: number): void => {
	if (code === ERROR_FILE_NOT_FOUND || noted.has(key)) return;
	noted.add(key);
	const cause =
		code === ERROR_ACCESS_DENIED ? "access denied" : `Windows error ${code}`;
	console.warn(`read ${key}: ${cause}`);
};

/** Run `read` on `key`, open for reading; `undefined` when it will not open. */
const withKey = <T>(
	api: Advapi,
	key: string,
	read: (handle: bigint) => T,
): T | undefined => {
	const hive = HIVES.find((h) => key.startsWith(h.prefix));
	if (!hive) return undefined;
	const opened = new BigUint64Array(1);
	const path = wide(key.slice(hive.prefix.length));
	const code = api.RegOpenKeyExW(hive.handle, path, 0, KEY_READ_64, opened);
	if (code !== 0) {
		noteFailure(key, code);
		return undefined;
	}
	const handle = opened[0] ?? 0n;
	try {
		return read(handle);
	} finally {
		api.RegCloseKey(handle);
	}
};

const enumSubKeys = (api: Advapi, handle: bigint): string[] => {
	const out: string[] = [];
	// A key name is at most 255 characters.
	const name = Buffer.alloc(512);
	const length = new Uint32Array(1);
	for (let i = 0; ; i++) {
		length[0] = 256;
		const code = api.RegEnumKeyExW(
			handle,
			i,
			name,
			length,
			null,
			null,
			null,
			null,
		);
		if (code !== 0) return out;
		out.push(name.subarray(0, (length[0] ?? 0) * 2).toString("utf16le"));
	}
};

/** A value's data as `reg.exe` prints it. A number too short for its type reads as zero. */
export const valueText = (type: string, bytes: Buffer): string => {
	switch (type) {
		case "REG_DWORD":
			return `0x${(bytes.length < 4 ? 0 : bytes.readUInt32LE(0)).toString(16)}`;
		case "REG_QWORD":
			return `0x${(bytes.length < 8 ? 0n : bytes.readBigUInt64LE(0)).toString(16)}`;
		case "REG_BINARY":
			return bytes.toString("hex").toUpperCase();
		case "REG_MULTI_SZ":
			return bytes
				.toString("utf16le")
				.replace(/\0?\0$/, "")
				.replaceAll("\0", "\\0");
		case "REG_NONE":
			return "";
		default:
			return bytes.toString("utf16le").split("\0")[0] ?? "";
	}
};

/** Every value of a known type, unexpanded, in the shape `reg.exe` prints. */
const enumValues = (api: Advapi, handle: bigint): RegValue[] => {
	const out: RegValue[] = [];
	const name = Buffer.alloc(MAX_VALUE_NAME * 2);
	const data = Buffer.alloc(MAX_VALUE_BYTES);
	const nameLength = new Uint32Array(1);
	const dataLength = new Uint32Array(1);
	const kind = new Uint32Array(1);
	for (let i = 0; ; i++) {
		nameLength[0] = MAX_VALUE_NAME;
		dataLength[0] = MAX_VALUE_BYTES;
		const code = api.RegEnumValueW(
			handle,
			i,
			name,
			nameLength,
			null,
			kind,
			data,
			dataLength,
		);
		if (code === ERROR_MORE_DATA) continue;
		if (code !== 0) return out;
		const type = REG_TYPES[kind[0] ?? 0];
		if (!type) continue;
		out.push({
			name: name.subarray(0, (nameLength[0] ?? 0) * 2).toString("utf16le"),
			type,
			data: valueText(type, data.subarray(0, dataLength[0])),
		});
	}
};

/**
 * Run `spawn`, and once more if the first run was killed rather than exiting.
 *
 * Bun on Windows can fire a `spawnSync` timeout within milliseconds when the spawn is the first
 * after an idle event loop. A killed read looks like an absent key, and an absent launcher
 * reconciles its store empty. The second spawn runs normally.
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
 * The values directly under one key. `[]` when the key is absent, unreadable, or this is not
 * Windows; a refused read logs its key and cause.
 */
export const regQueryValues = (key: string): RegValue[] => {
	if (!validRegKey(key)) return [];
	const api = advapi();
	if (api) return withKey(api, key, (h) => enumValues(api, h)) ?? [];
	const out = run(["query", key]);
	if (out === undefined) return [];
	return parseRegQuery(out);
};

/** One named value under a key, or `undefined`. */
export const regQueryValue = (key: string, name: string): string | undefined =>
	regQueryValues(key).find((v) => v.name.toLowerCase() === name.toLowerCase())
		?.data;

/** `key` as `reg.exe` prints it: the hive spelled out. */
const fullHive = (key: string): string => {
	const upper = key.toUpperCase();
	const hive = HIVES.find((h) => upper.startsWith(h.prefix));
	return hive ? hive.full + key.slice(hive.prefix.length) : key;
};

/**
 * Parse `reg.exe query <key>` output into the immediate subkey NAMES under `key`. `reg.exe` echoes
 * the full hive name in its rows, never the abbreviation it was given, so a row is matched against
 * the spelled-out key. Exported for tests and the plugins built against the spawned reader.
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

/** The immediate SUBKEY NAMES under one key (GOG lists one subkey per installed game). */
export const regSubKeys = (key: string): string[] => {
	if (!validRegKey(key)) return [];
	const api = advapi();
	if (api) return withKey(api, key, (h) => enumSubKeys(api, h)) ?? [];
	const out = run(["query", key]);
	if (out === undefined) return [];
	return parseRegSubKeys(out, key);
};

/** The hives loaded under `HKEY_USERS`: each signed-in account's, and the services'. */
export const regLoadedHives = (): string[] => {
	const api = advapi();
	if (api) return withKey(api, HKU, (h) => enumSubKeys(api, h)) ?? [];
	return parseRegSubKeys(run(["query", "HKEY_USERS"]) ?? "", "HKEY_USERS");
};

/**
 * Parse `reg.exe query` output rows: `    <name>    <TYPE>    <data>`, separated by runs of
 * whitespace. Data may itself contain spaces (a path), so only the first two columns are split off.
 *
 * Exported for tests and the plugins built against the spawned reader.
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
