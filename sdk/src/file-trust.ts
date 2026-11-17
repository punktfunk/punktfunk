// Unit-file trust (RFC §9.1/§9.4), the sshd rule on both platforms: a unit file a non-privileged
// principal could have written is refused loudly — mode bits on Unix, owner + DACL on Windows.
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import type { LogSink } from "./runner-log.js";

// SDDL access-mask bits that let a principal change the file's content or its protection:
// write/append data + EAs, DELETE (delete + recreate), WRITE_DAC / WRITE_OWNER (rewrite the
// protection itself), and the generic write/all bits. FILE_WRITE_ATTRIBUTES (0x100) is
// deliberately NOT here: it only toggles timestamps/readonly/hidden — never content — and the
// runner's own service principal legitimately holds it (bun's module loader opens unit files
// requesting RX+WA; `plugins enable` grants exactly that on the plugins/scripts dirs — counting
// WA as tampering would make the runner refuse every unit it is supposed to run).
const SDDL_WRITE_BITS =
	0x2 | 0x4 | 0x10 | 0x10000 | 0x40000 | 0x80000 | 0x10000000 | 0x40000000;

// SDDL two-letter rights tokens → access-mask bits (generic, standard, file-specific, and the
// low object-rights aliases hex masks sometimes render as). Anything unrecognized is treated as
// write-capable — fail closed.
const SDDL_RIGHT_TOKENS: Record<string, number> = {
	GA: 0x10000000,
	GX: 0x20000000,
	GW: 0x40000000,
	GR: 0x80000000,
	RC: 0x20000,
	SD: 0x10000,
	WD: 0x40000,
	WO: 0x80000,
	FA: 0x1f01ff,
	FR: 0x120089,
	FW: 0x120116,
	FX: 0x1200a0,
	CC: 0x1,
	DC: 0x2,
	LC: 0x4,
	SW: 0x8,
	RP: 0x10,
	WP: 0x20,
	DT: 0x40,
	LO: 0x80,
	CR: 0x100,
};

// SDDL two-letter account abbreviations we may meet on a unit file, → full SIDs.
const SDDL_SID_ABBREV: Record<string, string> = {
	SY: "S-1-5-18", // NT AUTHORITY\SYSTEM
	BA: "S-1-5-32-544", // BUILTIN\Administrators
	OW: "S-1-3-4", // OWNER RIGHTS
	CO: "S-1-3-0", // CREATOR OWNER
	LS: "S-1-5-19", // NT AUTHORITY\LOCAL SERVICE
	NS: "S-1-5-20", // NT AUTHORITY\NETWORK SERVICE
	BU: "S-1-5-32-545", // BUILTIN\Users
	AU: "S-1-5-11", // Authenticated Users
	IU: "S-1-5-4", // INTERACTIVE
	WD: "S-1-1-0", // Everyone
};

const TRUSTED_OWNER_SIDS = new Set([
	"S-1-5-18", // SYSTEM
	"S-1-5-32-544", // Administrators
	"S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464", // TrustedInstaller
]);

/** An SDDL rights field as an access mask; -1 when it can't be fully understood (fail closed). */
const sddlRightsMask = (rights: string): number => {
	if (rights.startsWith("0x") || rights.startsWith("0X")) {
		const n = Number.parseInt(rights, 16);
		return Number.isNaN(n) ? -1 : n;
	}
	if (rights.length % 2 !== 0) return -1;
	let mask = 0;
	for (let i = 0; i < rights.length; i += 2) {
		const bits = SDDL_RIGHT_TOKENS[rights.slice(i, i + 2)];
		if (bits === undefined) return -1;
		mask = (mask | bits) >>> 0;
	}
	return mask;
};

/**
 * The Windows half of the sshd rule, as a pure function over the file's SDDL (exported for
 * tests). Returns `null` when the descriptor is trustworthy — owner is
 * SYSTEM/Administrators/TrustedInstaller (or `extraTrustedSid`, the account running the runner:
 * the Unix rule's "your own file is fine") and no other principal holds a write-capable allow
 * ACE — else a human-readable refusal reason. Unknown ACE shapes count as write-capable.
 */
