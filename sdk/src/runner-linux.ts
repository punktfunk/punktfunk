// Launching a plugin out of process. The Linux shape is a bwrap sandbox (`bwrapArgv` in
// sandbox.ts). What every out-of-process shape shares lives here too, and runner-windows.ts
// builds on it: the state and token prep, the `--run-unit` argv, the spawn, the stderr tail and
// the exit outcome.
import { Effect } from "effect";
import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Writable } from "node:stream";
import { configDir, hostFetch, publishedMgmtUrl } from "./config.js";
import { serveHostProxy } from "./host-proxy.js";
import type { RunnerOptions, Unit } from "./runner.js";
import type { LogSink } from "./runner-log.js";
import {
	bwrapArgv,
	expandHome,
	grantedRoots,
	homeLink,
	netlinkFilter,
	type PluginManifest,
	refusedRoot,
	sandboxEnv,
} from "./sandbox.js";
import { forwardUi, type UiForward } from "./ui-forward.js";

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
export const pluginToken = (config: string, id: string): string | undefined => {
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
export const runSandboxed = (
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
			.filter((p) => path.isAbsolute(p) && refusedRoot(p, home, config));
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
					configDir: config,
					homeLink: homeLink(),
					...(ui ? { ui } : {}),
				},
				grants,
			),
			process.execPath,
			...unitArgv(unit),
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
export const tailStderr = (child: ChildProcess): (() => string) => {
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
export const exitOutcome = (
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

/** The runner bundle this process is running, which each sandbox re-execs. */
export const runnerEntry = (): string =>
	process.env.PUNKTFUNK_RUNNER_ENTRY ?? process.argv[1] ?? "";

/** The argv tail every out-of-process shape runs: this runner, in `--run-unit` mode, on `unit`. */
export const unitArgv = (unit: Unit): string[] => [
	runnerEntry(),
	"--run-unit",
	unit.file,
	"--unit-name",
	unit.name,
];

/**
 * Spawn a unit's child and resolve when it exits: 0 completes the unit, anything else fails it
 * with `label` and the tail of its stderr. Interruption (shutdown) sends SIGTERM, which runs the
 * child's own finalizers, or ends a helper whose job takes the plugin with it.
 */
export const spawnUnitChild = (
	cmd: string,
	args: string[],
	env: NodeJS.ProcessEnv,
	label: string,
): Effect.Effect<"plugin", unknown> =>
	Effect.callback<"plugin", unknown>((resume) => {
		const child = spawn(cmd, args, {
			env,
			stdio: ["ignore", "inherit", "pipe"],
			windowsHide: true,
		});
		const lastLines = tailStderr(child);
		child.on("error", (e) => resume(Effect.fail(e)));
		child.on("exit", (code, signal) =>
			resume(exitOutcome(label, code, signal, lastLines())),
		);
		return Effect.sync(() => {
			child.kill("SIGTERM");
		});
	});
