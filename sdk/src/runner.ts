// The managed script/plugin runner (RFC §8, M5) — what the `punktfunk-scripting` package runs:
// discover the operator's units, supervise them as Effect fibers, shut down structurally.
//
// Units:
// - **Plugins** — a file whose default export is a [`PluginDef`] (`definePlugin`), from the
//   scripts dir or an installed `punktfunk-plugin-*` package. Supervised: a failure restarts
//   it with capped exponential backoff; a clean return completes it. The Effect `main` shape
//   runs with the `PunktfunkHost` layer provided and is interrupted STRUCTURALLY on shutdown
//   (scoped finalizers run — release the preset, deregister cleanly); the async-fn shape gets
//   a connected facade client whose close is guaranteed by the same scope.
// - **Bare scripts** — any other `.ts`/`.js` file in the scripts dir: importing it IS the run
//   (top-level await). One-shot: completion logs, failure logs — no restart (a bare script's
//   background work is invisible to supervision; export a plugin to be supervised).
//
// Trust model (RFC §9.4): a unit is code the operator chose to run — no sandbox is pretended.
// The same sshd rule as hooks applies on BOTH platforms: a unit file a non-privileged principal
// could have written is refused loudly (mode bits on Unix, owner + DACL on Windows).
import {
	Cause,
	Duration,
	Effect,
	Schedule,
} from "effect";
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Writable } from "node:stream";
import { pathToFileURL } from "node:url";
import { PunktfunkHost } from "./client.js";
import { layer as hostLayer } from "./effect.js";
import { type ConnectOptions, configDir, publishedMgmtUrl } from "./config.js";
import { connect, type PluginDef } from "./index.js";
import {
	bwrapArgv,
	grantedRoots,
	netlinkFilter,
	type PluginManifest,
	readManifest,
	sandboxEnv,
	sandboxProbe,
} from "./sandbox.js";
import { serveHostProxy } from "./host-proxy.js";

export interface RunnerOptions {
	/** Where loose scripts live. Default `<config_dir>/scripts`. */
	scriptsDir?: string;
	/**
	 * Where plugin packages are installed (`<pluginsDir>/node_modules/punktfunk-plugin-*`,
	 * i.e. the operator runs `bun add punktfunk-plugin-x` there). Default `<config_dir>/plugins`.
	 */
	pluginsDir?: string;
	/** Connection overrides handed to every unit's client/layer. */
	connect?: ConnectOptions;
	/** Restart backoff base (test seam). Default 1 s, capped at 60 s, jittered. */
	restartBase?: Duration.Input;
	/** `"off"` runs plugins in-process. Default: `PUNKTFUNK_PLUGIN_SANDBOX` on Linux, off elsewhere. */
	sandbox?: "on" | "off";
	/** The pinned fetch the sandbox proxy forwards with (test seam). */
	sandboxFetch?: typeof globalThis.fetch;
	/**
	 * Line sink. Default: stamped stdout, with `warn`/`error` going to the matching console method
	 * (hence stderr, and hence the right level in the console's log page — see `log-ship.ts`).
	 *
	 * `level` is optional so an existing `(line: string) => void` sink stays assignable.
	 */
	log?: (line: string, level?: RunnerLogLevel) => void;
}

/**
 * Severity of a runner line. Only three, because that is all the runner distinguishes: it is
 * reporting on units, not producing application logs.
 */
export type RunnerLogLevel = "info" | "warn" | "error";

/** The sink shape used internally, with the level always supplied by the caller's default. */
type LogSink = (line: string, level?: RunnerLogLevel) => void;

export interface Unit {
	/** Display name: the file stem, or the plugin package name. */
	name: string;
	/** Absolute path of the module to import. */
	file: string;
	/** The installed package's directory — absent for a loose script. */
	packageDir?: string;
	/** Its `punktfunk` block, when it declared one. A plugin package without it gets no host-run
	 * commands, and does not run at all while sandboxing is on. */
	manifest?: PluginManifest;
}

const defaultLog: LogSink = (line, level = "info") => {
	const stamped = `${new Date().toISOString()} ${line}`;
	if (level === "error") console.error(stamped);
	else if (level === "warn") console.warn(stamped);
	else console.log(stamped);
};

