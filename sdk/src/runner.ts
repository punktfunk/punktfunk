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
	Fiber,
	Schedule,
} from "effect";
import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Writable } from "node:stream";
import { pathToFileURL } from "node:url";
import { PunktfunkHost } from "./client.js";
import { discoverUnits } from "./discover.js";
import { layer as hostLayer } from "./effect.js";
import { type ConnectOptions, configDir, hostFetch, publishedMgmtUrl } from "./config.js";
import { connect, type PluginDef } from "./index.js";
import {
	bwrapArgv,
	expandHome,
	grantedRoots,
	homeLink,
	netlinkFilter,
	type PluginManifest,
	refusedRoot,
	sandboxEnv,
	sandboxProbe,
} from "./sandbox.js";
import { serveHostProxy } from "./host-proxy.js";
import { hostExe } from "./sandbox.js";
import { forwardUi, type UiForward } from "./ui-forward.js";
import { defaultLog, type LogSink, type RunnerLogLevel } from "./runner-log.js";

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
	/**
	 * With the sandbox off, still give each plugin that declared a manifest a process of its
	 * own. Default: on Windows, where no sandbox exists yet and a plugin's crash must not take
	 * the others down. The Linux sandbox is already a process.
	 */
	ownProcess?: boolean;
	/** The pinned fetch the sandbox proxy forwards with (test seam). */
	sandboxFetch?: typeof globalThis.fetch;
	/** Config root holding plugin grants, tokens, and state. Default `configDir()`. */
	configDir?: string;
	/** Grants poll period. Default 2 seconds; tests shorten it. */
	grantPollInterval?: Duration.Input;
	/** Run one sandbox attempt without spawning bwrap (test seam). */
	sandboxRun?: (
		unit: Unit,
		manifest: PluginManifest,
		options: RunnerOptions,
		log: LogSink,
	) => Effect.Effect<"plugin", unknown>;
	/**
	 * Line sink. Default: stamped stdout, with `warn`/`error` going to the matching console method
	 * (hence stderr, and hence the right level in the console's log page — see `log-ship.ts`).
	 *
	 * `level` is optional so an existing `(line: string) => void` sink stays assignable.
	 */
	log?: (line: string, level?: RunnerLogLevel) => void;
}

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

/**
 * Whether plugins run sandboxed at all. Off is an explicit operator choice, logged where they
 * will see it — never a silent downgrade because a box could not manage a namespace.
 */
export const sandboxMode = (): "on" | "off" =>
	/^(0|off|false)$/i.test(process.env.PUNKTFUNK_PLUGIN_SANDBOX ?? "") ||
	// The Windows host publishes `host.env`'s answer here; the runner's task never sees host.env.
	(process.platform === "win32" &&
		fs.existsSync(path.join(configDir(), "plugin-run", "sandbox-off")))
		? "off"
		: "on";

/**
 * Move `<state>/<id>/` up into `<state>/`: 0.39 bound the state dir one level too high, so a
 * sandboxed plugin wrote there. On a clash the nested file is the newer one; the older is kept
 * beside it as `<name>.pre-sandbox`.
 */
export const adoptNestedState = (stateDir: string, id: string, log: LogSink): void => {
	const nested = path.join(stateDir, id);
	let names: string[];
	try {
		if (!fs.lstatSync(nested).isDirectory()) return;
		names = fs.readdirSync(nested);
	} catch {
		return;
	}
	for (const name of names) {
		const to = path.join(stateDir, name);
		try {
			if (fs.existsSync(to)) fs.renameSync(to, `${to}.pre-sandbox`);
			fs.renameSync(path.join(nested, name), to);
		} catch (e) {
			log(`[runner] ${id}: state ${name} stayed in ${nested}: ${e}`, "warn");
			return;
		}
	}
	try {
		fs.rmdirSync(nested);
	} catch {}
	log(`[runner] ${id}: moved its state up from ${nested}`);
};

