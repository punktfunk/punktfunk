// Launching a plugin out of process on Windows: a process of its own, or an AppContainer through
// `punktfunk-host plugins spawn`. A container reaches the host over the pipe it serves for the
// plugin's id; a process of its own over the pinned TLS port.
import { Effect } from "effect";
import * as fs from "node:fs";
import * as path from "node:path";
import { configDir } from "./config.js";
import type { RunnerOptions, Unit } from "./runner.js";
import { adoptNestedState, spawnUnitChild, unitArgv } from "./runner-linux.js";
import type { LogSink } from "./runner-log.js";
import { hostExe, type PluginManifest } from "./sandbox.js";

/**
 * Run one plugin in a process of its own with nothing around it: the Windows shape with the
 * sandbox off. The child re-execs this runner in `--run-unit` mode and reads its own token from
 * the config dir, so the runner adds nothing secret to its environment or argv; it reaches the
 * host over the plugin's pipe when the host serves one. A crash is an exit code, and the
 * supervisor restarts this plugin alone.
 */
export const runInOwnProcess = (
	unit: Unit,
	options: RunnerOptions,
): Effect.Effect<"plugin", unknown> =>
	Effect.suspend(() =>
		spawnUnitChild(process.execPath, unitArgv(unit), ownProcessEnv(options), "plugin process"),
	);

/**
 * The environment of a plugin in a process of its own. It sends the runner's bearer, so it never
 * dials a pipe, whose server nothing checks: it dials the TLS port, and the host's pinned
 * certificate proves the host before the bearer goes out.
 */
export const ownProcessEnv = (options: RunnerOptions): NodeJS.ProcessEnv => {
	const { PUNKTFUNK_MGMT_UNIX: _, ...env } = process.env;
	return { ...env, ...(options.configDir ? { PUNKTFUNK_CONFIG_DIR: options.configDir } : {}) };
};

/**
 * Run one plugin in its own AppContainer: `punktfunk-host plugins spawn` creates the profile for
 * this account and starts this runner's `--run-unit` child inside it, relaying its exit code
 * and ending the child with itself. The child holds no token: its own pipe is its credential.
 */
export const runInContainer = (
	unit: Unit,
	manifest: PluginManifest,
	options: RunnerOptions,
	log: LogSink,
): Effect.Effect<"plugin", unknown> =>
	Effect.suspend(() => {
		const id = manifest.id ?? unit.name;
		const config = options.configDir ?? configDir();
		adoptNestedState(path.join(config, "plugin-state", id), id, log);
		return spawnUnitChild(
			hostExe(),
			["plugins", "spawn", "--package", id, "--", process.execPath, ...unitArgv(unit)],
			containerEnv(unit, options),
			"sandboxed plugin",
		);
	});

/**
 * The pipe a seat host serves for plugin `id`, or the box's when `seat` is no seat id. The host
 * derives the same name (`mgmt/pipes.rs`).
 */
export const pipeName = (id: string, seat: string | undefined): string =>
	seat && /^[0-9a-f]{32}$/.test(seat)
		? `\\\\.\\pipe\\punktfunk-seat-${seat}-plugin-${id}`
		: `\\\\.\\pipe\\punktfunk-plugin-${id}`;

/**
 * The environment of a plugin in its AppContainer: the pipe the host serves for its id, plain
 * HTTP, no port. The host knows the container by its package, so it sends no bearer. Its page
 * goes over the pipe too, since the container binds nothing the console dials. A seat's runner
 * has its seat's id from its host. A host without the pipe leaves it out and the child dials the
 * port. `exists` is a test seam.
 */
export const containerEnv = (
	unit: Unit,
	options: RunnerOptions,
	exists: (pipe: string) => boolean = fs.existsSync,
): NodeJS.ProcessEnv => {
	const id = unit.manifest?.id;
	const pipe = id ? pipeName(id, process.env.PUNKTFUNK_SEAT_ID) : undefined;
	return {
		...process.env,
		...(options.configDir ? { PUNKTFUNK_CONFIG_DIR: options.configDir } : {}),
		...(id && pipe && exists(pipe)
			? {
					PUNKTFUNK_MGMT_URL: "http://punktfunk.host",
					PUNKTFUNK_MGMT_UNIX: pipe,
					PUNKTFUNK_UI_CHANNEL: id,
				}
			: {}),
	};
};
