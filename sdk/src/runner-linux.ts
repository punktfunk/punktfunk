// Launching a plugin out of process. The Linux shape is a bwrap sandbox (`bwrapArgv` in
// sandbox.ts). What every out-of-process shape shares lives here too, and runner-windows.ts and
// runner-macos.ts build on it: the state and token prep, the host proxy, the `--run-unit` argv,
// the spawn, the stderr tail and the exit outcome.
import { Effect } from "effect";
import { type ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { Readable, Writable } from "node:stream";
import { configDir, hostFetch, publishedMgmtUrl } from "./config.js";
import { type HostProxy, serveHostProxy } from "./host-proxy.js";
import type { RunnerOptions, Unit } from "./runner.js";
import type { LogSink } from "./runner-log.js";
import {
	bwrapArgv,
	expandHome,
	type GrantedRoot,
	grantedPorts,
	grantedRoots,
	homeLink,
	netlinkFilter,
	onPath,
	type PluginManifest,
	pastaArgv,
	refusedRoot,
	resolverFile,
	sandboxCommand,
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
 * link it left there is removed, never followed to another plugin's files. macOS reads it where
 * the plugin finds it, under `name`.
 */
export const writePluginToken = (
	config: string,
	stateDir: string,
	id: string,
	name = ".plugin-token",
): string | Error => {
	const token = pluginToken(config, id);
	if (token === undefined)
		return new Error(
			`No API credential exists for ${id} yet. Restart the host if this persists.`,
		);
	const file = path.join(stateDir, name);
	try {
		fs.mkdirSync(stateDir, { recursive: true });
		fs.rmSync(file, { force: true });
		fs.writeFileSync(file, `PUNKTFUNK_PLUGIN_TOKEN=${token}\n`, { mode: 0o600, flag: "wx" });
		return file;
	} catch (e) {
		return new Error(`Couldn't write the credential for ${id} into ${stateDir} — ${e}`);
	}
};

/** Say which of a plugin's declared paths and grants its sandbox leaves out, and why. */
export const warnRefused = (
	manifest: PluginManifest,
	grants: readonly GrantedRoot[],
	home: string,
	config: string,
	id: string,
	log: LogSink,
): void => {
	const refused = [
		...[...(manifest.reads ?? []), ...(manifest.files ?? [])].map((p) => [p, false, false] as const),
		// A declared write the owner may still allow is waiting, not refused.
		...(manifest.writes ?? []).map((p) => [p, true, true] as const),
		...grants.map((g) => [g.path, g.write, true] as const),
	]
		.map(([p, write, operator]) => [expandHome(p, home), write, operator] as const)
		.filter(
			([p, write, operator]) => path.isAbsolute(p) && refusedRoot(p, home, config, write, operator),
		)
		.map(([p]) => p);
	if (refused.length > 0)
		log(`[runner] ${id}: not sharing ${refused.join(", ")} — no plugin gets those`, "warn");
};

/** The supervisor's end of a sandbox's way to the host: `socket`, forwarded over the pinned TLS. */
export const proxyToHost = (socket: string, options: RunnerOptions): HostProxy => {
	const url = options.connect?.url ?? publishedMgmtUrl() ?? "https://127.0.0.1:47990";
	// The host's cert is self-signed: a bare `fetch` fails TLS and every plugin 502s at connect.
	const pinned = options.sandboxFetch
		? Promise.resolve(options.sandboxFetch)
		: hostFetch(url, options.connect);
	return serveHostProxy({
		socket,
		url,
		fetch: ((input, init) => pinned.then((f) => f(input, init))) as typeof fetch,
	});
};

/**
 * Run one plugin in its own sandbox: a child process under `bwrap`, re-executing this runner in
 * `--run-unit` mode so the plugin's code never shares an address space — or a token — with
 * another's. A network plugin also needs `unshare` and pasta, which `find` looks up; without
 * either it does not start.
 *
 * Resolves when the child exits; a non-zero exit fails the effect, which is what makes the
 * supervisor restart it with the same backoff an in-process failure gets.
 */
export const runSandboxed = (
	unit: Unit,
	manifest: PluginManifest,
	options: RunnerOptions,
	log: LogSink,
	find: (cmd: string) => string | undefined = onPath,
): Effect.Effect<"plugin", unknown> =>
	Effect.callback<"plugin", unknown>((resume) => {
		const id = manifest.id ?? unit.name;
		let tools: { unshare: string; pasta: string } | undefined;
		if (manifest.network) {
			const unshare = find("unshare");
			const pasta = find("pasta");
			if (!unshare || !pasta) {
				const missing = unshare ? "pasta from passt" : "unshare from util-linux";
				resume(Effect.fail(new Error(`connect ${id} to the network: ${missing} is not on PATH`)));
				return;
			}
			tools = { unshare, pasta };
		}
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
		warnRefused(manifest, grants, home, config, id, log);
		const runtime = process.env.XDG_RUNTIME_DIR ?? "/tmp";
		// One per attempt: a restart's new proxy binds while the old one is still closing, and a
		// shared name would let the old close delete the new socket.
		const socket = path.join(
			runtime,
			"punktfunk",
			`plugin-${id}-${randomBytes(4).toString("hex")}.sock`,
		);
		const proxy = proxyToHost(socket, options);
		// Its UI socket goes in a dir of its own, forwarded to the host's loopback.
		let ui: { dir: string; port: number } | undefined;
		let forward: UiForward | undefined;
		const dir = path.join(runtime, "punktfunk", `ui-${id}-${randomBytes(4).toString("hex")}`);
		try {
			fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
			forward = forwardUi(path.join(dir, "ui.sock"));
			ui = { dir, port: forward.port };
		} catch (e) {
			log(`[runner] ${id}: its settings page stays unreachable — ${e}`, "warn");
		}
		// A network plugin's resolver and its pasta's pid, in a dir no sandbox binds.
		const net = tools
			? path.join(runtime, "punktfunk", `net-${id}-${randomBytes(4).toString("hex")}`)
			: undefined;
		let stopNet: (() => void) | undefined;
		const release = (): void => {
			proxy.close();
			forward?.close();
			if (ui) fs.rmSync(ui.dir, { recursive: true, force: true });
			if (stopNet) stopNet();
			else if (net) fs.rmSync(net, { recursive: true, force: true });
		};
		if (net) {
			try {
				fs.mkdirSync(net, { recursive: true, mode: 0o700 });
				const host = (() => {
					try {
						return fs.readFileSync("/etc/resolv.conf", "utf8");
					} catch {
						return "";
					}
				})();
				fs.writeFileSync(path.join(net, "resolv.conf"), resolverFile(host));
			} catch (e) {
				release();
				resume(Effect.fail(new Error(`prepare the network of ${id} in ${net}: ${e}`)));
				return;
			}
		}
		const [cmd, args] = sandboxCommand(
			manifest,
			bwrapArgv(
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
					...(net ? { resolvConf: path.join(net, "resolv.conf") } : {}),
				},
				grants,
			),
			tools?.unshare,
		);
		const child = spawn(cmd, [...args, process.execPath, ...unitArgv(unit), "--stop-on-eof"], {
			env: sandboxEnv(os.homedir()),
			// stdin is the stop channel (`stopUnitChild`). fd 3 is `--add-seccomp-fd 3`; a network
			// plugin's 4 and 5 are `--info-fd` and `--block-fd`.
			stdio: ["pipe", "inherit", "pipe", "pipe", ...(net ? (["pipe", "pipe"] as const) : [])],
		});
		// stderr still reaches the journal; its tail also names why the sandbox exited, since
		// bwrap's own errors come before the plugin can ship a log line.
		const lastLines = tailStderr(child);
		// A bwrap that dies before reading surfaces through `exit`, not an EPIPE here.
		(child.stdio[3] as Writable).on("error", () => {}).end(filter);
		if (tools && net) {
			const failed = (why: string): void => {
				log(`[runner] ${id}: its network did not come up — ${why}`, "error");
				stopNet?.();
				void stopUnitChild(child);
			};
			// The ports on this computer the operator granted, and no other of its loopback.
			stopNet = connectNetwork(child, tools.pasta, net, failed, grantedPorts(config, id));
		}
		child.on("error", (e) => {
			release();
			resume(Effect.fail(e));
		});
		child.on("exit", (code, signal) => {
			release();
			resume(exitOutcome("sandboxed plugin", code, signal, lastLines()));
		});
		// Interruption (shutdown, a grant change) waits for the plugin's finalizers, its network and
		// host proxy still up. `--die-with-parent` is the backstop if this runner is killed outright.
		return Effect.promise(() => stopUnitChild(child).then(release));
	});

/**
 * Connect a network plugin's namespace once bwrap reports on `--info-fd` (fd 4): run pasta on
 * `child`, which holds the namespace `unshare` made, then let the sandbox go on through
 * `--block-fd` (fd 5). `fail` hears why the network did not come up. `ports` are the TCP ports
 * on this computer the plugin may connect to ({@link pastaArgv}).
 *
 * pasta copies the host's default routes once, as it starts, each family alone. `routes` is
 * read before and after: a namespace that got no IPv4 route is checked again every `watchMs`,
 * and `fail` hears it once the host has a route the namespace lacks, so the restart connects a
 * fresh one. One that has IPv4 stays as it is: a plugin's servers answer there.
 *
 * The returned stop ends the setup or the daemon it left and only then removes `dir`, which holds
 * the daemon's pid file. A sandbox still blocked gets SIGKILL: until it reads fd 5 it has no
 * parent-death signal, and a closed pipe would let it run on unsupervised.
 */
export const connectNetwork = (
	child: ChildProcess,
	pasta: string,
	dir: string,
	fail: (why: string) => void,
	ports: readonly number[] = [],
	routes: () => DefaultRoutes = hostDefaultRoutes,
	watchMs = 5000,
): (() => void) => {
	const pidFile = path.join(dir, "pasta.pid");
	const [info, block] = (child.stdio as readonly unknown[]).slice(4) as [Readable, Writable];
	info.on("error", () => {});
	block.on("error", () => {});
	let blocked: number | undefined;
	let setup: ChildProcess | undefined;
	let running = false;
	let stopped = false;
	let watch: ReturnType<typeof setInterval> | undefined;
	const end = (): void => {
		stopPasta(pidFile);
		fs.rmSync(dir, { recursive: true, force: true });
	};
	const kill = (pid: number | undefined): void => {
		try {
			// 1 or below would signal init or a whole process group.
			if (pid !== undefined && pid > 1) process.kill(pid, "SIGKILL");
		} catch {
			// Already gone.
		}
	};
	let text = "";
	const onInfo = (chunk: Buffer): void => {
		text += chunk.toString();
		const pid = infoPid(text);
		if (pid === undefined || child.pid === undefined) return;
		// bwrap is still writing: closing this end would end it on SIGPIPE, so drain to its EOF.
		info.off("data", onInfo);
		info.resume();
		if (stopped) {
			// Stopped before the report: the sandbox waits on fd 5 with no parent-death signal.
			kill(pid);
			return;
		}
		blocked = pid;
		const before = routes();
		setup = spawn(pasta, pastaArgv(child.pid, pidFile, ports), {
			stdio: ["ignore", "ignore", "pipe"],
		});
		running = true;
		const lastLines = tailStderr(setup);
		setup.on("error", (e) => {
			running = false;
			if (stopped) end();
			else fail(String(e));
		});
		setup.on("exit", (code, signal) => {
			running = false;
			if (stopped) end();
			else if (code === 0) {
				blocked = undefined;
				block.end("1");
				// Only a route there before pasta read the host's and after is surely copied.
				const after = routes();
				const got = { v4: before.v4 && after.v4, v6: before.v6 && after.v6 };
				if (got.v4) return;
				watch = setInterval(() => {
					const why = stopped ? undefined : staleRoutes(got, routes());
					if (why === undefined) return;
					clearInterval(watch);
					fail(why);
				}, watchMs);
				watch.unref();
			} else {
				const last = lastLines();
				fail(`pasta exited ${signal ? `on ${signal}` : `with ${code}`}${last ? `: ${last}` : ""}`);
			}
		});
	};
	info.on("data", onInfo);
	return () => {
		if (stopped) return;
		stopped = true;
		clearInterval(watch);
		kill(blocked);
		if (running) setup?.kill("SIGTERM");
		else end();
	};
};

/** Whether the host has a usable default route, per family. */
export interface DefaultRoutes {
	v4: boolean;
	v6: boolean;
}

/**
 * Why a namespace that copied the routes `got` must reconnect now that the host has `now`, or
 * `undefined` to leave it. One with an IPv4 route stays whatever comes: a plugin's servers
 * answer there, and a restart for IPv6 alone would cut its work for nothing.
 */
export const staleRoutes = (got: DefaultRoutes, now: DefaultRoutes): string | undefined =>
	!got.v4 && (now.v4 || (now.v6 && !got.v6))
		? `the host had no ${got.v6 ? "IPv4 " : ""}default route when it started`
		: undefined;

/**
 * The families the host has a usable default route in, from procfs. The kernel's
 * `unreachable ::/0` on `lo` is a reject route without the up flag, so it never counts.
 */
export const hostDefaultRoutes = (
	read: (file: string) => string = (file) => fs.readFileSync(file, "utf8"),
): DefaultRoutes => {
	const rows = (file: string): string[][] => {
		try {
			return read(file)
				.split("\n")
				.map((line) => line.trim().split(/\s+/));
		} catch {
			return [];
		}
	};
	// RTF_UP set, RTF_REJECT clear.
	const usable = (flags = ""): boolean => {
		const n = Number.parseInt(flags, 16);
		return (n & 0x1) !== 0 && (n & 0x200) === 0;
	};
	// Iface, Destination, Gateway, Flags, …, Mask in the eighth column.
	const v4 = rows("/proc/net/route").some(
		(f) => f[1] === "00000000" && f[7] === "00000000" && usable(f[3]),
	);
	// Destination, prefix length, …, flags in the ninth column.
	const v6 = rows("/proc/net/ipv6_route").some(
		(f) => f[0] === "0".repeat(32) && f[1] === "00" && usable(f[8]),
	);
	return { v4, v6 };
};

/** The sandbox's pid in bwrap's `--info-fd` text so far. A read may end inside the number. */
export const infoPid = (text: string): number | undefined => {
	const pid = /"child-pid":\s*(\d+)\D/.exec(text)?.[1];
	return pid === undefined ? undefined : Number(pid);
};

/** Stop the pasta daemon `pidFile` names. Never written, or already gone: nothing to stop. */
const stopPasta = (pidFile: string): void => {
	try {
		const pid = Number(fs.readFileSync(pidFile, "utf8"));
		// 1 or below would signal init or a whole process group.
		if (Number.isInteger(pid) && pid > 1) process.kill(pid, "SIGTERM");
	} catch {}
};

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

/**
 * How long a stopped sandbox has to run its plugin's finalizers. Every plugin stops at once, and
 * their leases and the log flush follow, inside the scripting unit's 30 s `TimeoutStopSec`.
 */
export const STOP_GRACE_MS = 10_000;

/**
 * Stop a sandbox's `--run-unit` child, spawned with a stdin pipe and `--stop-on-eof`. Closing
 * stdin interrupts its unit as SIGTERM would, and gets through bwrap, which forwards no signal.
 * One still running after `graceMs` gets SIGKILL. Resolves once it has exited.
 */
export const stopUnitChild = (child: ChildProcess, graceMs = STOP_GRACE_MS): Promise<void> =>
	new Promise((done) => {
		if (child.pid === undefined || child.exitCode !== null || child.signalCode !== null) {
			done();
			return;
		}
		const kill = setTimeout(() => child.kill("SIGKILL"), graceMs);
		child.once("exit", () => {
			clearTimeout(kill);
			done();
		});
		child.stdin?.on("error", () => {}).end();
	});

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