/** This plugin's own API token, from the map the host mints for every installed manifest. */
const pluginToken = (config: string, id: string): string | undefined => {
	try {
		const tokens = JSON.parse(
			fs.readFileSync(path.join(config, "plugin-run", "plugin-tokens.json"), "utf8"),
		) as Record<string, string>;
		return tokens[id];
	} catch {
		return undefined;
	}
};

/**
 * The connection an in-process plugin gets: its own token when its manifest has one, so it is
 * scoped to its own provider and its folder requests reach the host. Anything else keeps the
 * runner's.
 */
export const inProcessConnect = (
	unit: Unit,
	options: RunnerOptions,
): ConnectOptions | undefined => {
	const id = unit.manifest?.id;
	const token = id ? pluginToken(options.configDir ?? configDir(), id) : undefined;
	return token ? { ...options.connect, token } : options.connect;
};

/**
 * Write this plugin's own token under its state dir for the sandbox's read-only bind. A missing
 * token and an unwritable state dir are different faults and say so.
 *
 * The plugin writes that dir, so the old file is unlinked and a new one created exclusively: a
 * link it left there is removed, never followed to another plugin's files.
 */
export const writePluginToken = (
	config: string,
	stateDir: string,
	id: string,
): string | Error => {
	const token = pluginToken(config, id);
	if (token === undefined)
		return new Error(
			`No API credential exists for ${id} yet. Restart the host if this persists.`,
		);
	const file = path.join(stateDir, ".plugin-token");
	try {
		fs.mkdirSync(stateDir, { recursive: true });
		fs.rmSync(file, { force: true });
		fs.writeFileSync(file, `PUNKTFUNK_PLUGIN_TOKEN=${token}\n`, { mode: 0o600, flag: "wx" });
		return file;
	} catch (e) {
		return new Error(`Couldn't write the credential for ${id} into ${stateDir} — ${e}`);
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
		const config = options.configDir ?? configDir();
		const stateDir = path.join(config, "plugin-state", id);
		adoptNestedState(stateDir, id, log);
		const tokenFile = writePluginToken(config, stateDir, id);
		if (tokenFile instanceof Error) {
			resume(Effect.fail(tokenFile));
			return;
		}
		const filter = netlinkFilter();
		if (!filter) {
			resume(Effect.fail(new Error(`no sandbox syscall filter for ${process.arch}`)));
			return;
		}
		const home = os.homedir();
		const grants = grantedRoots(config, id);
		const refused = [
			...(manifest.reads ?? []),
			...(manifest.writes ?? []),
			...grants.map((g) => g.path),
		]
			.map((p) => expandHome(p, home))
			.filter((p) => path.isAbsolute(p) && refusedRoot(p, home));
		if (refused.length > 0)
			log(`[runner] ${id}: not sharing ${refused.join(", ")} — no plugin gets those`, "warn");
		const runtime = process.env.XDG_RUNTIME_DIR ?? "/tmp";
		// One per attempt: a restart's new proxy binds while the old one is still closing, and a
		// shared name would let the old close delete the new socket.
		const socket = path.join(
			runtime,
			"punktfunk",
			`plugin-${id}-${randomBytes(4).toString("hex")}.sock`,
		);
		const url = options.connect?.url ?? publishedMgmtUrl() ?? "https://127.0.0.1:47990";
		// The host's cert is self-signed: a bare `fetch` fails TLS and every plugin 502s at connect.
		const pinned = options.sandboxFetch
			? Promise.resolve(options.sandboxFetch)
			: hostFetch(url, options.connect);
		const proxy = serveHostProxy({
			socket,
			url,
			fetch: ((input, init) => pinned.then((f) => f(input, init))) as typeof fetch,
		});
		// No network: its UI socket goes in a dir of its own, forwarded to the host's loopback.
		let ui: { dir: string; port: number } | undefined;
		let forward: UiForward | undefined;
		if (!manifest.network) {
			const dir = path.join(runtime, "punktfunk", `ui-${id}-${randomBytes(4).toString("hex")}`);
			try {
				fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
				forward = forwardUi(path.join(dir, "ui.sock"));
				ui = { dir, port: forward.port };
			} catch (e) {
				log(`[runner] ${id}: its settings page stays unreachable — ${e}`, "warn");
			}
		}
		const release = (): void => {
			proxy.close();
			forward?.close();
			if (ui) fs.rmSync(ui.dir, { recursive: true, force: true });
		};
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
					home,
					homeLink: homeLink(),
					...(ui ? { ui } : {}),
				},
				grants,
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
			stdio: ["ignore", "inherit", "pipe", "pipe"],
		});
		// stderr still reaches the journal; its tail also names why the sandbox exited, since
		// bwrap's own errors come before the plugin can ship a log line.
		const lastLines = tailStderr(child);
		// A bwrap that dies before reading surfaces through `exit`, not an EPIPE here.
		(child.stdio[3] as Writable).on("error", () => {}).end(filter);
		child.on("error", (e) => {
			release();
			resume(Effect.fail(e));
		});
		child.on("exit", (code, signal) => {
			release();
			resume(exitOutcome("sandboxed plugin", code, signal, lastLines()));
		});
		return Effect.sync(() => {
			// Interruption (shutdown): SIGTERM lets the plugin's finalizers run; `--die-with-parent`
			// is the backstop if this runner is killed outright.
			child.kill("SIGTERM");
			release();
		});
	});