// ---- unit-file trust (the sshd rule, both halves) ---------------------------------------------

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

/** The SID this process runs as, fetched once (`undefined` when it can't be determined). */
let processSidCache: string | undefined | false;
const processSid = (): string | undefined => {
	if (processSidCache === undefined) {
		const res = spawnSync(
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
		);
		const sid = res.status === 0 ? (res.stdout ?? "").trim() : "";
		processSidCache = /^S-[0-9-]+$/.test(sid) ? sid : false;
	}
	return processSidCache === false ? undefined : processSidCache;
};

// Full System32 path, never PATH — a planted powershell.exe must not run with our privileges
// (mirrors the host CLI's powershell_path, security-review 2026-07-17).
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
// ("AuditToString is already present") — so Get-Acl dies and every unit is refused. Seen live
// on the .173 host box; intermittent per spawn, deterministic once stripped.
const windowsPowershellEnv = (): Record<string, string | undefined> => {
	const env: Record<string, string | undefined> = { ...process.env };
	delete env.PSModulePath;
	return env;
};

/** Read a file's SDDL and apply [`windowsSddlUnsafeReason`]. Unreadable ACL ⇒ refuse. */
const windowsFileIsSafe = (file: string, log: LogSink): boolean => {
	const escaped = file.replace(/'/g, "''");
	const res = spawnSync(
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
const fileIsSafe = (file: string, log: LogSink): boolean => {
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

const SCRIPT_EXTENSIONS = new Set([".ts", ".js", ".mjs", ".mts", ".cjs"]);

/** Enumerate the operator's units: loose scripts plus installed plugin packages. */
export const discoverUnits = (
	options: RunnerOptions = {},
	log: LogSink = options.log ?? defaultLog,
): Unit[] => {
	const units: Unit[] = [];
	const scriptsDir = options.scriptsDir ?? path.join(configDir(), "scripts");
	const pluginsDir = options.pluginsDir ?? path.join(configDir(), "plugins");
	try {
		for (const entry of fs.readdirSync(scriptsDir).sort()) {
			const file = path.join(scriptsDir, entry);
			if (!SCRIPT_EXTENSIONS.has(path.extname(entry))) continue;
			if (!fs.statSync(file).isFile()) continue;
			if (!fileIsSafe(file, log)) continue;
			units.push({ name: path.basename(entry, path.extname(entry)), file });
		}
	} catch {
		// no scripts dir — fine
	}
	const modules = path.join(pluginsDir, "node_modules");
	// The packages the operator actually installed — `bun add` records them as the plugins dir's
	// own `dependencies`. This is what separates a plugin from a plugin's LIBRARY:
	// `@punktfunk/plugin-kit` matches the `plugin-*` naming convention exactly but arrives as a
	// transitive dependency of every kit-built plugin, and running it as a unit is nonsense.
	// `undefined` only when there is no readable `package.json` at all — a hand-assembled tree then
	// falls back to the naming convention rather than discovering nothing. A package.json with no
	// `dependencies` key yields an EMPTY set, not `undefined`: `bun remove` drops the key when the
	// last plugin goes, and orphaned transitive packages can outlive it, so falling back there
	// would start running a plugin's library the moment you uninstall the last real plugin.
	let topLevel: Set<string> | undefined;
	try {
		const root = JSON.parse(
			fs.readFileSync(path.join(pluginsDir, "package.json"), "utf8"),
		) as { dependencies?: Record<string, string> };
		topLevel = new Set(Object.keys(root.dependencies ?? {}));
	} catch {
		// no package.json — fall back to the convention
	}
	// Read a plugin package's manifest (`module`/`main` entry) and add it as a unit.
	const addPlugin = (dir: string, name: string): void => {
		if (topLevel && !topLevel.has(name)) return; // a dependency, not an installed plugin
		try {
			const manifest = JSON.parse(
				fs.readFileSync(path.join(dir, "package.json"), "utf8"),
			) as { main?: string; module?: string };
			const rel = manifest.module ?? manifest.main ?? "index.js";
			const file = path.join(dir, rel);
			if (!fileIsSafe(file, log)) return;
			const declared = readManifest(dir);
			units.push({
				name,
				file,
				packageDir: dir,
				...(declared ? { manifest: declared } : {}),
			});
		} catch (e) {
			log(`[runner] skipping ${name}: unreadable package.json (${e})`, "warn");
		}
	};
	try {
		for (const pkg of fs.readdirSync(modules).sort()) {
			// Unscoped convention: `punktfunk-plugin-*`.
			if (pkg.startsWith("punktfunk-plugin-")) {
				addPlugin(path.join(modules, pkg), pkg);
				continue;
			}
			// Scoped convention: `<any scope>/plugin-*`. A scoped name resolves cleanly from a
			// registry scope-map, so a plugin can depend on `@punktfunk/host` + `effect` as shared
			// (hoisted) deps rather than bundling its own copy of each.
			//
			// ANY scope, not just `@punktfunk`: the plugin store requires catalog entries to be
			// scoped precisely so the scope can map to that entry's registry, so a third-party
			// plugin necessarily arrives as `@their-scope/plugin-*`. Limiting discovery to the
			// first-party scope would let such a plugin install and then never run.
			if (pkg.startsWith("@")) {
				try {
					for (const scoped of fs.readdirSync(path.join(modules, pkg)).sort()) {
						if (scoped.startsWith("plugin-")) {
							addPlugin(path.join(modules, pkg, scoped), `${pkg}/${scoped}`);
						}
					}
				} catch {
					// not a readable scope dir — fine
				}
			}
		}
	} catch {
		// no plugins dir — fine
	}
	return units;
};

/**
 * Whether plugins run sandboxed at all. Off is an explicit operator choice, logged where they
 * will see it — never a silent downgrade because a box could not manage a namespace.
 */
export const sandboxMode = (): "on" | "off" =>
	/^(0|off|false)$/i.test(process.env.PUNKTFUNK_PLUGIN_SANDBOX ?? "") ? "off" : "on";

/** This plugin's own token, written where only its sandbox can read it. */
const writePluginToken = (stateDir: string, id: string): string | undefined => {
	const file = path.join(stateDir, ".plugin-token");
	try {
		const tokens = JSON.parse(
			fs.readFileSync(path.join(configDir(), "plugin-tokens.json"), "utf8"),
		) as Record<string, string>;
		const token = tokens[id];
		if (token === undefined) return undefined;
		fs.mkdirSync(stateDir, { recursive: true });
		fs.writeFileSync(file, `PUNKTFUNK_PLUGIN_TOKEN=${token}\n`, { mode: 0o600 });
		return file;
	} catch {
		return undefined;
	}
};

/**
 * Run one plugin in its own sandbox: a child process under `bwrap`, re-executing this runner in
 * `--run-unit` mode so the plugin's code never shares an address space — or a token — with
 * another's.
 *
 * Resolves when the child exits; a non-zero exit fails the effect, which is what makes the
 * supervisor restart it with the same backoff an in-process failure gets.
 */
const runSandboxed = (
	unit: Unit,
	manifest: PluginManifest,
	options: RunnerOptions,
	log: LogSink,
): Effect.Effect<"plugin", unknown> =>
	Effect.callback<"plugin", unknown>((resume) => {
		const id = manifest.id ?? unit.name;
		const config = configDir();
		const stateDir = path.join(config, "plugin-state", id);
		const tokenFile = writePluginToken(stateDir, id);
		if (!tokenFile) {
			resume(
				Effect.fail(
					new Error(
						`no token for ${id} in plugin-tokens.json — the host mints one per installed plugin on its next start, and under the runner's unit that file has to be bound into the home it replaces`,
					),
				),
			);
			return;
		}
		const filter = netlinkFilter();
		if (!filter) {
			resume(Effect.fail(new Error(`no sandbox syscall filter for ${process.arch}`)));
			return;
		}
		const runtime = process.env.XDG_RUNTIME_DIR ?? "/tmp";
		const socket = path.join(runtime, "punktfunk", `plugin-${id}.sock`);
		const proxy = serveHostProxy({
			socket,
			url: options.connect?.url ?? publishedMgmtUrl() ?? "https://127.0.0.1:47990",
			fetch: options.sandboxFetch ?? fetch,
		});
		const argv = [
			...bwrapArgv(
				manifest,
				{
					stateDir,
					tokenFile,
					socket,
					pluginsDir: options.pluginsDir ?? path.join(config, "plugins"),
					bun: process.execPath,
					runner: runnerEntry(),
					home: os.homedir(),
				},
				grantedRoots(config, id),
			),
			process.execPath,
			runnerEntry(),
			"--run-unit",
			unit.file,
			"--unit-name",
			unit.name,
		];
		const child = spawn("bwrap", argv, {
			env: sandboxEnv(os.homedir()),
			// fd 3 is `--add-seccomp-fd 3`.
			stdio: ["ignore", "inherit", "inherit", "pipe"],
		});
		// A bwrap that dies before reading surfaces through `exit`, not an EPIPE here.
		(child.stdio[3] as Writable).on("error", () => {}).end(filter);
		child.on("error", (e) => {
			proxy.close();
			resume(Effect.fail(e));
		});
		child.on("exit", (code, signal) => {
			proxy.close();
			if (code === 0) resume(Effect.succeed("plugin" as const));
			else
				resume(
					Effect.fail(
						new Error(`sandboxed plugin exited ${signal ? `on ${signal}` : `with ${code}`}`),
					),
				);
		});
		return Effect.sync(() => {
			// Interruption (shutdown): SIGTERM lets the plugin's finalizers run; `--die-with-parent`
			// is the backstop if this runner is killed outright.
			child.kill("SIGTERM");
			proxy.close();
		});
	});

/** The runner bundle this process is running, which each sandbox re-execs. */
const runnerEntry = (): string =>
	process.env.PUNKTFUNK_RUNNER_ENTRY ?? process.argv[1] ?? "";

const isPluginDef = (v: unknown): v is PluginDef =>
	typeof v === "object" &&
	v !== null &&
	typeof (v as PluginDef).name === "string" &&
	(v as PluginDef).main !== undefined;

/** One attempt at a unit: import (cache-busted per attempt) and run whatever it exports. */
const attemptUnit = (
	unit: Unit,
	attempt: number,
	options: RunnerOptions,
	log: LogSink,
): Effect.Effect<"plugin" | "script", unknown> =>
	Effect.gen(function* () {
		// A plugin that declared a manifest runs in its own sandbox, in its own process. A loose
		// script is the operator's own code and stays here, as does everything on a box that
		// cannot sandbox — with the reason said out loud at startup, never silently.
		if (unit.manifest && options.sandbox !== "off") {
			return yield* runSandboxed(unit, unit.manifest, options, log);
		}
		const mod = (yield* Effect.tryPromise(
			() => import(`${pathToFileURL(unit.file).href}?attempt=${attempt}`),
		)) as { default?: unknown };
		if (!isPluginDef(mod.default)) {
			return "script" as const; // the import WAS the run (top-level await)
		}
		const def = mod.default;
		if (Effect.isEffect(def.main)) {
			// The well-behaved shape: interruption reaches it structurally, its scoped
			// finalizers run on shutdown.
			yield* (def.main as Effect.Effect<unknown, unknown, PunktfunkHost>).pipe(
				Effect.provide(hostLayer(options.connect)),
			);
		} else {
			// The simple shape: a facade client whose close is guaranteed by the scope —
			// on completion, failure, OR interruption (shutdown).
			const main = def.main as (pf: unknown) => Promise<unknown> | unknown;
			yield* Effect.scoped(
				Effect.gen(function* () {
					const pf = yield* Effect.acquireRelease(
						Effect.tryPromise(() => connect(options.connect)),
						(client) => Effect.sync(() => client.close()),
					);
					yield* Effect.tryPromise(async () => await main(pf));
				}),
			);
		}
		return "plugin" as const;
	});

/**
 * A unit under supervision: plugins restart on failure (capped exponential backoff, jittered);
 * a clean completion ends the unit; bare scripts are one-shot either way. Never fails the
 * runner — every outcome is logged.
 */
export const superviseUnit = (
	unit: Unit,
	options: RunnerOptions = {},
): Effect.Effect<void> => {
	const log = options.log ?? defaultLog;
	// Exponential backoff, capped at 60 s (min-delay of the two schedules), then jittered.
	// (v4 replaced `Schedule.union` with the array-form `Schedule.min`.)
	const restart = Schedule.min([
		Schedule.exponential(options.restartBase ?? "1 second"),
		Schedule.spaced("60 seconds"),
	]).pipe(Schedule.jittered);
	let attempt = 0;
	const once = Effect.suspend(() => {
		attempt += 1;
		if (attempt > 1)
			log(`[${unit.name}] restarting (attempt ${attempt})`, "warn");
		return attemptUnit(unit, attempt, options, log);
	});
	return once.pipe(
		Effect.tap((kind) =>
			Effect.sync(() =>
				log(
					kind === "script"
						? `[${unit.name}] script completed`
						: `[${unit.name}] plugin completed`,
				),
			),
		),
		Effect.tapCause((cause) =>
			Effect.sync(() =>
				log(
					`[${unit.name}] failed: ${Cause.pretty(cause).split("\n")[0]}`,
					"error",
				),
			),
		),
		Effect.retry(restart),
		Effect.catchCause((cause) =>
			// A retry schedule that gives up (it doesn't, but stay total) — log and end.
			Effect.sync(() =>
				log(`[${unit.name}] gave up: ${Cause.pretty(cause)}`, "error"),
			),
		),
		Effect.asVoid,
	);
};

/**
 * Run exactly one unit, here, with no sandbox and no restart: this is what the child process
 * inside a sandbox does. Restarting is the supervisor's job on the other side of the process
 * boundary, which is also what makes a crashed plugin visible as an exit code.
 */
export const runOneUnit = (
	unit: Unit,
	options: RunnerOptions = {},
): Effect.Effect<void, unknown> => {
	const log = options.log ?? defaultLog;
	log(`[${unit.name}] starting (${unit.file})`);
	return attemptUnit(unit, 1, { ...options, sandbox: "off" }, log).pipe(
		Effect.tap((kind) =>
			Effect.sync(() =>
				log(
					kind === "script"
						? `[${unit.name}] script completed`
						: `[${unit.name}] plugin completed`,
				),
			),
		),
		Effect.tapCause((cause) =>
			Effect.sync(() =>
				log(`[${unit.name}] failed: ${Cause.pretty(cause).split("\n")[0]}`, "error"),
			),
		),
		Effect.asVoid,
	);
};

/**
 * The runner: discover units, supervise each as a fiber, run until interrupted — at which
 * point every unit is interrupted STRUCTURALLY (scoped finalizers run: facade clients close,
 * Effect plugins release what they acquired).
 */
export const runner = (options: RunnerOptions = {}): Effect.Effect<void> => {
	const log = options.log ?? defaultLog;
	return Effect.scoped(
		Effect.gen(function* () {
			// bwrap is the Linux sandbox. Windows confines the whole runner with its own account.
			const linux = process.platform === "linux";
			const sandbox = options.sandbox ?? (linux ? sandboxMode() : "off");
			if (sandbox === "off" && linux) {
				log(
					"[runner] PUNKTFUNK_PLUGIN_SANDBOX=off — plugins run in this process, with your account's access",
					"warn",
				);
			} else if (sandbox === "on") {
				const probe = sandboxProbe();
				if (!probe.ok) {
					log(`[runner] plugins cannot be sandboxed: ${probe.reason}`, "error");
					log("[runner] refusing to run plugins unsandboxed — set PUNKTFUNK_PLUGIN_SANDBOX=off to accept that", "error");
				}
			}
			// A package with no manifest has nothing to build its sandbox from, so it does not run.
			const units = discoverUnits(options, log).filter((unit) => {
				if (sandbox === "off" || !unit.packageDir || unit.manifest) return true;
				log(
					`[runner] not starting ${unit.name}: no punktfunk manifest to sandbox it with — update the plugin`,
					"error",
				);
				return false;
			});
			if (units.length === 0) {
				log(
					"[runner] nothing to run — add scripts to the scripts dir or install punktfunk-plugin-* packages",
				);
			}
			for (const unit of units) {
				log(`[runner] starting ${unit.name} (${unit.file})`);
				yield* Effect.forkScoped(superviseUnit(unit, { ...options, sandbox }));
			}
			yield* Effect.never; // interruption (shutdown) collapses the scope → all units
		}),
	);
};