export const windowsSddlUnsafeReason = (
	sddl: string,
	extraTrustedSid?: string,
): string | null => {
	const trusted = new Set(TRUSTED_OWNER_SIDS);
	trusted.add("S-1-3-4"); // OWNER RIGHTS — constrained by the owner check below
	if (extraTrustedSid) trusted.add(extraTrustedSid);

	const owner = /^O:(S-[0-9-]+|[A-Z]{2})/.exec(sddl)?.[1];
	const ownerSid =
		owner === undefined
			? undefined
			: owner.startsWith("S-")
				? owner
				: SDDL_SID_ABBREV[owner];
	if (ownerSid === undefined || !trusted.has(ownerSid)) {
		return `owner ${owner ?? "unknown"} is not SYSTEM/Administrators/TrustedInstaller`;
	}

	const daclAt = sddl.indexOf("D:");
	if (daclAt < 0) return "no DACL in the security descriptor";
	// ACE format: (type;flags;rights;objectGuid;inheritGuid;sid[;condition]). SACL ACEs after
	// "S:" match the regex too but are audit types, filtered by the allow-type check.
	for (const [, ace] of sddl.slice(daclAt + 2).matchAll(/\(([^)]*)\)/g)) {
		const [type, flags = "", rights = "", , , sid = ""] = ace.split(";");
		if (type !== "A" && type !== "XA") continue; // deny/audit ACEs only ever tighten
		// Inherit-only ACEs are templates for children; they grant nothing on this file. Flags
		// come in two-letter tokens — compare exactly, not by substring.
		const flagTokens: string[] = flags.match(/.{2}/g) ?? [];
		if (flagTokens.includes("IO")) continue;
		const mask = sddlRightsMask(rights);
		if (mask !== -1 && (mask & SDDL_WRITE_BITS) === 0) continue; // read-only ACE
		const resolved = sid.startsWith("S-") ? sid : (SDDL_SID_ABBREV[sid] ?? sid);
		if (!trusted.has(resolved)) {
			return `${sid} can write it (only SYSTEM/Administrators may)`;
		}
	}
	return null;
};

/**
 * Run `spawn`, and once more if the first run was killed rather than exiting.
 *
 * Bun on Windows fires a `spawnSync` timeout within milliseconds when the spawn is the first
 * after an idle event loop. A killed ACL read is an unreadable ACL, which refuses the unit.
 */
export const spawnAgainIfKilled = <T extends { status: number | null }>(
	spawn: () => T,
): T => {
	const first = spawn();
	return first.status === null ? spawn() : first;
};

/** The SID this process runs as, fetched once (`undefined` when it can't be determined). */
let processSidCache: string | undefined | false;
const processSid = (): string | undefined => {
	if (processSidCache === undefined) {
		const res = spawnAgainIfKilled(() =>
			spawnSync(
				windowsPowershell(),
				[
					"-NoProfile",
					"-NonInteractive",
					"-Command",
					"[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
				],
				{
					encoding: "utf8",
					windowsHide: true,
					timeout: 15_000,
					env: windowsPowershellEnv(),
				},
			),
		);
		const sid = res.status === 0 ? (res.stdout ?? "").trim() : "";
		processSidCache = /^S-[0-9-]+$/.test(sid) ? sid : false;
	}
	return processSidCache === false ? undefined : processSidCache;
};

// Full System32 path, never PATH — a planted powershell.exe must not run with our privileges
// (mirrors the host CLI's powershell_path).
const windowsPowershell = (): string =>
	path.join(
		process.env.SystemRoot ?? "C:\\Windows",
		"System32",
		"WindowsPowerShell",
		"v1.0",
		"powershell.exe",
	);

// Spawn env for Windows PowerShell 5.1 with PSModulePath STRIPPED (5.1 rebuilds its default).
// An inherited PSModulePath that includes a PowerShell 7 module dir (pwsh adds a machine-scope
// entry) makes 5.1 fail to autoload Microsoft.PowerShell.Security — type-data conflict
// ("AuditToString is already present") — so Get-Acl dies and every unit is refused.
const windowsPowershellEnv = (): Record<string, string | undefined> => {
	const env: Record<string, string | undefined> = { ...process.env };
	delete env.PSModulePath;
	return env;
};

/** Read a file's SDDL and apply [`windowsSddlUnsafeReason`]. Unreadable ACL ⇒ refuse. */
const windowsFileIsSafe = (file: string, log: LogSink): boolean => {
	const escaped = file.replace(/'/g, "''");
	const res = spawnAgainIfKilled(() =>
		spawnSync(
			windowsPowershell(),
			[
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				`(Get-Acl -LiteralPath '${escaped}').Sddl`,
			],
			{
				encoding: "utf8",
				windowsHide: true,
				timeout: 15_000,
				env: windowsPowershellEnv(),
			},
		),
	);
	const sddl = res.status === 0 ? (res.stdout ?? "").trim() : "";
	if (!sddl) {
		log(`[runner] REFUSING ${file} — its ACL is unreadable`, "error");
		return false;
	}
	const reason = windowsSddlUnsafeReason(sddl, processSid());
	if (reason !== null) {
		log(
			`[runner] REFUSING ${file} — ${reason}. Reinstall the plugin with ` +
				`\`punktfunk-host plugins add\`, or re-own the file to Administrators and strip ` +
				`non-admin write ACEs (icacls).`,
			"error",
		);
		return false;
	}
	return true;
};

/**
 * The sshd rule (RFC §9.1/§9.4): refuse a unit file anyone less privileged than the operator
 * could have written — group/world-writable mode on Unix; on Windows, an owner outside
 * SYSTEM/Administrators/TrustedInstaller or a write-capable ACE for a non-admin principal.
 */
export const fileIsSafe = (file: string, log: LogSink): boolean => {
	if (process.platform === "win32") return windowsFileIsSafe(file, log);
	try {
		const mode = fs.statSync(file).mode & 0o022;
		if (mode !== 0) {
			log(
				`[runner] REFUSING ${file} — group/world-writable (chmod go-w it first)`,
				"error",
			);
			return false;
		}
	} catch {
		return false;
	}
	return true;
};