/** Mirror a child's stderr to ours and keep its last lines for the failure message. */
const tailStderr = (child: ChildProcess): (() => string) => {
	let tail = "";
	child.stderr?.on("data", (chunk: Buffer) => {
		process.stderr.write(chunk);
		tail = (tail + chunk.toString()).slice(-2000);
	});
	return () =>
		tail
			.split("\n")
			.filter((line) => line.trim() !== "")
			.slice(-3)
			.join(" | ");
};

/** A child's exit as the unit's outcome: 0 completes it, anything else fails it with the cause. */
const exitOutcome = (
	what: string,
	code: number | null,
	signal: NodeJS.Signals | null,
	last: string,
): Effect.Effect<"plugin", Error> =>
	code === 0
		? Effect.succeed("plugin" as const)
		: Effect.fail(
				new Error(`${what} exited ${signal ? `on ${signal}` : `with ${code}`}${last ? ` — ${last}` : ""}`),
			);

/**
 * Run one plugin in a process of its own with nothing around it: the Windows shape until the
 * AppContainer lands. The child re-execs this runner in `--run-unit` mode and resolves its own
 * token and host URL from the config dir, so nothing secret rides in its environment or argv.
 * A crash is an exit code, and the supervisor restarts this plugin alone.
 */
const runInOwnProcess = (unit: Unit, options: RunnerOptions): Effect.Effect<"plugin", unknown> =>
	Effect.callback<"plugin", unknown>((resume) => {
		const child = spawn(
			process.execPath,
			[runnerEntry(), "--run-unit", unit.file, "--unit-name", unit.name],
			{
				env: {
					...process.env,
					...(options.configDir ? { PUNKTFUNK_CONFIG_DIR: options.configDir } : {}),
					...pipeEnv(unit),
				},
				stdio: ["ignore", "inherit", "pipe"],
				windowsHide: true,
			},
		);
		const lastLines = tailStderr(child);
		child.on("error", (e) => resume(Effect.fail(e)));
		child.on("exit", (code, signal) =>
			resume(exitOutcome("plugin process", code, signal, lastLines())),
		);
		// Interruption (shutdown): the child's own signal handler runs its finalizers.
		return Effect.sync(() => {
			child.kill("SIGTERM");
		});
	});

/**
 * Run one plugin in its own AppContainer: `punktfunk-host plugins spawn` creates the profile for
 * this account and starts this runner's `--run-unit` child inside it, relaying its exit code
 * and ending the child with itself. The child holds no token: its own pipe is its credential.
 */
const runInContainer = (
	unit: Unit,
	manifest: PluginManifest,
	options: RunnerOptions,
	log: LogSink,
): Effect.Effect<"plugin", unknown> =>
	Effect.callback<"plugin", unknown>((resume) => {
		const id = manifest.id ?? unit.name;
		const config = options.configDir ?? configDir();
		adoptNestedState(path.join(config, "plugin-state", id), id, log);
		const pipe = pipeEnv(unit);
		const child = spawn(
			hostExe(),
			[
				"plugins",
				"spawn",
				"--package",
				id,
				"--",
				process.execPath,
				runnerEntry(),
				"--run-unit",
				unit.file,
				"--unit-name",
				unit.name,
			],
			{
				env: {
					...process.env,
					...(options.configDir ? { PUNKTFUNK_CONFIG_DIR: options.configDir } : {}),
					...pipe,
					// The container can bind nothing the console dials: its page goes over the pipe.
					...(pipe.PUNKTFUNK_MGMT_UNIX ? { PUNKTFUNK_UI_CHANNEL: id } : {}),
				},
				stdio: ["ignore", "inherit", "pipe"],
				windowsHide: true,
			},
		);
		const lastLines = tailStderr(child);
		child.on("error", (e) => resume(Effect.fail(e)));
		child.on("exit", (code, signal) =>
			resume(exitOutcome("sandboxed plugin", code, signal, lastLines())),
		);
		// Interruption (shutdown): the helper dies, and its job takes the plugin with it.
		return Effect.sync(() => {
			child.kill("SIGTERM");
		});
	});

/**
 * Where a Windows plugin reaches the host: the pipe the host serves for its id, plain HTTP, no
 * port. A host without the pipe (before 0.44) leaves this empty and the child dials the port.
 */
const pipeEnv = (unit: Unit): Record<string, string> => {
	const id = unit.manifest?.id;
	if (process.platform !== "win32" || !id) return {};
	const pipe = `\\\\.\\pipe\\punktfunk-plugin-${id}`;
	if (!fs.existsSync(pipe)) return {};
	return { PUNKTFUNK_MGMT_URL: "http://punktfunk.host", PUNKTFUNK_MGMT_UNIX: pipe };
};

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
			const run =
				options.sandboxRun ?? (process.platform === "win32" ? runInContainer : runSandboxed);
			return yield* run(unit, unit.manifest, options, log);
		}
		if (unit.manifest && (options.ownProcess ?? process.platform === "win32")) {
			return yield* runInOwnProcess(unit, options);
		}
		const mod = (yield* Effect.tryPromise(
			() => import(`${pathToFileURL(unit.file).href}?attempt=${attempt}`),
		)) as { default?: unknown };
		if (!isPluginDef(mod.default)) {
			return "script" as const; // the import WAS the run (top-level await)
		}
		const def = mod.default;
		const own = inProcessConnect(unit, options);
		if (Effect.isEffect(def.main)) {
			// The well-behaved shape: interruption reaches it structurally, its scoped
			// finalizers run on shutdown.
			yield* (def.main as Effect.Effect<unknown, unknown, PunktfunkHost>).pipe(
				Effect.provide(hostLayer(own)),
			);
		} else {
			// The simple shape: a facade client whose close is guaranteed by the scope —
			// on completion, failure, OR interruption (shutdown).
			const main = def.main as (pf: unknown) => Promise<unknown> | unknown;
			yield* Effect.scoped(
				Effect.gen(function* () {
					const pf = yield* Effect.acquireRelease(
						Effect.tryPromise(() => connect(own)),
						(client) => Effect.sync(() => client.close()),
					);
					yield* Effect.tryPromise(async () => await main(pf));
				}),
			);
		}
		return "plugin" as const;
	});

/**
 * The first lines of what actually failed. `Effect.tryPromise` wraps a rejection in an
 * `UnknownError` whose own message says nothing, so the wrapper is peeled off.
 */
export const describeFailure = (cause: Cause.Cause<unknown>): string => {
	let err: unknown = Cause.squash(cause);
	while (
		typeof err === "object" &&
		err !== null &&
		(err as { _tag?: unknown })._tag === "UnknownError" &&
		"cause" in err
	)
		err = (err as { cause: unknown }).cause;
	const text = err instanceof Error ? err.message || err.name : String(err);
	return text
		.split("\n")
		.filter((line) => line.trim() !== "")
		.slice(0, 6)
		.join(" | ");
};

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
			Effect.sync(() => log(`[${unit.name}] failed: ${describeFailure(cause)}`, "error")),
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
			Effect.sync(() => log(`[${unit.name}] failed: ${describeFailure(cause)}`, "error")),
		),
		Effect.asVoid,
	);
};

type FileStamp = { mtimeMs: number; size: number } | undefined;

/** The grants file's rename-safe polling stamp; absence is a stable state too. */
const grantFileStamp = (config: string): FileStamp => {
	try {
		const stat = fs.statSync(path.join(config, "plugin-run", "plugin-grants.json"));
		return { mtimeMs: stat.mtimeMs, size: stat.size };
	} catch {
		return undefined;
	}
};

const sameStamp = (a: FileStamp, b: FileStamp): boolean =>
	a === undefined ? b === undefined : b !== undefined && a.mtimeMs === b.mtimeMs && a.size === b.size;

const grantKey = (config: string, unit: Unit): string =>
	JSON.stringify(grantedRoots(config, unit.manifest?.id ?? unit.name));

/** Poll effective grants and replace only the supervisor whose roots changed. */
const watchGrantChanges = (
	config: string,
	units: Unit[],
	fibers: Map<string, Fiber.Fiber<void, never>>,
	options: RunnerOptions & { sandbox: "on" | "off" },
	log: LogSink,
) => {
	const grantKeys = new Map(units.map((unit) => [unit.name, grantKey(config, unit)]));
	let stamp = grantFileStamp(config);
	return Effect.forever(
		Effect.sleep(options.grantPollInterval ?? "2 seconds").pipe(
			Effect.andThen(
				Effect.gen(function* () {
					const nextStamp = grantFileStamp(config);
					if (sameStamp(stamp, nextStamp)) return;
					stamp = nextStamp;
					for (const unit of units) {
						const nextKey = grantKey(config, unit);
						if (grantKeys.get(unit.name) === nextKey) continue;
						const id = unit.manifest?.id ?? unit.name;
						log(`[runner] ${id}: folder access changed — restarting`);
						const old = fibers.get(unit.name);
						if (old) yield* Fiber.interrupt(old);
						fibers.set(
							unit.name,
							yield* Effect.forkScoped(superviseUnit(unit, options)),
						);
						grantKeys.set(unit.name, nextKey);
					}
				}),
			),
		),
	);
};

/**
 * Discover and supervise every unit until interrupted. Sandboxed units are tracked separately:
 * a grants-file poll compares their effective roots and restarts only the plugin whose roots
 * changed. Every supervisor and the poller share this scope, so shutdown interrupts all of them
 * and runs their finalizers.
 */
export const runner = (options: RunnerOptions = {}): Effect.Effect<void> => {
	const log = options.log ?? defaultLog;
	return Effect.scoped(
		Effect.gen(function* () {
			// bwrap is the Linux sandbox, an AppContainer the Windows one; macOS has neither.
			const confines = process.platform === "linux" || process.platform === "win32";
			const sandbox = options.sandbox ?? (confines ? sandboxMode() : "off");
			if (sandbox === "off" && confines) {
				log(
					"[runner] PUNKTFUNK_PLUGIN_SANDBOX=off — plugins run with the runner's whole access",
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

			const config = options.configDir ?? configDir();
			const supervised = { ...options, sandbox };
			const fibers = new Map<string, Fiber.Fiber<void, never>>();
			for (const unit of units) {
				log(`[runner] starting ${unit.name} (${unit.file})`);
				fibers.set(unit.name, yield* Effect.forkScoped(superviseUnit(unit, supervised)));
			}

			const sandboxed = units.filter((unit) => sandbox !== "off" && unit.manifest);
			yield* Effect.forkScoped(
				watchGrantChanges(config, sandboxed, fibers, supervised, log),
			);
			yield* Effect.never; // interruption collapses the scope → poller and every unit
		}),
	);
};
